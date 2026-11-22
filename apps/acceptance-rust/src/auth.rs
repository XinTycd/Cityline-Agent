mod provider;

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier, password_hash::SaltString};
use askama::Template;
use axum::{
    Extension, Form, Router,
    extract::{DefaultBodyLimit, Query, State, rejection::FormRejection},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    audit, profile,
    refresh::Refresh,
    settings,
    settings::Brand,
    store::Store,
    web::{WebError, database, render},
};

const SESSION: &str = "cityline_session";
const FLOW: &str = "cityline_login_flow";
const BAD_LOGIN: WebError = (
    StatusCode::UNAUTHORIZED,
    "登录未完成，请核对账号或重新发起验证。",
);
const UNAVAILABLE: WebError = (StatusCode::SERVICE_UNAVAILABLE, "登录服务暂时不可用。");

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoginMode {
    #[default]
    Disabled,
    Local,
    Oidc,
    Feishu,
}

impl LoginMode {
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Local => "local",
            Self::Oidc => "oidc",
            Self::Feishu => "feishu",
        }
    }

    fn external(self) -> bool {
        matches!(self, Self::Oidc | Self::Feishu)
    }
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct LoginConfig {
    pub(crate) mode: LoginMode,
    pub(crate) session_minutes: u32,
    pub(crate) provider_timeout: u32,
    pub(crate) username: String,
    password_hash: String,
    pub(crate) issuer: String,
    pub(crate) client_id: String,
    pub(crate) feishu_app_id: String,
}

impl Default for LoginConfig {
    fn default() -> Self {
        Self {
            mode: LoginMode::Disabled,
            session_minutes: 480,
            provider_timeout: 120,
            username: String::new(),
            password_hash: String::new(),
            issuer: String::new(),
            client_id: String::new(),
            feishu_app_id: String::new(),
        }
    }
}

impl LoginConfig {
    pub(crate) fn enabled(&self) -> bool {
        self.mode != LoginMode::Disabled
    }
}

#[derive(Deserialize)]
pub(crate) struct LoginSettingsForm {
    enabled: Option<String>,
    mode: Option<LoginMode>,
    session_minutes: Option<u32>,
    provider_timeout: Option<u32>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
    issuer: Option<String>,
    client_id: Option<String>,
    feishu_app_id: Option<String>,
}

#[derive(Clone)]
pub(crate) struct User {
    pub(crate) display: String,
    pub(crate) identifier: String,
    pub(crate) admin: bool,
    pub(crate) method: LoginMode,
    pub(crate) avatar: Option<reqwest::Url>,
    pub(crate) platform_scope: Uuid,
}

impl User {
    fn demo(platform_scope: Uuid) -> Self {
        Self {
            display: "演示账号".into(),
            identifier: "本机演示".into(),
            admin: true,
            method: LoginMode::Disabled,
            avatar: None,
            platform_scope,
        }
    }

    pub(crate) fn initial(&self) -> char {
        self.display.chars().next().unwrap_or('用')
    }

    pub(crate) fn feishu_id(&self) -> Option<&str> {
        self.identifier.strip_prefix("feishu:")
    }

    pub(crate) fn account_label(&self) -> &str {
        self.feishu_id().unwrap_or(&self.display)
    }

    pub(crate) fn method_label(&self) -> &'static str {
        match self.method {
            LoginMode::Disabled => "本机演示会话",
            LoginMode::Local => "本地账号",
            LoginMode::Oidc => "OIDC 单点登录",
            LoginMode::Feishu => "飞书登录",
        }
    }
}

struct Session {
    user: User,
    expires: Instant,
}

struct Identity {
    config: LoginConfig,
    version: Uuid,
    sessions: HashMap<Uuid, Session>,
    flows: HashMap<Uuid, provider::Flow>,
    attempts: Vec<Instant>,
}

#[derive(Clone)]
pub(crate) struct Auth {
    store: Store,
    origin: String,
    state: Arc<Mutex<Identity>>,
    writes: Arc<tokio::sync::Mutex<()>>,
}

