//! The JSON API under `/api/v1`, for the CLI and TUI after `login` (and any
//! other front end). Requests carry a bearer token (see
//! [`crate::auth::api_guard`]). Plans stay on the server under an id, so apply
//! only ever runs a plan this server made. Config edits are [`Op`]s; the
//! review queue, feeds, resources, ideas and activity go through
//! `chipsoc_admin`, as the bot's buttons do.

use std::sync::{Arc, PoisonError};
use std::time::Duration;

use anyhow::anyhow;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use chipsoc_admin::join::Notice;
use chipsoc_admin::{Decided, Drift, Entry, FeedInfo, Host as _, NewResource, Past, Pending, Post, Roster};
use chipsoc_config::ops::{Change, Schema};
use chipsoc_config::template::{self, Rendered};
use chipsoc_config::{Snowflake, authoring};
use chipsoc_discord::Rest;
use chipsoc_engine::{Options, Problems, Pulled, plan_lines};
use chipsoc_model::{Action, Expiring, Plan};
use serde::Deserialize;
use tokio::time::Instant;

use crate::App;
use crate::auth::{Session, random_token};
use crate::projects;
use crate::remote::{
    ApplyRequest, Appoint, Checked, Decision, Exported, Me, NewFeed, NewProject, Note, Pause, Planned, Preview,
    ProjectList, PullRequest, Refusal, Saved, SetApplications, SetStatus, Status, Step,
};

/// How long a plan waits to be applied.
pub(crate) const PLAN_TTL: Duration = Duration::from_secs(10 * 60);

/// Plans kept at once; the oldest goes first.
const MAX_KEPT: usize = 200;
/// Plans kept for any one session; its own oldest goes first, so nobody's
/// planning pushes out another admin's plan.
const MAX_KEPT_EACH: usize = 5;

/// A plan kept for the session that asked for it.
pub(crate) struct Kept {
    plan: Plan,
    /// The asking session's CSRF token, which no other session has.
    owner: String,
}

/// Plans made through the API, waiting to be applied.
pub(crate) fn kept_plans() -> std::sync::Mutex<Expiring<Kept>> {
    std::sync::Mutex::new(Expiring::new(PLAN_TTL, MAX_KEPT))
}

/// A refusal, as JSON.
pub(crate) struct Failure(StatusCode, Refusal);

pub(crate) fn failure(status: StatusCode, error: impl Into<String>) -> Failure {
    Failure(status, Refusal { error: error.into(), problems: Vec::new() })
}

