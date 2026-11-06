//! Runs jobs off the UI loop and reports back as [`Msg`]s: plans, applies,
//! pulls, config edits and admin work, all through a [`Backend`] (the files
//! here, or the bot's web server when signed in to one).

use std::path::Path;
use std::sync::Arc;

use anyhow::bail;
use chipsoc_client::remote::{
    Client, Drift, Entry, FeedInfo, NewResource, Past, Pending, Planned, Post, Roster, Status,
};
use chipsoc_client::{Backend, Local};
use chipsoc_config::Snowflake;
use chipsoc_config::ops::{Change, Op, Schema};
use chipsoc_discord::Rest;
use chipsoc_engine::Options;
use chipsoc_model::{Action, Plan};
use tokio::sync::{Mutex, mpsc};

/// Admin data a tab shows, loaded on demand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Area {
    Queue,
    History,
    Members,
    Feeds,
    Resources,
    Activity,
}

pub enum Data {
    Queue(Vec<Pending>),
    History(Vec<Past>),
    Members(Roster),
    Feeds(Vec<FeedInfo>),
    /// Resources, and ideas (which may not be set up).
    Resources(Vec<Post>, Result<Vec<Post>, String>),
    /// The audit log, and the config's latest changes (there may be no history).
    Activity(Vec<Entry>, Result<Vec<chipsoc_client::remote::Done>, String>),
}

/// Admin work that changes something.
#[derive(Debug, Clone, PartialEq)]
pub enum Admin {
    Decide { id: i64, accept: bool, reason: Option<String> },
    MakeLead(i64),
    AddFeed(String),
    PauseFeed(i64, bool),
    RemoveFeed(i64),
    AddResource(NewResource),
    ArchiveResource(Snowflake),
    Appoint(String, Snowflake),
    Unappoint(String, Snowflake),
    AddLead(String, Snowflake),
    RemoveLead(String, Snowflake),
    Restore(Snowflake),
    Forget(Snowflake),
}

impl Admin {
    /// The data to load again afterwards.
    pub fn areas(&self) -> &'static [Area] {
        match self {
            // So a just-accepted applicant can be made a lead from the history.
            Self::Decide { .. } => &[Area::Queue, Area::History],
            Self::MakeLead(_) => &[Area::History, Area::Members],
            Self::AddFeed(_) | Self::PauseFeed(..) | Self::RemoveFeed(_) => &[Area::Feeds],
            Self::AddResource(_) | Self::ArchiveResource(_) => &[Area::Resources],
            _ => &[Area::Members],
        }
    }

    /// Whether it may change the config files.
    pub fn edits_config(&self) -> bool {
        matches!(
            self,
            Self::Decide { accept: true, .. }
                | Self::MakeLead(_)
                | Self::Appoint(..)
                | Self::Unappoint(..)
                | Self::AddLead(..)
                | Self::RemoveLead(..)
                | Self::Restore(_)
        )
    }
}

pub enum Job {
    Plan,
    Apply(Box<Plan>),
    Pull,
    Drift,
    Status,
    /// The server's copy of the config (remote mode).
    Schema,
    /// Config edits, in order; stops at the first refused.
    Ops(Vec<Op>),
    Load(Area),
    Admin(Admin),
}

pub enum Msg {
    Planned(Result<Plan, String>),
    Step(Action, Result<(), String>),
    Applied(Result<(), String>),
    Pulled(Result<Vec<chipsoc_engine::Pulled>, String>),
    Drifted(Result<Drift, String>),
    Status(Result<Status, String>),
    /// The server's config; the bool says whether that's all the job does.
    Schema(Result<Box<Schema>, String>, bool),
    Edited(Result<Vec<Change>, String>),
    Loaded(Area, Result<Data, String>),
    /// What admin work did, or why not.
    Did(Result<String, String>),
}

pub struct Worker<R> {
    /// The Discord client local work uses.
    pub(crate) rest: R,
    backend: Arc<Backend<R>>,
    /// The last plan made and the id to apply it by; one plan or apply at a time.
    planned: Arc<Mutex<Option<Planned>>>,
    tx: mpsc::UnboundedSender<Msg>,
}

impl<R: Clone> Clone for Worker<R> {
    fn clone(&self) -> Self {
        Self {
            rest: self.rest.clone(),
            backend: self.backend.clone(),
            planned: self.planned.clone(),
            tx: self.tx.clone(),
        }
    }
}

fn plain(e: anyhow::Error) -> String {
    format!("{e:#}")
}

