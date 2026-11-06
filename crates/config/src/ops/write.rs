//! Making each [`Op`] on a config directory with toml_edit, so comments,
//! layout and the order of everything else stay as they were.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::Serialize;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike, Value};

use super::*;
use crate::authoring::{self, find_id};
use crate::bot::{BotConfig, default_questions};
use crate::schema::{Category, CategoryFile};
use crate::{category_paths, read_text, write_atomic};

const BOT: &str = "bot.toml";
const ONBOARDING: &str = "onboarding.toml";

/// A `[[section]]` list whose entries are told apart by `field`.
struct List {
    section: &'static str,
    field: &'static str,
    what: &'static str,
}

const ROLES: List = List { section: "role", field: "key", what: "role" };
const CHANNELS: List = List { section: "channel", field: "key", what: "channel" };
const POSITIONS: List = List { section: "position", field: "role", what: "position" };
const PROJECTS: List = List { section: "project", field: "slug", what: "project" };
const RULES: List = List { section: "rule", field: "key", what: "AutoMod rule" };
const PEERS: List = List { section: "peer", field: "name", what: "partner" };

pub(super) fn edit(dir: &Path, op: &Op) -> Result<()> {
    let file = |name: &str| dir.join(name);
    match op {
        Op::SetServer { set } => in_path(&file("server.toml"), |doc| {
            let (k, v) = server_entry(set);
            let t = doc.entry("guild").or_insert(toml_edit::table()).as_table_like_mut().context("no [guild] table")?;
            put(t, k, v);
            Ok(())
        }),
        Op::OrderRoles { keys } => authoring::reorder_roles(dir, keys),
        Op::OrderCategories { keys } => authoring::reorder_categories(dir, keys),
        Op::OrderChannels { category, keys } => authoring::reorder_channels(dir, category.as_deref(), keys),
        Op::AddRole { role } => authoring::add_role(dir, role),
        Op::SetRole { key, set } => set_in(&file("roles.toml"), &ROLES, key, role_entry(set)?),
        Op::MoveRole { key, to } => move_in(&file("roles.toml"), &ROLES, key, *to),
        Op::RemoveRole { key } => remove_from(&file("roles.toml"), &ROLES, key),
        Op::AddCategory { key, name } => add_category(dir, key, name),
        Op::RenameCategory { key, name } => in_path(&category_file(dir, key)?, |doc| {
            let t = doc.get_mut("category").and_then(Item::as_table_like_mut).context("no [category] table")?;
            put(t, "name", Some(name.as_str().into()));
            Ok(())
        }),
        Op::MoveCategory { key, to } => move_category(dir, key, *to),
        Op::SetCategoryOverwrites { key, overwrites } => in_path(&category_file(dir, key)?, |doc| {
            let t = doc.get_mut("category").and_then(Item::as_table_like_mut).context("no [category] table")?;
            put(t, "overwrites", overwrite_list(overwrites)?);
            Ok(())
        }),
        Op::RemoveCategory { key } => remove_category(dir, key),
        Op::AddChannel { category, channel } => authoring::add_channel(dir, category.as_deref(), channel),
        Op::SetChannel { key, set } => set_in(&channel_file(dir, key)?, &CHANNELS, key, channel_entry(set)?),
        Op::MoveChannel { key, to } => move_in(&channel_file(dir, key)?, &CHANNELS, key, *to),
        Op::MoveChannelTo { key, category } => move_channel_to(dir, key, category.as_deref()),
        Op::RemoveChannel { key } => remove_from(&channel_file(dir, key)?, &CHANNELS, key),
        Op::AddPosition { position } => add_to(&file(BOT), &POSITIONS, &position.role, position),
        Op::SetPosition { role, set } => set_in(&file(BOT), &POSITIONS, role, position_entry(set)?),
        Op::MovePosition { role, to } => move_in(&file(BOT), &POSITIONS, role, *to),
        Op::RemovePosition { role } => remove_from(&file(BOT), &POSITIONS, role),
        Op::AddHolder { role, user } => {
            ensure(authoring::add_holder(dir, role, user.0)?, || format!("{user} already holds {role}"))
        }
        Op::RemoveHolder { role, user } => {
            ensure(authoring::remove_holder(dir, role, user.0)?, || format!("{user} isn't listed for {role}"))
        }
        Op::AddProject { name, blurb, area, status } => {
            if name.trim().is_empty() {
                bail!("give the project a name");
            }
            let bot: BotConfig = crate::read_or_default(&file(BOT))?;
            authoring::add_project(dir, &authoring::new_project(name, blurb, area, *status, &bot))
        }
        Op::SetProject { slug, set } => {
            set_in(&file("projects.toml"), &PROJECTS, slug, project_entry(set)?)?;
            // Only active projects take members: leaving active closes applications too.
            if let ProjectField::Status(status) = set
                && *status != ProjectStatus::Active
                && takes_applications(dir, slug)?
            {
                set_in(&file("projects.toml"), &PROJECTS, slug, project_entry(&ProjectField::Applications(false))?)?;
            }
            Ok(())
        }
        Op::RenameProject { from, to } => super::rename::rename_project(dir, from, to),
        Op::AddLead { slug, user } => {
            ensure(authoring::add_project_lead(dir, slug, user.0)?, || format!("{user} already leads {slug}"))
        }
        Op::RemoveLead { slug, user } => in_path(&file("projects.toml"), |doc| {
            let arr = doc_list(doc, &PROJECTS)?;
            let i = index(arr, &PROJECTS, slug)?;
            let not_lead = || format!("{user} isn't a lead of {slug}");
            let leads = nth(arr, i)?.get_mut("leads").and_then(Item::as_array_mut).with_context(not_lead)?;
            let at = find_id(leads, user.0).with_context(not_lead)?;
            leads.remove(at);
            Ok(())
        }),
        Op::SetPost { channel, body } => set_post(dir, channel, body),
        Op::SetBrief { slug, body } => set_brief(dir, slug, body),
        Op::SetOnboarding { set } => in_path(&file(ONBOARDING), |doc| {
            let (k, v) = onboarding_entry(set)?;
            put(doc.as_table_mut(), k, v);
            Ok(())
        }),
        Op::AddPrompt { prompt } => in_path(&file(ONBOARDING), |doc| push(prompts(doc)?, prompt)),
        Op::SetPrompt { prompt, set } => in_path(&file(ONBOARDING), |doc| {
            let (k, v) = prompt_entry(set)?;
            put(nth(prompts(doc)?, *prompt)?, k, v);
            Ok(())
        }),
        Op::MovePrompt { prompt, to } => in_path(&file(ONBOARDING), |doc| move_at(prompts(doc)?, *prompt, *to)),
        Op::RemovePrompt { prompt } => in_path(&file(ONBOARDING), |doc| remove_at(prompts(doc)?, *prompt)),
        Op::AddOption { prompt, option } => in_path(&file(ONBOARDING), |doc| push(options(doc, *prompt)?, option)),
        Op::SetOption { prompt, option, set } => in_path(&file(ONBOARDING), |doc| {
            let (k, v) = option_entry(set)?;
            put(nth(options(doc, *prompt)?, *option)?, k, v);
            Ok(())
        }),
        Op::MoveOption { prompt, option, to } => {
            in_path(&file(ONBOARDING), |doc| move_at(options(doc, *prompt)?, *option, *to))
        }
        Op::RemoveOption { prompt, option } => {
            in_path(&file(ONBOARDING), |doc| remove_at(options(doc, *prompt)?, *option))
        }
        Op::SetScreening { set } => in_path(&file("screening.toml"), |doc| {
            let (k, v) = screening_entry(set)?;
            put(doc.as_table_mut(), k, v);
            Ok(())
        }),
        Op::AddRule { rule } => add_to(&file("automod.toml"), &RULES, &rule.key, rule),
        Op::SetRule { rule } => replace_in(&file("automod.toml"), &RULES, &rule.key, rule),
        Op::RemoveRule { key } => remove_from(&file("automod.toml"), &RULES, key),
        Op::SetBot { set } => in_path(&file(BOT), |doc| {
            let (k, v) = bot_entry(set)?;
            put_path(doc.as_table_mut(), k, v)
        }),
        Op::SetSection { section } => in_path(&file(BOT), |doc| set_section(doc, section)),
        Op::SetFederation { set } => in_path(&file("federation.toml"), |doc| {
            let (k, v) = federation_entry(set)?;
            put(doc.as_table_mut(), k, v);
            Ok(())
        }),
        Op::AddPeer { peer } => add_to(&file("federation.toml"), &PEERS, &peer.name, peer),
        Op::SetPeer { peer } => replace_in(&file("federation.toml"), &PEERS, &peer.name, peer),
        Op::RemovePeer { name } => remove_from(&file("federation.toml"), &PEERS, name),
    }
}

