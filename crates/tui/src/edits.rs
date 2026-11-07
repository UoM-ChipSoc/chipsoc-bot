//! What each tab's keys do: forms and confirmations that end in config
//! [`Op`]s or admin jobs. Nothing is changed here directly; every change is a
//! [`Job`] the worker runs through the backend.

use chipsoc_client::remote::NewResource;
use chipsoc_config::bot::Position;
use chipsoc_config::ops::{ChannelField, Move, Op, PositionField, ProjectField, RoleField, Schema};
use chipsoc_config::schema::{Channel, ChannelKind, ProjectStatus, Role};
use chipsoc_config::{Snowflake, slug};

use crate::app::{App, Command, Overlay, Tab};
use crate::form::{self, Form, Input, Key, StatusPicker, status_help};
use crate::keys::{self, Act, Binding};
use crate::lists::{self, Shared, Spot, Who};
use crate::worker::{Admin, Job};

/// What saving a form does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Then {
    NewRole,
    EditRole(String),
    NewChannel,
    NewCategory,
    EditChannel(String),
    EditCategory(String),
    MoveTo(String),
    NewProject,
    EditProject(String),
    AddLead(String),
    RemoveLead(String),
    NewPosition,
    EditPosition(String),
    Reject(i64),
    Appoint,
    AddFeed,
    AddResource,
    /// Close this idea's post, with the reason given.
    CloseIdea(Snowflake),
}

impl Then {
    /// What Enter does on the form, for the footer: "save" only for plain config edits.
    pub fn verb(&self) -> &'static str {
        match self {
            Self::NewRole => "add role",
            Self::NewChannel => "add channel",
            Self::NewCategory => "add category",
            Self::NewProject => "add project",
            Self::NewPosition => "add position",
            Self::MoveTo(_) => "move",
            Self::EditRole(_)
            | Self::EditChannel(_)
            | Self::EditCategory(_)
            | Self::EditProject(_)
            | Self::EditPosition(_) => "save",
            Self::AddLead(_) => "add lead",
            Self::RemoveLead(_) => "remove lead…",
            Self::Reject(_) => "reject",
            Self::Appoint => "appoint",
            Self::AddFeed => "follow",
            Self::AddResource => "share",
            Self::CloseIdea(_) => "close idea",
        }
    }
}

/// Text edited in $EDITOR, and where it goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    Post(String),
    Brief(String),
    Intro(String),
    Questions(String),
    RolePerms(String),
    ChannelPerms(String),
    CategoryPerms(String),
}

const USER_HINT: &str =
    "Their Discord user ID: Settings → Advanced → Developer Mode, then right-click them → Copy User ID";

const ADMIN_HINT: &str = "Holders also get the admin role: the dashboard, /server and Approve";

/// A Discord user id typed into a form.
fn user_id(s: &str) -> Result<Snowflake, String> {
    Snowflake::parse_mention(s).ok_or_else(|| "That isn't a Discord user id (a long number).".to_owned())
}

/// `Some(x)` if `new` differs from `old`.
fn changed<T: PartialEq>(old: &T, new: T) -> Option<T> {
    (*old != new).then_some(new)
}

impl App {
    fn spot(&self) -> Option<Spot> {
        lists::spots(self.schema.as_ref()?).get(self.sel()).cloned()
    }

    fn role(&self) -> Option<&Role> {
        self.schema.as_ref()?.roles.get(self.sel())
    }

    fn project(&self) -> Option<&chipsoc_config::Project> {
        self.schema.as_ref()?.projects.get(self.sel())
    }

    fn position(&self) -> Option<&Position> {
        self.schema.as_ref()?.bot.positions.get(self.sel())
    }

    fn who(&self) -> Option<Who> {
        lists::people(self.roster.get()?).get(self.sel()).cloned()
    }

    fn shared(&self) -> Option<Shared> {
        let (r, i) = self.resources.get()?;
        lists::shared(r, i.as_deref().unwrap_or_default()).get(self.sel()).copied()
    }

    fn channel(&self, key: &str) -> Option<&Channel> {
        let s = self.schema.as_ref()?;
        s.top_level.iter().chain(s.categories.iter().flat_map(|c| &c.channels)).find(|c| c.key == key)
    }

