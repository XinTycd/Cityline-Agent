use reqwest::Url;

pub(crate) fn endpoint(name: &str) -> Option<Url> {
    let value = std::env::var(name).ok()?;
    let url = Url::parse(&value).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none())
    .then_some(url)
}

pub(crate) fn origin(name: &str) -> Option<Url> {
    endpoint(name).filter(|url| url.path() == "/")
}

pub(crate) fn login_url(origin_name: &str, path_name: &str) -> Option<Url> {
    let origin = origin(origin_name)?;
    let path = std::env::var(path_name).ok()?;
    if !path.starts_with('/')
        || path.starts_with("//")
        || path.contains(['\\', '?', '#'])
        || path.chars().any(char::is_control)
    {
        return None;
    }
    let url = origin.join(&path).ok()?;
    (url.origin() == origin.origin()).then_some(url)
}