/// Whether projects.toml has `slug` taking applications.
/// Change a project's `slug` field in projects.toml.
pub(super) fn set_slug(dir: &Path, from: &str, to: &str) -> Result<()> {
    set_in(&dir.join("projects.toml"), &PROJECTS, from, ("slug", text(to)))
}

pub(super) fn takes_applications(dir: &Path, slug: &str) -> Result<bool> {
    let p: crate::schema::Projects = crate::read_or_default(&dir.join("projects.toml"))?;
    Ok(p.projects.iter().any(|p| p.slug == slug && p.application.open))
}

fn ensure(ok: bool, why: impl FnOnce() -> String) -> Result<()> {
    if ok { Ok(()) } else { Err(anyhow!(why())) }
}

// ---- files and lists ----

/// Edit one TOML file in place (a missing file starts empty).
fn in_path(path: &Path, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
    let text = read_text(path)?.unwrap_or_default();
    let mut doc: DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
    f(&mut doc)?;
    write_atomic(path, &doc.to_string())
}

/// The `[[section]]` list in `t`, made empty if it isn't there.
fn list<'a>(t: &'a mut Table, l: &List) -> Result<&'a mut ArrayOfTables> {
    // An empty list is written `x = []`; it's still a list of tables.
    let empty = t.get(l.section).and_then(Item::as_array).is_some_and(|a| a.is_empty());
    if empty || !t.contains_key(l.section) {
        t.insert(l.section, Item::ArrayOfTables(ArrayOfTables::new()));
    }
    t[l.section].as_array_of_tables_mut().with_context(|| format!("`{}` isn't a list of tables", l.section))
}