    /// The keys for the current tab and selection (while nothing runs).
    pub(crate) fn tab_keys(&self) -> Vec<Binding> {
        use keys::*;
        let loaded = self.schema.is_some();
        let any = self.len(self.tab) > 0;
        let mut v = if any { vec![MOVE] } else { Vec::new() };
        match self.tab {
            Tab::Overview => return vec![SCROLL, PAGE, EDIT_SERVER],
            Tab::Changes => {
                let mut v = vec![SCROLL, PAGE, FETCH];
                v.extend(self.can_apply_now().then_some(APPLY));
                v.extend([PULL, DRIFT]);
                return v;
            }
            Tab::Roles if loaded => match self.role() {
                Some(r) if r.managed => v.extend([NEW_ROLE, EDIT]),
                Some(_) => v.extend([OPEN, NEW_ROLE, PERMISSIONS, REORDER, REMOVE, EDIT]),
                None => v.push(NEW_ROLE),
            },
            Tab::Channels if loaded => match self.spot() {
                Some(Spot::Category(_)) => {
                    v.extend([OPEN, NEW_CHANNEL, NEW_CATEGORY, PERMISSIONS, REORDER, REMOVE_CATEGORY, EDIT])
                }
                Some(Spot::Channel(..)) => {
                    v.extend([OPEN, NEW_CHANNEL, NEW_CATEGORY, PERMISSIONS, POST, REORDER, MOVE_TO, ARCHIVE, EDIT])
                }
                None => v.extend([NEW_CHANNEL, NEW_CATEGORY]),
            },
            Tab::Projects if loaded => match self.project() {
                Some(p) => {
                    v.extend([OPEN, STATUS, APPLICATIONS, BRIEF]);
                    v.extend(p.lead_role.then_some(ADD_LEAD));
                    v.extend((!p.leads.is_empty()).then_some(REMOVE_LEAD));
                    v.extend([NEW_PROJECT, EDIT]);
                }
                None => v.extend([NEW_PROJECT, EDIT]),
            },
            Tab::Positions if loaded => {
                if any {
                    v.extend([OPEN, OPEN_POSITION, INTRO, QUESTIONS, REORDER, REMOVE_POSITION]);
                }
                v.extend([NEW_POSITION, EDIT]);
            }
            Tab::Queue => {
                if self.show_history {
                    let i = self.sel();
                    let past = self.history.get().and_then(|h| h.get(i));
                    v.extend(
                        past.filter(|p| p.kind == "project" && p.status == "accepted" && p.as_lead).map(|_| MAKE_LEAD),
                    );
                } else if any {
                    v.extend([ACCEPT, REJECT]);
                }
                if self.show_history {
                    v.push(BACK_TO_QUEUE);
                } else if self.queue.get().is_some() || self.history.get().is_some() {
                    v.push(HISTORY);
                }
            }
            Tab::Members => {
                let roles = self.roster.get().is_some_and(|r| !r.holders.is_empty());
                v.extend(roles.then_some(APPOINT));
                match self.who() {
                    Some(Who::Holder(..) | Who::Lead(..)) => v.push(REMOVE),
                    Some(Who::Former(_)) => v.extend([RESTORE, FORGET]),
                    _ => {}
                }
                if matches!(self.who(), Some(Who::Project(_) | Who::Lead(..))) {
                    v.push(ADD_LEAD);
                }
            }
            Tab::Feeds if self.feeds.get().is_some() => {
                v.extend(self.feeds_on().then_some(NEW_FEED));
                if any {
                    v.extend([PAUSE, REMOVE_FEED]);
                }
            }
            Tab::Resources if self.resources.get().is_some() => {
                v.extend(self.resources_on().then_some(NEW_RESOURCE));
                v.extend(matches!(self.shared(), Some(Shared::Resource(_))).then_some(ARCHIVE_POST));
                v.extend(self.open_idea().is_some().then_some(CLOSE_IDEA));
            }
            _ => {}
        }
        // As last drawn: only when the details don't fit.
        if any && self.detail_max.get() > 0 && !matches!(self.tab, Tab::Overview | Tab::Changes) {
            v.push(DETAILS);
        }
        v
    }

    /// Whether bot.toml has [feeds] (the server's copy when signed in).
    pub(crate) fn feeds_on(&self) -> bool {
        self.schema.as_ref().is_none_or(|s| s.bot.feeds.is_some())
    }

    pub(crate) fn resources_on(&self) -> bool {
        self.schema.as_ref().is_none_or(|s| s.bot.resources.is_some())
    }

    pub(crate) fn review_on(&self) -> bool {
        self.schema.as_ref().is_none_or(|s| s.bot.review.is_some())
    }

    fn can_apply_now(&self) -> bool {
        self.problems().is_empty() && self.changes.plan().is_some_and(|p| !p.is_empty())
    }

    pub(crate) fn tab_act(&mut self, act: Act) -> Option<Command> {
        match (self.tab, act) {
            (Tab::Queue, Act::History) => self.toggle_history(),
            (Tab::Roles, a) => self.role_act(a),
            (Tab::Channels, a) => self.channel_act(a),
            (Tab::Projects, a) => self.project_act(a),
            (Tab::Positions, a) => self.position_act(a),
            (Tab::Queue, a) => self.queue_act(a),
            (Tab::Members, a) => self.member_act(a),
            (Tab::Feeds, a) => self.feed_act(a),
            (Tab::Resources, a) => self.resource_act(a),
            _ => None,
        }
    }

    fn form(&mut self, title: impl Into<String>, fields: Vec<Input>, then: Then) -> Option<Command> {
        self.overlay = Some(Overlay::Form(Form::new(title, fields, then)));
        None
    }

    /// Move the selection along with what it moved.
    fn moved(&mut self, by: isize, op: Op) -> Option<Command> {
        let i = self.tab.index();
        let n = self.len(self.tab);
        self.sel[i] = self.sel[i].saturating_add_signed(by).min(n.saturating_sub(1));
        self.ops(vec![op])
    }

