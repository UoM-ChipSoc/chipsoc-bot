//! Feeds, resources and ideas.

use super::*;

#[derive(Subcommand)]
pub enum FeedCmd {
    /// Feeds followed.
    List,
    /// Follow a feed, or a website's feed (found from the page); new posts are shared.
    Add {
        /// Its address (https).
        url: String,
    },
    /// Stop checking it for a while.
    Pause {
        /// Its number in `feed list`.
        id: i64,
    },
    /// Check a paused feed again.
    Resume {
        /// Its number in `feed list`.
        id: i64,
    },
    /// Stop following it.
    Remove {
        /// Its number in `feed list`.
        id: i64,
    },
}

pub async fn feed<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: FeedCmd) -> Result<()> {
    match cmd {
        FeedCmd::List => {
            let f = c.b.feeds().await?;
            if c.json {
                return print_json(&f);
            }
            if f.is_empty() {
                println!("No feeds yet. Follow one with `{BIN} feed add <url>`.");
            }
            for x in &f {
                let paused = if x.paused { " · paused" } else { "" };
                println!("#{:<4} {} → #{} · since {}{paused}", x.id, x.url, x.channel, when(x.added_at));
            }
            Ok(())
        }
        FeedCmd::Add { url } => c.done(&c.b.add_feed(&url).await?),
        FeedCmd::Pause { id } => {
            c.b.pause_feed(id, true).await?;
            c.done(&format!("Paused feed #{id}."))
        }
        FeedCmd::Resume { id } => {
            c.b.pause_feed(id, false).await?;
            c.done(&format!("Resumed feed #{id}."))
        }
        FeedCmd::Remove { id } => {
            if !confirm(&format!("Stop following feed #{id}?"), c.yes)? {
                return Ok(());
            }
            c.b.remove_feed(id).await?;
            c.done(&format!("Stopped following feed #{id}."))
        }
    }
}

#[derive(Subcommand)]
pub enum ResourceCmd {
    /// The newest posts in the resources forum.
    List {
        /// How many.
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Share a link (asks when flags are missing).
    Add {
        /// The post's title.
        #[arg(long)]
        title: Option<String>,
        /// The link (https).
        #[arg(long)]
        url: Option<String>,
        /// What it is, in a sentence or two.
        #[arg(long)]
        description: Option<String>,
        /// Forum tag names, comma-separated.
        #[arg(long)]
        tags: Option<String>,
    },
    /// Close and lock a post (it isn't deleted).
    Archive {
        /// The post's id or link.
        id: String,
    },
}

pub(crate) fn print_posts(posts: &[chipsoc_admin::Post], empty: &str) {
    if posts.is_empty() {
        println!("{empty}");
    }
    for p in posts {
        let tags = if p.tags.is_empty() { String::new() } else { format!(" [{}]", p.tags.join(", ")) };
        let closed = if p.archived { " · closed" } else { "" };
        println!("{} {}{tags}{closed}\n    {}", p.id, p.title, p.link);
    }
}

pub async fn resource<R: Rest + Clone>(c: &Ctx<'_, R>, cmd: ResourceCmd) -> Result<()> {
    match cmd {
        ResourceCmd::List { limit } => {
            let posts = c.b.resources(limit).await?;
            if c.json {
                return print_json(&posts);
            }
            print_posts(&posts, "No resources shared yet.");
            Ok(())
        }
        ResourceCmd::Add { title, url, description, tags } => {
            let asking = tty() && (title.is_none() || url.is_none());
            let r = NewResource {
                title: ask(title, "Title", "title")?,
                url: ask(url, "Address (https://…)", "url")?,
                description: match description {
                    Some(d) => d,
                    None if asking => Text::new("A line about it (optional)").prompt()?,
                    None => String::new(),
                },
                tags: edits::list(&match tags {
                    Some(t) => t,
                    None if asking => Text::new("Tags, comma-separated (optional)").prompt()?,
                    None => String::new(),
                }),
            };
            let post = c.b.add_resource(&r).await?;
            c.say(&format!("✓ Shared {} ({})", post.title, post.link), &post)
        }
        ResourceCmd::Archive { id } => {
            let id: Snowflake = id.trim().parse().with_context(|| format!("`{id}` isn't a post id"))?;
            if !confirm(&format!("Close and lock resource post {id}? It isn't deleted."), c.yes)? {
                return Ok(());
            }
            c.b.archive_resource(id).await?;
            c.done("Closed and locked.")
        }
    }
}

/// `share`: an announcement or event, to partners.
pub async fn share<R: Rest + Clone>(c: &Ctx<'_, R>, what: String, item: String, to: Option<String>) -> Result<()> {
    let partners = edits::list(&to.unwrap_or_default());
    let r = chipsoc_client::remote::ShareRequest { what, item, partners };
    let names = c.b.share(&r).await?;
    let text = format!("✓ Shared with {}. The bot sends it within a minute.", names.join(", "));
    c.say(&text, &serde_json::json!({ "partners": names }))
}

pub async fn ideas<R: Rest + Clone>(c: &Ctx<'_, R>, limit: usize) -> Result<()> {
    let posts = c.b.ideas(limit).await?;
    if c.json {
        return print_json(&posts);
    }
    print_posts(&posts, "No project ideas yet.");
    Ok(())
}