impl From<anyhow::Error> for Failure {
    fn from(e: anyhow::Error) -> Self {
        let problems = e.downcast_ref::<Problems>().map(|p| p.0.clone()).unwrap_or_default();
        Self(StatusCode::UNPROCESSABLE_ENTITY, Refusal { error: format!("{e:#}"), problems })
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

type Reply<T> = Result<Json<T>, Failure>;

impl<R> App<R> {
    fn plans(&self) -> std::sync::MutexGuard<'_, Expiring<Kept>> {
        self.plans.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Keep a plan for `s` to apply; its id.
    pub(crate) fn keep(&self, plan: Plan, s: &Session) -> String {
        let id = random_token();
        let mut all = self.plans();
        let now = Instant::now().into_std();
        all.make_room(now, |k| k.owner == s.csrf, MAX_KEPT_EACH);
        all.put(id.clone(), Kept { plan, owner: s.csrf.clone() }, now);
        id
    }

    /// The plan kept as `id` for `s`, once.
    pub(crate) fn take(&self, id: &str, s: &Session) -> Option<Plan> {
        let mut all = self.plans();
        let now = Instant::now().into_std();
        // Someone else's id leaves their plan where it is.
        if all.peek(id, now)?.owner != s.csrf {
            return None;
        }
        all.take(id, now).map(|k| k.plan)
    }
}

pub(crate) async fn me<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Json<Me> {
    let server = app.config().await.map(|c| c.bot.name.clone()).unwrap_or_default();
    Json(Me { user: s.actor().unwrap_or_default().to_owned(), server })
}

/// `GET /export`: the server as Discord has it now, as a new config's files
/// (what `export` writes), so it needs no bot token where it's run.
pub(crate) async fn export<R: Rest + 'static>(State(app): State<Arc<App<R>>>) -> Reply<Exported> {
    let cfg = app.config().await.ok();
    let guild = app.guild(cfg.as_deref()).await?;
    let dir = std::env::temp_dir().join(format!("chipsoc-export-{}", random_token()));
    let made = async {
        chipsoc_engine::export_to(&app.rest, guild, &dir).await?;
        let mut files = std::collections::BTreeMap::new();
        let mut todo = vec![dir.clone()];
        while let Some(d) = todo.pop() {
            for e in std::fs::read_dir(&d)? {
                let path = e?.path();
                if path.is_dir() {
                    todo.push(path);
                } else if let Ok(rel) = path.strip_prefix(&dir) {
                    files.insert(rel.to_string_lossy().replace('\\', "/"), std::fs::read_to_string(&path)?);
                }
            }
        }
        anyhow::Ok(files)
    }
    .await;
    let _ = std::fs::remove_dir_all(&dir);
    Ok(Json(Exported { files: made? }))
}

#[derive(Deserialize)]
pub(crate) struct PlanQuery {
    #[serde(default)]
    delete: bool,
    #[serde(default)]
    sweep: bool,
}

pub(crate) async fn plan<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<PlanQuery>,
) -> Reply<Planned> {
    let opts = Options { delete: q.delete, sweep: q.sweep, ..Default::default() };
    let plan = app.engine().await?.plan(opts).await?;
    let id = app.keep(plan.clone(), &s);
    Ok(Json(Planned { id, lines: plan_lines(&plan), plan }))
}

pub(crate) async fn apply<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(req): Json<ApplyRequest>,
) -> Reply<Vec<Step>> {
    let Some(plan) = app.take(&req.plan_id, &s) else {
        return Err(failure(StatusCode::NOT_FOUND, "That plan has expired or was already used; plan again."));
    };
    let plan = match req.steps {
        Some(steps) => only(plan, &steps)?,
        None => plan,
    };
    tracing::info!(actor = s.actor().unwrap_or("-"), "API apply");
    let by = s.by();
    // Spawned so a dropped connection can't stop an apply halfway.
    let run = async move {
        let mut e = app.engine().await?;
        e.apply_by(&plan, &by, &mut |_: &Action, _: &Result<(), String>| {}).await
    };
    let report = tokio::spawn(run).await.map_err(|e| anyhow!(e))??;
    let steps = report.outcomes.into_iter().map(|o| Step { action: o.action, error: o.result.err() });
    Ok(Json(steps.collect()))
}

/// Just the picked steps of `plan`, in its order.
fn only(plan: Plan, steps: &[usize]) -> Result<Plan, Failure> {
    if let Some(i) = steps.iter().find(|&&i| !plan.actions.get(i).is_some_and(Action::mutates)) {
        return Err(failure(StatusCode::BAD_REQUEST, format!("step {i} isn't a change in that plan")));
    }
    let picked = plan.actions.iter().enumerate().filter(|(i, _)| steps.contains(i));
    let actions = picked.map(|(_, a)| a.clone()).collect();
    Ok(Plan { actions, ..plan })
}

pub(crate) async fn pull<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(req): Json<PullRequest>,
) -> Reply<Vec<Pulled>> {
    tracing::info!(actor = s.actor().unwrap_or("-"), "API pull from Discord");
    let by = s.by();
    let run = async move { app.engine().await?.adopt_by(req.permissions, &by).await };
    Ok(Json(tokio::spawn(run).await.map_err(|e| anyhow!(e))??))
}

pub(crate) async fn check<R: Rest + 'static>(State(app): State<Arc<App<R>>>) -> Reply<Checked> {
    let dir = app.dir.clone();
    // A file that doesn't load is a problem to report, not a failure.
    let r = chipsoc_config::git::blocking(move || authoring::check(&dir)).await?;
    Ok(Json(Checked { errors: r.errors, warnings: r.warnings }))
}

