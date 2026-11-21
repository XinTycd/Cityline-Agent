use axum::{
    extract::{Query, State, rejection::QueryRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Redirect, Response},
};
use openidconnect::{
    AccessTokenHash, AuthorizationCode, ClientId, ClientSecret, CsrfToken, EndpointMaybeSet,
    EndpointNotSet, EndpointSet, IssuerUrl, Nonce, OAuth2TokenResponse, PkceCodeChallenge,
    PkceCodeVerifier, RedirectUrl, Scope, TokenResponse,
    core::{CoreAuthenticationFlow, CoreClient, CoreProviderMetadata},
};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;
use uuid::Uuid;

use super::{Auth, BAD_LOGIN, FLOW, LoginConfig, LoginMode, UNAVAILABLE, User, audit, read_cookie};
use crate::{provider_config::endpoint, web::WebError};
use std::time::{Duration, Instant};

type VerifiedClient = CoreClient<
    EndpointSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointNotSet,
    EndpointMaybeSet,
    EndpointMaybeSet,
>;

enum Provider {
    Oidc {
        client: Box<VerifiedClient>,
        nonce: Nonce,
    },
    Feishu,
}

pub(super) struct Flow {
    state: String,
    provider: Provider,
    verifier: PkceCodeVerifier,
    expires: Instant,
    version: Uuid,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

pub(super) fn validate_config(config: &LoginConfig) -> Result<(), WebError> {
    let invalid = |message| (StatusCode::UNPROCESSABLE_ENTITY, message);
    match config.mode {
        LoginMode::Oidc => {
            let issuer = reqwest::Url::parse(&config.issuer)
                .map_err(|_| invalid("请输入有效的 OIDC Issuer。"))?;
            if issuer.scheme() != "https"
                || issuer.host_str().is_none()
                || issuer.query().is_some()
                || issuer.fragment().is_some()
                || !issuer.username().is_empty()
                || issuer.password().is_some()
                || config.client_id.is_empty()
                || config.client_id.len() > 160
                || env("CITYLINE_OIDC_ISSUER").as_deref() != Some(&config.issuer)
                || env("CITYLINE_OIDC_CLIENT_ID").as_deref() != Some(&config.client_id)
                || env("CITYLINE_OIDC_CLIENT_SECRET").is_none()
                || env("CITYLINE_OIDC_SUBJECTS").is_none()
            {
                return Err(invalid(
                    "请先在服务端配置可信 OIDC Issuer、客户端凭据及允许登录的用户标识。",
                ));
            }
        }
        LoginMode::Feishu
            if !config.feishu_app_id.starts_with("cli_")
                || config.feishu_app_id.len() > 100
                || env("CITYLINE_FEISHU_APP_ID").as_deref() != Some(&config.feishu_app_id)
                || env("CITYLINE_FEISHU_APP_SECRET").is_none()
                || env("CITYLINE_FEISHU_OPEN_IDS").is_none()
                || endpoint("CITYLINE_FEISHU_AUTHORIZE_ENDPOINT").is_none()
                || endpoint("CITYLINE_FEISHU_TOKEN_ENDPOINT").is_none()
                || endpoint("CITYLINE_FEISHU_PROFILE_ENDPOINT").is_none() =>
        {
            return Err(invalid(
                "请先在服务端配置飞书应用凭据、接口地址和允许登录的 open_id。",
            ));
        }
        _ => {}
    }
    Ok(())
}

fn oidc_http(timeout: u32) -> Result<openidconnect::reqwest::Client, WebError> {
    let mut http = openidconnect::reqwest::Client::builder()
        .no_proxy()
        .redirect(openidconnect::reqwest::redirect::Policy::none())
        .https_only(true)
        .timeout(Duration::from_secs(u64::from(timeout.min(25))))
        .connect_timeout(Duration::from_secs(5));
    if let Some(path) = std::env::var_os("CITYLINE_OIDC_CA_FILE") {
        use std::io::Read;
        let mut pem = Vec::new();
        std::fs::File::open(path)
            .map_err(|_| UNAVAILABLE)?
            .take(256 * 1024 + 1)
            .read_to_end(&mut pem)
            .map_err(|_| UNAVAILABLE)?;
        if pem.len() > 256 * 1024 {
            return Err(UNAVAILABLE);
        }
        let authority =
            openidconnect::reqwest::Certificate::from_pem(&pem).map_err(|_| UNAVAILABLE)?;
        http = http.add_root_certificate(authority);
    }
    http.build().map_err(|_| UNAVAILABLE)
}

pub(super) async fn start(
    State(auth): State<Auth>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    if headers.get("host").and_then(|value| value.to_str().ok()) != Some(auth.authority()) {
        return Ok(Redirect::to(&format!("{}/auth/start", auth.origin())).into_response());
    }
    let (config, version) = {
        let state = auth.state.lock().expect("auth lock");
        (state.config.clone(), state.version)
    };
    if !config.mode.external() {
        return Ok(Redirect::to("/demo/sign-in").into_response());
    }
    validate_config(&config)?;
    let callback = auth.callback_url();
    let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
    let (url, state_token, provider) = match config.mode {
        LoginMode::Oidc => {
            let http = oidc_http(config.provider_timeout)?;
            let issuer = IssuerUrl::new(config.issuer).map_err(|_| BAD_LOGIN)?;
            let metadata = CoreProviderMetadata::discover_async(issuer, &http)
                .await
                .map_err(|_| UNAVAILABLE)?;
            if metadata.authorization_endpoint().url().scheme() != "https"
                || metadata
                    .token_endpoint()
                    .is_none_or(|endpoint| endpoint.url().scheme() != "https")
            {
                return Err(UNAVAILABLE);
            }
            let secret = env("CITYLINE_OIDC_CLIENT_SECRET").ok_or(BAD_LOGIN)?;
            let client = CoreClient::from_provider_metadata(
                metadata,
                ClientId::new(config.client_id),
                Some(ClientSecret::new(secret)),
            )
            .set_redirect_uri(RedirectUrl::new(callback).map_err(|_| BAD_LOGIN)?);
            let (url, state, nonce) = client
                .authorize_url(
                    CoreAuthenticationFlow::AuthorizationCode,
                    CsrfToken::new_random,
                    Nonce::new_random,
                )
                .add_scope(Scope::new("profile".into()))
                .set_pkce_challenge(challenge)
                .url();
            (
                url,
                state.secret().to_owned(),
                Provider::Oidc {
                    client: Box::new(client),
                    nonce,
                },
            )
        }
        LoginMode::Feishu => {
            let mut url = endpoint("CITYLINE_FEISHU_AUTHORIZE_ENDPOINT").ok_or(UNAVAILABLE)?;
            let state = CsrfToken::new_random().secret().to_string();
            url.query_pairs_mut()
                .append_pair("client_id", &config.feishu_app_id)
                .append_pair("response_type", "code")
                .append_pair("redirect_uri", &callback)
                .append_pair("state", &state)
                .append_pair("code_challenge", challenge.as_str())
                .append_pair("code_challenge_method", "S256")
                .append_pair("scope", "contact:user.base:readonly");
            (url, state, Provider::Feishu)
        }
        _ => return Err(BAD_LOGIN),
    };
    let id = Uuid::new_v4();
    let now = Instant::now();
    let expires = u64::from(config.provider_timeout);
    let mut identity = auth.state.lock().expect("auth lock");
    identity.flows.retain(|_, flow| flow.expires > now);
    if identity.version != version || identity.flows.len() >= 32 {
        return Err((StatusCode::TOO_MANY_REQUESTS, "认证会话较多，请稍后再试。"));
    }
    identity.flows.insert(
        id,
        Flow {
            state: state_token,
            provider,
            verifier,
            expires: now + Duration::from_secs(expires),
            version,
        },
    );
    Ok(auth.cookie_response(url.as_str(), FLOW, &id.to_string(), expires))
}

#[derive(Deserialize)]
pub(super) struct Callback {
    code: Option<String>,
    state: Option<String>,
}

pub(super) async fn callback(
    State(auth): State<Auth>,
    headers: HeaderMap,
    query: Result<Query<Callback>, QueryRejection>,
) -> Result<Response, WebError> {
    let retry = || auth.cookie_response("/demo/sign-in?failed=1", FLOW, "", 0);
    if headers.get("host").and_then(|value| value.to_str().ok()) != Some(auth.authority()) {
        return Ok(retry());
    }
    let Some(id) = read_cookie(&headers, FLOW) else {
        return Ok(retry());
    };
    let (flow, config, version) = {
        let mut state = auth.state.lock().expect("auth lock");
        (state.flows.remove(&id), state.config.clone(), state.version)
    };
    let Some(flow) = flow else {
        return Ok(retry());
    };
    if flow.expires <= Instant::now() || flow.version != version {
        return rejected(&auth, "认证会话已过期或登录配置已改变。").await;
    }
    let Ok(Query(query)) = query else {
        return rejected(&auth, "认证回调参数无法读取。").await;
    };
    if query.state.as_deref() != Some(flow.state.as_str()) {
        return rejected(&auth, "认证回调状态未通过校验。").await;
    }
    let Some(code) = query.code.filter(|value| {
        !value.is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
    }) else {
        return rejected(&auth, "提供方未返回有效授权。").await;
    };
    let user = match flow.provider {
        Provider::Oidc { client, nonce } => {
            oidc_user(&client, flow.verifier, nonce, code, config.provider_timeout).await
        }
        Provider::Feishu => feishu_user(&auth, &config, flow.verifier, &code).await,
    };
    match user {
        Ok(user) => auth.complete_login(user, version, "外部登录成功").await,
        Err(_) => rejected(&auth, "提供方未通过验证，或账号未获授权。").await,
    }
}

async fn rejected(auth: &Auth, reason: &'static str) -> Result<Response, WebError> {
    audit::record(&auth.store, "登录", "外部登录失败", None, reason.into()).await?;
    Ok(auth.cookie_response("/demo/sign-in?failed=1", FLOW, "", 0))
}

fn allowed(name: &str, id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && !id.chars().any(char::is_control)
        && env(name).is_some_and(|list| list.split(',').map(str::trim).any(|entry| entry == id))
}

async fn oidc_user(
    client: &VerifiedClient,
    verifier: PkceCodeVerifier,
    nonce: Nonce,
    code: String,
    timeout: u32,
) -> Result<User, WebError> {
    let result = client
        .exchange_code(AuthorizationCode::new(code))
        .map_err(|_| BAD_LOGIN)?
        .set_pkce_verifier(verifier)
        .request_async(&oidc_http(timeout)?)
        .await
        .map_err(|_| BAD_LOGIN)?;
    let token = result.id_token().ok_or(BAD_LOGIN)?;
    let verifier = client.id_token_verifier();
    let claims = token.claims(&verifier, &nonce).map_err(|_| BAD_LOGIN)?;
    if let Some(expected) = claims.access_token_hash() {
        let actual = AccessTokenHash::from_token(
            result.access_token(),
            token.signing_alg().map_err(|_| BAD_LOGIN)?,
            token.signing_key(&verifier).map_err(|_| BAD_LOGIN)?,
        )
        .map_err(|_| BAD_LOGIN)?;
        if *expected != actual {
            return Err(BAD_LOGIN);
        }
    }
    let subject = claims.subject().as_str();
    if !allowed("CITYLINE_OIDC_SUBJECTS", subject) {
        return Err(BAD_LOGIN);
    }
    let identifier = format!("oidc:{subject}");
    Ok(User {
        display: claims
            .preferred_username()
            .map(|name| name.as_str())
            .unwrap_or(subject)
            .chars()
            .filter(|ch| !ch.is_control())
            .take(64)
            .collect(),
        identifier,
        admin: allowed("CITYLINE_OIDC_ADMINS", subject),
        method: LoginMode::Oidc,
        avatar: None,
    })
}

#[derive(Deserialize)]
struct FeishuToken {
    code: i64,
    access_token: Option<String>,
}

#[derive(Deserialize)]
struct FeishuProfile {
    code: i64,
    data: Option<FeishuUser>,
}

#[derive(Deserialize)]
struct FeishuUser {
    open_id: String,
    #[serde(default)]
    name: String,
    avatar_thumb: Option<String>,
    avatar_url: Option<String>,
}

fn feishu_avatar(value: &str) -> Option<reqwest::Url> {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return None;
    }
    let url = reqwest::Url::parse(value).ok()?;
    let host = url.host_str()?;
    let source = host == "feishu.cn"
        || host.ends_with(".feishu.cn")
        || host.ends_with(".feishucdn.com")
        || host
            .strip_suffix("-lark-file.byteimg.com")
            .and_then(|prefix| prefix.strip_prefix('p'))
            .is_some_and(|number| {
                !number.is_empty() && number.bytes().all(|byte| byte.is_ascii_digit())
            });
    (source
        && url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

async fn feishu_json<T: DeserializeOwned>(
    response: Result<reqwest::Response, reqwest::Error>,
) -> Result<T, WebError> {
    const MAX_RESPONSE: usize = 64 * 1024;
    let mut response = response
        .and_then(reqwest::Response::error_for_status)
        .map_err(|_| BAD_LOGIN)?;
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| BAD_LOGIN)? {
        if chunk.len() > MAX_RESPONSE - body.len() {
            return Err(BAD_LOGIN);
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body).map_err(|_| BAD_LOGIN)
}

async fn feishu_user(
    auth: &Auth,
    config: &LoginConfig,
    verifier: PkceCodeVerifier,
    code: &str,
) -> Result<User, WebError> {
    let app_secret = env("CITYLINE_FEISHU_APP_SECRET").ok_or(BAD_LOGIN)?;
    let http = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(u64::from(
            config.provider_timeout.min(25),
        )))
        .build()
        .map_err(|_| UNAVAILABLE)?;
    let token_endpoint = endpoint("CITYLINE_FEISHU_TOKEN_ENDPOINT").ok_or(UNAVAILABLE)?;
    let profile_endpoint = endpoint("CITYLINE_FEISHU_PROFILE_ENDPOINT").ok_or(UNAVAILABLE)?;
    let token: FeishuToken = feishu_json(
        http.post(token_endpoint)
            .json(&json!({
                "grant_type": "authorization_code", "client_id": config.feishu_app_id,
                "client_secret": app_secret, "code": code, "redirect_uri": auth.callback_url(),
                "code_verifier": verifier.secret(),
            }))
            .send()
            .await,
    )
    .await?;
    let token = token
        .access_token
        .filter(|token_string| {
            token.code == 0 && !token_string.is_empty() && token_string.len() <= 8192
        })
        .ok_or(BAD_LOGIN)?;
    let profile: FeishuProfile =
        feishu_json(http.get(profile_endpoint).bearer_auth(token).send().await).await?;
    let profile = profile
        .data
        .filter(|user| profile.code == 0 && allowed("CITYLINE_FEISHU_OPEN_IDS", &user.open_id))
        .ok_or(BAD_LOGIN)?;
    let name: String = profile
        .name
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(64)
        .collect();
    let avatar = [
        profile.avatar_thumb.as_deref(),
        profile.avatar_url.as_deref(),
    ]
    .into_iter()
    .flatten()
    .find_map(feishu_avatar);
    Ok(User {
        display: if name.is_empty() {
            profile.open_id.clone()
        } else {
            name
        },
        identifier: format!("feishu:{}", profile.open_id),
        admin: allowed("CITYLINE_FEISHU_ADMINS", &profile.open_id),
        method: LoginMode::Feishu,
        avatar,
    })
}

#[cfg(test)]
mod tests {
    use super::feishu_avatar;

    #[test]
    fn avatar_sources_are_restricted_to_feishu_https_hosts() {
        for source in [
            "https://s3-imfile.feishucdn.com/avatar",
            "https://p3-lark-file.byteimg.com/img/lark-avatar.jpg",
            "https://www.feishu.cn/avatar/icon_thumb",
        ] {
            assert!(feishu_avatar(source).is_some(), "{source}");
        }
        for source in [
            "http://s3-imfile.feishucdn.com/avatar",
            "https://s3-imfile.feishucdn.com.example.net/avatar",
            "https://attacker@p3-lark-file.byteimg.com/avatar",
            "https://p3-lark-file.byteimg.com:8443/avatar",
            "https://127.0.0.1/avatar",
        ] {
            assert!(feishu_avatar(source).is_none(), "{source}");
        }
    }
}
