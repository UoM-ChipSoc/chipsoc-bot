//! `/propose-project`: a form that goes to the review queue; once an admin
//! accepts it, it becomes a tagged post in the ideas forum.

use chipsoc_admin::{Answer, Lead, NewIdea};
use chipsoc_config::authoring::MAX_FORM_FIELDS;
use chipsoc_config::bot::{IdeaQuestion, default_idea_questions};
use chipsoc_store::AppKind;
use twilight_model::channel::message::Component;
use twilight_model::channel::message::component::TextInputStyle;

use crate::review::{Filing, Vetted};
use crate::ui::Values;
use crate::{Ctx, ui};

/// Ideas need their forum and the review queue.
pub fn ready(ctx: &Ctx) -> Result<(), String> {
    let cfg = ctx.cfg();
    if cfg.bot.ideas.is_none() || cfg.bot.review.is_none() {
        return Err("Project ideas aren't set up here.".into());
    }
    Ok(())
}

/// Whether `tag` is one the bot puts on ideas itself, never a member's pick.
fn bot_tag(i: &chipsoc_config::bot::Ideas, tag: &str) -> bool {
    [&i.needs_lead_tag, &i.closed_tag].into_iter().flatten().any(|t| t.eq_ignore_ascii_case(tag))
}

/// The form `[ideas] questions` asks (by default: name, one-line
/// description, area, what it would do, and whether they'd lead it).
pub fn form(ctx: &Ctx) -> (String, Vec<Component>) {
    let cfg = ctx.cfg();
    let areas = cfg.bot.projects.as_ref().map(|p| p.areas.join(", ")).unwrap_or_default();
    let lead: Vec<(String, String)> = Lead::ALL.iter().map(|l| (l.label().to_owned(), l.key().to_owned())).collect();
    // The area becomes a tag on the post: a menu of the forum's tags, if it
    // has any, but not those the bot sets itself (needs a lead, closed).
    let ideas = cfg.bot.ideas.as_ref();
    let tags: Vec<(String, String)> = ideas
        .and_then(|i| cfg.desired.channel(&i.forum).map(|f| (i, f)))
        .map(|(i, f)| f.tags.iter().filter(|t| !bot_tag(i, t)).map(|t| (t.clone(), t.clone())).collect())
        .unwrap_or_default();
    let questions = ideas.map_or_else(default_idea_questions, |i| i.questions.clone());
    let fields = questions
        .iter()
        .take(MAX_FORM_FIELDS)
        .map(|q| {
            let ask = &q.question.ask;
            match q.key.as_str() {
                "area" if tags.is_empty() => {
                    let hint = q.question.hint.as_deref().or((!areas.is_empty()).then_some(areas.as_str()));
                    ui::input("area", ask, TextInputStyle::Short, !q.question.optional, hint)
                }
                "area" => ui::choices("area", &format!("{ask} (optional)"), &tags, 1),
                "lead" => ui::choice("lead", ask, &lead),
                key => crate::apps::ask(key, &q.question),
            }
        })
        .collect();
    ("Propose a project".into(), fields)
}

/// The form sent: a card in the review channel, with every answer. Accepting it posts the idea.
pub fn vet(ctx: &Ctx, v: &Values) -> Vetted {
    let cfg = ctx.cfg();
    let questions = cfg.bot.ideas.as_ref().map_or_else(default_idea_questions, |i| i.questions.clone());
    let questions: Vec<IdeaQuestion> = questions.into_iter().take(MAX_FORM_FIELDS).collect();
    let more = questions
        .iter()
        .filter(|q| !q.is_own())
        .map(|q| Answer::keyed(&q.key, &q.question.ask, v.text(&q.key)))
        .collect();
    let idea = NewIdea {
        name: v.text("name"),
        blurb: v.text("blurb"),
        area: v.pick("area").unwrap_or_else(|| v.text("area")),
        scope: v.text("scope"),
        lead: Lead::parse(&v.pick("lead").unwrap_or_default()),
        more,
    };
    let name = ui::escape(&idea.name);
    let thanks =
        format!("Thanks! **{name}** is with the admins for a look. You'll get a DM when it's posted (or if it isn't).");
    let answers = idea.answers_for(&questions);
    let mut f = Filing::new(AppKind::Idea, &idea.name, format!("{} · {name}", AppKind::Idea.label()), answers, thanks);
    f.lead = idea.lead == Lead::Lead;
    Vetted::Review(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_area_menu_leaves_out_the_tags_the_bot_sets() {
        let i: chipsoc_config::bot::Ideas =
            toml::from_str("forum = \"ideas\"\nneeds_lead_tag = \"needs-lead\"\nclosed_tag = \"Closed\"").unwrap();
        assert!(bot_tag(&i, "needs-lead") && bot_tag(&i, "closed") && bot_tag(&i, "CLOSED"));
        assert!(!bot_tag(&i, "cpu"));
    }
}
