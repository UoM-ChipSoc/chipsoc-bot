//! Categories and channels, and the permissions both carry.

use super::*;

#[derive(Args, Default)]
pub struct PermsArgs {
    /// `role:PERM,PERM`: what that role may do (replaces its allowed list). Repeat for more roles.
    #[arg(long)]
    allow: Vec<String>,
    /// `role:PERM,PERM`: what that role may not do (replaces its denied list).
    #[arg(long)]
    deny: Vec<String>,
    /// Take a role's entry out.
    #[arg(long)]
    clear: Vec<String>,
    /// Replace them all with a JSON list like `[{"role": "member", "allow": ["SEND_MESSAGES"]}]`.
    #[arg(long, value_name = "FILE")]
    from_json: Option<PathBuf>,
}

impl PermsArgs {
    fn perms(&self) -> Perms {
        Perms { allow: self.allow.clone(), deny: self.deny.clone(), clear: self.clear.clone() }
    }

    fn is_empty(&self) -> bool {
        self.perms().is_empty() && self.from_json.is_none()
    }

    /// The new list, from `current`.
    fn apply(&self, current: &[Overwrite]) -> Result<Vec<Overwrite>> {
        match &self.from_json {
            Some(f) => {
                let text = std::fs::read_to_string(f).with_context(|| format!("reading {}", f.display()))?;
                serde_json::from_str(&text).with_context(|| format!("reading {} as a list of overwrites", f.display()))
            }
            None => edits::merge_overwrites(current, &self.perms()),
        }
    }
}

pub(crate) fn print_overwrites(title: &str, o: &[Overwrite]) {
    println!("{title}");
    if o.is_empty() {
        println!("  (none: everyone gets their roles' permissions)");
    }
    for x in o {
        println!("  {}", edits::overwrite_line(x));
    }
}

#[derive(Subcommand)]
pub enum CategoryCmd {
    /// Categories in order, with how many channels each has.
    List,
    /// Add an empty category after the others.
    New {
        /// Its name in Discord.
        name: String,
        /// Its permanent key (default: from the name).
        #[arg(long)]
        key: Option<String>,
    },
    /// Give a category a new name (its key stays).
    Rename {
        /// The category's key.
        key: String,
        /// The new name.
        name: String,
    },
    /// Move a category up or down.
    Move {
        /// The category's key.
        key: String,
        /// `up`, `down`, or a place from 1.
        to: String,
    },
    /// Take out an empty category (apply archives it).
    Remove {
        /// The category's key.
        key: String,
    },
    /// Show or change who can see and do what; its channels inherit these.
    Perms {
        /// The category's key.
        key: String,
        #[command(flatten)]
        perms: PermsArgs,
    },
}

pub async fn category<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: CategoryCmd) -> Result<()> {
    match cmd {
        CategoryCmd::List => {
            let cats = c.schema().await?.categories;
            if c.json {
                return print_json(&cats);
            }
            for cf in cats {
                println!(
                    "{:<20} {} ({})",
                    cf.category.key,
                    cf.category.name,
                    chipsoc_admin::fmt::count(cf.channels.len(), "channel", "channels")
                );
            }
            Ok(())
        }
        CategoryCmd::New { name, key } => {
            let key = key.unwrap_or_else(|| slug(&name));
            c.edits(&[Op::AddCategory { key, name }]).await
        }
        CategoryCmd::Rename { key, name } => c.edits(&[Op::RenameCategory { key, name }]).await,
        CategoryCmd::Move { key, to } => c.edits(&[Op::MoveCategory { key, to: parse_move(&to)? }]).await,
        CategoryCmd::Remove { key } => {
            if !confirm(&format!("Take category `{key}` out of the config? Apply archives it."), c.yes)? {
                return Ok(());
            }
            c.edits(&[Op::RemoveCategory { key }]).await
        }
        CategoryCmd::Perms { key, perms } => {
            let schema = c.schema().await?;
            let cf = schema.categories.iter().find(|cf| cf.category.key == key);
            let current = cf.with_context(|| format!("no category `{key}`"))?.category.overwrites.clone();
            if perms.is_empty() {
                return if c.json {
                    print_json(&current)
                } else {
                    print_overwrites(&format!("Category {key}:"), &current);
                    Ok(())
                };
            }
            let overwrites = perms.apply(&current)?;
            c.edits(&[Op::SetCategoryOverwrites { key, overwrites }]).await
        }
    }
}

#[derive(Subcommand)]
pub enum ChannelCmd {
    /// Channels by category, in order.
    List,
    /// Add a channel at the end of a category (asks when flags are missing).
    New {
        /// Category key, or `none` for outside any.
        #[arg(long)]
        category: Option<String>,
        /// text, voice, announcement, forum or stage.
        #[arg(long = "type", value_parser = parse_kind)]
        kind: Option<ChannelKind>,
        /// Its name in Discord.
        #[arg(long)]
        name: Option<String>,
        /// Its permanent key (default: from the name).
        #[arg(long)]
        key: Option<String>,
        /// The line under its name.
        #[arg(long)]
        topic: Option<String>,
        /// Forum tags, comma-separated.
        #[arg(long)]
        tags: Option<String>,
    },
    /// Change a channel.
    Set {
        /// The channel's key.
        key: String,
        #[command(flatten)]
        set: ChannelSetArgs,
    },
    /// Move a channel within its category.
    Move {
        /// The channel's key.
        key: String,
        /// `up`, `down`, or a place from 1.
        to: String,
    },
    /// Move a channel to the end of another category.
    MoveTo {
        /// The channel's key.
        key: String,
        /// The category's key, or `none` for outside any.
        category: String,
    },
    /// Take a channel out of the config; apply moves it to the archive (history kept).
    Archive {
        /// The channel's key.
        key: String,
    },
    /// Show or change who can see and do what here (added to the category's).
    Perms {
        /// The channel's key.
        key: String,
        #[command(flatten)]
        perms: PermsArgs,
        /// on/off: use the category's permissions too.
        #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
        inherit: Option<bool>,
    },
}