fn doc_list<'a>(doc: &'a mut DocumentMut, l: &List) -> Result<&'a mut ArrayOfTables> {
    list(doc.as_table_mut(), l)
}

fn find(arr: &ArrayOfTables, l: &List, id: &str) -> Option<usize> {
    arr.iter().position(|t| t.get(l.field).and_then(Item::as_str) == Some(id))
}

fn index(arr: &ArrayOfTables, l: &List, id: &str) -> Result<usize> {
    find(arr, l, id).with_context(|| format!("no {} `{id}`", l.what))
}

fn nth(arr: &mut ArrayOfTables, i: usize) -> Result<&mut Table> {
    arr.get_mut(i).with_context(|| format!("there's no number {}", i + 1))
}

fn add_to(path: &Path, l: &List, id: &str, value: &impl Serialize) -> Result<()> {
    in_path(path, |doc| {
        let arr = doc_list(doc, l)?;
        if find(arr, l, id).is_some() {
            bail!("a {} `{id}` already exists", l.what);
        }
        push(arr, value)
    })
}

fn set_in(path: &Path, l: &List, id: &str, (key, v): (&str, Option<Value>)) -> Result<()> {
    in_path(path, |doc| {
        let arr = doc_list(doc, l)?;
        let i = index(arr, l, id)?;
        put_path(nth(arr, i)?, key, v)
    })
}