pub(crate) async fn projects<R: Rest + 'static>(State(app): State<Arc<App<R>>>) -> Reply<ProjectList> {
    let cfg = app.config().await?;
    let areas = cfg.bot.projects.as_ref().map(|p| p.areas.clone()).unwrap_or_default();
    Ok(Json(ProjectList { projects: cfg.projects.clone(), areas }))
}

/// Run `work` to the end even if the connection drops, so a change is never
/// left half made.
async fn to_end<T: Send + 'static>(
    work: impl std::future::Future<Output = anyhow::Result<T>> + Send + 'static,
) -> anyhow::Result<T> {
    tokio::spawn(work).await.map_err(|e| anyhow!(e))?
}

pub(crate) async fn add_project<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(p): Json<NewProject>,
) -> Reply<Saved> {
    let slug = to_end(async move { projects::save_new(&app, &s, &p).await }).await?;
    Ok(Json(Saved { slug }))
}

pub(crate) async fn set_status<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(slug): Path<String>,
    Json(f): Json<SetStatus>,
) -> Reply<Saved> {
    let to = slug.clone();
    to_end(async move { projects::save_status(&app, &s, &to, f.status).await }).await?;
    Ok(Json(Saved { slug }))
}

pub(crate) async fn set_applications<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(slug): Path<String>,
    Json(f): Json<SetApplications>,
) -> Reply<Saved> {
    let to = slug.clone();
    to_end(async move { projects::save_applications(&app, &s, &to, f.open).await }).await?;
    Ok(Json(Saved { slug }))
}

/// `GET /ops/schema`: what [`Op`]s can edit, with the current values.
pub(crate) async fn schema<R: Rest + 'static>(State(app): State<Arc<App<R>>>) -> Reply<Schema> {
    Ok(Json(app.schema().await?))
}

/// `POST /ops`: one change to the config (an op, or a list of them made as
/// one: one check, one commit, all or nothing), refused (with the problems)
/// if it would break it.
pub(crate) async fn op<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(ops): Json<crate::remote::Ops>,
) -> Reply<Change> {
    let ops = ops.into_vec();
    Ok(Json(to_end(async move { app.edit_ops(&s, &ops).await }).await?))
}

/// `POST /ops/preview`: a managed post's markdown as Discord would get it.
pub(crate) async fn preview<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Json(p): Json<Preview>,
) -> Reply<Rendered> {
    let cfg = app.config().await?;
    Ok(Json(template::render(&cfg.fill_lists(&p.body), &cfg.desired.lock)))
}

#[derive(Deserialize)]
pub(crate) struct Limit {
    limit: Option<usize>,
}

impl Limit {
    fn or(&self, default: usize) -> usize {
        self.limit.unwrap_or(default).clamp(1, 100)
    }
}

/// `GET /queue`: applications waiting for a decision, with their answers.
pub(crate) async fn queue<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Vec<Pending>> {
    let desk = app.desk(&s).await?;
    Ok(Json(chipsoc_admin::pending(&desk).await?))
}

/// `POST /queue/{id}`: accept or reject, as the Discord card's buttons do.
pub(crate) async fn decide<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<i64>,
    Json(d): Json<Decision>,
) -> Reply<Decided> {
    // Spawned so a dropped connection can't stop it halfway.
    let run = async move {
        use crate::people::Spoken as _;
        let desk = app.desk(&s).await?;
        anyhow::Ok(
            match chipsoc_admin::decide_seen(&desk, id, d.accept, d.reason.as_deref(), d.seen.as_deref(), &desk.actor)
                .await
            {
                Ok(mut d) => {
                    d.outcome = d.outcome.named(&desk).await;
                    Ok(d)
                }
                Err(why) => Err(why.spoken(&desk).await),
            },
        )
    };
    let decided = tokio::spawn(run).await.map_err(|e| anyhow!(e))??;
    decided.map(Json).map_err(|why| failure(StatusCode::CONFLICT, why))
}

