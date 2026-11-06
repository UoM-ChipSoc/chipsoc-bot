//! Categories and their channels: add, rename, edit, reorder, move between
//! categories, archive, and who can see or do what (permission overwrites).
//! Each change is an `Op` on the category files or channels.toml.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_config::ops::{ChannelField, Op, Schema};
use chipsoc_config::{Channel, ChannelKind, Overwrite};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{
    self, Fields, Shown, check, confirm, edit, input_of, is_key, key_from, need_confirm, select, shown, text, wrong,
};
use crate::view::{self, Tab, csrf, empty, permission};

const HERE: &str = "/channels";

pub(crate) async fn page<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

async fn view<R: Rest>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    match app.schema().await {
        Ok(cfg) => shown(sh, render(s, &cfg, sh)),
        Err(e) => view::broken(Tab::Channels, s, &e).into_response(),
    }
}

/// `/channels/permissions/{kind}/{key}`: the overwrites editor for one category or channel.
pub(crate) async fn permissions<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path((kind, key)): Path<(String, String)>,
) -> Response {
    perms_view(&app, &s, &kind, &key, &Shown::none()).await
}

async fn perms_view<R: Rest>(app: &App<R>, s: &Session, kind: &str, key: &str, sh: &Shown) -> Response {
    match app.schema().await {
        Ok(cfg) => match perms_page(s, &cfg, kind, key, sh) {
            Some(page) => shown(sh, page),
            None => view::page(Tab::Channels, s, view::trouble("There's no such category or channel.")).into_response(),
        },
        Err(e) => view::broken(Tab::Channels, s, &e).into_response(),
    }
}

pub(crate) async fn act<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
    Path(action): Path<String>,
    Form(f): Form<Vec<(String, String)>>,
) -> Response {
    let f = Fields(f);
    let cfg = match app.schema().await {
        Ok(c) => c,
        Err(e) => return view::broken(Tab::Channels, &s, &e).into_response(),
    };
    let built = build(&action, &f, &cfg);
    match edit(&app, &s, HERE, f.clone(), built).await {
        Ok(done) => done,
        Err(sh) if action.ends_with("-perms") => {
            let kind = if action == "category-perms" { "category" } else { "channel" };
            perms_view(&app, &s, kind, f.get("key"), &sh).await
        }
        Err(sh) => view(&app, &s, &sh).await,
    }
}

/// Every channel the files define, with its category's key.
fn all_channels(cfg: &Schema) -> impl Iterator<Item = (Option<&str>, &Channel)> {
    let cats = cfg.categories.iter().flat_map(|c| c.channels.iter().map(|ch| (Some(c.category.key.as_str()), ch)));
    cats.chain(cfg.top_level.iter().map(|ch| (None, ch)))
}

fn find<'a>(cfg: &'a Schema, key: &str) -> Option<&'a Channel> {
    all_channels(cfg).map(|(_, c)| c).find(|c| c.key == key)
}

