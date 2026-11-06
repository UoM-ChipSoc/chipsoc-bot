//! Light and dark mode: follows the system unless a `theme` cookie says
//! otherwise. Set by `/theme/{auto|light|dark}`; no JavaScript involved.

use axum::body::{Body, to_bytes};
use axum::extract::{Path, Query, Request};
use axum::http::header::{CONTENT_TYPE, COOKIE, REFERER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

const MAX_PAGE: usize = 4 * 1024 * 1024;

fn chosen(headers: &HeaderMap) -> Option<&'static str> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    cookies.split(';').map(str::trim).find_map(|c| match c {
        "theme=light" => Some("light"),
        "theme=dark" => Some("dark"),
        _ => None,
    })
}

/// Stamp the chosen theme onto HTML pages, and mark it in the theme switcher.
pub(crate) async fn stamp(req: Request, next: Next) -> Response {
    let theme = chosen(req.headers());
    let res = next.run(req).await;
    let is_html =
        res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/html"));
    if !is_html {
        return res;
    }
    let (parts, body) = res.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_PAGE).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let mut page = String::from_utf8_lossy(&bytes).into_owned();
    if let Some(theme) = theme {
        page = page.replacen("<html lang=\"en\">", &format!("<html lang=\"en\" data-theme=\"{theme}\">"), 1);
    }
    let link = format!("class=\"theme-{}\"", theme.unwrap_or("auto"));
    page = page.replacen(&link, &format!("{link} aria-current=\"true\""), 1);
    let mut res = Response::from_parts(parts, Body::from(page));
    res.headers_mut().remove(axum::http::header::CONTENT_LENGTH);
    res
}

#[derive(serde::Deserialize)]
pub(crate) struct Back {
    back: Option<String>,
}

/// A path on this site to go back to: no other host, scheme or theme link.
fn local(path: &str) -> Option<&str> {
    let ok = path.starts_with('/')
        && !path.starts_with("//")
        && !path.starts_with("/theme/")
        && !path.contains(['\\', '\r', '\n'])
        && !path.chars().any(char::is_control);
    ok.then_some(path)
}

/// Remember a theme and go back to the page it was chosen on (the page says
/// which, as `back`; pages send no Referer, but other links might).
pub(crate) async fn set(Path(mode): Path<String>, Query(q): Query<Back>, headers: HeaderMap) -> Response {
    let cookie = match mode.as_str() {
        "light" | "dark" => format!("theme={mode}; Path=/; Max-Age=31536000; SameSite=Lax; HttpOnly"),
        _ => "theme=; Path=/; Max-Age=0; SameSite=Lax; HttpOnly".to_owned(),
    };
    // Only ever back to a page on this site.
    let referer = headers
        .get(REFERER)
        .and_then(|r| r.to_str().ok())
        .and_then(|r| r.split_once("://").map(|(_, rest)| rest))
        .and_then(|rest| rest.find('/').map(|i| &rest[i..]));
    let back = q.back.as_deref().and_then(local).or(referer.and_then(local)).unwrap_or("/").to_owned();
    let mut res = Redirect::to(&back).into_response();
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    res
}