/// `POST /queue/{id}/lead`: make an accepted project applicant one of its leads.
pub(crate) async fn make_lead<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<i64>,
) -> Reply<Note> {
    let run = async move {
        use crate::people::Spoken as _;
        let desk = app.desk(&s).await?;
        anyhow::Ok(match chipsoc_admin::make_lead(&desk, id, &desk.actor).await {
            Ok(said) => Ok(Note { text: said.spoken(&desk).await }),
            Err(why) => Err(why.spoken(&desk).await),
        })
    };
    let done = tokio::spawn(run).await.map_err(|e| anyhow!(e))??;
    done.map(Json).map_err(|why| failure(StatusCode::CONFLICT, why))
}

/// `GET /feeds`: the feeds being followed.
pub(crate) async fn feeds<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Vec<FeedInfo>> {
    Ok(Json(chipsoc_admin::feeds(&app.desk(&s).await?).await?))
}

/// `DELETE /feeds/{id}`: stop following a feed.
pub(crate) async fn remove_feed<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<i64>,
) -> Result<StatusCode, Failure> {
    let removed = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::remove_feed(&desk, id, &desk.actor).await
    });
    match removed.await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(failure(StatusCode::NOT_FOUND, "No such feed.")),
    }
}

/// `GET /resources?limit=`: the newest posts in the resources forum.
pub(crate) async fn resources<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Limit>,
) -> Reply<Vec<Post>> {
    Ok(Json(chipsoc_admin::resources(&app.desk(&s).await?, q.or(20)).await?))
}

/// `POST /resources`: share a link in the resources forum.
pub(crate) async fn post_resource<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(r): Json<NewResource>,
) -> Reply<Post> {
    let posted = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::post_resource(&desk, &r, &desk.actor).await
    });
    Ok(Json(posted.await?))
}

/// `POST /share`: share an announcement or event with partners (the bot sends it).
pub(crate) async fn share<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(r): Json<crate::remote::ShareRequest>,
) -> Reply<crate::remote::Shared> {
    let pick = chipsoc_admin::shares::Pick::parse(&r.what, &r.item)
        .map_err(|why| failure(StatusCode::UNPROCESSABLE_ENTITY, why))?;
    let shared = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::shares::share_now(&desk, pick, &r.partners, &desk.actor).await
    });
    Ok(Json(crate::remote::Shared { partners: shared.await? }))
}

/// `GET /ideas?limit=`: the newest posts in the ideas forum.
pub(crate) async fn ideas<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Limit>,
) -> Reply<Vec<Post>> {
    Ok(Json(chipsoc_admin::ideas(&app.desk(&s).await?, q.or(20)).await?))
}

/// `GET /activity?limit=`: the latest audit-log entries.
pub(crate) async fn activity<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Limit>,
) -> Reply<Vec<Entry>> {
    Ok(Json(chipsoc_admin::activity(&app.desk(&s).await?, q.or(50) as i64).await?))
}

#[derive(Deserialize)]
pub(crate) struct Whose {
    user: Option<Snowflake>,
    limit: Option<usize>,
}

/// `GET /changes`: the config's latest changes, or (`?user=`) what one admin did this week.
pub(crate) async fn changes<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Whose>,
) -> Reply<Vec<chipsoc_admin::Done>> {
    let limit = q.limit.unwrap_or(50).clamp(1, 200);
    Ok(Json(chipsoc_admin::history_of(&app.desk(&s).await?, q.user, limit).await?))
}

/// `POST /undo`: go back on a config commit (the server owner only); apply to send it to Discord.
pub(crate) async fn undo<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(req): Json<crate::remote::UndoRequest>,
) -> Reply<crate::remote::Undone> {
    let made = to_end(async move {
        let desk = app.desk(&s).await?;
        anyhow::Ok(chipsoc_admin::undo(&desk, &req.commit, req.how, &desk.actor).await)
    })
    .await?;
    match made {
        Ok(made) => Ok(Json(crate::remote::Undone {
            subject: made.as_ref().map(|m| m.subject.clone()),
            warning: made.and_then(|m| m.warning),
        })),
        Err(why) if why.starts_with("Only the server owner") => Err(failure(StatusCode::FORBIDDEN, why)),
        Err(why) => Err(failure(StatusCode::CONFLICT, why)),
    }
}

