use std::sync::{Arc, RwLock};

use askama::Template;
use axum::{
    Extension, Form, Json, Router,
    extract::{DefaultBodyLimit, Multipart, Query, State},
    http::{StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

use crate::{
    audit,
    auth::{Auth, LoginMode, User},
    refresh::Refresh,
    store::Store,
    web::{WebError, database, render},
};

const MAX_LOGO: usize = 192 * 1024;
pub(crate) const DEFAULT_DESCRIPTION: &str = "户外媒体 Agent 提案工作台";
pub(crate) const LEGACY_DESCRIPTIONS: &[&str] = &[
    "户外媒体提案",
    "户外媒体Agent工作台",
    "户外媒体 Agent 工作台",
    "户外媒体提案工作台",
];

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct Brand {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) logo: Option<String>,
}

impl Brand {
    pub(crate) async fn read(store: &Store) -> Result<Self, WebError> {
        let store = store.clone();
        database(move || read(&store.connect()?, "brand")).await
    }
}

impl Default for Brand {
    fn default() -> Self {
        Self {
            name: "城线".into(),
            description: DEFAULT_DESCRIPTION.into(),
            logo: None,
        }
    }
}

pub(crate) fn read<T: DeserializeOwned + Default>(
    db: &rusqlite::Connection,
    key: &str,
) -> rusqlite::Result<T> {
    let value: Option<String> = db
        .query_row(
            "SELECT value FROM system_settings WHERE key=?1",
            [key],
            |row| row.get(0),
        )
        .optional()?;
    value
        .as_deref()
        .map(serde_json::from_str)
        .transpose()
        .map(Option::unwrap_or_default)
        .map_err(|error| {
            rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Text,
                Box::new(error),
            )
        })
}

pub(crate) fn write(
    db: &rusqlite::Connection,
    key: &str,
    value: &impl Serialize,
) -> rusqlite::Result<()> {
    let payload = serde_json::to_string(value)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    db.execute("INSERT INTO system_settings (key,value) VALUES (?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value", params![key, payload])?;
    Ok(())
}

#[derive(Clone)]
pub(crate) struct Settings {
    store: Store,
    brand: Arc<RwLock<Brand>>,
    updates: Arc<tokio::sync::Mutex<()>>,
    pub(crate) auth: Auth,
}

impl Settings {
    pub(crate) fn initialize(store: Store, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        let db = store.connect()?;
        let mut brand: Brand = read(&db, "brand")?;
        if LEGACY_DESCRIPTIONS.contains(&brand.description.trim()) {
            brand.description = DEFAULT_DESCRIPTION.into();
            write(&db, "brand", &brand)?;
        }
        let auth = Auth::initialize(store.clone(), port)?;
        Ok(Self {
            store,
            brand: Arc::new(RwLock::new(brand)),
            updates: Arc::new(tokio::sync::Mutex::new(())),
            auth,
        })
    }
}

#[derive(Template)]
#[template(path = "settings.html")]
struct SettingsPage {
    heading: &'static str,
    section: &'static str,
    tab: &'static str,
    saved: bool,
    brand: Brand,
    login: crate::auth::LoginConfig,
    callback_url: String,
    redbean_configured: bool,
    redbean_stored: bool,
    redbean_authorized: bool,
    redbean_issue: Option<&'static str>,
}

#[derive(Deserialize)]
struct SettingsQuery {
    tab: Option<String>,
    saved: Option<u8>,
}

