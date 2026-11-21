use askama::Template;
use axum::{
    Router,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::get,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};

use crate::{
    agents::Agents, audit, auth::Auth, media, profile, refresh, settings::Settings, store::Store,
};

pub(crate) const LOGO: &str = include_str!("../assets/cityline.svg");
pub(crate) const CSS: &str = include_str!("../assets/workbench.css");
const REPORT_POLICY: &str = "default-src 'none'; style-src 'unsafe-inline'; img-src data:; base-uri 'none'; frame-ancestors 'none'; form-action 'none'";
const REPORT_DOCUMENT_STYLE: &str = include_str!("../assets/report-document.css");
const REPORT_MAP_STYLE: &str = include_str!("../assets/report-map.css");
const REPORT_MAP_SCRIPT: &str = include_str!("../assets/report-map.js");
const POLICY: &str = "default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; connect-src 'self'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

fn workspace_policy(avatar: Option<&reqwest::Url>) -> HeaderValue {
    avatar
        .and_then(|url| {
            let origin = url.origin().ascii_serialization();
            let policy = POLICY.replace("img-src 'self';", &format!("img-src 'self' {origin};"));
            HeaderValue::from_str(&policy).ok()
        })
        .unwrap_or_else(|| HeaderValue::from_static(POLICY))
}
pub(crate) type WebError = (StatusCode, &'static str);
pub(crate) const LIST_LIMIT: usize = 50;
const PAGE_SIZES: [u32; 4] = [10, 20, 50, 100];

pub(crate) fn report_response(html: String) -> Response {
    const META: &str = "<meta http-equiv=\"Content-Security-Policy\" content=\"";
    // Browsers normalize CRLF before computing the hash of inline scripts.
    let script = REPORT_MAP_SCRIPT.replace("\r\n", "\n");
    let hash = STANDARD.encode(Sha256::digest(script.as_bytes()));
    let script_policy = format!("script-src 'sha256-{hash}'");
    let html = html
        .replacen(META, &format!("{META}{script_policy}; "), 1)
        .replacen(
            "</head>",
            &format!("<style id=\"cityline-report-ui\">{REPORT_DOCUMENT_STYLE}\n{REPORT_MAP_STYLE}</style>\n</head>"),
            1,
        )
        .replacen("</body>", &format!("<script>{script}</script>\n</body>"), 1);
    let mut response = Html(html).into_response();
    let policy = format!("{REPORT_POLICY}; {script_policy}");
    response.headers_mut().insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_str(&policy).expect("valid report policy"),
    );
    response
}

pub(crate) struct ListPages {
    pub(crate) page: u32,
    pub(crate) previous: Option<u32>,
    pub(crate) next: Option<u32>,
    pub(crate) size: u32,
    pub(crate) sizes: [u32; 4],
    pub(crate) total: Option<i64>,
    pub(crate) page_count: u32,
}

impl ListPages {
    pub(crate) fn number(value: Option<u32>) -> u32 {
        value.unwrap_or(1).max(1)
    }

    pub(crate) fn offset(page: u32, size: u32) -> i64 {
        i64::from(page.saturating_sub(1)) * i64::from(size)
    }

    pub(crate) fn size(value: Option<u32>, default: u32) -> u32 {
        value
            .filter(|size| PAGE_SIZES.contains(size))
            .unwrap_or(default)
    }

    pub(crate) fn counted(page: Option<u32>, size: u32, total: i64) -> Self {
        let page_count = (total.saturating_add(i64::from(size) - 1) / i64::from(size))
            .clamp(1, i64::from(u32::MAX)) as u32;
        let page = Self::number(page).min(page_count);
        Self {
            page,
            previous: (page > 1).then(|| page - 1),
            next: (page < page_count).then(|| page + 1),
            size,
            sizes: PAGE_SIZES,
            total: Some(total),
            page_count,
        }
    }

    pub(crate) fn new<T>(page: u32, records: &mut Vec<T>) -> Self {
        let has_next = records.len() > LIST_LIMIT;
        records.truncate(LIST_LIMIT);
        Self {
            page,
            previous: (page > 1).then(|| page - 1),
            next: page.checked_add(1).filter(|_| has_next),
            size: LIST_LIMIT as u32,
            sizes: PAGE_SIZES,
            total: None,
            page_count: 0,
        }
    }
}
const UNAVAILABLE: WebError = (StatusCode::INTERNAL_SERVER_ERROR, "工作台暂时不可用。");

pub(crate) fn render(template: impl Template) -> Response {
    match template.render() {
        Ok(content) => Html(content).into_response(),
        Err(_) => UNAVAILABLE.into_response(),
    }
}

pub(crate) async fn database<T: Send + 'static>(
    task: impl FnOnce() -> rusqlite::Result<T> + Send + 'static,
) -> Result<T, WebError> {
    tokio::task::spawn_blocking(task)
        .await
        .map_err(|_| UNAVAILABLE)?
        .map_err(|_| UNAVAILABLE)
}

#[derive(Clone)]
struct Guard {
    port: u16,
    store: Store,
    auth: Auth,
    refresh: refresh::Refresh,
}