fn build(action: &str, f: &Fields, cfg: &Schema) -> Result<(String, Vec<Op>), Shown> {
    let key = f.get("key").to_owned();
    match action {
        "add-category" => {
            let form = "cat-add";
            let name = f.get("name");
            if name.is_empty() {
                return Err(wrong(form, f, "name", "Give the category a name."));
            }
            let key = key_from(f.get("key"), name);
            if !is_key(&key) {
                return Err(wrong(form, f, "key", "Use lower-case letters, digits and dashes."));
            }
            Ok((form.into(), vec![Op::AddCategory { key, name: name.to_owned() }]))
        }
        "rename-category" => {
            let form = format!("cat-{key}");
            let name = f.get("name");
            if name.is_empty() {
                return Err(wrong(&form, f, "name", "A category needs a name."));
            }
            Ok((form, vec![Op::RenameCategory { key, name: name.to_owned() }]))
        }
        "move-category" => Ok((format!("cat-{key}"), vec![Op::MoveCategory { key, to: form::direction(f) }])),
        "remove-category" => {
            let form = format!("cat-{key}");
            need_confirm(&form, f)?;
            Ok((form, vec![Op::RemoveCategory { key }]))
        }
        "category-perms" => {
            let form = format!("catperm-{key}");
            Ok((form, vec![Op::SetCategoryOverwrites { key, overwrites: overwrites(f) }]))
        }
        "add-channel" => {
            let form = "chan-add";
            let name = f.get("name");
            if name.is_empty() {
                return Err(wrong(form, f, "name", "Give the channel a name."));
            }
            let key = key_from(f.get("key"), name);
            if !is_key(&key) {
                return Err(wrong(form, f, "key", "Use lower-case letters, digits and dashes."));
            }
            let Some(kind) = kind_of(f.get("type")) else { return Err(wrong(form, f, "type", "Pick a type.")) };
            let channel = Channel { key, name: name.to_owned(), kind, topic: f.opt("topic"), ..Default::default() };
            Ok((form.into(), vec![Op::AddChannel { category: f.opt("category"), channel }]))
        }
        "edit-channel" => {
            let form = format!("chan-{key}");
            let Some(c) = find(cfg, &key) else { return Err(form::refuse(&form, f, "No such channel.")) };
            let name = f.get("name");
            if name.is_empty() {
                return Err(wrong(&form, f, "name", "A channel needs a name."));
            }
            let Some(kind) = kind_of(f.get("type")) else { return Err(wrong(&form, f, "type", "Pick a type.")) };
            let slowmode = match f.get("slowmode") {
                "" => 0,
                v => match v.parse::<u16>() {
                    Ok(n) => n,
                    Err(_) => return Err(wrong(&form, f, "slowmode", "Seconds, as a whole number (0 for off).")),
                },
            };
            let limit = match f.get("user_limit") {
                "" | "0" => None,
                n => match n.parse::<u16>() {
                    Ok(n) => Some(n),
                    Err(_) => return Err(wrong(&form, f, "user_limit", "A whole number, or empty for no limit.")),
                },
            };
            let set = |set| Op::SetChannel { key: key.clone(), set };
            let mut ops = Vec::new();
            if name != c.name {
                ops.push(set(ChannelField::Name(name.to_owned())));
            }
            if f.opt("topic") != c.topic {
                ops.push(set(ChannelField::Topic(f.opt("topic"))));
            }
            if kind != c.kind {
                ops.push(set(ChannelField::Kind(kind)));
            }
            if slowmode != c.slowmode {
                ops.push(set(ChannelField::Slowmode(slowmode)));
            }
            if f.has("nsfw") != c.nsfw {
                ops.push(set(ChannelField::Nsfw(f.has("nsfw"))));
            }
            if f.has("user_limit") && limit != c.user_limit {
                ops.push(set(ChannelField::UserLimit(limit)));
            }
            if f.has("tags") && f.list("tags") != c.tags {
                ops.push(set(ChannelField::Tags(f.list("tags"))));
            }
            Ok((form, ops))
        }
        "channel-perms" => {
            let form = format!("chanperm-{key}");
            let Some(c) = find(cfg, &key) else { return Err(form::refuse(&form, f, "No such channel.")) };
            let mut ops = vec![Op::SetChannel { key: key.clone(), set: ChannelField::Overwrites(overwrites(f)) }];
            if f.has("inherit") != c.inherit {
                ops.push(Op::SetChannel { key, set: ChannelField::Inherit(f.has("inherit")) });
            }
            Ok((form, ops))
        }
        "move-channel" => Ok((format!("chan-{key}"), vec![Op::MoveChannel { key, to: form::direction(f) }])),
        "move-channel-to" => Ok((format!("chan-{key}"), vec![Op::MoveChannelTo { key, category: f.opt("category") }])),
        "archive-channel" => {
            let form = format!("chan-{key}");
            need_confirm(&form, f)?;
            Ok((form, vec![Op::RemoveChannel { key }]))
        }
        _ => Err(form::refuse("", f, "Unknown action.")),
    }
}

fn kind_of(s: &str) -> Option<ChannelKind> {
    s.parse().ok()
}

/// The overwrites editor's entries: `ow{i}-role` (or `-member`), and per
/// permission `ow{i}-{PERM}` = allow or deny; `ow{i}-drop` leaves one out.
fn overwrites(f: &Fields) -> Vec<Overwrite> {
    let n: usize = f.get("count").parse().unwrap_or(0);
    let perms = chipsoc_model::perms::all();
    let mut out = Vec::new();
    for i in 0..n.min(50) {
        let at = |name: &str| format!("ow{i}-{name}");
        let role = f.opt(&at("role"));
        let member = form::user_id(f.get(&at("member")));
        if f.has(&at("drop")) || (role.is_none() && member.is_none()) {
            continue;
        }
        let set = |want: &str| perms.iter().filter(|p| f.get(&at(p)) == want).map(|p| (*p).to_owned()).collect();
        let (allow, deny): (Vec<String>, Vec<String>) = (set("allow"), set("deny"));
        out.push(Overwrite { role: if member.is_some() { None } else { role }, member, allow, deny });
    }
    out
}

