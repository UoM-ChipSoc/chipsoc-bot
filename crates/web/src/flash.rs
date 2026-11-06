//! One-off messages after a change ("Saved …"): set as a short-lived cookie
//! on the redirect, shown at the top of the next page, then cleared.

use axum::body::{Body, to_bytes};
use axum::extract::Request;
use axum::http::header::{CONTENT_LENGTH, CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};

const NAME: &str = "flash";
const MAX_PAGE: usize = 4 * 1024 * 1024;
/// Longest message kept.
const MAX_TEXT: usize = 1500;

/// Go to `to` and say `text` there (`good`: green, else red).
pub(crate) fn redirect(to: &str, text: &str, good: bool) -> Response {
    let text: String = text.chars().take(MAX_TEXT).collect();
    let value =
        serde_urlencoded::to_string([("m", format!("{}{text}", if good { "g" } else { "b" }))]).unwrap_or_default();
    let value = value.strip_prefix("m=").unwrap_or_default();
    let mut res = Redirect::to(to).into_response();
    let cookie = format!("{NAME}={value}; Path=/; Max-Age=60; SameSite=Strict; HttpOnly");
    if let Ok(v) = HeaderValue::from_str(&cookie) {
        res.headers_mut().append(SET_COOKIE, v);
    }
    res
}

/// The message waiting in the request's cookies: (good, text).
fn waiting(headers: &HeaderMap) -> Option<(bool, String)> {
    let cookies = headers.get(COOKIE)?.to_str().ok()?;
    let value = cookies.split(';').map(str::trim).find_map(|c| c.strip_prefix("flash="))?;
    let fields: Vec<(String, String)> = serde_urlencoded::from_str(&format!("m={value}")).ok()?;
    let m = fields.into_iter().next()?.1;
    let (kind, text) = m.split_at_checked(1)?;
    (!text.is_empty()).then(|| (kind == "g", text.to_owned()))
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A message written for Discord as HTML: **bold** marks and markdown
/// escapes dropped, `code` as code, the rest escaped. Mentions are turned
/// into names before the message is set (`people::unmention`).
pub(crate) fn discord_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let (mut chars, mut code) = (text.chars().peekable(), false);
    while let Some(c) = chars.next() {
        match c {
            '\\' if chars.peek().is_some_and(char::is_ascii_punctuation) => {
                out.push_str(&escape(&chars.next().unwrap_or_default().to_string()));
            }
            '*' if !code && chars.peek() == Some(&'*') => {
                chars.next();
            }
            '`' => {
                out.push_str(if code { "</code>" } else { "<code>" });
                code = !code;
            }
            c => out.push_str(&escape(&c.to_string())),
        }
    }
    if code {
        out.push_str("</code>");
    }
    out
}

/// Show a waiting message at the top of the next page, and clear it.
pub(crate) async fn stamp(req: Request, next: Next) -> Response {
    let message = waiting(req.headers());
    let res = next.run(req).await;
    let Some((good, text)) = message else { return res };
    let is_page = res.status() == StatusCode::OK
        && res.headers().get(CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.starts_with("text/html"));
    if !is_page {
        return res;
    }
    let (mut parts, body) = res.into_parts();
    let Ok(bytes) = to_bytes(body, MAX_PAGE).await else {
        return Response::from_parts(parts, Body::empty());
    };
    let page = String::from_utf8_lossy(&bytes);
    let Some(at) = page.find("<main").and_then(|i| page[i..].find('>').map(|j| i + j + 1)) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    let (class, role) = if good { ("good", "status") } else { ("bad", "alert") };
    let note = format!("<p class=\"note {class} flash\" role=\"{role}\">{}</p>", discord_html(&text));
    let page = format!("{}{note}{}", &page[..at], &page[at..]);
    parts.headers.remove(CONTENT_LENGTH);
    let clear = format!("{NAME}=; Path=/; Max-Age=0; SameSite=Strict; HttpOnly");
    if let Ok(v) = HeaderValue::from_str(&clear) {
        parts.headers.append(SET_COOKIE, v);
    }
    Response::from_parts(parts, Body::from(page))
}

#[cfg(test)]
mod tests {
    use super::discord_html;

    #[test]
    fn discord_markdown_reads_as_plain_text() {
        assert_eq!(discord_html("Took **Treasurer** away."), "Took Treasurer away.");
        assert_eq!(discord_html("Run `plan` <now> & a\\_b"), "Run <code>plan</code> &lt;now&gt; &amp; a_b");
        assert_eq!(discord_html("half `open"), "half <code>open</code>");
    }
}