/// Replace an entry's fields with `value`'s, keeping unchanged lines (and their comments).
fn replace_in(path: &Path, l: &List, id: &str, value: &impl Serialize) -> Result<()> {
    in_path(path, |doc| {
        let arr = doc_list(doc, l)?;
        let i = index(arr, l, id)?;
        merge(nth(arr, i)?, to_table(value)?);
        Ok(())
    })
}

fn move_in(path: &Path, l: &List, id: &str, to: Move) -> Result<()> {
    in_path(path, |doc| {
        let arr = doc_list(doc, l)?;
        let i = index(arr, l, id)?;
        move_at(arr, i, to)
    })
}

fn remove_from(path: &Path, l: &List, id: &str) -> Result<()> {
    in_path(path, |doc| {
        let arr = doc_list(doc, l)?;
        let i = index(arr, l, id)?;
        remove_at(arr, i)
    })
}

fn push(arr: &mut ArrayOfTables, value: &impl Serialize) -> Result<()> {
    arr.push(to_table(value)?);
    Ok(())
}

fn remove_at(arr: &mut ArrayOfTables, i: usize) -> Result<()> {
    nth(arr, i)?;
    arr.remove(i);
    Ok(())
}

/// Where `from` ends up after `to`, in a list of `n`.
fn destination(from: usize, to: Move, n: usize) -> Result<usize> {
    let dest = match to {
        Move::Up => from.checked_sub(1),
        Move::Down => Some(from + 1).filter(|d| *d < n),
        Move::To(i) => Some(i).filter(|d| *d < n),
    };
    dest.context("it can't move there")
}

fn move_at(arr: &mut ArrayOfTables, from: usize, to: Move) -> Result<()> {
    nth(arr, from)?;
    let dest = destination(from, to, arr.len())?;
    let mut tables: Vec<Table> = arr.iter().cloned().collect();
    let t = tables.remove(from);
    tables.insert(dest, t);
    // Unplaced tables are written in list order, each with its own sub-tables.
    let mut out = ArrayOfTables::new();
    for mut t in tables {
        unplace(&mut t);
        out.push(t);
    }
    *arr = out;
    Ok(())
}

/// Forget where a table (and its sub-tables) sat, so it's written after the one before it.
fn unplace(t: &mut Table) {
    t.set_position(None);
    for (_, item) in t.iter_mut() {
        match item {
            Item::Table(t) => unplace(t),
            Item::ArrayOfTables(a) => a.iter_mut().for_each(unplace),
            _ => {}
        }
    }
}

// ---- values ----

/// Set (or with `None`, remove) one value, keeping the comments around the old one.
fn put(t: &mut dyn TableLike, key: &str, v: Option<Value>) {
    let Some(mut v) = v else {
        t.remove(key);
        return;
    };
    if let Some(old) = t.get(key).and_then(Item::as_value) {
        *v.decor_mut() = old.decor().clone();
    }
    replace(t, key, Item::Value(v));
}

/// Set an item, in place if it's there (so comments on its key stay).
fn replace(t: &mut dyn TableLike, key: &str, item: Item) {
    match t.get_mut(key) {
        Some(old) => *old = item,
        None => {
            t.insert(key, item);
        }
    }
}

/// [`put`] at a dotted path, making tables on the way.
fn put_path(t: &mut dyn TableLike, path: &str, v: Option<Value>) -> Result<()> {
    let Some((head, rest)) = path.split_once('.') else {
        put(t, path, v);
        return Ok(());
    };
    if t.get(head).is_none() {
        if v.is_none() {
            return Ok(());
        }
        t.insert(head, Item::Table(Table::new()));
    }
    let inner = t.get_mut(head).and_then(Item::as_table_like_mut).with_context(|| format!("`{head}` isn't a table"))?;
    put_path(inner, rest, v)
}

