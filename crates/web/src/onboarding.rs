//! What newcomers see: Discord's onboarding questions (each option can give
//! a role and show channels), and the posts the bot keeps in channels, with
//! a preview of how Discord shows them.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::response::{Html, IntoResponse, Response};
use axum::{Extension, Form};
use chipsoc_config::ops::{Move, OnboardingField, Op, OptionField, PromptField, Schema};
use chipsoc_config::schema::{OnbOption, OnbPrompt};
use chipsoc_config::{Lock, template};
use chipsoc_discord::Rest;
use dioxus::prelude::*;

use crate::App;
use crate::auth::Session;
use crate::form::{self, Fields, Shown, area, check, confirm, edit, need_confirm, select, shown, text, wrong};
use crate::view::{self, Tab, csrf, empty};

const HERE: &str = "/onboarding";

pub(crate) async fn page<R: Rest + 'static>(
    State(app): State<Arc<App<R>>>,
    Extension(s): Extension<Session>,
) -> Response {
    view(&app, &s, &Shown::none()).await
}

async fn view<R: Rest>(app: &App<R>, s: &Session, sh: &Shown) -> Response {
    let loaded = async { anyhow::Ok((app.schema().await?, app.config().await?.desired.lock.clone())) };
    match loaded.await {
        Ok((cfg, lock)) => shown(sh, render(s, &cfg, &lock, sh)),
        Err(e) => view::broken(Tab::Onboarding, s, &e).into_response(),
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
        Err(e) => return view::broken(Tab::Onboarding, &s, &e).into_response(),
    };
    if action == "post" && f.has("preview") {
        return view(&app, &s, &Shown::again(&post_form_id(&cfg, f.get("channel")), f)).await;
    }
    let built = build(&action, &f, &cfg);
    match edit(&app, &s, HERE, f, built).await {
        Ok(done) => done,
        Err(sh) => view(&app, &s, &sh).await,
    }
}

fn index(f: &Fields, name: &str) -> usize {
    f.get(name).parse().unwrap_or(usize::MAX)
}

fn build(action: &str, f: &Fields, cfg: &Schema) -> Result<(String, Vec<Op>), Shown> {
    let prompt = index(f, "prompt");
    let option = index(f, "option");
    let prompts = cfg.onboarding.as_ref().map(|o| o.prompts.as_slice()).unwrap_or_default();
    let to = if f.get("to") == "up" { Move::Up } else { Move::Down };
    match action {
        "settings" => {
            let form = "onb";
            let o = cfg.onboarding.clone().unwrap_or_default();
            let mut ops = Vec::new();
            if f.has("enabled") != o.enabled {
                ops.push(Op::SetOnboarding { set: OnboardingField::Enabled(f.has("enabled")) });
            }
            if f.has("advanced") != o.advanced {
                ops.push(Op::SetOnboarding { set: OnboardingField::Advanced(f.has("advanced")) });
            }
            let defaults = ticked(f, "default_channels");
            if defaults != o.default_channels {
                ops.push(Op::SetOnboarding { set: OnboardingField::DefaultChannels(defaults) });
            }
            Ok((form.into(), ops))
        }
        "add-prompt" => {
            let form = "prompt-add";
            if f.get("title").is_empty() {
                return Err(wrong(form, f, "title", "Write the question."));
            }
            // Asked with its first answer: a question needs one, and the
            // file then holds its answers as a list the others join.
            if f.get("answer").is_empty() {
                return Err(wrong(form, f, "answer", "Give it a first answer; add more after."));
            }
            let first =
                OnbOption { title: f.get("answer").to_owned(), role: f.get("role").to_owned(), ..Default::default() };
            let prompt = OnbPrompt {
                title: f.get("title").to_owned(),
                single_select: f.has("single_select"),
                required: f.has("required"),
                options: vec![first],
            };
            Ok((form.into(), vec![Op::AddPrompt { prompt }]))
        }
        "edit-prompt" => {
            let form = format!("prompt-{prompt}");
            let Some(p) = prompts.get(prompt) else { return Err(form::refuse(&form, f, "No such question.")) };
            if f.get("title").is_empty() {
                return Err(wrong(&form, f, "title", "Write the question."));
            }
            let set = |set| Op::SetPrompt { prompt, set };
            let mut ops = Vec::new();
            if f.get("title") != p.title {
                ops.push(set(PromptField::Title(f.get("title").to_owned())));
            }
            if f.has("single_select") != p.single_select {
                ops.push(set(PromptField::SingleSelect(f.has("single_select"))));
            }
            if f.has("required") != p.required {
                ops.push(set(PromptField::Required(f.has("required"))));
            }
            Ok((form, ops))
        }
        "move-prompt" => Ok((format!("prompt-{prompt}"), vec![Op::MovePrompt { prompt, to }])),
        "remove-prompt" => {
            let form = format!("prompt-{prompt}");
            need_confirm(&form, f)?;
            Ok((form, vec![Op::RemovePrompt { prompt }]))
        }
        "add-option" => {
            let form = format!("opt-add-{prompt}");
            if f.get("title").is_empty() {
                return Err(wrong(&form, f, "title", "Give the answer a title."));
            }
            let option = OnbOption {
                title: f.get("title").to_owned(),
                description: f.opt("description"),
                emoji: f.opt("emoji"),
                role: f.get("role").to_owned(),
                channels: ticked(f, "channels"),
            };
            Ok((form, vec![Op::AddOption { prompt, option }]))
        }
        "edit-option" => {
            let form = format!("opt-{prompt}-{option}");
            let Some(o) = prompts.get(prompt).and_then(|p| p.options.get(option)) else {
                return Err(form::refuse(&form, f, "No such answer."));
            };
            if f.get("title").is_empty() {
                return Err(wrong(&form, f, "title", "Give the answer a title."));
            }
            let set = |set| Op::SetOption { prompt, option, set };
            let mut ops = Vec::new();
            if f.get("title") != o.title {
                ops.push(set(OptionField::Title(f.get("title").to_owned())));
            }
            if f.opt("description") != o.description {
                ops.push(set(OptionField::Description(f.opt("description"))));
            }
            if f.opt("emoji") != o.emoji {
                ops.push(set(OptionField::Emoji(f.opt("emoji"))));
            }
            if f.get("role") != o.role {
                ops.push(set(OptionField::Role(f.get("role").to_owned())));
            }
            if ticked(f, "channels") != o.channels {
                ops.push(set(OptionField::Channels(ticked(f, "channels"))));
            }
            Ok((form, ops))
        }
        "move-option" => Ok((format!("opt-{prompt}-{option}"), vec![Op::MoveOption { prompt, option, to }])),
        "remove-option" => {
            let form = format!("opt-{prompt}-{option}");
            need_confirm(&form, f)?;
            Ok((form, vec![Op::RemoveOption { prompt, option }]))
        }
        "post" => {
            let channel = f.get("channel").to_owned();
            let form = post_form_id(cfg, &channel);
            if channel.is_empty() {
                return Err(wrong("post-new", f, "channel", "Pick a channel."));
            }
            Ok((form, vec![Op::SetPost { channel, body: f.raw("body").replace("\r\n", "\n") }]))
        }
        _ => Err(form::refuse("", f, "Unknown action.")),
    }
}

