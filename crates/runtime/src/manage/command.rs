//! The `/server` command as Discord registers it: its groups and options,
//! with the lists to pick from taken from the config.

use super::*;

/// `(label, value)` choices, if there are few enough for Discord to list.
fn choices(all: Vec<(String, String)>) -> Option<Vec<(String, String)>> {
    (!all.is_empty() && all.len() <= MAX_CHOICES).then_some(all)
}

/// A required text option: a list to pick from when it's short, else typed.
fn pick(name: &str, description: &str, all: Vec<(String, String)>) -> CommandOption {
    match choices(all) {
        Some(c) => choice(name, description, c),
        None => required(option(CommandOptionType::String, name, description)),
    }
}

fn required(o: CommandOption) -> CommandOption {
    CommandOption { required: Some(true), ..o }
}

fn id_option(description: &str) -> CommandOption {
    CommandOption {
        min_value: Some(Kind::Integer(1)),
        ..required(option(CommandOptionType::Integer, "id", description))
    }
}

fn group(name: &str, description: &str, subs: Vec<CommandOption>) -> CommandOption {
    CommandOption { options: Some(subs), ..option(CommandOptionType::SubCommandGroup, name, description) }
}

/// `/server` with its groups, the lists to pick from taken from `cfg`.
pub fn command(cfg: &Config) -> Command {
    let permissions = CommandOption {
        required: Some(false),
        ..option(CommandOptionType::Boolean, "permissions", "Copy permission changes too")
    };
    let projects =
        || pick("project", "Which project", cfg.projects.iter().map(|p| (p.display(), p.slug.clone())).collect());
    let positions = || {
        let all = cfg.bot.positions.iter().map(|p| {
            let name = p.label.clone().unwrap_or_else(|| chipsoc_admin::role_name(cfg, &p.role));
            (name, p.role.clone())
        });
        pick("position", "Which position", all.collect())
    };
    let roles = || {
        let all = cfg.desired.holders.keys().map(|k| (chipsoc_admin::role_name(cfg, k), k.clone()));
        pick("role", "Which position or role", all.collect())
    };
    let user = || required(option(CommandOptionType::User, "user", "Who"));
    let statuses = ProjectStatus::ALL.iter().map(|s| (s.as_str().to_owned(), s.as_str().to_owned())).collect();
    let text = |name: &str, description: &str| required(option(CommandOptionType::String, name, description));
    let optional = |name: &str, description: &str| option(CommandOptionType::String, name, description);
    let open = required(option(CommandOptionType::Boolean, "open", "Taking applications?"));
    let mut subs = vec![
        subcommand("help", "What admins can do here, and where (start here)", vec![]),
        subcommand("status", "The server at a glance: config problems, the review queue", vec![]),
        subcommand("drift", "Does Discord match the config? What changed, and who", vec![]),
        subcommand("plan", "What applying the config would change, with an Apply button", vec![]),
        subcommand("apply", "Apply the config (asks to confirm first)", vec![]),
        subcommand("pull", "Copy changes made in Discord into the config", vec![permissions]),
        subcommand("web", "DM me a link to the web dashboard (over Tailscale)", vec![]),
        subcommand(
            "history",
            "The latest config changes, or what one admin did this week",
            vec![option(CommandOptionType::User, "admin", "Whose changes")],
        ),
        subcommand(
            "revert",
            "Undo a config change, then apply it (the server owner only)",
            vec![
                text("change", "Its id, from /server history"),
                CommandOption {
                    required: Some(false),
                    ..option(
                        CommandOptionType::Boolean,
                        "rewind",
                        "Put the whole config back as it was right after that change",
                    )
                },
            ],
        ),
        group(
            "queue",
            "Review queue: applications and suggestions waiting for Accept or Reject",
            vec![
                subcommand("list", "What's waiting", vec![]),
                subcommand("accept", "Accept one", vec![id_option("Its number, from the list")]),
                subcommand(
                    "reject",
                    "Reject one",
                    vec![id_option("Its number, from the list"), optional("reason", "Reason (optional, sent to them)")],
                ),
                subcommand("history", "The latest decisions", vec![]),
            ],
        ),
        group(
            "member",
            "Positions and who holds them",
            vec![
                subcommand("list", "Who holds what, and who left", vec![]),
                subcommand("info", "What someone told the bot: their profile and join answers", vec![user()]),
                subcommand("appoint", "Appoint someone to a position or role", vec![roles(), user()]),
                subcommand("remove", "Take someone out of a position or role (asks first)", vec![roles(), user()]),
            ],
        ),
    ];
    if !cfg.projects.is_empty() {
        subs.push(group(
            "project",
            "Projects",
            vec![
                subcommand(
                    "status",
                    "Proposed, active or past (asks first)",
                    vec![projects(), choice("status", "The new status", statuses)],
                ),
                subcommand("applications", "Open or close applications", vec![projects(), open]),
                subcommand(
                    "rename",
                    "Give it a new slug, keeping its roles and channels",
                    vec![projects(), text("to", "The new slug: lower-case letters, digits and dashes")],
                ),
                subcommand("lead", "Make someone a lead (gives the Lead role; they're told)", vec![projects(), user()]),
            ],
        ));
    }
    if !cfg.bot.positions.is_empty() {
        subs.push(group(
            "position",
            "Committee positions people apply for",
            vec![
                subcommand("open", "Take applications for a position", vec![positions()]),
                subcommand("close", "Stop taking applications for a position", vec![positions()]),
            ],
        ));
    }
    if cfg.bot.feeds.is_some() {
        subs.push(group(
            "feed",
            "Feeds the bot posts from",
            vec![
                subcommand("list", "Feeds followed", vec![]),
                subcommand(
                    "add",
                    "Follow a blog, newsletter or channel",
                    vec![text("url", "Its feed, or the website's address (its feed is found)")],
                ),
                subcommand("pause", "Stop posting from a feed for now", vec![id_option("Its number, from the list")]),
                subcommand("resume", "Post from a paused feed again", vec![id_option("Its number, from the list")]),
                subcommand(
                    "remove",
                    "Stop following a feed (asks first)",
                    vec![id_option("Its number, from the list")],
                ),
            ],
        ));
    }
    if cfg.join.is_some() {
        subs.push(group(
            "join",
            "The join flow",
            vec![subcommand("notify", "DM members who haven't been through it (shows how many first)", vec![])],
        ));
    }
    if cfg.bot.resources.is_some() {
        subs.push(group(
            "resource",
            "Links shared in the resources forum",
            vec![
                subcommand("list", "The latest resources", vec![]),
                subcommand(
                    "add",
                    "Share a link",
                    vec![
                        text("title", "What it is"),
                        text("url", "Its address"),
                        optional("description", "A line or two about it"),
                        optional("tags", "Forum tags, separated by commas"),
                    ],
                ),
                subcommand(
                    "archive",
                    "Close and lock a resource post (asks first)",
                    vec![text("post", "Its link or id")],
                ),
            ],
        ));
    }
    if shares_on_demand(cfg) {
        subs.push(group(
            "share",
            "Share with partner communities",
            vec![
                subcommand(
                    "announcement",
                    "Share an announcement with partners you pick",
                    vec![text("message", "Its link (⋯ → Copy Message Link)")],
                ),
                subcommand(
                    "event",
                    "Share one of our events with partners you pick",
                    vec![text("event", "Its link or id")],
                ),
            ],
        ));
    }
    commands::command(
        "server",
        "Run the server: config, applications, feeds, roles (admins)",
        subs,
        // Shown to everyone: the committee may not have Manage Server. Who may use it is checked when it runs.
        None,
    )
}