/// `old` takes `new`'s keys; lines whose value is unchanged are left alone.
fn merge(old: &mut Table, new: Table) {
    let gone: Vec<String> = old.iter().map(|(k, _)| k.to_owned()).filter(|k| !new.contains_key(k)).collect();
    for k in gone {
        old.remove(&k);
    }
    for (k, item) in new {
        if old.get(&k).is_some_and(|o| same(o, &item)) {
            continue;
        }
        match item.into_value() {
            Ok(v) => put(old, &k, Some(v)),
            Err(item) => replace(old, &k, item),
        }
    }
}

/// Whether two values mean the same, however they're written.
fn same(a: &Item, b: &Item) -> bool {
    let parse = |i: &Item| {
        let v = i.clone().into_value().ok()?;
        toml::from_str::<toml::Table>(&format!("x = {v}")).ok()
    };
    matches!((parse(a), parse(b)), (Some(x), Some(y)) if x == y)
}

/// A value as a fresh table, written the way the schema writes it.
fn to_table(value: &impl Serialize) -> Result<Table> {
    let doc: DocumentMut = toml::to_string(value)?.parse()?;
    let mut t = doc.as_table().clone();
    unplace(&mut t);
    Ok(t)
}

/// A value as TOML (lists of tables become inline).
fn val(value: &impl Serialize) -> Result<Value> {
    #[derive(Serialize)]
    struct One<'a, T: Serialize> {
        v: &'a T,
    }
    let doc: DocumentMut = toml::to_string(&One { v: value })?.parse()?;
    let item = doc.as_table().get("v").cloned().context("nothing to write")?;
    item.into_value().map_err(|_| anyhow!("can't write that as a value"))
}

fn text(s: &str) -> Option<Value> {
    Some(s.into())
}

fn opt(s: &Option<String>) -> Option<Value> {
    s.as_deref().map(Value::from)
}

/// Only `true` is written (`false` is the default).
fn flag(b: bool) -> Option<Value> {
    b.then(|| true.into())
}

/// Only `false` is written (`true` is the default).
fn unless_true(b: bool) -> Option<Value> {
    (!b).then(|| false.into())
}

fn strings(v: &[String]) -> Option<Value> {
    Some(Value::Array(v.iter().map(String::as_str).collect()))
}

/// Left out when empty.
fn some_strings(v: &[String]) -> Option<Value> {
    (!v.is_empty()).then(|| strings(v)).flatten()
}

fn number(n: impl Into<i64>) -> Option<Value> {
    Some(n.into().into())
}