    fn text(&mut self, text: String, target: Target) -> Option<Command> {
        Some(Command::Text(text, target))
    }

    // ---- roles ----

    fn role_act(&mut self, act: Act) -> Option<Command> {
        if act == Act::New {
            let fields =
                vec![Input::text(Key::Name, ""), Input::text(Key::Colour, "").hint("#rrggbb, or blank for none")];
            return self.form("New role", fields, Then::NewRole);
        }
        let r = self.role()?.clone();
        let key = r.key.clone();
        match act {
            Act::Open => {
                let fields = vec![
                    Input::text(Key::Name, &r.name),
                    Input::text(Key::Colour, r.color.clone().unwrap_or_default()).hint("#rrggbb, or blank for none"),
                    Input::toggle(Key::ListedApart, r.hoist).hint("Shown in its own group in the member list"),
                    Input::toggle(Key::Mentionable, r.mentionable),
                ];
                self.form(format!("Edit role {}", r.name), fields, Then::EditRole(key))
            }
            Act::Permissions => {
                let all = self.schema.as_ref().map(|s| s.choices.permissions.clone()).unwrap_or_default();
                self.text(form::permissions_text(&r.permissions, &all), Target::RolePerms(key))
            }
            Act::MoveUp => self.moved(-1, Op::MoveRole { key, to: Move::Up }),
            Act::MoveDown => self.moved(1, Op::MoveRole { key, to: Move::Down }),
            Act::Remove => {
                let lines = vec![
                    format!("Take the role {} out of the config?", r.name),
                    "Discord keeps the role; delete it there if you want it gone.".into(),
                ];
                self.confirm("Remove role?", lines, Job::Ops(vec![Op::RemoveRole { key }]));
                None
            }
            _ => None,
        }
    }

    // ---- channels ----

    fn channel_act(&mut self, act: Act) -> Option<Command> {
        let spot = self.spot();
        let cats: Vec<(String, String)> = self
            .schema
            .as_ref()
            .map(|s| s.categories.iter().map(|c| (c.category.key.clone(), c.category.name.clone())).collect())
            .unwrap_or_default();
        match (act, spot) {
            (Act::New, spot) => {
                let here = match &spot {
                    Some(Spot::Category(c)) | Some(Spot::Channel(_, Some(c))) => c.clone(),
                    _ => String::new(),
                };
                let kinds = ChannelKind::ALL.iter().map(|k| (k.as_str().to_owned(), k.as_str().to_owned())).collect();
                let mut places = vec![(String::new(), "(no category)".to_owned())];
                places.extend(cats);
                let fields = vec![
                    Input::text(Key::Name, ""),
                    Input::choice(Key::Type, kinds, "text"),
                    Input::choice(Key::Category, places, &here),
                ];
                self.form("New channel", fields, Then::NewChannel)
            }
            (Act::NewCategory, _) => self.form("New category", vec![Input::text(Key::Name, "")], Then::NewCategory),
            (Act::Open, Some(Spot::Category(k))) => {
                let name = cats.iter().find(|(c, _)| *c == k).map(|(_, n)| n.clone()).unwrap_or_default();
                self.form("Rename category", vec![Input::text(Key::Name, name)], Then::EditCategory(k))
            }
            (Act::Open, Some(Spot::Channel(k, _))) => {
                let c = self.channel(&k)?.clone();
                let mut fields = vec![
                    Input::text(Key::Name, &c.name),
                    Input::text(Key::Topic, c.topic.clone().unwrap_or_default()),
                    Input::text(Key::SlowMode, c.slowmode.to_string()).hint("Seconds between messages; 0 for off"),
                    Input::toggle(Key::AgeRestricted, c.nsfw),
                ];
                if c.kind.has_tags() {
                    fields.push(Input::text(Key::Tags, c.tags.join(", ")).hint("Forum tags, separated by commas"));
                }
                if matches!(c.kind, ChannelKind::Voice | ChannelKind::Stage) {
                    let limit = c.user_limit.map(|n| n.to_string()).unwrap_or_default();
                    fields.push(Input::text(Key::UserLimit, limit).hint("Blank or 0 for no limit"));
                }
                self.form(format!("Edit #{}", c.name), fields, Then::EditChannel(k))
            }
            (Act::Permissions, Some(spot)) => {
                let s = self.schema.as_ref()?;
                let roles: Vec<String> = s.roles.iter().map(|r| r.key.clone()).collect();
                let (ows, target) = match spot {
                    Spot::Category(k) => {
                        let cf = s.categories.iter().find(|c| c.category.key == k)?;
                        (cf.category.overwrites.clone(), Target::CategoryPerms(k))
                    }
                    Spot::Channel(k, _) => (self.channel(&k)?.overwrites.clone(), Target::ChannelPerms(k)),
                };
                self.text(form::overwrites_text(&ows, &roles), target)
            }
            (Act::Post, Some(Spot::Channel(k, _))) => {
                let body = self.schema.as_ref()?.posts.get(&k).map(|p| p.body.clone()).unwrap_or_default();
                self.text(body, Target::Post(k))
            }
            (Act::MoveUp | Act::MoveDown, Some(spot)) => {
                let (by, to) = if act == Act::MoveUp { (-1, Move::Up) } else { (1, Move::Down) };
                match spot {
                    Spot::Category(key) => self.ops(vec![Op::MoveCategory { key, to }]),
                    Spot::Channel(key, _) => self.moved(by, Op::MoveChannel { key, to }),
                }
            }
            (Act::MoveTo, Some(Spot::Channel(k, here))) => {
                let mut places = vec![(String::new(), "(no category)".to_owned())];
                places.extend(cats);
                let field = Input::choice(Key::Category, places, here.as_deref().unwrap_or_default());
                self.form("Move to category", vec![field], Then::MoveTo(k))
            }
            (Act::Remove, Some(Spot::Channel(k, _))) => {
                let lines = vec![
                    format!("Take #{k} out of the config?"),
                    "On apply it moves to the archive category: hidden, history kept, never deleted.".into(),
                ];
                self.confirm("Archive channel?", lines, Job::Ops(vec![Op::RemoveChannel { key: k }]));
                None
            }
            (Act::Remove, Some(Spot::Category(k))) => {
                let lines =
                    vec![format!("Remove the empty category {k}?"), "When you apply, it moves to the archive.".into()];
                self.confirm("Remove category?", lines, Job::Ops(vec![Op::RemoveCategory { key: k }]));
                None
            }
            _ => None,
        }
    }