async fn local_only(State(guard): State<Guard>, mut request: Request, next: Next) -> Response {
    let port = guard.port;
    let authority = request
        .headers()
        .get(header::HOST)
        .and_then(|value| value.to_str().ok());
    let loopback = ["127.0.0.1", "localhost"]
        .iter()
        .any(|host| authority == Some(&format!("{host}:{port}")));
    let path = request.uri().path();
    let import = if request.headers().contains_key("x-cityline-media-import") {
        let proof = path == "/api/media-proposals/platform-evidence";
        let route = (path == "/api/media-proposals/import" && request.method() == Method::POST)
            || (proof && matches!(*request.method(), Method::GET | Method::POST));
        let authorized = (route && loopback)
            .then(|| guard.refresh.authorized(request.headers(), proof))
            .flatten();
        let Some(authorized) = authorized else {
            return StatusCode::FORBIDDEN.into_response();
        };
        Some(authorized)
    } else {
        None
    };
    let secure_request = guard.auth.secure() && authority == Some(guard.auth.authority());
    if !loopback && !secure_request {
        return StatusCode::FORBIDDEN.into_response();
    }
    if guard.auth.secure() && import.is_none() {
        if guard.auth.login_enabled() {
            if !secure_request {
                return Redirect::to(guard.auth.origin()).into_response();
            }
        } else if secure_request {
            return (StatusCode::FORBIDDEN, "请先在本机设置并开启登录。").into_response();
        }
    }
    if request.method() == Method::POST {
        let origin = request
            .headers()
            .get(header::ORIGIN)
            .and_then(|value| value.to_str().ok());
        let fetch_site = request.headers().get("sec-fetch-site");
        let expected = if secure_request {
            Some(guard.auth.origin().to_owned())
        } else {
            authority.map(|host| format!("http://{host}"))
        };
        if origin != expected.as_deref() || fetch_site.is_some_and(|value| value != "same-origin") {
            return StatusCode::FORBIDDEN.into_response();
        }
    }
    let public = path.starts_with("/assets/")
        || matches!(path, "/demo/sign-in" | "/auth/start" | "/auth/callback");
    let user = guard.auth.user(request.headers());
    let policy = workspace_policy(user.as_ref().and_then(|user| user.avatar.as_ref()));
    let mut response = match user {
        None if !public && import.is_none() => {
            if path.starts_with("/api/") {
                (StatusCode::UNAUTHORIZED, "请重新登录工作台。").into_response()
            } else {
                Redirect::to("/demo/sign-in").into_response()
            }
        }
        Some(ref user)
            if !user.admin
                && (path.starts_with("/settings")
                    || path.starts_with("/agents")
                    || path == "/audit") =>
        {
            (StatusCode::FORBIDDEN, "此栏目仅对管理员开放。").into_response()
        }
        user => {
            let api = path.starts_with("/api/");
            if let Some(user) = user {
                request.extensions_mut().insert(user);
            }
            if let Some(import) = import {
                request.extensions_mut().insert(import);
            }
            let result = next.run(request).await;
            if api && result.status().is_server_error() {
                let _ = audit::record(
                    &guard.store,
                    "API 服务",
                    "请求异常",
                    None,
                    format!("本地 API 返回 HTTP {}。", result.status().as_u16()),
                )
                .await;
            }
            result
        }
    };
    let headers = response.headers_mut();
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("same-origin"),
    );
    headers
        .entry(header::CONTENT_SECURITY_POLICY)
        .or_insert(policy);
    if secure_request {
        headers.insert(
            header::STRICT_TRANSPORT_SECURITY,
            HeaderValue::from_static("max-age=31536000"),
        );
    }
    response
}

pub(crate) fn app(port: u16, store: Store, agents: Agents, settings: Settings) -> Router {
    let refresh = refresh::Refresh::new(port);
    let auth = settings.auth.clone();
    let guard = Guard {
        port,
        auth: auth.clone(),
        refresh: refresh.clone(),
        store: store.clone(),
    };
    Router::new()
        .route("/", get(|| async { Redirect::to("/media-proposals") }))
        .route("/profile", get(profile::page))
        .route("/connections", get(profile::connections))
        .merge(crate::auth::router(auth))
        .merge(crate::settings::router(settings).layer(axum::Extension(refresh.clone())))
        .merge(media::router(
            store.clone(),
            agents.clone(),
            refresh.clone(),
        ))
        .merge(audit::router(store))
        .merge(crate::agents::router(agents))
        .merge(refresh::router(refresh))
        .route(
            "/assets/cityline.svg",
            get(|| async { ([(header::CONTENT_TYPE, "image/svg+xml")], LOGO) }),
        )
        .route(
            "/assets/media-icon.svg",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "image/svg+xml")],
                    include_str!("../assets/lucide/clipboard-list.svg"),
                )
            }),
        )
        .route(
            "/assets/workbench.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css; charset=utf-8")], CSS) }),
        )
        .route(
            "/assets/workspace.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
                    include_str!("../assets/workspace.js"),
                )
            }),
        )
        .layer(middleware::from_fn_with_state(guard, local_only))
}
