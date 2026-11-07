//! `/share-resource`: a form for a useful link that goes to the review queue;
//! once an admin accepts it, it becomes a post in the resources forum,
//! crediting whoever shared it.

use chipsoc_admin::NewResource;
use chipsoc_admin::forums::{RESOURCE_ABOUT, RESOURCE_LINK, RESOURCE_TAGS, RESOURCE_TITLE};
use chipsoc_store::AppKind;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::TextInputStyle;

use crate::review::{Filing, Refusal, Vetted};
use crate::ui::Values;
use crate::{Ctx, ui};

/// Tags a post can have (Discord's limit).
const MAX_TAGS: u8 = 5;

/// Sharing needs the resources forum and the review queue.
pub fn ready(ctx: &Ctx) -> Result<(), String> {
    let cfg = ctx.cfg();
    if cfg.bot.resources.is_none() || cfg.bot.review.is_none() {
        return Err("Sharing resources isn't set up here.".into());
    }
    Ok(())
}

pub fn form(ctx: &Ctx) -> (String, Vec<Component>) {
    let cfg = ctx.cfg();
    let forum = cfg.bot.resources.as_ref().and_then(|r| cfg.desired.channel(&r.forum));
    let tags = forum.map(|c| c.tags.clone()).unwrap_or_default();
    let mut fields = vec![
        ui::input("title", RESOURCE_TITLE, TextInputStyle::Short, true, None),
        ui::input("url", RESOURCE_LINK, TextInputStyle::Short, true, Some("https://…")),
        ui::input("about", RESOURCE_ABOUT, TextInputStyle::Paragraph, true, Some("A guide, a book, a tool, a paper…")),
    ];
    if !tags.is_empty() {
        let options: Vec<(String, String)> = tags.iter().map(|t| (t.clone(), t.clone())).collect();
        fields.push(ui::choices("tags", &format!("{RESOURCE_TAGS} (optional)"), &options, MAX_TAGS));
    }
    ("Share a resource".into(), fields)
}

/// What the form says, as a resource.
fn resource(v: &Values) -> NewResource {
    NewResource { title: v.text("title"), url: v.text("url"), description: v.text("about"), tags: v.picked("tags") }
}

/// The checks that cost nothing: an https link, a title, a line on what it is.
pub fn precheck(v: &Values) -> Result<(), Refusal> {
    chipsoc_admin::precheck_resource(&resource(v)).map_err(|e| Refusal::fix(e.to_string()))
}

/// The form sent: checked (a new link, the forum's tags; this reads the
/// forum), then a card in the review channel. Accepting it posts the link.
pub async fn vet(ctx: &Ctx, v: &Values) -> Result<Vetted, Refusal> {
    let r = resource(v);
    if let Err(e) = chipsoc_admin::vet_resource(ctx, &r).await {
        return Err(match e.downcast_ref::<chipsoc_admin::Refusal>() {
            Some(why) => Refusal::fix(why.markdown()),
            None => {
                tracing::warn!("checking a shared resource failed: {e:#}");
                Refusal::fix("Couldn't check that link just now; please try again later.")
            }
        });
    }
    let title = ui::escape(&r.title);
    let thanks = format!(
        "Thanks! **{title}** is with the admins for a look. You'll get a DM when it's posted (or if it isn't)."
    );
    Ok(Vetted::Review(Filing::new(
        AppKind::Resource,
        &r.url,
        format!("{} · {title}", AppKind::Resource.label()),
        r.answers(),
        thanks,
    )))
}
