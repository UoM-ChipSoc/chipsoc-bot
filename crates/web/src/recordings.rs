//! Recordings of voice meetings (made with `/record` in Discord): listen to
//! or download each speaker's track and the mix, read the transcript, and
//! delete a recording. Nothing deletes them but an admin here, and each
//! deletion is in the audit log.

use std::io::SeekFrom;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::header::{
    ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_SECURITY_POLICY, CONTENT_TYPE, RANGE,
};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_discord::Rest;
use chipsoc_voice::store::{self, Meta, Transcript};
use chipsoc_voice::{duration, transcribe::stamp};
use dioxus::prelude::*;
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::App;
use crate::auth::Session;
use crate::form::{Fields, confirm, said_back, when};
use crate::oauth::Members as _;
use crate::view::{self, Tab, empty};

const HERE: &str = "/recordings";

/// What a page that plays audio may load.
fn csp() -> HeaderValue {
    HeaderValue::from_str(&format!("{}; media-src 'self'", crate::CSP)).expect("a valid header")
}

fn size(bytes: u64) -> String {
    match bytes {
        b if b >= 1_000_000_000 => format!("{:.1} GB", b as f64 / 1e9),
        b if b >= 1_000_000 => format!("{:.0} MB", b as f64 / 1e6),
        b => format!("{:.0} KB", (b as f64 / 1e3).max(1.0)),
    }
}

fn transcript_state(t: &Transcript) -> String {
    match t {
        Transcript::Waiting => "waiting".into(),
        Transcript::Making => "being made".into(),
        Transcript::Done => "ready".into(),
        Transcript::Skipped { why } => format!("none: {why}"),
        Transcript::Failed { why } => format!("failed: {why}"),
    }
}

/// A server's recordings, and whether recording is set up.
async fn load<R: Rest + Clone>(app: &App<R>, s: &Session) -> anyhow::Result<(Vec<Meta>, bool, u64)> {
    let desk = app.desk(s).await?;
    let on = chipsoc_admin::Host::cfg(&desk).bot.recording.is_some();
    let guild = chipsoc_admin::Host::guild(&desk).0;
    let root = app.recordings.clone();
    let list = tokio::task::spawn_blocking(move || store::list(&root, guild)).await?;
    Ok((list, on, guild))
}

pub(crate) async fn page<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    match load(&app, &s).await {
        Ok((list, on, _)) => list_page(&s, &list, on).into_response(),
        Err(e) => view::broken(Tab::Recordings, &s, &e).into_response(),
    }
}

fn list_page(s: &Session, list: &[Meta], on: bool) -> Html<String> {
    view::page(
        Tab::Recordings,
        s,
        rsx! {
            h1 { "Recordings" }
            if on {
                p { class: "hint",
                    "Meetings recorded with /record in Discord: each speaker's track, everyone mixed, and the transcript. "
                    "They're kept until an admin deletes one here."
                }
            } else {
                p { class: "note bad", role: "alert",
                    "Recording is off. Ask whoever runs the bot to turn it on (bot.toml, [recording])."
                }
            }
            if list.is_empty() {
                {empty("No recordings yet.")}
            } else {
                {view::table(&["When", "Call", "Length", "People", "Size", "Transcript"], rsx! {
                    for m in list {
                        tr {
                            td { a { href: "/recordings/{m.id}", "{when(m.started_at)}" } }
                            td { "{m.channel_name}" if m.ended_at.is_none() { span { class: "tag", "recording" } } }
                            td { "{duration(m.length_ms)}" }
                            td { class: "wrap", "{m.names().join(\", \")}" }
                            td { "{size(m.bytes)}" }
                            td { class: "wrap", "{transcript_state(&m.transcript)}" }
                        }
                    }
                })}
            }
        },
    )
}

pub(crate) async fn one<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<String>,
) -> Response {
    let loaded = async {
        let (list, _, guild) = load(&app, &s).await?;
        let meta = list.into_iter().find(|m| m.id == id);
        let text = match &meta {
            Some(m) if m.transcript == Transcript::Done => {
                store::file(&app.recordings, guild, &m.id, store::TRANSCRIPT)
                    .and_then(|p| std::fs::read_to_string(p).ok())
            }
            _ => None,
        };
        anyhow::Ok((meta, text))
    };
    match loaded.await {
        Ok((Some(m), text)) => {
            let mut res = detail(&s, &m, text.as_deref()).into_response();
            res.headers_mut().insert(CONTENT_SECURITY_POLICY, csp());
            res
        }
        Ok((None, _)) => said_back(HERE, Err("No such recording (it may have been deleted).".into())),
        Err(e) => view::broken(Tab::Recordings, &s, &e).into_response(),
    }
}