type Entry = (&'static str, Option<Value>);

fn server_entry(f: &ServerField) -> Entry {
    let level = |n: &Option<u8>| (*n).and_then(number);
    match f {
        ServerField::Name(s) => ("name", text(s)),
        ServerField::Description(s) => ("description", opt(s)),
        ServerField::SystemChannel(c) => ("system_channel", opt(c)),
        ServerField::RulesChannel(c) => ("rules_channel", opt(c)),
        ServerField::UpdatesChannel(c) => ("updates_channel", opt(c)),
        ServerField::AfkChannel(c) => ("afk_channel", opt(c)),
        ServerField::VerificationLevel(n) => ("verification_level", level(n)),
        ServerField::ExplicitContentFilter(n) => ("explicit_content_filter", level(n)),
        ServerField::DefaultMessageNotifications(n) => ("default_message_notifications", level(n)),
    }
}

fn role_entry(f: &RoleField) -> Result<Entry> {
    Ok(match f {
        RoleField::Name(s) => ("name", text(s)),
        RoleField::Color(c) => ("color", opt(c)),
        RoleField::Hoist(b) => ("hoist", flag(*b)),
        RoleField::Mentionable(b) => ("mentionable", flag(*b)),
        RoleField::Permissions(p) => ("permissions", some_strings(p)),
    })
}

fn channel_entry(f: &ChannelField) -> Result<Entry> {
    Ok(match f {
        ChannelField::Name(s) => ("name", text(s)),
        ChannelField::Topic(t) => ("topic", opt(t)),
        ChannelField::Kind(k) => ("type", (*k != ChannelKind::Text).then(|| k.as_str().into())),
        ChannelField::Slowmode(n) => ("slowmode", (*n > 0).then(|| number(*n)).flatten()),
        ChannelField::Nsfw(b) => ("nsfw", flag(*b)),
        ChannelField::UserLimit(n) => ("user_limit", (*n).and_then(number)),
        ChannelField::Tags(t) => ("tags", some_strings(t)),
        ChannelField::Overwrites(o) => ("overwrites", overwrite_list(o)?),
        ChannelField::Inherit(b) => ("inherit", unless_true(*b)),
    })
}

/// Overwrites as an inline list; left out when there are none.
fn overwrite_list(o: &[Overwrite]) -> Result<Option<Value>> {
    if o.is_empty() { Ok(None) } else { val(&o.to_vec()).map(Some) }
}

fn position_entry(f: &PositionField) -> Result<Entry> {
    Ok(match f {
        PositionField::Label(s) => ("label", opt(s)),
        PositionField::Admin(b) => ("admin", flag(*b)),
        PositionField::Open(b) => ("open", unless_true(*b)),
        PositionField::Intro(s) => ("intro", opt(s)),
        PositionField::Questions(q) if *q == default_questions() => ("questions", None),
        PositionField::Questions(q) => ("questions", Some(val(q)?)),
    })
}

fn project_entry(f: &ProjectField) -> Result<Entry> {
    let nonempty = |s: &str| (!s.is_empty()).then(|| s.into());
    Ok(match f {
        ProjectField::Name(s) => ("name", text(s)),
        ProjectField::Blurb(s) => ("blurb", nonempty(s)),
        ProjectField::Area(s) => ("kind", nonempty(s)),
        ProjectField::Status(s) => ("status", text(s.as_str())),
        ProjectField::Stage(s) => ("stage", opt(s)),
        ProjectField::Github(s) => ("github", opt(s)),
        ProjectField::Channels(c) => ("channels", strings(c)),
        ProjectField::Tags(t) => ("tags", some_strings(t)),
        ProjectField::TeamRole(b) => ("team_role", unless_true(*b)),
        ProjectField::LeadRole(b) => ("lead_role", unless_true(*b)),
        ProjectField::Applications(b) => ("application.open", Some((*b).into())),
        ProjectField::LeadQuestion(b) => ("application.lead_question", flag(*b)),
        ProjectField::Questions(q) => ("application.questions", some_strings(q)),
    })
}

fn onboarding_entry(f: &OnboardingField) -> Result<Entry> {
    Ok(match f {
        OnboardingField::Enabled(b) => ("enabled", Some((*b).into())),
        OnboardingField::Advanced(b) => ("advanced", flag(*b)),
        OnboardingField::DefaultChannels(c) => ("default_channels", some_strings(c)),
    })
}

fn prompt_entry(f: &PromptField) -> Result<Entry> {
    Ok(match f {
        PromptField::Title(s) => ("title", text(s)),
        PromptField::SingleSelect(b) => ("single_select", unless_true(*b)),
        PromptField::Required(b) => ("required", flag(*b)),
    })
}

fn option_entry(f: &OptionField) -> Result<Entry> {
    Ok(match f {
        OptionField::Title(s) => ("title", text(s)),
        OptionField::Description(s) => ("description", opt(s)),
        OptionField::Emoji(s) => ("emoji", opt(s)),
        OptionField::Role(s) => ("role", (!s.is_empty()).then(|| s.as_str().into())),
        OptionField::Channels(c) => ("channels", some_strings(c)),
    })
}

fn screening_entry(f: &ScreeningField) -> Result<Entry> {
    Ok(match f {
        ScreeningField::Enabled(b) => ("enabled", Some((*b).into())),
        ScreeningField::Description(s) => ("description", text(s)),
        ScreeningField::Label(s) => ("label", text(s)),
        ScreeningField::Rules(r) => ("rules", strings(r)),
    })
}

fn bot_entry(f: &BotField) -> Result<Entry> {
    Ok(match f {
        BotField::Name(s) => ("name", text(s)),
        BotField::Timezone(s) => ("timezone", text(s)),
        BotField::AdminRole(s) => ("admin_role", opt(s)),
        BotField::Archive(s) => ("archive", opt(s)),
        BotField::LogPeople(s) => ("log.people", opt(s)),
        BotField::LogBot(s) => ("log.bot", opt(s)),
        BotField::KeepDays(n) => ("log.keep_days", number(*n)),
    })
}

fn federation_entry(f: &FederationField) -> Result<Entry> {
    Ok(match f {
        FederationField::Name(s) => ("name", text(s)),
        FederationField::Inbox(s) => ("inbox", opt(s)),
        FederationField::Publish(p) => ("publish", strings(p)),
    })
}

/// A bot.toml section's table name and new contents (none: turned off).
fn section_table(s: &Section) -> Result<(&'static str, Option<Table>)> {
    fn t(v: &Option<impl Serialize>) -> Result<Option<Table>> {
        v.as_ref().map(to_table).transpose()
    }
    Ok(match s {
        Section::Review(v) => ("review", t(v)?),
        Section::Events(v) => ("events", t(v)?),
        Section::Feeds(v) => ("feeds", t(v)?),
        Section::Ideas(v) => ("ideas", t(v)?),
        Section::Help(v) => ("help", t(v)?),
        Section::Projects(v) => ("projects", t(v)?),
        Section::Profile(v) => ("profile", t(v)?),
        Section::Resources(v) => ("resources", t(v)?),
        Section::Web(v) => ("web", t(v)?),
        Section::Drift(DriftMode::Undo) => ("drift", None),
        Section::Drift(mode) => ("drift", Some(to_table(&crate::bot::Drift { mode: *mode })?)),
    })
}

fn set_section(doc: &mut DocumentMut, s: &Section) -> Result<()> {
    let (key, new) = section_table(s)?;
    match (new, doc.get_mut(key).and_then(Item::as_table_mut)) {
        (None, _) => {
            doc.remove(key);
        }
        (Some(new), Some(old)) => merge(old, new),
        (Some(new), None) => {
            doc.insert(key, Item::Table(new));
        }
    }
    Ok(())
}

// ---- onboarding ----

fn prompts(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables> {
    list(doc.as_table_mut(), &List { section: "prompt", field: "title", what: "question" })
}

fn options(doc: &mut DocumentMut, prompt: usize) -> Result<&mut ArrayOfTables> {
    list(nth(prompts(doc)?, prompt)?, &List { section: "option", field: "title", what: "option" })
}

// ---- structure ----

/// The category file defining `key`.
fn category_file(dir: &Path, key: &str) -> Result<PathBuf> {
    let files = authoring::category_files(dir)?;
    files.into_iter().find(|(k, ..)| k == key).map(|(.., p)| p).with_context(|| format!("no category `{key}`"))
}

/// The file defining channel `key` (a category file or channels.toml).
fn channel_file(dir: &Path, key: &str) -> Result<PathBuf> {
    let mut files = category_paths(dir)?;
    files.push(dir.join("channels.toml"));
    for path in files {
        let Some(text) = read_text(&path)? else { continue };
        let doc: DocumentMut = text.parse().with_context(|| format!("parsing {}", path.display()))?;
        let channels = doc.get("channel").and_then(Item::as_array_of_tables);
        if channels.is_some_and(|a| find(a, &CHANNELS, key).is_some()) {
            return Ok(path);
        }
    }
    bail!("no channel `{key}` in the config files (project channels are set in projects.toml)")
}

fn add_category(dir: &Path, key: &str, name: &str) -> Result<()> {
    let files = authoring::category_files(dir)?;
    if files.iter().any(|(k, ..)| k == key) {
        bail!("a category `{key}` already exists");
    }
    let number = |p: &PathBuf| p.file_stem()?.to_str()?.split('-').next()?.parse::<u32>().ok();
    let next = files.iter().filter_map(|(.., p)| number(p)).max().unwrap_or(0) + 10;
    let cf = CategoryFile {
        category: Category { key: key.into(), name: name.into(), overwrites: Vec::new() },
        channels: Vec::new(),
    };
    let body = toml::to_string_pretty(&cf)?;
    fs::create_dir_all(dir.join("categories"))?;
    if next < 100 {
        return write_atomic(&dir.join("categories").join(format!("{next:02}-{}.toml", crate::slug(name))), &body);
    }
    // No two-digit number left after the last one: number them all again.
    write_atomic(&dir.join("categories").join(format!("0-{}.toml", crate::slug(name))), &body)?;
    let keys: Vec<String> = files.into_iter().map(|(k, ..)| k).chain([key.to_owned()]).collect();
    authoring::reorder_categories(dir, &keys)
}

fn move_category(dir: &Path, key: &str, to: Move) -> Result<()> {
    let mut keys: Vec<String> = authoring::category_files(dir)?.into_iter().map(|(k, ..)| k).collect();
    let from = keys.iter().position(|k| k == key).with_context(|| format!("no category `{key}`"))?;
    let k = keys.remove(from);
    keys.insert(destination(from, to, keys.len() + 1)?, k);
    authoring::reorder_categories(dir, &keys)
}

fn remove_category(dir: &Path, key: &str) -> Result<()> {
    let path = category_file(dir, key)?;
    let cf: CategoryFile = toml::from_str(&read_text(&path)?.unwrap_or_default())?;
    if !cf.channels.is_empty() {
        bail!("category `{key}` still has channels; move or remove them first");
    }
    fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))
}