fn kinds() -> Vec<(String, String)> {
    ChannelKind::ALL.iter().map(|k| (k.as_str().to_owned(), kind_label(*k).to_owned())).collect()
}

fn kind_label(k: ChannelKind) -> &'static str {
    match k {
        ChannelKind::Text => "Text",
        ChannelKind::Voice => "Voice",
        ChannelKind::Announcement => "Announcements",
        ChannelKind::Stage => "Stage",
        ChannelKind::Forum => "Forum (posts with tags)",
        ChannelKind::Media => "Media (posts with tags)",
    }
}

/// (key, name) of each category, with "Not in a category" first.
fn places(cfg: &Schema) -> Vec<(String, String)> {
    let cats = cfg.categories.iter().map(|c| (c.category.key.clone(), c.category.name.clone()));
    std::iter::once((String::new(), "Not in a category".to_owned())).chain(cats).collect()
}

fn render(s: &Session, cfg: &Schema, sh: &Shown) -> Html<String> {
    let n = cfg.categories.len();
    view::page(
        Tab::Channels,
        s,
        rsx! {
            h1 { "Channels" }
            p { class: "hint",
                "In Discord's order. Channels take their category's permissions unless they say otherwise. "
                "Archived channels are hidden with their history kept, never deleted. Changes reach Discord when you apply them."
            }
            if cfg.categories.is_empty() && cfg.top_level.is_empty() {
                {empty("No channels yet. Add a category, then channels in it, below.")}
            }
            for (i, cf) in cfg.categories.iter().enumerate() {
                section { class: "card", id: "cat-{cf.category.key}",
                    {category_head(s, cfg, &cf.category.key, &cf.category.name, &cf.category.overwrites, cf.channels.is_empty(), i, n, sh)}
                    if cf.channels.is_empty() {
                        {empty("No channels in this category yet.")}
                    }
                    {channel_list(s, cfg, &cf.channels, &cf.category.overwrites, sh)}
                }
            }
            if !cfg.top_level.is_empty() {
                section { class: "card",
                    h2 { "Not in a category" }
                    {channel_list(s, cfg, &cfg.top_level, &[], sh)}
                }
            }
            {add_channel(s, cfg, sh)}
            {add_category(s, sh)}
        },
    )
}

#[allow(clippy::too_many_arguments)]
fn category_head(
    s: &Session,
    cfg: &Schema,
    key: &str,
    name: &str,
    ows: &[Overwrite],
    is_empty: bool,
    i: usize,
    n: usize,
    sh: &Shown,
) -> Element {
    let form = format!("cat-{key}");
    let who = who_can_see(cfg, ows);
    rsx! {
        div { class: "item",
            h2 {
                "{name}"
                if let Some(w) = who {
                    span { class: "tag", "{w}" }
                }
            }
            div { class: "actions",
                if i > 0 {
                    {form::mover(s, "/channels/move-category", &[("key", key)], true, name)}
                }
                if i + 1 < n {
                    {form::mover(s, "/channels/move-category", &[("key", key)], false, name)}
                }
                a { class: "button quiet small", href: "/channels/permissions/category/{key}", "Permissions" }
                if is_empty {
                    {confirm(s, "/channels/remove-category", &[("key", key)], "Remove", &format!("Yes, remove {name}"), "Only empty categories can be removed; apply archives it in Discord.")}
                }
            }
        }
        details { class: "edit", open: sh.open(&form),
            summary { "Rename" }
            form { method: "post", action: "/channels/rename-category",
                {csrf(s)}
                {form::hidden(&[("key", key)])}
                {sh.problems(&form)}
                {text(sh, &form, "name", "Name", name, "")}
                button { "Rename" }
            }
        }
    }
}

fn channel_list(s: &Session, cfg: &Schema, chans: &[Channel], cat_ows: &[Overwrite], sh: &Shown) -> Element {
    let inherited = who_can_see(cfg, cat_ows);
    let n = chans.len();
    rsx! {
        ul { class: "plain",
            for (i, ch) in chans.iter().enumerate() {
                li { id: "chan-{ch.key}", {channel_row(s, cfg, ch, i, n, inherited.as_deref(), sh)} }
            }
        }
    }
}