fn detail(s: &Session, m: &Meta, transcript: Option<&str>) -> Html<String> {
    let file = |f: &str| format!("/recordings/{}/files/{f}", m.id);
    let ended = m.ended_because.clone().unwrap_or_else(|| "still recording".into());
    let lines: Vec<&str> = transcript.map(|t| t.lines().filter(|l| !l.trim().is_empty()).collect()).unwrap_or_default();
    view::page(
        Tab::Recordings,
        s,
        rsx! {
            p { a { href: HERE, "← All recordings" } }
            h1 { "Recording of {m.channel_name}" }
            p { class: "hint",
                "{when(m.started_at)} · {duration(m.length_ms)} · started by {m.started_by.name} · ended: {ended} · {size(m.bytes)}"
            }
            if let Some(mix) = &m.mix {
                h2 { "Everyone" }
                div { class: "card",
                    audio { controls: true, preload: "none", src: "{file(mix)}" }
                    p { a { href: "{file(mix)}?download=1", "Download the mix" } }
                }
            }
            h2 { "Each speaker" }
            if m.tracks.is_empty() {
                {empty("Nobody spoke.")}
            } else {
                {view::table(&["Who", "Listen", "", "Size"], rsx! {
                    for t in &m.tracks {
                        tr {
                            td { "{t.name}" }
                            td { audio { controls: true, preload: "none", src: "{file(&t.file)}" } }
                            td { a { href: "{file(&t.file)}?download=1", "Download" } }
                            td { "{size(t.bytes)}" }
                        }
                    }
                })}
            }
            h2 { "Who was in the call" }
            {view::table(&["Who", "When"], rsx! {
                for p in &m.participants {
                    tr {
                        td { "{p.name}" }
                        td { class: "wrap",
                            {p.spans.iter().map(|s| format!("{}–{}", stamp(s.from_ms), s.to_ms.map_or_else(|| "now".into(), stamp))).collect::<Vec<_>>().join(", ")}
                        }
                    }
                }
            })}
            h2 { "Transcript" }
            if transcript.is_some() {
                p { a { href: "{file(store::TRANSCRIPT)}?download=1", "Download the transcript" } " (Markdown)" }
                div { class: "card transcript",
                    for l in &lines {
                        p { "{chipsoc_voice::md_plain(l.trim_start_matches(\"# \"))}" }
                    }
                }
            } else {
                p { class: "hint", "{transcript_state(&m.transcript)}" }
            }
            h2 { "Delete" }
            p { class: "hint", "Recordings are only ever deleted here, by an admin. It's logged." }
            {confirm(s, "/recordings/delete", &[("id", &m.id)], "Delete this recording", "Yes, delete it for good",
                "Its audio and transcript are deleted for good; this can't be undone.")}
        },
    )
}

#[derive(Deserialize)]
pub(crate) struct Download {
    download: Option<String>,
}

/// One of a recording's files, for the player (with ranges, to seek) or as a download.
pub(crate) async fn file<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path((id, name)): Path<(String, String)>,
    Query(q): Query<Download>,
    headers: HeaderMap,
) -> Response {
    let found = async {
        let desk = app.desk(&s).await?;
        let guild = chipsoc_admin::Host::guild(&desk).0;
        anyhow::Ok(store::file(&app.recordings, guild, &id, &name))
    };
    let path = match found.await {
        Ok(Some(p)) => p,
        Ok(None) => return (StatusCode::NOT_FOUND, "No such file.").into_response(),
        Err(e) => return view::broken(Tab::Recordings, &s, &e).into_response(),
    };
    let ctype = if name.ends_with(".md") { "text/markdown; charset=utf-8" } else { "audio/ogg" };
    let disposition = q.download.is_some().then(|| format!("attachment; filename=\"{id}-{name}\""));
    let range = headers.get(RANGE).and_then(|v| v.to_str().ok()).map(str::to_owned);
    match serve(&path, range.as_deref(), ctype, disposition).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("serving {} failed: {e}", path.display());
            (StatusCode::INTERNAL_SERVER_ERROR, "Couldn't read that file.").into_response()
        }
    }
}

/// `bytes=a-b`, `bytes=a-` or `bytes=-n` of a file `len` long, as the first
/// and last byte; `Err` if it's outside the file.
fn range(header: &str, len: u64) -> Result<(u64, u64), ()> {
    let spec = header.trim().strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') || len == 0 {
        return Err(());
    }
    let (a, b) = spec.split_once('-').ok_or(())?;
    let (a, b) = (a.trim(), b.trim());
    let (from, to) = match (a.is_empty(), b.is_empty()) {
        (true, false) => {
            let n: u64 = b.parse().map_err(drop)?;
            (len.saturating_sub(n), len - 1)
        }
        (false, _) => {
            let from: u64 = a.parse().map_err(drop)?;
            let to = if b.is_empty() { len - 1 } else { b.parse::<u64>().map_err(drop)?.min(len - 1) };
            (from, to)
        }
        _ => return Err(()),
    };
    if from > to || from >= len { Err(()) } else { Ok((from, to)) }
}