    // ---- projects ----

    fn areas(&self) -> Vec<String> {
        self.schema.as_ref().and_then(|s| s.bot.projects.as_ref()).map(|p| p.areas.clone()).unwrap_or_default()
    }

    fn area_input(&self, current: &str) -> Input {
        let areas = self.areas();
        if areas.is_empty() {
            Input::text(Key::Area, current)
        } else {
            let mut options: Vec<(String, String)> = areas.into_iter().map(|a| (a.clone(), a)).collect();
            if !current.is_empty() && !options.iter().any(|(a, _)| a == current) {
                options.push((current.to_owned(), current.to_owned()));
            }
            Input::choice(Key::Area, options, current)
        }
    }

    fn area_of(&self, f: &Form<Then>) -> String {
        if self.areas().is_empty() { f.text(Key::Area) } else { f.picked(Key::Area) }
    }

    fn project_act(&mut self, act: Act) -> Option<Command> {
        if act == Act::New {
            let first = self.areas().first().cloned().unwrap_or_default();
            let statuses = ProjectStatus::ALL.iter().map(|s| (s.as_str().to_owned(), s.as_str().to_owned())).collect();
            let fields = vec![
                Input::text(Key::Name, ""),
                Input::text(Key::Blurb, ""),
                self.area_input(&first),
                Input::choice(Key::Status, statuses, "proposed"),
            ];
            return self.form("New project", fields, Then::NewProject);
        }
        let p = self.project()?.clone();
        let slug = p.slug.clone();
        match act {
            Act::Open => {
                let fields = vec![
                    Input::text(Key::Name, &p.name),
                    Input::text(Key::Blurb, &p.blurb),
                    self.area_input(&p.kind),
                    Input::text(Key::Stage, p.stage.clone().unwrap_or_default()).hint("e.g. taping out"),
                    Input::text(Key::CodeLink, p.github.clone().unwrap_or_default()).hint("Optional"),
                    Input::text(Key::Tags, p.tags.join(", ")).hint("Separated by commas"),
                    Input::text(Key::Channels, p.channels.join(", ")).hint("Its channels' names, separated by commas"),
                ];
                self.form(format!("Edit {}", p.name), fields, Then::EditProject(slug))
            }
            Act::SetStatus => {
                self.overlay = Some(Overlay::Status(StatusPicker { slug, name: p.name, status: p.status }));
                None
            }
            Act::ToggleApplications => {
                self.ops(vec![Op::SetProject { slug, set: ProjectField::Applications(!p.application.open) }])
            }
            Act::Brief => {
                let body = self.schema.as_ref()?.briefs.get(&slug).map(|b| b.body.clone()).unwrap_or_default();
                self.text(body, Target::Brief(slug))
            }
            Act::AddLead => {
                let field = Input::text(Key::UserId, "").hint(USER_HINT);
                self.form(format!("Add a lead to {}", p.name), vec![field], Then::AddLead(slug))
            }
            Act::RemoveLead => {
                let names = self.roster.get();
                let leads = p
                    .leads
                    .iter()
                    .map(|u| (u.to_string(), names.map_or_else(|| u.to_string(), |r| lists::name_of(r, *u))))
                    .collect();
                let field = Input::choice(Key::Lead, leads, "").hint("They lose its Lead role straight away");
                self.form(format!("Remove a lead from {}", p.name), vec![field], Then::RemoveLead(slug))
            }
            _ => None,
        }
    }