fn move_channel_to(dir: &Path, key: &str, category: Option<&str>) -> Result<()> {
    let from = channel_file(dir, key)?;
    let to = match category {
        Some(c) => category_file(dir, c)?,
        None => dir.join("channels.toml"),
    };
    if from == to {
        bail!("channel `{key}` is already there");
    }
    let mut table = None;
    in_path(&from, |doc| {
        let arr = doc_list(doc, &CHANNELS)?;
        table = Some(arr.remove(index(arr, &CHANNELS, key)?));
        Ok(())
    })?;
    in_path(&to, |doc| {
        let mut t = table.context("channel not found")?;
        unplace(&mut t);
        doc_list(doc, &CHANNELS)?.push(t);
        Ok(())
    })
}

// ---- posts ----

fn set_post(dir: &Path, channel: &str, body: &str) -> Result<()> {
    let snap = crate::load_snapshot(dir)?;
    let ch = snap.channel(channel).with_context(|| format!("no channel `{channel}` in the config files"))?;
    let file = match &ch.post {
        Some(f) => f.clone(),
        None => {
            let f = format!("content/{channel}.md");
            set_in(&channel_file(dir, channel)?, &CHANNELS, channel, ("post", text(&f)))?;
            f
        }
    };
    write_text(&crate::inside(dir, &file)?, body)
}

fn set_brief(dir: &Path, slug: &str, body: &str) -> Result<()> {
    let projects: crate::schema::Projects = crate::read_or_default(&dir.join("projects.toml"))?;
    let p = projects.projects.iter().find(|p| p.slug == slug).with_context(|| format!("no project `{slug}`"))?;
    let file = match &p.brief {
        Some(f) => f.clone(),
        None => {
            let f = format!("content/projects/{slug}.md");
            set_in(&dir.join("projects.toml"), &PROJECTS, slug, ("brief", text(&f)))?;
            f
        }
    };
    write_text(&crate::inside(dir, &file)?, body)
}

/// Markdown, ending in one newline.
fn write_text(path: &Path, body: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    write_atomic(path, &format!("{}\n", body.trim_end()))
}