/// `GET /join`: how the join flow is going (from the bot's database).
pub(crate) async fn join<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<chipsoc_admin::join::Progress> {
    Ok(Json(chipsoc_admin::join::progress(&app.desk(&s).await?).await?))
}

/// `GET /join/notify`: how many members `POST` would DM, and what it says.
pub(crate) async fn join_notice<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Notice> {
    let desk = app.desk(&s).await?;
    if chipsoc_admin::Host::cfg(&desk).join.is_none() {
        return Err(failure(StatusCode::NOT_FOUND, "There's no join.toml."));
    }
    Ok(Json(chipsoc_admin::join::preview(&desk).await?))
}

/// `POST /join/notify`: DM them (once each, ever), in the background; the bot log hears how it went.
pub(crate) async fn send_join_notice<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Notice> {
    let Json(notice) = join_notice(State(app.clone()), Extension(s.clone())).await?;
    tokio::spawn(async move {
        let sent = match app.desk(&s).await {
            Ok(desk) => chipsoc_admin::join::send_notice(&desk, &desk.actor, chipsoc_admin::join::NOTICE_PACE).await,
            Err(e) => Err(e),
        };
        if let Err(e) = sent {
            tracing::warn!("sending the join notice failed: {e:#}");
        }
    });
    Ok(Json(notice))
}

/// `GET /status`: the server at a glance, without asking Discord.
pub(crate) async fn status<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Status> {
    Ok(Json(app.status(&s).await?))
}

impl<R: Rest + Clone> App<R> {
    /// [`Status`] for `s`; the queue count is left at 0 if the database can't be read.
    pub(crate) async fn status(&self, s: &Session) -> anyhow::Result<Status> {
        let cfg = self.config().await?;
        let queue = match self.desk(s).await {
            Ok(desk) => desk.db().pending().await.unwrap_or_default(),
            Err(_) => 0,
        };
        Ok(Status::of(&cfg, queue))
    }
}

/// `GET /drift`: how Discord differs from the config, by who changed what.
pub(crate) async fn drift<R: Rest + 'static>(State(app): State<Arc<App<R>>>) -> Reply<Drift> {
    let plan = app.engine().await?.plan(Options::default()).await?;
    Ok(Json(chipsoc_admin::drift(&plan)))
}

/// `GET /queue/history?limit=`: the latest decisions.
pub(crate) async fn history<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(q): Query<Limit>,
) -> Reply<Vec<Past>> {
    Ok(Json(chipsoc_admin::history(&app.desk(&s).await?, q.or(50) as i64).await?))
}

/// Run admin work that may change Discord or the config to the end, even if
/// the connection drops, on the request's desk as `$d`; `Err(why)` is a 409.
macro_rules! finish {
    ($app:ident, $s:ident, |$d:ident| $work:expr) => {{
        let run = async move {
            use crate::people::Spoken as _;
            let desk = $app.desk(&$s).await?;
            let $d = &desk;
            anyhow::Ok(match $work {
                Ok(v) => Ok(v),
                Err(why) => Err(why.spoken(&desk).await),
            })
        };
        let done: Result<_, String> = tokio::spawn(run).await.map_err(|e| anyhow!(e))??;
        done.map(Json).map_err(|why| failure(StatusCode::CONFLICT, why))
    }};
}

fn note(text: String) -> Note {
    Note { text: text.trim().to_owned() }
}

/// What admin work said, as a [`Note`] in plain text with people's names.
async fn noted<H: chipsoc_admin::Host, T: crate::people::Spoken, E>(h: &H, r: Result<T, E>) -> Result<Note, E> {
    match r {
        Ok(t) => Ok(note(t.spoken(h).await)),
        Err(e) => Err(e),
    }
}

/// `POST /feeds`: follow a feed (only public http(s) addresses).
pub(crate) async fn add_feed<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(f): Json<NewFeed>,
) -> Reply<Note> {
    finish!(app, s, |d| chipsoc_admin::add_feed(d, &f.url, &d.actor).await.map(|t| note(format!("Now following {t}."))))
}