    /// Enter on the status picker: every change of status does something
    /// to roles or channels, so ask first.
    pub(crate) fn status_picked(&mut self, p: StatusPicker) -> Option<Command> {
        let current = self.schema.as_ref()?.projects.iter().find(|x| x.slug == p.slug)?.status;
        let Some(what) = chipsoc_config::projects::status_change(current, p.status) else {
            self.info("Nothing changed.");
            return None;
        };
        let job = Job::Ops(vec![Op::SetProject { slug: p.slug.clone(), set: ProjectField::Status(p.status) }]);
        let lines = vec![
            format!("Make {} {}: {}?", p.name, p.status, status_help(p.status)),
            format!("{}{}.", what[..1].to_uppercase(), &what[1..]),
        ];
        self.confirm("Change status?", lines, job);
        None
    }

    // ---- positions ----

    fn position_act(&mut self, act: Act) -> Option<Command> {
        if act == Act::New {
            let s = self.schema.as_ref()?;
            let taken: Vec<&str> = s.bot.positions.iter().map(|p| p.role.as_str()).collect();
            let roles: Vec<(String, String)> = s
                .roles
                .iter()
                .filter(|r| !r.managed && r.key != "everyone" && !taken.contains(&r.key.as_str()))
                .map(|r| (r.key.clone(), r.name.clone()))
                .collect();
            if roles.is_empty() {
                self.error("Every role already has a position; add a role first (tab 9).");
                return None;
            }
            let fields = vec![
                Input::choice(Key::Role, roles, ""),
                Input::text(Key::Label, "").hint("Shown in /volunteer; blank uses the role's name"),
                Input::toggle(Key::AdminPosition, false).labelled(self.admin_label()).hint(ADMIN_HINT),
            ];
            return self.form("New position", fields, Then::NewPosition);
        }
        let p = self.position()?.clone();
        let role = p.role.clone();
        match act {
            Act::Open => {
                let fields = vec![
                    Input::text(Key::Label, p.label.clone().unwrap_or_default())
                        .hint("Shown in /volunteer; blank uses the role's name"),
                    Input::toggle(Key::AdminPosition, p.admin).labelled(self.admin_label()).hint(ADMIN_HINT),
                    Input::toggle(Key::TakingApplications, p.open),
                ];
                let name = self.position_name(&p);
                self.form(format!("Edit position {name}"), fields, Then::EditPosition(role))
            }
            Act::ToggleOpen => self.ops(vec![Op::SetPosition { role, set: PositionField::Open(!p.open) }]),
            Act::Intro => self.text(p.intro.clone().unwrap_or_default(), Target::Intro(role)),
            Act::Questions => self.text(form::questions_text(&p.questions), Target::Questions(role)),
            Act::MoveUp => self.moved(-1, Op::MovePosition { role, to: Move::Up }),
            Act::MoveDown => self.moved(1, Op::MovePosition { role, to: Move::Down }),
            Act::Remove => {
                let lines = vec![
                    format!("Remove the position {}?", self.position_name(&p)),
                    "The role stays; people just can't apply for it.".into(),
                ];
                self.confirm("Remove position?", lines, Job::Ops(vec![Op::RemovePosition { role }]));
                None
            }
            _ => None,
        }
    }

    // ---- queue, members, feeds, resources ----

    fn queue_act(&mut self, act: Act) -> Option<Command> {
        if act == Act::MakeLead {
            // Only those who asked to lead, as the web UI and the README say.
            let past = self.history.get()?.get(self.sel()).filter(|p| p.as_lead)?.clone();
            let lines = vec![
                format!("Make {} a lead of {}?", lists::sender(&past.user, &past.from), past.what),
                "They get its Lead role straight away.".into(),
            ];
            self.confirm("Make lead?", lines, Job::Admin(Admin::MakeLead(past.id)));
            return None;
        }
        let app = self.queue.get()?.get(self.sel())?.clone();
        let who = lists::sender(&app.user, &app.from);
        let item = lists::item(&app.kind, &app.what);
        let (accepting, rejecting, reason) = chipsoc_admin::says_of(&app.kind);
        match act {
            Act::Accept => {
                let lines = vec![format!("Accept {who}'s {item}?"), accepting.into()];
                let job = Job::Admin(Admin::Decide { id: app.id, accept: true, reason: None });
                self.confirm("Accept?", lines, job);
                None
            }
            Act::Reject => {
                let field = Input::text(Key::Reason, "").labelled(reason).hint(rejecting);
                self.form(format!("Reject {who}'s {item}"), vec![field], Then::Reject(app.id))
            }
            _ => None,
        }
    }