async fn page(
    State(settings): State<Settings>,
    Extension(refresh): Extension<Refresh>,
    Extension(user): Extension<User>,
    Query(query): Query<SettingsQuery>,
) -> Response {
    let brand = settings.brand.read().expect("brand lock").clone();
    render(SettingsPage {
        heading: "系统设置",
        section: "settings",
        tab: match query.tab.as_deref() {
            Some("login") => "login",
            Some("platforms") => "platforms",
            _ => "brand",
        },
        saved: query.saved == Some(1),
        brand,
        login: settings.auth.config(),
        callback_url: settings.auth.callback_url(),
        redbean_configured: refresh.redbean.configured(),
        redbean_stored: refresh.redbean.uses_stored_account(),
        redbean_authorized: refresh.redbean.has_token(&user.identifier),
        redbean_issue: refresh
            .redbean
            .check_stored_configuration()
            .err()
            .map(|(_, message)| message),
    })
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct RedbeanCredentials {
    username: String,
    password: String,
}

async fn authorize_redbean(
    State(settings): State<Settings>,
    Extension(refresh): Extension<Refresh>,
    Extension(user): Extension<User>,
    Form(credentials): Form<RedbeanCredentials>,
) -> Result<Redirect, WebError> {
    let stored = refresh.redbean.uses_stored_account();
    let result = if stored {
        refresh
            .redbean
            .token_for_task(&user.identifier)
            .await
            .map(|_| ())
    } else {
        refresh
            .redbean
            .authorize(
                &user.identifier,
                &credentials.username,
                &credentials.password,
            )
            .await
    };
    audit::record(
        &settings.store,
        "平台",
        if result.is_ok() {
            "红豆 OAuth 已授权"
        } else {
            "红豆 OAuth 授权未完成"
        },
        None,
        if result.is_ok() && stored {
            "服务端预存账号已取得临时库存令牌。".into()
        } else if result.is_ok() {
            "已取得当前操作员的临时库存令牌。".into()
        } else {
            "请核对红豆平台授权条件。".into()
        },
    )
    .await?;
    result?;
    Ok(Redirect::to("/settings?tab=platforms"))
}

async fn clear_redbean(
    State(settings): State<Settings>,
    Extension(refresh): Extension<Refresh>,
    Extension(user): Extension<User>,
) -> Result<Redirect, WebError> {
    if refresh.redbean.clear(&user.identifier) {
        audit::record(
            &settings.store,
            "平台",
            "清除红豆本地授权",
            None,
            "后续任务需重新授权。".into(),
        )
        .await?;
    }
    Ok(Redirect::to("/settings?tab=platforms"))
}

async fn site(State(settings): State<Settings>, Extension(user): Extension<User>) -> Json<Value> {
    let brand = settings.brand.read().expect("brand lock");
    Json(json!({
        "name": brand.name,
        "description": brand.description,
        "logo": if brand.logo.is_some() { "/assets/site-logo.png" } else { "/assets/cityline.svg" },
        "user": user.display,
        "account_label": user.account_label(),
        "admin": user.admin,
        "authenticated": user.method != LoginMode::Disabled,
        "initial": user.initial(),
        "avatar": user.avatar.as_ref().map(reqwest::Url::as_str),
    }))
}

async fn logo(State(settings): State<Settings>) -> Response {
    let brand = settings.brand.read().expect("brand lock");
    if let Some(bytes) = brand
        .logo
        .as_deref()
        .and_then(|encoded| STANDARD.decode(encoded).ok())
    {
        return ([(header::CONTENT_TYPE, "image/png")], bytes).into_response();
    }
    Redirect::temporary("/assets/cityline.svg").into_response()
}

async fn save_brand(
    State(settings): State<Settings>,
    mut body: Multipart,
) -> Result<Redirect, WebError> {
    const INVALID: WebError = (
        StatusCode::UNPROCESSABLE_ENTITY,
        "请填写站点名称和简介，Logo 请使用 192 KiB 内的 PNG。",
    );
    let mut name = None;
    let mut description = None;
    let mut png = None;
    let mut remove = false;
    while let Some(field) = body.next_field().await.map_err(|_| INVALID)? {
        match field.name() {
            Some("name") if name.is_none() => {
                name = Some(field.text().await.map_err(|_| INVALID)?.trim().to_owned())
            }
            Some("description") if description.is_none() => {
                description = Some(field.text().await.map_err(|_| INVALID)?.trim().to_owned())
            }
            Some("reset_logo") => remove = field.text().await.map_err(|_| INVALID)? == "yes",
            Some("logo") if png.is_none() => {
                let data = field.bytes().await.map_err(|_| INVALID)?;
                if !data.is_empty() {
                    if data.len() > MAX_LOGO
                        || !data.starts_with(b"\x89PNG\r\n\x1a\n")
                        || data.get(12..16) != Some(b"IHDR")
                    {
                        return Err(INVALID);
                    }
                    let width = u32::from_be_bytes(
                        data.get(16..20)
                            .and_then(|bytes| bytes.try_into().ok())
                            .ok_or(INVALID)?,
                    );
                    let height = u32::from_be_bytes(
                        data.get(20..24)
                            .and_then(|bytes| bytes.try_into().ok())
                            .ok_or(INVALID)?,
                    );
                    if width == 0 || height == 0 || width > 512 || height > 512 {
                        return Err((
                            StatusCode::UNPROCESSABLE_ENTITY,
                            "Logo 的宽度和高度均需在 1 到 512 像素之间。",
                        ));
                    }
                    png = Some(STANDARD.encode(data));
                }
            }
            _ => return Err(INVALID),
        }
    }
    let name = name
        .filter(|text| {
            !text.is_empty() && text.chars().count() <= 24 && !text.chars().any(char::is_control)
        })
        .ok_or(INVALID)?;
    let description = description
        .filter(|text| {
            !text.is_empty() && text.chars().count() <= 80 && !text.chars().any(char::is_control)
        })
        .ok_or(INVALID)?;
    let _guard = settings.updates.lock().await;
    let current = settings.brand.read().expect("brand lock").logo.clone();
    let brand = Brand {
        name,
        description,
        logo: if remove { None } else { png.or(current) },
    };
    let saved = brand.clone();
    let store = settings.store.clone();
    database(move || {
        let mut db = store.connect()?;
        let tx = db.transaction()?;
        write(&tx, "brand", &saved)?;
        audit::write(
            &tx,
            "设置",
            "界面设置修改",
            None,
            "已保存站点名称、简介和品牌图标配置。",
        )?;
        tx.commit()
    })
    .await?;
    *settings.brand.write().expect("brand lock") = brand;
    Ok(Redirect::to("/settings?saved=1"))
}

async fn save_login(
    State(settings): State<Settings>,
    Form(form): Form<crate::auth::LoginSettingsForm>,
) -> Result<Response, WebError> {
    let mode = settings.auth.save(form).await?;
    if !matches!(mode, LoginMode::Disabled) && settings.auth.secure() {
        return Ok(
            Redirect::to(&format!("{}/demo/sign-in", settings.auth.origin())).into_response(),
        );
    }
    Ok(Redirect::to(if matches!(mode, LoginMode::Disabled) {
        "/settings?tab=login&saved=1"
    } else {
        "/demo/sign-in"
    })
    .into_response())
}

pub(crate) fn router(settings: Settings) -> Router {
    Router::new()
        .route("/settings", get(page))
        .route(
            "/settings/brand",
            post(save_brand).layer(DefaultBodyLimit::max(MAX_LOGO + 8192)),
        )
        .route(
            "/settings/login",
            post(save_login).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/api/site", get(site))
        .route(
            "/settings/redbean",
            post(authorize_redbean).layer(DefaultBodyLimit::max(4096)),
        )
        .route("/settings/redbean/clear", post(clear_redbean))
        .route("/assets/site-logo.png", get(logo))
        .with_state(settings)
}