fn channel_row(
    s: &Session,
    cfg: &Schema,
    ch: &Channel,
    i: usize,
    n: usize,
    inherited: Option<&str>,
    sh: &Shown,
) -> Element {
    let form = format!("chan-{}", ch.key);
    let key = ch.key.as_str();
    let who = who_can_see(cfg, &ch.overwrites).filter(|w| Some(w.as_str()) != inherited);
    let slowmode = ch.slowmode.to_string();
    let limit = ch.user_limit.map(|n| n.to_string()).unwrap_or_default();
    let tags = ch.tags.join(", ");
    let topic = ch.topic.clone().unwrap_or_default();
    let here = places(cfg)
        .into_iter()
        .find(|(k, _)| {
            cfg.categories.iter().any(|c| &c.category.key == k && c.channels.iter().any(|x| x.key == ch.key))
        })
        .map(|(k, _)| k)
        .unwrap_or_default();
    rsx! {
        div { class: "item",
            div {
                if matches!(ch.kind, ChannelKind::Text | ChannelKind::Announcement) {
                    "#"
                }
                strong { "{ch.name}" }
                if !matches!(ch.kind, ChannelKind::Text) {
                    span { class: "tag", "{ch.kind}" }
                }
                if let Some(w) = who {
                    span { class: "tag", "{w}" }
                }
                if !ch.inherit {
                    span { class: "tag", "own permissions only" }
                }
                if ch.post.is_some() {
                    span { class: "tag", "bot post" }
                }
                if let Some(t) = &ch.topic {
                    div { class: "hint", "{t}" }
                }
            }
            div { class: "actions",
                if i > 0 {
                    {form::mover(s, "/channels/move-channel", &[("key", key)], true, &format!("#{}", ch.name))}
                }
                if i + 1 < n {
                    {form::mover(s, "/channels/move-channel", &[("key", key)], false, &format!("#{}", ch.name))}
                }
                a { class: "button quiet small", href: "/channels/permissions/channel/{key}", "Permissions" }
                {confirm(s, "/channels/archive-channel", &[("key", key)], "Archive", &format!("Yes, archive {}", ch.name), "Taken out of the config; apply hides it in the archive with its history kept.")}
            }
        }
        details { class: "edit", open: sh.open(&form),
            summary { "Edit" }
            form { method: "post", action: "/channels/edit-channel",
                {csrf(s)}
                {form::hidden(&[("key", key)])}
                {sh.problems(&form)}
                {text(sh, &form, "name", "Name", &ch.name, "")}
                {text(sh, &form, "topic", "Topic", &topic, "")}
                {select(sh, &form, "type", "Type", &kinds(), ch.kind.as_str())}
                {input_of(sh, &form, "slowmode", "Slow mode (seconds between messages)", &slowmode, "0 is off.", "number")}
                {check(sh, &form, "nsfw", "Age-restricted", ch.nsfw)}
                if ch.kind.is_voice() {
                    {input_of(sh, &form, "user_limit", "User limit", &limit, "Empty for no limit.", "number")}
                }
                if ch.kind.has_tags() {
                    {text(sh, &form, "tags", "Post tags", &tags, "Separated by commas.")}
                }
                button { "Save {ch.name}" }
            }
            form { method: "post", action: "/channels/move-channel-to", class: "row",
                {csrf(s)}
                {form::hidden(&[("key", key)])}
                {select(sh, &form, "category", "Move to", &places(cfg), &here)}
                button { class: "quiet", "Move" }
            }
        }
    }
}

fn add_channel(s: &Session, cfg: &Schema, sh: &Shown) -> Element {
    let form = "chan-add";
    let first = cfg.categories.first().map(|c| c.category.key.clone()).unwrap_or_default();
    rsx! {
        h2 { "Add a channel" }
        form { method: "post", action: "/channels/add-channel", class: "card", id: "{form}",
            {csrf(s)}
            {sh.problems(form)}
            {select(sh, form, "category", "In", &places(cfg), &first)}
            {text(sh, form, "name", "Name", "", "")}
            {select(sh, form, "type", "Type", &kinds(), "text")}
            {text(sh, form, "topic", "Topic (optional)", "", "")}
            {text(sh, form, "key", "Key (optional)", "", "A permanent id for the config; made from the name if left empty.")}
            p { class: "hint", "It goes at the end of the category and takes its permissions." }
            button { "Add channel" }
        }
    }
}

fn add_category(s: &Session, sh: &Shown) -> Element {
    let form = "cat-add";
    rsx! {
        h2 { "Add a category" }
        form { method: "post", action: "/channels/add-category", class: "card", id: "{form}",
            {csrf(s)}
            {sh.problems(form)}
            {text(sh, form, "name", "Name", "", "")}
            {text(sh, form, "key", "Key (optional)", "", "Made from the name if left empty.")}
            button { "Add category" }
        }
    }
}