    fn member_act(&mut self, act: Act) -> Option<Command> {
        let who = self.who();
        let roster = self.roster.get()?.clone();
        match (act, who) {
            (Act::New, who) => {
                let here = match who {
                    Some(Who::Role(r) | Who::Holder(r, _)) => r,
                    _ => String::new(),
                };
                let roles = roster.holders.iter().map(|h| (h.role.clone(), h.name.clone())).collect();
                let fields = vec![
                    Input::choice(Key::PositionOrRole, roles, &here).hint("Roles the bot keeps up to date"),
                    Input::text(Key::UserId, "").hint(USER_HINT),
                ];
                self.form("Appoint", fields, Then::Appoint)
            }
            (Act::Remove, Some(Who::Holder(role, user))) => {
                let lines = vec![format!(
                    "Take {} away from {}? They lose it in Discord straight away.",
                    lists::role_name(self.schema.as_ref()?, &role),
                    lists::name_of(&roster, user)
                )];
                self.confirm("Remove?", lines, Job::Admin(Admin::Unappoint(role, user)));
                None
            }
            (Act::Remove, Some(Who::Lead(slug, user))) => {
                self.confirm_remove_lead(slug, user);
                None
            }
            (Act::AddLead, Some(Who::Project(slug) | Who::Lead(slug, _))) => {
                let field = Input::text(Key::UserId, "").hint(USER_HINT);
                self.form(format!("Add a lead to {slug}"), vec![field], Then::AddLead(slug))
            }
            (Act::Restore, Some(Who::Former(user))) => {
                let roles = roster.former.iter().find(|f| f.person.id == user).map_or_else(String::new, |f| {
                    f.roles.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
                });
                let lines = vec![
                    format!("Give {} back what they held when they left: {roles}?", lists::name_of(&roster, user)),
                    "Only once they're back in the server.".into(),
                ];
                self.confirm("Restore?", lines, Job::Admin(Admin::Restore(user)));
                None
            }
            (Act::Forget, Some(Who::Former(user))) => {
                let lines = vec![format!(
                    "Forget {}? They won't be offered their old roles back.",
                    lists::name_of(&roster, user)
                )];
                self.confirm("Forget?", lines, Job::Admin(Admin::Forget(user)));
                None
            }
            _ => None,
        }
    }

    fn feed_act(&mut self, act: Act) -> Option<Command> {
        if act == Act::New {
            let field = Input::text(Key::Address, "").hint("A feed's https:// address, or a website that has one");
            return self.form("Follow a feed", vec![field], Then::AddFeed);
        }
        let f = self.feeds.get()?.get(self.sel())?.clone();
        match act {
            Act::TogglePause => Some(self.start(Job::Admin(Admin::PauseFeed(f.id, !f.paused)))),
            Act::Remove => {
                let lines =
                    vec![format!("Stop following {}?", chipsoc_admin::feeds::shown(&f)), "Its past posts stay.".into()];
                self.confirm("Remove feed?", lines, Job::Admin(Admin::RemoveFeed(f.id)));
                None
            }
            _ => None,
        }
    }

    fn resource_act(&mut self, act: Act) -> Option<Command> {
        match (act, self.shared()) {
            (Act::New, _) => {
                let fields = vec![
                    Input::text(Key::Title, ""),
                    Input::text(Key::Address, "").hint("https://…"),
                    Input::text(Key::Description, ""),
                    Input::text(Key::Tags, "").hint("The forum's tags, separated by commas"),
                ];
                self.form("Share a link", fields, Then::AddResource)
            }
            (Act::Remove, Some(Shared::Resource(i))) => {
                let post = self.resources.get()?.0.get(i)?.clone();
                let lines = vec![format!("Archive \"{}\"? The post is closed and locked, not deleted.", post.title)];
                self.confirm("Archive post?", lines, Job::Admin(Admin::ArchiveResource(post.id)));
                None
            }
            (Act::Remove, Some(Shared::Idea(_))) => {
                let post = self.open_idea()?;
                let field = Input::text(Key::Reason, "")
                    .labelled("Reason (optional, posted on the idea)")
                    .hint("It's locked and stays readable; it's never deleted");
                self.form(format!("Close \"{}\"", post.title), vec![field], Then::CloseIdea(post.id))
            }
            _ => None,
        }
    }

    /// The idea selected, if it's still open.
    fn open_idea(&self) -> Option<chipsoc_admin::Post> {
        let Some(Shared::Idea(i)) = self.shared() else { return None };
        let ideas = self.resources.get()?.1.as_ref().ok()?;
        ideas.get(i).filter(|p| !p.locked).cloned()
    }

    // ---- saving ----

    /// Save a form: its job, or the form again with what's wrong.
    pub(crate) fn submit(&mut self, mut f: Form<Then>) -> Option<Command> {
        // Taking a lead off acts in Discord at once: ask first.
        if let Then::RemoveLead(slug) = &f.then {
            match user_id(&f.picked(Key::Lead)) {
                Ok(user) => self.confirm_remove_lead(slug.clone(), user),
                Err(e) => self.error(e),
            }
            return None;
        }
        match self.job_for(&f) {
            Ok(Some(job)) => {
                let cmd = self.start(job);
                self.waiting = Some(f);
                Some(cmd)
            }
            Ok(None) => {
                self.info("Nothing changed.");
                None
            }
            Err(e) => {
                f.error = Some(e);
                self.overlay = Some(Overlay::Form(f));
                None
            }
        }
    }

    fn confirm_remove_lead(&mut self, slug: String, user: Snowflake) {
        let name = self.roster.get().map_or_else(|| user.to_string(), |r| lists::name_of(r, user));
        let project = self.schema.as_ref().and_then(|s| s.projects.iter().find(|p| p.slug == slug));
        let project = project.map_or(slug.clone(), |p| p.name.clone());
        let lines = vec![format!("Take {name} off {project}'s leads? They lose the Lead role straight away.")];
        self.confirm("Remove lead?", lines, Job::Admin(Admin::RemoveLead(slug, user)));
    }