#[derive(Args)]
pub struct ChannelSetArgs {
    /// Its name in Discord.
    #[arg(long)]
    name: Option<String>,
    /// The line under its name, or `none`.
    #[arg(long)]
    topic: Option<String>,
    /// text, voice, announcement, forum or stage.
    #[arg(long = "type", value_parser = parse_kind)]
    kind: Option<ChannelKind>,
    /// Seconds between messages; 0 is off.
    #[arg(long)]
    slowmode: Option<u16>,
    /// on/off: age-restricted.
    #[arg(long, value_parser = parse_bool, action = clap::ArgAction::Set)]
    nsfw: Option<bool>,
    /// Voice: most people at once; 0 for no limit.
    #[arg(long)]
    user_limit: Option<u16>,
    /// Forum tags, comma-separated; replaces the list.
    #[arg(long)]
    tags: Option<String>,
}

/// A channel the files define, and its category's key.
pub(crate) fn find_channel<'a>(s: &'a Schema, key: &str) -> Option<(Option<&'a str>, &'a Channel)> {
    let in_cats =
        s.categories.iter().flat_map(|cf| cf.channels.iter().map(move |ch| (Some(cf.category.key.as_str()), ch)));
    in_cats.chain(s.top_level.iter().map(|ch| (None, ch))).find(|(_, ch)| ch.key == key)
}

pub async fn channel<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: ChannelCmd) -> Result<()> {
    match cmd {
        ChannelCmd::List => {
            let s = c.schema().await?;
            if c.json {
                return print_json(&serde_json::json!({ "categories": s.categories, "top_level": s.top_level }));
            }
            let line = |indent: &str, ch: &Channel| {
                let topic = ch.topic.as_deref().map(|t| format!("  {t}")).unwrap_or_default();
                println!("{indent}{:<13} {:<24} {}{topic}", ch.kind, ch.name, ch.key);
            };
            for cf in &s.categories {
                println!("{} ({})", cf.category.name, cf.category.key);
                cf.channels.iter().for_each(|ch| line("  ", ch));
            }
            if !s.top_level.is_empty() {
                println!("Not in a category");
                s.top_level.iter().for_each(|ch| line("  ", ch));
            }
            Ok(())
        }
        ChannelCmd::New { category, kind, name, key, topic, tags } => {
            let s = c.schema().await?;
            let cats: Vec<(String, String)> =
                s.categories.iter().map(|cf| (cf.category.key.clone(), cf.category.name.clone())).collect();
            let (category, channel) =
                author::ask_channel(author::NewChannel { category, kind, name, key, topic, tags }, &cats)?;
            c.edits(&[Op::AddChannel { category, channel }]).await
        }
        ChannelCmd::Set { key, set } => {
            let s = edits::ChannelSet {
                name: set.name,
                topic: set.topic,
                kind: set.kind,
                slowmode: set.slowmode,
                nsfw: set.nsfw,
                user_limit: set.user_limit,
                tags: set.tags,
            };
            c.edits(&edits::channel_ops(&key, &s)).await
        }
        ChannelCmd::Move { key, to } => c.edits(&[Op::MoveChannel { key, to: parse_move(&to)? }]).await,
        ChannelCmd::MoveTo { key, category } => {
            c.edits(&[Op::MoveChannelTo { key, category: edits::optional(&category) }]).await
        }
        ChannelCmd::Archive { key } => {
            let q = format!("Take #{key} out of the config? Applying moves it to the archive, history kept.");
            if !confirm(&q, c.yes)? {
                return Ok(());
            }
            c.edits(&[Op::RemoveChannel { key }]).await
        }
        ChannelCmd::Perms { key, perms, inherit } => {
            let s = c.schema().await?;
            let (cat, ch) = find_channel(&s, &key).with_context(|| format!("no channel `{key}`"))?;
            if perms.is_empty() && inherit.is_none() {
                if c.json {
                    return print_json(&serde_json::json!({ "overwrites": ch.overwrites, "inherit": ch.inherit }));
                }
                if let Some(cf) = cat.and_then(|k| s.categories.iter().find(|cf| cf.category.key == k)) {
                    let used = if ch.inherit { "used" } else { "ignored" };
                    print_overwrites(&format!("From category {} ({used}):", cf.category.key), &cf.category.overwrites);
                }
                print_overwrites(&format!("#{key}'s own:"), &ch.overwrites);
                return Ok(());
            }
            let mut ops = Vec::new();
            if !perms.is_empty() {
                let overwrites = perms.apply(&ch.overwrites)?;
                ops.push(Op::SetChannel {
                    key: key.clone(),
                    set: chipsoc_config::ops::ChannelField::Overwrites(overwrites),
                });
            }
            if let Some(i) = inherit {
                ops.push(Op::SetChannel { key, set: chipsoc_config::ops::ChannelField::Inherit(i) });
            }
            c.edits(&ops).await
        }
    }
}