/// `POST /feeds/{id}/pause`: pause or resume a feed.
pub(crate) async fn pause_feed<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<i64>,
    Json(p): Json<Pause>,
) -> Result<StatusCode, Failure> {
    let paused = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::pause_feed(&desk, id, p.paused, &desk.actor).await
    });
    match paused.await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(failure(StatusCode::NOT_FOUND, "No such feed.")),
    }
}

/// `DELETE /resources/{id}`: close and lock a resource's post (it isn't deleted).
pub(crate) async fn archive_resource<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(id): Path<Snowflake>,
) -> Result<StatusCode, Failure> {
    let archived = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::archive_resource(&desk, id, &desk.actor).await
    });
    match archived.await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(failure(StatusCode::NOT_FOUND, "No such post in the resources forum.")),
    }
}

/// `GET /members`: who holds what, and who left holding something.
pub(crate) async fn members<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Reply<Roster> {
    Ok(Json(chipsoc_admin::roster(&app.desk(&s).await?).await?))
}

#[derive(Deserialize)]
pub(crate) struct Find {
    #[serde(default)]
    q: String,
}

/// `GET /members/find?q=@ada`: who a username or name could be (exact
/// matches, else names containing it; at most 10).
pub(crate) async fn find_members<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Query(f): Query<Find>,
) -> Reply<chipsoc_admin::Found> {
    Ok(Json(chipsoc_admin::find_members(&app.desk(&s).await?, &f.q).await?))
}

/// `GET /members/about/{user}`: what someone told the bot (profile, join answers).
pub(crate) async fn about<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(user): Path<Snowflake>,
) -> Reply<chipsoc_admin::join::About> {
    Ok(Json(chipsoc_admin::join::about(&app.desk(&s).await?, user).await?))
}

/// `POST /members`: give someone a role members.toml lists.
pub(crate) async fn appoint<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Json(a): Json<Appoint>,
) -> Reply<Note> {
    finish!(app, s, |d| noted(d, chipsoc_admin::appoint(d, &a.role, a.user, &d.actor).await).await)
}

/// `DELETE /members/{role}/{user}`: take someone out of members.toml for a role.
pub(crate) async fn unappoint<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path((role, user)): Path<(String, Snowflake)>,
) -> Reply<Note> {
    finish!(app, s, |d| noted(d, chipsoc_admin::unappoint(d, &role, user, &d.actor).await).await)
}

/// `POST /projects/{slug}/leads`: make someone a lead (and give them its Lead role).
pub(crate) async fn add_lead<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(slug): Path<String>,
    Json(a): Json<Appoint>,
) -> Reply<Note> {
    finish!(app, s, |d| noted(d, chipsoc_admin::add_project_lead(d, &slug, a.user, &d.actor).await).await)
}

/// `DELETE /projects/{slug}/leads/{user}`: take someone off a project's leads.
pub(crate) async fn remove_lead<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path((slug, user)): Path<(String, Snowflake)>,
) -> Reply<Note> {
    finish!(app, s, |d| noted(d, chipsoc_admin::remove_lead(d, &slug, user, &d.actor).await).await)
}

/// `POST /members/former/{user}/restore`: give someone who came back their old roles.
pub(crate) async fn restore<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(user): Path<Snowflake>,
) -> Reply<Note> {
    finish!(app, s, |d| noted(d, chipsoc_admin::restore(d, user, &d.actor).await).await)
}

/// `DELETE /members/former/{user}`: stop offering someone's old roles back.
pub(crate) async fn forget<R: Rest + Clone + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(user): Path<Snowflake>,
) -> Result<StatusCode, Failure> {
    let forgot = to_end(async move {
        let desk = app.desk(&s).await?;
        chipsoc_admin::forget_former(&desk, user, &desk.actor).await
    });
    match forgot.await? {
        true => Ok(StatusCode::NO_CONTENT),
        false => Err(failure(StatusCode::NOT_FOUND, "Nobody by that id left holding roles.")),
    }
}