impl<R: Rest + Clone + 'static> Worker<R> {
    /// Locally, with the bot's database where `DATABASE_URL` says.
    pub fn new(rest: R, dir: &Path, tx: mpsc::UnboundedSender<Msg>) -> Self {
        let backend = Backend::Local(Local::new(dir, rest.clone()));
        Self::with_backend(rest, dir, tx, backend)
    }

    /// The config is loaded per job, so a broken file never stops the TUI starting.
    pub fn with_backend(rest: R, _dir: &Path, tx: mpsc::UnboundedSender<Msg>, backend: Backend<R>) -> Self {
        Self { rest, backend: Arc::new(backend), planned: Default::default(), tx }
    }

    /// Send everything through the bot's web server.
    pub fn through(self, client: Client) -> anyhow::Result<Self> {
        Ok(Self { backend: Arc::new(Backend::Remote(client)), ..self })
    }

    pub fn spawn(&self, job: Job) {
        let w = self.clone();
        tokio::spawn(async move { w.run(job).await });
    }

    pub async fn run(&self, job: Job) {
        let msg = match job {
            Job::Plan => Msg::Planned(self.plan().await.map_err(plain)),
            Job::Apply(plan) => Msg::Applied(self.apply(&plan).await.map_err(plain)),
            Job::Pull => Msg::Pulled(self.backend.pull(false).await.map_err(plain)),
            Job::Drift => Msg::Drifted(self.backend.drift().await.map_err(plain)),
            Job::Status => Msg::Status(self.backend.status().await.map_err(plain)),
            Job::Schema => Msg::Schema(self.backend.schema().await.map(Box::new).map_err(plain), true),
            Job::Ops(ops) => {
                let done = self.backend.ops(&ops).await.map(|c| vec![c]).map_err(plain);
                self.server_schema().await;
                Msg::Edited(done)
            }
            Job::Load(area) => Msg::Loaded(area, self.load(area).await.map_err(plain)),
            Job::Admin(a) => {
                let (areas, config) = (a.areas(), a.edits_config());
                let did = self.admin(a).await.map_err(plain);
                if config {
                    self.server_schema().await;
                }
                for &area in areas {
                    let _ = self.tx.send(Msg::Loaded(area, self.load(area).await.map_err(plain)));
                }
                Msg::Did(did)
            }
        };
        let _ = self.tx.send(msg);
    }

    /// Through the web server, its copy of the config changed: fetch it again.
    async fn server_schema(&self) {
        if self.backend.is_remote() {
            let _ = self.tx.send(Msg::Schema(self.backend.schema().await.map(Box::new).map_err(plain), false));
        }
    }

    async fn load(&self, area: Area) -> anyhow::Result<Data> {
        let b = &self.backend;
        Ok(match area {
            Area::Queue => Data::Queue(b.queue().await?),
            Area::History => Data::History(b.history(50).await?),
            Area::Members => Data::Members(b.members().await?),
            Area::Feeds => Data::Feeds(b.feeds().await?),
            Area::Resources => Data::Resources(b.resources(25).await?, b.ideas(25).await.map_err(plain)),
            Area::Activity => Data::Activity(b.activity(50).await?, b.changes(None, 10).await.map_err(plain)),
        })
    }

    async fn admin(&self, a: Admin) -> anyhow::Result<String> {
        let b = &self.backend;
        Ok(match a {
            Admin::Decide { id, accept, reason } => {
                let d = b.decide(id, accept, reason.as_deref()).await?;
                if d.pending {
                    bail!("Accept didn't go through, so it's still waiting. {}", d.outcome);
                }
                let mark = if d.accepted { "Accepted." } else { "Rejected; they've been told." };
                let lead = if d.lead { " To make them a lead: h (history), then l." } else { "" };
                format!("{mark} {}{lead}", d.outcome).replace("  ", " ")
            }
            Admin::MakeLead(id) => b.make_lead(id).await?,
            Admin::AddFeed(url) => b.add_feed(&url).await?,
            Admin::PauseFeed(id, paused) => {
                b.pause_feed(id, paused).await?;
                if paused { "Paused." } else { "Resumed." }.into()
            }
            Admin::RemoveFeed(id) => {
                b.remove_feed(id).await?;
                "Removed the feed; its past posts stay.".into()
            }
            Admin::AddResource(r) => format!("Shared {}.", b.add_resource(&r).await?.title),
            Admin::ArchiveResource(id) => {
                b.archive_resource(id).await?;
                "Archived: the post is closed and locked, not deleted.".into()
            }
            Admin::Appoint(role, user) => b.appoint(&role, user).await?,
            Admin::Unappoint(role, user) => b.unappoint(&role, user).await?,
            Admin::AddLead(slug, user) => b.add_lead(&slug, user).await?,
            Admin::RemoveLead(slug, user) => b.remove_lead(&slug, user).await?,
            Admin::Restore(user) => b.restore(user).await?,
            Admin::Forget(user) => {
                b.forget(user).await?;
                "Forgotten; they won't be offered their old roles.".into()
            }
        })
    }

    async fn plan(&self) -> anyhow::Result<Plan> {
        let mut kept = self.planned.lock().await;
        let p = self.backend.plan(Options::default()).await?;
        let plan = p.plan.clone();
        *kept = Some(p);
        Ok(plan)
    }

    /// Apply the plan last shown, if `plan` is still it.
    async fn apply(&self, plan: &Plan) -> anyhow::Result<()> {
        let mut kept = self.planned.lock().await;
        let Some(p) = kept.take().filter(|p| p.plan == *plan) else {
            bail!("that plan is out of date; press f to check again");
        };
        let tx = self.tx.clone();
        let mut progress = move |a: &Action, r: &Result<(), String>| {
            let _ = tx.send(Msg::Step(a.clone(), r.clone()));
        };
        self.backend.apply(&p, &mut progress).await.map(drop)
    }
}
