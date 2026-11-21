use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use serde::Deserialize;

use crate::{provider_config, web::WebError};

const MAINTENANCE: WebError = (StatusCode::SERVICE_UNAVAILABLE, "红豆授权服务暂时不可用。");
const REJECTED: WebError = (
    StatusCode::UNPROCESSABLE_ENTITY,
    "红豆授权失败，请核对平台账号、密码和库存权限。",
);

struct Client {
    id: String,
    secret: String,
    endpoint: reqwest::Url,
    tenant: String,
    http: reqwest::Client,
}

impl Client {
    fn exchange(&self, username: &str, password: &str) -> reqwest::RequestBuilder {
        self.http
            .post(self.endpoint.clone())
            .basic_auth(&self.id, Some(&self.secret))
            .header("tenant-id", &self.tenant)
            .form(&[
                ("grant_type", "password"),
                ("username", username),
                ("password", password),
            ])
    }
}

struct Token {
    value: String,
    expires: Instant,
}

#[derive(Clone)]
pub(crate) struct Redbean {
    client: Option<Arc<Client>>,
    authorizations: Arc<Mutex<HashMap<String, Token>>>,
    requests: Arc<tokio::sync::Semaphore>,
}

#[derive(Deserialize)]
struct TokenResponse {
    code: i64,
    data: Option<TokenData>,
}

#[derive(Deserialize)]
struct TokenData {
    access_token: String,
    expires_in: u64,
}

fn parse_token(bytes: &[u8]) -> Result<Token, WebError> {
    let result: TokenResponse = serde_json::from_slice(bytes).map_err(|_| MAINTENANCE)?;
    let data = result.data.filter(|_| result.code == 0).ok_or(REJECTED)?;
    if !valid_bearer(&data.access_token) || data.expires_in <= 30 {
        return Err(MAINTENANCE);
    }
    Ok(Token {
        value: data.access_token,
        expires: Instant::now() + Duration::from_secs(data.expires_in.min(86_400) - 15),
    })
}

fn valid_bearer(value: &str) -> bool {
    (6..=4096).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._~-".contains(&byte))
}