/// The overwrites editor page for a category (`kind` "category") or channel.
fn perms_page(s: &Session, cfg: &Schema, kind: &str, key: &str, sh: &Shown) -> Option<Html<String>> {
    let (name, ows, inherit, category) = match kind {
        "category" => {
            let c = cfg.categories.iter().find(|c| c.category.key == key)?;
            (c.category.name.clone(), c.category.overwrites.clone(), None, None)
        }
        "channel" => {
            let (cat, c) = all_channels(cfg).find(|(_, c)| c.key == key)?;
            let cat_name =
                cat.and_then(|k| cfg.categories.iter().find(|c| c.category.key == k)).map(|c| c.category.name.clone());
            (c.name.clone(), c.overwrites.clone(), Some(c.inherit), cat_name)
        }
        _ => return None,
    };
    let action = if kind == "category" { "/channels/category-perms" } else { "/channels/channel-perms" };
    let form = if kind == "category" { format!("catperm-{key}") } else { format!("chanperm-{key}") };
    // What was sent, if it came back; else what the config says, plus one blank entry.
    let mut entries: Vec<Overwrite> = if sh.open(&form) { overwrites(&sh.fields) } else { ows };
    entries.push(Overwrite::default());
    let count = entries.len().to_string();
    let roles: Vec<(String, String)> = std::iter::once((String::new(), "Pick a role".to_owned()))
        .chain(cfg.roles.iter().map(|r| (r.key.clone(), r.name.clone())))
        .collect();
    let perms = view::permissions_in_order();
    Some(view::page(
        Tab::Channels,
        s,
        rsx! {
            p { a { href: "{HERE}", "← Channels" } }
            h1 { "Permissions: {name}" }
            if kind == "category" {
                p { class: "hint", "Channels in this category take these, unless a channel says otherwise for the same role." }
            } else if let Some(c) = &category {
                p { class: "hint", "On top of {c}'s permissions; an entry here for the same role replaces the category's." }
            }
            form { method: "post", action: "{action}",
                {csrf(s)}
                {form::hidden(&[("key", key), ("count", &count)])}
                {sh.problems(&form)}
                if let Some(on) = inherit {
                    div { class: "card",
                        {check(sh, &form, "inherit", "Also use the category's permissions", on)}
                    }
                }
                for (i, o) in entries.iter().enumerate() {
                    fieldset { class: "card overwrite",
                        legend {
                            if o.role.is_none() && o.member.is_none() { "Add an entry" } else { "Entry {i + 1}" }
                        }
                        if let Some(m) = o.member {
                            p { "For member {m} (kept as it is in the config)." }
                            {form::hidden(&[(&format!("ow{i}-member"), &m.to_string())])}
                        } else {
                            {select(&Shown::none(), &form, &format!("ow{i}-role"), "Role", &roles, o.role.as_deref().unwrap_or(""))}
                        }
                        div { class: "perm-grid",
                            for p in &perms {
                                {tri(i, p, o)}
                            }
                        }
                        if o.role.is_some() || o.member.is_some() {
                            label { class: "check",
                                input { r#type: "checkbox", name: "ow{i}-drop", value: "yes" }
                                "Remove this entry"
                            }
                        }
                    }
                }
                button { "Save permissions" }
            }
        },
    ))
}

/// Allow / deny / leave as is, for one permission in one entry.
fn tri(i: usize, p: &str, o: &Overwrite) -> Element {
    let now = if o.allow.iter().any(|a| a == p) {
        "allow"
    } else if o.deny.iter().any(|d| d == p) {
        "deny"
    } else {
        ""
    };
    let id = format!("ow{i}-{p}");
    rsx! {
        div { class: "tri",
            label { r#for: "{id}", "{permission(p)}" }
            select { id: "{id}", name: "{id}",
                option { value: "", selected: now.is_empty(), "Not set" }
                option { value: "allow", selected: now == "allow", "Allow" }
                option { value: "deny", selected: now == "deny", "Deny" }
            }
        }
    }
}

/// "only Admin, Committee" for channels @everyone can't see.
fn who_can_see(cfg: &Schema, ows: &[Overwrite]) -> Option<String> {
    let says_view = |list: &[String]| list.iter().any(|p| p == "VIEW_CHANNEL");
    let everyone = ows.iter().find(|o| o.role.as_deref() == Some("everyone"))?;
    if !says_view(&everyone.deny) {
        return None;
    }
    let names: Vec<&str> = ows
        .iter()
        .filter(|o| says_view(&o.allow))
        .filter_map(|o| o.role.as_deref())
        .map(|k| cfg.roles.iter().find(|r| r.key == k).map_or(k, |r| r.name.as_str()))
        .collect();
    Some(if names.is_empty() { "hidden".into() } else { format!("only {}", names.join(", ")) })
}