async fn serve(
    path: &std::path::Path,
    want: Option<&str>,
    ctype: &str,
    disposition: Option<String>,
) -> std::io::Result<Response> {
    let mut f = tokio::fs::File::open(path).await?;
    let len = f.metadata().await?.len();
    let mut res = match want.map(|h| range(h, len)) {
        Some(Err(())) => {
            let mut r = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
            r.headers_mut().insert(CONTENT_RANGE, HeaderValue::from_str(&format!("bytes */{len}")).expect("ascii"));
            return Ok(r);
        }
        Some(Ok((from, to))) => {
            f.seek(SeekFrom::Start(from)).await?;
            let body = Body::from_stream(tokio_util::io::ReaderStream::new(f.take(to - from + 1)));
            let mut r = (StatusCode::PARTIAL_CONTENT, body).into_response();
            let h = r.headers_mut();
            h.insert(CONTENT_RANGE, HeaderValue::from_str(&format!("bytes {from}-{to}/{len}")).expect("ascii"));
            h.insert(CONTENT_LENGTH, HeaderValue::from(to - from + 1));
            r
        }
        None => {
            let mut r = Body::from_stream(tokio_util::io::ReaderStream::new(f)).into_response();
            r.headers_mut().insert(CONTENT_LENGTH, HeaderValue::from(len));
            r
        }
    };
    let h = res.headers_mut();
    h.insert(CONTENT_TYPE, HeaderValue::from_str(ctype).expect("ascii"));
    h.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if let Some(d) = disposition.and_then(|d| HeaderValue::from_str(&d).ok()) {
        h.insert(CONTENT_DISPOSITION, d);
    }
    Ok(res)
}

/// Delete a recording (confirmed), saying so in the audit log and the bot log.
pub(crate) async fn delete<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    if !f.confirmed() {
        return said_back(HERE, Err(crate::form::CONFIRM_FIRST.into()));
    }
    let id = f.get("id").to_owned();
    let done = async {
        let desk = app.desk(&s).await.map_err(|e| format!("{e:#}"))?;
        let guild = chipsoc_admin::Host::guild(&desk).0;
        let Some((_, meta)) = store::find(&app.recordings, guild, &id) else {
            return Err("No such recording (it may have been deleted already).".to_owned());
        };
        if meta.ended_at.is_none() {
            return Err("It's still recording: stop it with /record stop first.".to_owned());
        }
        if meta.transcript == Transcript::Making {
            return Err("Its transcript is being made: delete it once that's done.".to_owned());
        }
        let root = app.recordings.clone();
        let gone = {
            let id = id.clone();
            tokio::task::spawn_blocking(move || store::delete(&root, guild, &id)).await
        };
        match gone.map_err(std::io::Error::other).and_then(|r| r) {
            Ok(true) => {}
            Ok(false) => return Err("No such recording.".to_owned()),
            Err(e) => return Err(format!("Couldn't delete it: {e}")),
        }
        let what = format!("{}, {} ({})", meta.channel_name, when(meta.started_at), duration(meta.length_ms));
        let by = desk.actor.key();
        if let Err(e) = chipsoc_admin::Host::db(&desk)
            .audit(chipsoc_admin::now(), Some(&by), "recording_deleted", Some(&id), Some(&what))
            .await
        {
            tracing::warn!("auditing a deletion failed: {e:#}");
        }
        tracing::info!(actor = desk.actor.name, "deleted recording {id}");
        app.note(format!("🗑️ {} deleted the recording of {what} in the web dashboard.", desk.actor.mention())).await;
        Ok(format!("Deleted the recording of {what}."))
    };
    said_back(HERE, done.await)
}

#[cfg(test)]
mod tests {
    use super::range;

    #[test]
    fn ranges_are_read_as_players_send_them() {
        assert_eq!(range("bytes=0-", 100), Ok((0, 99)));
        assert_eq!(range("bytes=10-19", 100), Ok((10, 19)));
        assert_eq!(range("bytes=90-500", 100), Ok((90, 99)));
        assert_eq!(range("bytes=-10", 100), Ok((90, 99)));
        assert_eq!(range("bytes=100-", 100), Err(()));
        assert_eq!(range("bytes=5-1", 100), Err(()));
        assert_eq!(range("bytes=0-1,5-6", 100), Err(()));
        assert_eq!(range("items=0-1", 100), Err(()));
    }
}
