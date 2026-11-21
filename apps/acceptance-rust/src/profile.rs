use askama::Template;
use axum::{
    Extension,
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Redirect, Response},
};

use crate::{auth::User, provider_config, web::render};

const SIGNED_OUT: &str = "hackson_demo_signed_out=1";

#[derive(Template)]
#[template(path = "profile.html")]
struct Profile {
    heading: &'static str,
    section: &'static str,
    user: User,
}

#[derive(Template)]
#[template(path = "connections.html")]
struct Connections {
    heading: &'static str,
    section: &'static str,
    cloud: Option<String>,
    red: Option<String>,
}

#[derive(Template)]
#[template(path = "sign-in.html")]
struct SignIn;

pub(crate) async fn page(Extension(user): Extension<User>) -> Response {
    render(Profile {
        heading: "个性化设置",
        section: "profile",
        user,
    })
}

pub(crate) async fn connections() -> Response {
    render(Connections {
        heading: "平台连接",
        section: "connections",
        cloud: provider_config::login_url("CITYLINE_NEWCLOUD_ORIGIN", "CITYLINE_NEWCLOUD_LOGIN")
            .map(|url| url.to_string()),
        red: provider_config::login_url("CITYLINE_REDBEAN_ORIGIN", "CITYLINE_REDBEAN_LOGIN")
            .map(|url| url.to_string()),
    })
}
pub(crate) async fn sign_in_page() -> Response {
    render(SignIn)
}

pub(crate) async fn sign_in() -> Response {
    session(
        "/media-proposals",
        "hackson_demo_signed_out=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0",
    )
}

pub(crate) async fn sign_out() -> Response {
    session(
        "/demo/sign-in",
        "hackson_demo_signed_out=1; Path=/; HttpOnly; SameSite=Strict",
    )
}

fn session(destination: &str, value: &'static str) -> Response {
    let mut response = Redirect::to(destination).into_response();
    response
        .headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_static(value));
    response
}

pub(crate) fn is_signed_out(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .any(|cookie| cookie.trim() == SIGNED_OUT)
}