    /// The admin-position toggle, by the admin role's name: "Also gives Committee".
    fn admin_label(&self) -> String {
        let s = self.schema.as_ref();
        match s.and_then(|s| Some((s, s.bot.admin_role.as_deref()?))) {
            Some((s, k)) => format!("Also gives {}", lists::role_name(s, k)),
            None => Key::AdminPosition.label().to_owned(),
        }
    }

    fn position_name(&self, p: &Position) -> String {
        let role = self.schema.as_ref().map_or(p.role.as_str(), |s| lists::role_name(s, &p.role));
        p.label.clone().unwrap_or_else(|| role.to_owned())
    }

    fn job_for(&self, f: &Form<Then>) -> Result<Option<Job>, String> {
        let s = self.schema.as_ref().ok_or("The config isn't loaded.")?;
        let key_of = |f: &Form<Then>| slug(&f.text(Key::Name));
        let need_name = |f: &Form<Then>| {
            if f.text(Key::Name).is_empty() { Err("Give it a name.".to_owned()) } else { Ok(()) }
        };
        let ops = |v: Vec<Op>| Ok((!v.is_empty()).then_some(Job::Ops(v)));
        match &f.then {
            Then::NewRole => {
                need_name(f)?;
                let role =
                    Role { key: key_of(f), name: f.text(Key::Name), color: f.opt(Key::Colour), ..Role::default() };
                ops(vec![Op::AddRole { role }])
            }
            Then::EditRole(key) => {
                let r = s.roles.iter().find(|r| &r.key == key).ok_or("That role is gone.")?;
                need_name(f)?;
                let set = |set| Op::SetRole { key: key.clone(), set };
                let v = [
                    changed(&r.name, f.text(Key::Name)).map(RoleField::Name),
                    changed(&r.color, f.opt(Key::Colour)).map(RoleField::Color),
                    changed(&r.hoist, f.on(Key::ListedApart)).map(RoleField::Hoist),
                    changed(&r.mentionable, f.on(Key::Mentionable)).map(RoleField::Mentionable),
                ];
                ops(v.into_iter().flatten().map(set).collect())
            }
            Then::NewChannel => {
                need_name(f)?;
                let kind = f.picked(Key::Type).parse::<ChannelKind>().unwrap_or_default();
                let channel = Channel { key: key_of(f), name: f.text(Key::Name), kind, ..Channel::default() };
                let category = Some(f.picked(Key::Category)).filter(|c| !c.is_empty());
                ops(vec![Op::AddChannel { category, channel }])
            }
            Then::NewCategory => {
                need_name(f)?;
                ops(vec![Op::AddCategory { key: key_of(f), name: f.text(Key::Name) }])
            }
            Then::EditCategory(key) => {
                need_name(f)?;
                let old = s.categories.iter().find(|c| &c.category.key == key).map(|c| c.category.name.clone());
                let name = f.text(Key::Name);
                ops(if old.as_ref() == Some(&name) {
                    vec![]
                } else {
                    vec![Op::RenameCategory { key: key.clone(), name }]
                })
            }
            Then::EditChannel(key) => {
                let c = self.channel(key).ok_or("That channel is gone.")?;
                need_name(f)?;
                let slowmode: u16 = f.text(Key::SlowMode).parse().map_err(|_| "Slow mode is a number of seconds.")?;
                let mut v = vec![
                    changed(&c.name, f.text(Key::Name)).map(ChannelField::Name),
                    changed(&c.topic, f.opt(Key::Topic)).map(ChannelField::Topic),
                    changed(&c.slowmode, slowmode).map(ChannelField::Slowmode),
                    changed(&c.nsfw, f.on(Key::AgeRestricted)).map(ChannelField::Nsfw),
                ];
                if c.kind.has_tags() {
                    v.push(changed(&c.tags, f.list(Key::Tags)).map(ChannelField::Tags));
                }
                if matches!(c.kind, ChannelKind::Voice | ChannelKind::Stage) {
                    let limit = match f.text(Key::UserLimit).as_str() {
                        "" | "0" => None,
                        n => Some(n.parse::<u16>().map_err(|_| "The user limit is a number.")?),
                    };
                    v.push(changed(&c.user_limit, limit).map(ChannelField::UserLimit));
                }
                ops(v.into_iter().flatten().map(|set| Op::SetChannel { key: key.clone(), set }).collect())
            }
            Then::MoveTo(key) => {
                let category = Some(f.picked(Key::Category)).filter(|c| !c.is_empty());
                ops(vec![Op::MoveChannelTo { key: key.clone(), category }])
            }
            Then::NewProject => {
                if f.text(Key::Name).is_empty() {
                    return Err("Give the project a name.".into());
                }
                let status = f.picked(Key::Status).parse::<ProjectStatus>().unwrap_or_default();
                let op = Op::AddProject {
                    name: f.text(Key::Name),
                    blurb: f.text(Key::Blurb),
                    area: self.area_of(f),
                    status,
                };
                ops(vec![op])
            }
            Then::EditProject(slug) => {
                let p = s.projects.iter().find(|p| &p.slug == slug).ok_or("That project is gone.")?;
                need_name(f)?;
                let v = [
                    changed(&p.name, f.text(Key::Name)).map(ProjectField::Name),
                    changed(&p.blurb, f.text(Key::Blurb)).map(ProjectField::Blurb),
                    changed(&p.kind, self.area_of(f)).map(ProjectField::Area),
                    changed(&p.stage, f.opt(Key::Stage)).map(ProjectField::Stage),
                    changed(&p.github, f.opt(Key::CodeLink)).map(ProjectField::Github),
                    changed(&p.tags, f.list(Key::Tags)).map(ProjectField::Tags),
                    changed(&p.channels, f.list(Key::Channels)).map(ProjectField::Channels),
                ];
                ops(v.into_iter().flatten().map(|set| Op::SetProject { slug: slug.clone(), set }).collect())
            }
            Then::AddLead(slug) => Ok(Some(Job::Admin(Admin::AddLead(slug.clone(), user_id(&f.text(Key::UserId))?)))),
            Then::RemoveLead(_) => Ok(None),
            Then::NewPosition => {
                let position = Position {
                    role: f.picked(Key::Role),
                    label: f.opt(Key::Label),
                    admin: f.on(Key::AdminPosition),
                    ..Position::default()
                };
                ops(vec![Op::AddPosition { position }])
            }
            Then::EditPosition(role) => {
                let p = s.bot.positions.iter().find(|p| &p.role == role).ok_or("That position is gone.")?;
                let v = [
                    changed(&p.label, f.opt(Key::Label)).map(PositionField::Label),
                    changed(&p.admin, f.on(Key::AdminPosition)).map(PositionField::Admin),
                    changed(&p.open, f.on(Key::TakingApplications)).map(PositionField::Open),
                ];
                ops(v.into_iter().flatten().map(|set| Op::SetPosition { role: role.clone(), set }).collect())
            }
            Then::Reject(id) => {
                Ok(Some(Job::Admin(Admin::Decide { id: *id, accept: false, reason: f.opt(Key::Reason) })))
            }
            Then::CloseIdea(post) => Ok(Some(Job::Admin(Admin::CloseIdea(*post, f.opt(Key::Reason))))),
            Then::Appoint => {
                let role = f.picked(Key::PositionOrRole);
                Ok(Some(Job::Admin(Admin::Appoint(role, user_id(&f.text(Key::UserId))?))))
            }
            Then::AddFeed => match f.opt(Key::Address) {
                Some(url) => Ok(Some(Job::Admin(Admin::AddFeed(url)))),
                None => Err("Give the feed's address.".into()),
            },
            Then::AddResource => {
                let r = NewResource {
                    title: f.text(Key::Title),
                    url: f.text(Key::Address),
                    description: f.text(Key::Description),
                    tags: f.list(Key::Tags),
                };
                if r.title.is_empty() || r.url.is_empty() {
                    return Err("Give it a title and an address.".into());
                }
                Ok(Some(Job::Admin(Admin::AddResource(r))))
            }
        }
    }