impl Redbean {
    pub(crate) fn from_environment() -> Self {
        let client = || {
            let id = std::env::var("CITYLINE_REDBEAN_OAUTH_CLIENT_ID").ok()?;
            let secret = std::env::var("CITYLINE_REDBEAN_OAUTH_CLIENT_SECRET").ok()?;
            let endpoint = provider_config::endpoint("CITYLINE_REDBEAN_OAUTH_ENDPOINT")?;
            let origin = provider_config::origin("CITYLINE_REDBEAN_ORIGIN")?;
            let tenant = std::env::var("CITYLINE_REDBEAN_TENANT_ID").unwrap_or_else(|_| "1".into());
            if id.is_empty() || id.contains(':') || secret.is_empty() {
                return None;
            }
            if endpoint.origin() != origin.origin()
                || tenant.is_empty()
                || tenant.len() > 12
                || !tenant.bytes().all(|byte| byte.is_ascii_digit())
            {
                return None;
            }
            let http = reqwest::Client::builder()
                .https_only(true)
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(15))
                .build()
                .ok()?;
            Some(Arc::new(Client {
                id,
                secret,
                endpoint,
                tenant,
                http,
            }))
        };
        Self {
            client: client(),
            authorizations: Arc::default(),
            requests: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub(crate) fn configured(&self) -> bool {
        self.client.is_some()
    }

    fn access(&self, user: &str, minimum_seconds: u64) -> Option<String> {
        self.authorizations
            .lock()
            .expect("Redbean authorization lock")
            .get(user)
            .filter(|entry| {
                entry
                    .expires
                    .saturating_duration_since(Instant::now())
                    .as_secs()
                    >= minimum_seconds
            })
            .map(|entry| entry.value.clone())
    }

    pub(crate) fn has_token(&self, user: &str) -> bool {
        self.access(user, 30).is_some()
    }

    pub(super) fn token_for_task(&self, user: &str) -> Option<(String, String)> {
        Some((self.access(user, 30)?, self.client.as_ref()?.tenant.clone()))
    }

    pub(crate) fn clear(&self, user: &str) -> bool {
        self.authorizations
            .lock()
            .expect("Redbean authorization lock")
            .remove(user)
            .is_some()
    }

    pub(crate) async fn authorize(
        &self,
        user: &str,
        username: &str,
        password: &str,
    ) -> Result<(), WebError> {
        let client = self.client.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "本机尚未配置红豆 OAuth 客户端。请使用红豆官方登录窗口。",
        ))?;
        let username = username.trim();
        if username.is_empty()
            || username.len() > 192
            || username.chars().any(char::is_control)
            || password.is_empty()
            || password.len() > 512
            || password.chars().any(char::is_control)
        {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "请填写有效的红豆账号和密码。",
            ));
        }
        let _slot = self.requests.try_acquire().map_err(|_| {
            (
                StatusCode::TOO_MANY_REQUESTS,
                "已有红豆授权请求，请稍后重试。",
            )
        })?;
        let mut response = client
            .exchange(username, password)
            .send()
            .await
            .map_err(|_| MAINTENANCE)?;
        if response.status().is_client_error() {
            return Err(REJECTED);
        }
        if !response.status().is_success() {
            return Err(MAINTENANCE);
        }
        let mut bytes = Vec::new();
        const MAX_RESPONSE: usize = 32 * 1024;
        while let Some(chunk) = response.chunk().await.map_err(|_| MAINTENANCE)? {
            if chunk.len() > MAX_RESPONSE - bytes.len() {
                return Err(MAINTENANCE);
            }
            bytes.extend_from_slice(&chunk);
        }
        let token = parse_token(&bytes)?;
        let mut entries = self
            .authorizations
            .lock()
            .expect("Redbean authorization lock");
        entries.retain(|_, entry| entry.expires > Instant::now());
        if entries.len() >= 128 && !entries.contains_key(user) {
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "红豆授权会话已满，请稍后再试。",
            ));
        }
        entries.insert(user.to_owned(), token);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_contract_validates_success_rejection_and_expiry() {
        let good =
            parse_token(br#"{"code":0,"data":{"access_token":"fixture-bearer","expires_in":600}}"#)
                .unwrap();
        assert!(good.expires > Instant::now());
        assert_eq!(
            parse_token(br#"{"code":400,"data":null}"#).err(),
            Some(REJECTED)
        );
        assert_eq!(
            parse_token(br#"{"code":0,"data":{"access_token":"fixture bearer","expires_in":600}}"#)
                .err(),
            Some(MAINTENANCE)
        );
        assert_eq!(
            parse_token(br#"{"code":0,"data":{"access_token":"fixture-bearer","expires_in":10}}"#)
                .err(),
            Some(MAINTENANCE)
        );
        let auth = Redbean::from_environment();
        auth.authorizations
            .lock()
            .unwrap()
            .insert("operator".into(), good);
        assert!(auth.has_token("operator"));
        assert!(!auth.has_token("another-operator"));
        assert!(auth.clear("operator"));
        assert!(!auth.has_token("operator"));
    }

    #[test]
    fn exchange_builds_the_official_basic_and_form_request() {
        let client = Client {
            id: "fixture-client".into(),
            secret: "fixture:secret".into(),
            endpoint: "https://inventory.example.invalid/token".parse().unwrap(),
            tenant: "23".into(),
            http: reqwest::Client::new(),
        };
        let request = client.exchange("a+b", "甲&乙").build().unwrap();
        assert_eq!(request.method(), reqwest::Method::POST);
        assert_eq!(request.url(), &client.endpoint);
        assert_eq!(request.headers()["tenant-id"], "23");
        assert_eq!(
            request.headers()["content-type"],
            "application/x-www-form-urlencoded"
        );
        assert_eq!(
            request.headers()["authorization"],
            "Basic Zml4dHVyZS1jbGllbnQ6Zml4dHVyZTpzZWNyZXQ="
        );
        assert!(request.headers()["authorization"].is_sensitive());
        assert_eq!(
            request.body().and_then(reqwest::Body::as_bytes),
            Some(b"grant_type=password&username=a%2Bb&password=%E7%94%B2%26%E4%B9%99".as_slice())
        );
    }
}