fn site_origin(value: Option<&str>, port: u16) -> Result<String, &'static str> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(format!("http://127.0.0.1:{port}"));
    };
    let url = reqwest::Url::parse(value)
        .map_err(|_| "CITYLINE_PUBLIC_ORIGIN 不是有效的 HTTPS 站点地址。")?;
    if url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("CITYLINE_PUBLIC_ORIGIN 仅接受 HTTPS 站点地址，不含路径或凭据。");
    }
    Ok(url.origin().ascii_serialization())
}

impl Auth {
    pub(crate) fn initialize(store: Store, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let config = settings::read(&store.connect()?, "login")?;
        let configured = std::env::var("CITYLINE_PUBLIC_ORIGIN").ok();
        Ok(Self {
            store,
            origin: site_origin(configured.as_deref().map(str::trim), port)?,
            state: Arc::new(Mutex::new(Identity {
                config,
                version: Uuid::new_v4(),
                sessions: HashMap::new(),
                flows: HashMap::new(),
                attempts: Vec::new(),
            })),
            writes: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    pub(crate) fn origin(&self) -> &str {
        &self.origin
    }

    pub(crate) fn authority(&self) -> &str {
        self.origin.split_once("://").expect("valid site origin").1
    }

    pub(crate) fn secure(&self) -> bool {
        self.origin.starts_with("https://")
    }

    pub(crate) fn callback_url(&self) -> String {
        format!("{}/auth/callback", self.origin)
    }

    pub(crate) fn config(&self) -> LoginConfig {
        self.state.lock().expect("auth lock").config.clone()
    }

    pub(crate) fn login_enabled(&self) -> bool {
        self.state.lock().expect("auth lock").config.enabled()
    }

    pub(crate) fn user(&self, headers: &HeaderMap) -> Option<User> {
        let state = self.state.lock().expect("auth lock");
        if !state.config.enabled() {
            return (!profile::is_signed_out(headers)).then(|| User::demo(state.version));
        }
        let token = read_cookie(headers, SESSION)?;
        state
            .sessions
            .get(&token)
            .filter(|session| session.expires > Instant::now())
            .map(|session| session.user.clone())
    }

    pub(crate) async fn save(&self, form: LoginSettingsForm) -> Result<LoginMode, WebError> {
        let _guard = self.writes.lock().await;
        let mut config = self.config();
        let invalid = |message| (StatusCode::UNPROCESSABLE_ENTITY, message);
        config.mode = if form.enabled.as_deref() == Some("yes") {
            form.mode
                .filter(|mode| *mode != LoginMode::Disabled)
                .ok_or_else(|| invalid("请选择登录方式。"))?
        } else {
            LoginMode::Disabled
        };
        if config.mode == LoginMode::Disabled {
            // Preserve the profile for future enablement.
        } else {
            let session_minutes = form.session_minutes.unwrap_or_default();
            let provider_timeout = form.provider_timeout.unwrap_or(config.provider_timeout);
            if !(5..=720).contains(&session_minutes) || !(15..=300).contains(&provider_timeout) {
                return Err(invalid("请核对会话时长和外部认证超时。"));
            }
            let username = form.username.trim();
            if !(2..=48).contains(&username.chars().count())
                || username.chars().any(char::is_control)
            {
                return Err(invalid("请设置本地管理员账号，用于登录和管理恢复。"));
            }
            config.username = username.to_owned();
            if !form.password.is_empty() {
                if form.password.chars().count() < 12 || form.password.len() > 384 {
                    return Err(invalid("本地密码最少 12 个字符。"));
                }
                let password = form.password;
                config.password_hash = tokio::task::spawn_blocking(move || {
                    let salt = SaltString::encode_b64(Uuid::new_v4().as_bytes()).ok()?;
                    Argon2::default()
                        .hash_password(password.as_bytes(), &salt)
                        .ok()
                        .map(|hash| hash.to_string())
                })
                .await
                .map_err(|_| UNAVAILABLE)?
                .ok_or(UNAVAILABLE)?;
            }
            if config.password_hash.is_empty() {
                return Err(invalid("首次启用时需设置本地管理员密码。"));
            }
            config.session_minutes = session_minutes;
            config.provider_timeout = provider_timeout;
            if let Some(issuer) = form.issuer {
                config.issuer = issuer.trim().to_owned();
            }
            if let Some(client_id) = form.client_id {
                config.client_id = client_id.trim().to_owned();
            }
            if let Some(app_id) = form.feishu_app_id {
                config.feishu_app_id = app_id.trim().to_owned();
            }
            provider::validate_config(&config)?;
        }
        let saved = config.clone();
        let new_mode = config.mode;
        let store = self.store.clone();
        database(move || {
            let mut db = store.connect()?;
            let tx = db.transaction()?;
            settings::write(&tx, "login", &saved)?;
            audit::write(
                &tx,
                "设置",
                "登录设置修改",
                None,
                &format!("工作台登录方式设为 {}。", new_mode.key()),
            )?;
            tx.commit()
        })
        .await?;
        let mut state = self.state.lock().expect("auth lock");
        state.config = config;
        state.version = Uuid::new_v4();
        state.sessions.clear();
        state.flows.clear();
        state.attempts.clear();
        Ok(new_mode)
    }

    fn reserve_session(&self, mut user: User, version: Uuid) -> Result<(Uuid, u64), WebError> {
        let mut state = self.state.lock().expect("auth lock");
        if state.version != version {
            return Err(BAD_LOGIN);
        }
        let now = Instant::now();
        let seconds = u64::from(state.config.session_minutes) * 60;
        state.sessions.retain(|_, value| value.expires > now);
        if state.sessions.len() >= 128 {
            return Err((StatusCode::TOO_MANY_REQUESTS, "工作台会话数已满。"));
        }
        let token = Uuid::new_v4();
        user.platform_scope = token;
        state.sessions.insert(
            token,
            Session {
                user,
                expires: now + Duration::from_secs(seconds),
            },
        );
        Ok((token, seconds))
    }

    async fn complete_login(
        &self,
        user: User,
        version: Uuid,
        action: &'static str,
    ) -> Result<Response, WebError> {
        let identifier = user.identifier.clone();
        let (token, seconds) = self.reserve_session(user, version)?;
        if let Err(error) = audit::record(&self.store, "登录", action, None, identifier).await {
            self.state
                .lock()
                .expect("auth lock")
                .sessions
                .remove(&token);
            return Err(error);
        }
        let mut response = self.cookie_response("/overview", SESSION, &token.to_string(), seconds);
        self.append_cookie(&mut response, FLOW, "", 0);
        Ok(response)
    }

    fn cookie_response(&self, destination: &str, name: &str, value: &str, age: u64) -> Response {
        let mut response = Redirect::to(destination).into_response();
        self.append_cookie(&mut response, name, value, age);
        response
    }

    fn append_cookie(&self, response: &mut Response, name: &str, value: &str, age: u64) {
        let secure = if self.secure() { "; Secure" } else { "" };
        if let Ok(cookie) = HeaderValue::from_str(&format!(
            "{name}={value}; Path=/; Max-Age={age}; HttpOnly; SameSite=Lax{secure}"
        )) {
            response.headers_mut().append(header::SET_COOKIE, cookie);
        }
    }
}

fn read_cookie(headers: &HeaderMap, name: &str) -> Option<Uuid> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find_map(|(key, value)| (key == name).then(|| Uuid::parse_str(value).ok()).flatten())
}

#[derive(Template)]
#[template(path = "auth-sign-in.html")]
struct SignIn {
    mode: LoginMode,
    notice: Option<&'static str>,
    brand: Brand,
}

#[derive(Default, Deserialize)]
struct SignInQuery {
    failed: Option<u8>,
    provider_failed: Option<u8>,
    expired: Option<u8>,
}

async fn sign_in_page(State(auth): State<Auth>, Query(query): Query<SignInQuery>) -> Response {
    let config = auth.config();
    let brand = Brand::read(&auth.store).await.unwrap_or_default();
    if !config.enabled() {
        return profile::sign_in_page(brand);
    }
    let notice = if query.provider_failed == Some(1) {
        Some("外部认证服务暂不可用。可使用本地管理员登录。")
    } else if query.expired == Some(1) {
        Some("外部认证已超时，请重新发起登录，或使用本地管理员登录。")
    } else {
        (query.failed == Some(1)).then_some(BAD_LOGIN.1)
    };
    render(SignIn {
        mode: config.mode,
        notice,
        brand,
    })
}

async fn external_start(
    State(auth): State<Auth>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    match provider::start(State(auth.clone()), headers).await {
        Ok(response) => Ok(response),
        Err((status, _)) => {
            audit::record(
                &auth.store,
                "登录",
                "外部认证暂不可用",
                None,
                format!(
                    "{} 认证入口返回 HTTP {}。",
                    auth.config().mode.key(),
                    status.as_u16()
                ),
            )
            .await?;
            Ok(Redirect::to("/demo/sign-in?provider_failed=1").into_response())
        }
    }
}

#[derive(Deserialize, Default)]
struct PasswordForm {
    username: Option<String>,
    password: Option<String>,
}

async fn sign_in(
    State(auth): State<Auth>,
    form: Result<Form<PasswordForm>, FormRejection>,
) -> Result<Response, WebError> {
    let (config, version) = {
        let mut state = auth.state.lock().expect("auth lock");
        let now = Instant::now();
        state
            .attempts
            .retain(|time| now.duration_since(*time) < Duration::from_secs(60));
        if state.attempts.len() >= 8 {
            return Err((StatusCode::TOO_MANY_REQUESTS, "登录尝试过多，请稍后再试。"));
        }
        if state.config.enabled() {
            state.attempts.push(now);
        }
        (state.config.clone(), state.version)
    };
    if !config.enabled() {
        return Ok(profile::sign_in().await);
    }
    let Form(form) = form.map_err(|_| BAD_LOGIN)?;
    let password = form
        .password
        .filter(|text| text.len() <= 384)
        .ok_or(BAD_LOGIN)?;
    let username = form
        .username
        .filter(|text| text.len() <= 192)
        .ok_or(BAD_LOGIN)?;
    let hash = config.password_hash.clone();
    let password_ok = tokio::task::spawn_blocking(move || {
        PasswordHash::new(&hash).is_ok_and(|parsed| {
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok()
        })
    })
    .await
    .map_err(|_| UNAVAILABLE)?;
    if !password_ok || username != config.username {
        audit::record(
            &auth.store,
            "登录",
            "本地登录失败",
            None,
            "未通过本地账号验证。".into(),
        )
        .await?;
        return Ok(Redirect::to("/demo/sign-in?failed=1").into_response());
    }
    auth.complete_login(
        User {
            display: config.username.clone(),
            identifier: format!("local:{}", config.username),
            admin: true,
            method: LoginMode::Local,
            avatar: None,
            platform_scope: Uuid::nil(),
        },
        version,
        "登录成功",
    )
    .await
}

async fn sign_out(
    State(auth): State<Auth>,
    Extension(refresh): Extension<Refresh>,
    Extension(user): Extension<User>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    refresh.clear_newcloud(user.platform_scope);
    if !auth.config().enabled() {
        return Ok(profile::sign_out().await);
    }
    let user = {
        let mut state = auth.state.lock().expect("auth lock");
        if let Some(flow) = read_cookie(&headers, FLOW) {
            state.flows.remove(&flow);
        }
        read_cookie(&headers, SESSION).and_then(|token| state.sessions.remove(&token))
    };
    if let Some(session) = user {
        audit::record(&auth.store, "登录", "退出", None, session.user.identifier).await?;
    }
    let mut response = auth.cookie_response("/demo/sign-in", SESSION, "", 0);
    auth.append_cookie(&mut response, FLOW, "", 0);
    Ok(response)
}

pub(crate) fn router(auth: Auth) -> Router {
    Router::new()
        .route(
            "/demo/sign-in",
            get(sign_in_page)
                .post(sign_in)
                .layer(DefaultBodyLimit::max(2048)),
        )
        .route("/demo/sign-out", post(sign_out))
        .route("/auth/start", get(external_start))
        .route("/auth/callback", get(provider::callback))
        .with_state(auth)
}