    /// After $EDITOR closes on some text: the edit to make, if any.
    pub fn text_edited(&mut self, target: Target, before: &str, after: anyhow::Result<String>) -> Option<Command> {
        let text = match after {
            Ok(t) => t,
            Err(e) => {
                self.error(format!("The editor didn't run: {e:#}"));
                return None;
            }
        };
        if text.trim_end() == before.trim_end() {
            self.info("Nothing changed.");
            return None;
        }
        let op = match target {
            Target::Post(channel) => Ok(Op::SetPost { channel, body: text }),
            Target::Brief(slug) => Ok(Op::SetBrief { slug, body: text }),
            Target::Intro(role) => {
                let intro = Some(text.trim().to_owned()).filter(|t| !t.is_empty());
                Ok(Op::SetPosition { role, set: PositionField::Intro(intro) })
            }
            Target::Questions(role) => {
                form::parse_questions(&text).map(|q| Op::SetPosition { role, set: PositionField::Questions(q) })
            }
            Target::RolePerms(key) => {
                Ok(Op::SetRole { key, set: RoleField::Permissions(form::parse_permissions(&text)) })
            }
            Target::ChannelPerms(key) => {
                form::parse_overwrites(&text).map(|o| Op::SetChannel { key, set: ChannelField::Overwrites(o) })
            }
            Target::CategoryPerms(key) => {
                form::parse_overwrites(&text).map(|overwrites| Op::SetCategoryOverwrites { key, overwrites })
            }
        };
        match op {
            Ok(op) => self.ops(vec![op]),
            Err(e) => {
                self.error(format!("Not saved: {e}"));
                None
            }
        }
    }
}

/// For drawing: the schema's channel, wherever it is.
pub fn find_channel<'a>(s: &'a Schema, key: &str) -> Option<&'a Channel> {
    s.top_level.iter().chain(s.categories.iter().flat_map(|c| &c.channels)).find(|c| c.key == key)
}