/// The form a post is edited in: its own, or the one for starting a post.
fn post_form_id(cfg: &Schema, channel: &str) -> String {
    if cfg.posts.contains_key(channel) { format!("post-{channel}") } else { "post-new".into() }
}

fn roles(cfg: &Schema) -> Vec<(String, String)> {
    std::iter::once((String::new(), "No role".to_owned()))
        .chain(cfg.roles.iter().filter(|r| !r.managed && r.key != "everyone").map(|r| (r.key.clone(), r.name.clone())))
        .collect()
}

/// The channels ticked in a list of boxes named `name` (keys).
fn ticked(f: &Fields, name: &str) -> Vec<String> {
    f.all(name).into_iter().flat_map(form::split_list).collect()
}

/// A box per channel, by name, ticked for those in `chosen`.
fn channel_boxes(
    sh: &Shown,
    cfg: &Schema,
    form: &str,
    name: &str,
    legend: &str,
    chosen: &[String],
    hint: &str,
) -> Element {
    let ticked: Vec<String> =
        if sh.open(form) { sh.fields.all(name).into_iter().map(str::to_owned).collect() } else { chosen.to_vec() };
    let channels = cfg.categories.iter().flat_map(|c| c.channels.iter()).chain(cfg.top_level.iter());
    let channels: Vec<(String, String)> = channels.map(|c| (c.key.clone(), c.name.clone())).collect();
    rsx! {
        fieldset { class: "perms",
            legend { "{legend}" }
            if !hint.is_empty() {
                p { class: "hint", "{hint}" }
            }
            for (key, label) in channels {
                label { class: "check",
                    input { r#type: "checkbox", name: "{name}", value: "{key}", checked: ticked.contains(&key) }
                    "#{label}"
                }
            }
        }
    }
}

fn render(s: &Session, cfg: &Schema, lock: &Lock, sh: &Shown) -> Html<String> {
    let o = cfg.onboarding.clone().unwrap_or_default();
    let n = o.prompts.len();
    let syntax = format!(
        "Markdown. `{{#key}}` and `{{@key}}` become mentions of that channel or role; a line \
         `{{button:action|Label}}` adds a button (actions: {}).",
        cfg.choices.button_actions.join(", ")
    );
    let without: Vec<(String, String)> = std::iter::once((String::new(), "Pick a channel".to_owned()))
        .chain(
            cfg.categories
                .iter()
                .flat_map(|c| c.channels.iter())
                .chain(cfg.top_level.iter())
                .filter(|c| !cfg.posts.contains_key(&c.key))
                .map(|c| (c.key.clone(), format!("#{}", c.name))),
        )
        .collect();
    view::page(
        Tab::Onboarding,
        s,
        rsx! {
            h1 { "Onboarding" }
            p { class: "hint", "What newcomers are asked when they join, and the posts the bot keeps in channels. Changes reach Discord when you apply them." }
            h2 { "Settings" }
            form { method: "post", action: "/onboarding/settings", class: "card", id: "onb",
                {csrf(s)}
                {sh.problems("onb")}
                {check(sh, "onb", "enabled", "Ask newcomers these questions when they join", o.enabled)}
                {check(sh, "onb", "advanced", "Count channels that answers unlock towards Discord's 7-channel minimum (Discord's “Advanced mode”)", o.advanced)}
                {channel_boxes(sh, cfg, "onb", "default_channels", "Channels everyone sees", &o.default_channels, "None ticked: every channel @everyone can see.")}
                button { "Save settings" }
            }
            h2 { "Questions" }
            if o.prompts.is_empty() {
                {empty("No questions yet. Add one below, then give it answers that hand out roles.")}
            }
            for (i, p) in o.prompts.iter().enumerate() {
                {prompt_card(s, cfg, i, n, p, sh)}
            }
            {add_prompt(s, cfg, sh)}
            h2 { "Bot posts" }
            p { class: "hint", "Posts the bot keeps in a channel and edits in place. Use Preview to see them as Discord will." }
            details { class: "edit",
                summary { "Formatting help" }
                p { class: "hint", {view::plain(&syntax)} }
            }
            if cfg.posts.is_empty() {
                {empty("No bot posts yet. Start one in a channel below.")}
            }
            for (key, post) in &cfg.posts {
                {post_form(s, lock, key, &post.file, &post.body, sh)}
            }
            h3 { "Start a post in another channel" }
            form { method: "post", action: "/onboarding/post", class: "card", id: "post-new",
                {csrf(s)}
                {sh.problems("post-new")}
                {select(sh, "post-new", "channel", "Channel", &without, "")}
                {area(sh, "post-new", "body", "Text", "", 6, "")}
                div { class: "row",
                    button { name: "preview", value: "yes", class: "quiet", "Preview" }
                    button { "Save post" }
                }
            }
            if sh.open("post-new") {
                {preview(lock, sh.fields.raw("body"), true)}
            }
        },
    )
}

fn prompt_card(s: &Session, cfg: &Schema, i: usize, n: usize, p: &OnbPrompt, sh: &Shown) -> Element {
    let form = format!("prompt-{i}");
    let idx = i.to_string();
    let m = p.options.len();
    rsx! {
        section { class: "card", id: "{form}",
            div { class: "item",
                h3 {
                    "{i + 1}. {p.title}"
                    if p.required {
                        span { class: "tag", "required" }
                    }
                    span { class: "tag", if p.single_select { "pick one" } else { "pick any" } }
                }
                div { class: "actions",
                    if i > 0 {
                        {form::mover(s, "/onboarding/move-prompt", &[("prompt", &idx)], true, &p.title)}
                    }
                    if i + 1 < n {
                        {form::mover(s, "/onboarding/move-prompt", &[("prompt", &idx)], false, &p.title)}
                    }
                    {confirm(s, "/onboarding/remove-prompt", &[("prompt", &idx)], "Remove", "Yes, remove this question", "Its answers go too.")}
                }
            }
            details { class: "edit", open: sh.open(&form),
                summary { "Edit the question" }
                form { method: "post", action: "/onboarding/edit-prompt",
                    {csrf(s)}
                    {form::hidden(&[("prompt", &idx)])}
                    {sh.problems(&form)}
                    {text(sh, &form, "title", "Question", &p.title, "")}
                    {check(sh, &form, "single_select", "Pick one answer only", p.single_select)}
                    {check(sh, &form, "required", "Must be answered", p.required)}
                    button { "Save question" }
                }
            }
            if p.options.is_empty() {
                {empty("No answers yet.")}
            }
            ul { class: "plain",
                for (j, o) in p.options.iter().enumerate() {
                    li { {option_row(s, cfg, i, j, m, o, sh)} }
                }
            }
            {add_option(s, cfg, i, sh)}
        }
    }
}

fn option_row(s: &Session, cfg: &Schema, i: usize, j: usize, m: usize, o: &OnbOption, sh: &Shown) -> Element {
    let form = format!("opt-{i}-{j}");
    let (pi, oi) = (i.to_string(), j.to_string());
    let role = cfg.roles.iter().find(|r| r.key == o.role).map(|r| r.name.clone());
    let desc = o.description.clone().unwrap_or_default();
    let emoji = o.emoji.clone().unwrap_or_default();
    let chans = o.channels.join(", ");
    rsx! {
        div { class: "item",
            div {
                if !emoji.is_empty() {
                    "{emoji} "
                }
                strong { "{o.title}" }
                if let Some(r) = role {
                    span { class: "tag", "gives {r}" }
                }
                if !o.channels.is_empty() {
                    span { class: "tag", "shows {chans}" }
                }
                if !desc.is_empty() {
                    div { class: "hint", "{desc}" }
                }
            }
            div { class: "actions",
                if j > 0 {
                    {form::mover(s, "/onboarding/move-option", &[("prompt", &pi), ("option", &oi)], true, &o.title)}
                }
                if j + 1 < m {
                    {form::mover(s, "/onboarding/move-option", &[("prompt", &pi), ("option", &oi)], false, &o.title)}
                }
                {confirm(s, "/onboarding/remove-option", &[("prompt", &pi), ("option", &oi)], "Remove", &format!("Yes, remove {}", o.title), "")}
            }
        }
        details { class: "edit", open: sh.open(&form),
            summary { "Edit" }
            form { method: "post", action: "/onboarding/edit-option",
                {csrf(s)}
                {form::hidden(&[("prompt", &pi), ("option", &oi)])}
                {option_fields(sh, cfg, &form, o)}
                button { "Save answer" }
            }
        }
    }
}

fn option_fields(sh: &Shown, cfg: &Schema, form: &str, o: &OnbOption) -> Element {
    let desc = o.description.clone().unwrap_or_default();
    let emoji = o.emoji.clone().unwrap_or_default();
    rsx! {
        {sh.problems(form)}
        {text(sh, form, "title", "Answer", &o.title, "")}
        {text(sh, form, "description", "Description (optional)", &desc, "")}
        {text(sh, form, "emoji", "Emoji (optional)", &emoji, "")}
        {select(sh, form, "role", "Gives the role", &roles(cfg), &o.role)}
        {channel_boxes(sh, cfg, form, "channels", "Shows channels (optional)", &o.channels, "")}
    }
}

fn add_option(s: &Session, cfg: &Schema, i: usize, sh: &Shown) -> Element {
    let form = format!("opt-add-{i}");
    rsx! {
        details { class: "edit", open: sh.open(&form),
            summary { "Add an answer" }
            form { method: "post", action: "/onboarding/add-option", id: "{form}",
                {csrf(s)}
                {form::hidden(&[("prompt", &i.to_string())])}
                {option_fields(sh, cfg, &form, &OnbOption::default())}
                button { "Add answer" }
            }
        }
    }
}

fn add_prompt(s: &Session, cfg: &Schema, sh: &Shown) -> Element {
    let form = "prompt-add";
    rsx! {
        details { class: "edit card", open: sh.open(form),
            summary { "Add a question" }
            form { method: "post", action: "/onboarding/add-prompt", id: "{form}",
                {csrf(s)}
                {sh.problems(form)}
                {text(sh, form, "title", "Question", "", "")}
                {text(sh, form, "answer", "First answer", "", "Add more answers after.")}
                {select(sh, form, "role", "It gives the role", &roles(cfg), "")}
                {check(sh, form, "single_select", "Pick one answer only", true)}
                {check(sh, form, "required", "Must be answered", false)}
                button { "Add question" }
            }
        }
    }
}

fn post_form(s: &Session, lock: &Lock, key: &str, file: &str, body: &str, sh: &Shown) -> Element {
    let form = format!("post-{key}");
    let shown_body = sh.val(&form, "body", body);
    let rows = shown_body.lines().count().clamp(4, 24) as u32;
    rsx! {
        section { class: "card", id: "{form}",
            h3 { "#{key}" span { class: "tag", "{file}" } }
            form { method: "post", action: "/onboarding/post",
                {csrf(s)}
                {form::hidden(&[("channel", key)])}
                {sh.problems(&form)}
                {area(sh, &form, "body", "Text", body, rows, "")}
                div { class: "row",
                    button { name: "preview", value: "yes", class: "quiet", "Preview" }
                    button { "Save post" }
                }
            }
            {preview(lock, &shown_body, sh.open(&form))}
        }
    }
}

/// How Discord shows `body`: its text and buttons.
fn preview(lock: &Lock, body: &str, open: bool) -> Element {
    let rendered = template::render(body, lock);
    rsx! {
        details { class: "preview", open,
            summary { "How Discord shows it" }
            div { class: "discord", "{rendered.content}" }
            if !rendered.buttons.is_empty() {
                div { class: "choices",
                    for b in &rendered.buttons {
                        span { class: "button small", title: "{b.action}", "{b.label}" }
                    }
                }
            }
        }
    }
}
