use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::http::StatusCode;
use serde::Deserialize;

use crate::{provider_config, web::WebError};

const MAINTENANCE: WebError = (StatusCode::SERVICE_UNAVAILABLE, "红豆授权服务暂时不可用。");
const STORED_CONFIG: WebError = (
    StatusCode::SERVICE_UNAVAILABLE,
    "红豆预存账号或授权客户端配置不完整。",
);
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

struct StoredAccount {
    username: String,
    password: String,
    token: Mutex<Option<Token>>,
}

#[derive(Clone)]
pub(crate) struct Redbean {
    client: Option<Arc<Client>>,
    stored: Option<Arc<StoredAccount>>,
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

fn valid_credentials(username: &str, password: &str) -> bool {
    !username.is_empty()
        && username.len() <= 192
        && !username.chars().any(char::is_control)
        && !password.is_empty()
        && password.len() <= 512
        && !password.chars().any(char::is_control)
}

fn valid_access(entry: &Token) -> bool {
    entry
        .expires
        .saturating_duration_since(Instant::now())
        .as_secs()
        >= 30
}

async fn exchange(client: &Client, username: &str, password: &str) -> Result<Token, WebError> {
    let mut response = client
        .exchange(username, password)
        .send()
        .await
        .map_err(|_| MAINTENANCE)?;
    if matches!(
        response.status(),
        StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
    ) {
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
    parse_token(&bytes)
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
        let username = std::env::var("CITYLINE_REDBEAN_USERNAME").unwrap_or_default();
        let password = std::env::var("CITYLINE_REDBEAN_PASSWORD").unwrap_or_default();
        let stored = (!username.is_empty() || !password.is_empty()).then(|| {
            Arc::new(StoredAccount {
                username: username.trim().to_owned(),
                password,
                token: Mutex::new(None),
            })
        });
        Self {
            client: client(),
            stored,
            authorizations: Arc::default(),
            requests: Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    pub(crate) fn configured(&self) -> bool {
        self.client.is_some()
    }

    pub(crate) fn uses_stored_account(&self) -> bool {
        self.stored.is_some()
    }

    fn stored_client(&self, account: &StoredAccount) -> Result<&Client, WebError> {
        self.client
            .as_deref()
            .filter(|_| valid_credentials(&account.username, &account.password))
            .ok_or(STORED_CONFIG)
    }

    pub(crate) fn check_stored_configuration(&self) -> Result<(), WebError> {
        if let Some(account) = &self.stored {
            self.stored_client(account)?;
        }
        Ok(())
    }

    fn access(&self, user: &str) -> Option<String> {
        if let Some(stored) = &self.stored {
            return stored
                .token
                .lock()
                .expect("Redbean stored authorization lock")
                .as_ref()
                .filter(|entry| valid_access(entry))
                .map(|entry| entry.value.clone());
        }
        self.authorizations
            .lock()
            .expect("Redbean authorization lock")
            .get(user)
            .filter(|entry| valid_access(entry))
            .map(|entry| entry.value.clone())
    }

    pub(crate) fn has_token(&self, user: &str) -> bool {
        self.access(user).is_some()
    }

    pub(crate) async fn token_for_task(
        &self,
        user: &str,
    ) -> Result<Option<(String, String)>, WebError> {
        if let Some(token) = self.access(user) {
            return Ok(self
                .client
                .as_ref()
                .map(|client| (token, client.tenant.clone())));
        }
        let Some(stored) = &self.stored else {
            return Ok(None);
        };
        let client = self.stored_client(stored)?;
        let _slot = self.requests.acquire().await.map_err(|_| MAINTENANCE)?;
        if let Some(token) = self.access(user) {
            return Ok(Some((token, client.tenant.clone())));
        }
        let token = exchange(client, &stored.username, &stored.password)
            .await
            .map_err(|error| {
                if error == REJECTED {
                    (
                        error.0,
                        "红豆平台拒绝预存账号授权，请核对服务端凭据和库存权限。",
                    )
                } else {
                    error
                }
            })?;
        let value = token.value.clone();
        *stored
            .token
            .lock()
            .expect("Redbean stored authorization lock") = Some(token);
        Ok(Some((value, client.tenant.clone())))
    }

    pub(crate) fn clear(&self, user: &str) -> bool {
        if self.uses_stored_account() {
            return false;
        }
        self.authorizations
            .lock()
            .expect("Redbean authorization lock")
            .remove(user)
            .is_some()
    }

    pub(super) fn clear_rejected(&self, user: &str, rejected: &str) {
        if let Some(stored) = &self.stored {
            let mut token = stored
                .token
                .lock()
                .expect("Redbean stored authorization lock");
            if token.as_ref().is_some_and(|entry| entry.value == rejected) {
                *token = None;
            }
            return;
        }
        let mut entries = self
            .authorizations
            .lock()
            .expect("Redbean authorization lock");
        if entries
            .get(user)
            .is_some_and(|entry| entry.value == rejected)
        {
            entries.remove(user);
        }
    }

    pub(crate) async fn authorize(
        &self,
        user: &str,
        username: &str,
        password: &str,
    ) -> Result<(), WebError> {
        if self.uses_stored_account() {
            return Err((StatusCode::CONFLICT, "红豆已使用服务端预存账号。"));
        }
        let client = self.client.as_ref().ok_or((
            StatusCode::SERVICE_UNAVAILABLE,
            "本机尚未配置红豆 OAuth 客户端。请使用红豆官方登录窗口。",
        ))?;
        let username = username.trim();
        if !valid_credentials(username, password) {
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
        let token = exchange(client, username, password).await?;
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
        let mut auth = Redbean::from_environment();
        auth.stored = None;
        auth.authorizations
            .lock()
            .unwrap()
            .insert("operator".into(), good);
        assert!(auth.has_token("operator"));
        assert!(!auth.has_token("another-operator"));
        auth.clear_rejected("operator", "previous-bearer");
        assert!(auth.has_token("operator"));
        auth.clear_rejected("operator", "fixture-bearer");
        assert!(!auth.has_token("operator"));
    }

    #[tokio::test]
    async fn stored_account_reuses_its_own_token_and_disallows_manual_override() {
        let mut auth = Redbean::from_environment();
        auth.client = None;
        auth.stored = Some(Arc::new(StoredAccount {
            username: "fixture-platform".into(),
            password: "fixture-password".into(),
            token: Mutex::new(Some(
                parse_token(
                    br#"{"code":0,"data":{"access_token":"fixture-bearer","expires_in":600}}"#,
                )
                .unwrap(),
            )),
        }));
        assert_eq!(auth.check_stored_configuration(), Err(STORED_CONFIG));
        auth.client = Some(Arc::new(Client {
            id: "fixture-client".into(),
            secret: "fixture-secret".into(),
            endpoint: "https://inventory.example.invalid/token".parse().unwrap(),
            tenant: "1".into(),
            http: reqwest::Client::new(),
        }));
        assert_eq!(auth.check_stored_configuration(), Ok(()));
        assert_eq!(
            auth.token_for_task("first-operator").await.unwrap(),
            Some(("fixture-bearer".into(), "1".into()))
        );
        assert!(auth.has_token("first-operator"));
        assert!(auth.has_token("second-operator"));
        assert!(!auth.clear("first-operator"));
        assert_eq!(
            auth.authorize("first-operator", "override", "fixture-password")
                .await
                .unwrap_err()
                .0,
            StatusCode::CONFLICT,
        );
        auth.clear_rejected("second-operator", "previous-bearer");
        assert!(auth.has_token("second-operator"));
        auth.clear_rejected("second-operator", "fixture-bearer");
        assert!(!auth.has_token("first-operator"));
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
