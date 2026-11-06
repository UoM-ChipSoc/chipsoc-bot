//! `/solved`: close a help-forum post and tag it solved.

use chipsoc_discord::Method;
use twilight_model::application::interaction::Interaction;
use twilight_model::id::Id;
use twilight_model::id::marker::TagMarker;

use crate::apps::invoker;
use crate::{Ctx, ui};

pub async fn solved(ctx: &Ctx, i: &Interaction) {
    let Some(user) = invoker(i) else { return };
    let Some(help) = ctx.cfg().bot.help.clone() else { return };
    let Some(forum) = ctx.channel(&help.forum) else {
        return ctx.respond(i, &ui::ephemeral("The help forum isn't set up yet.")).await;
    };
    let Some(thread) = i.channel.as_ref().filter(|c| c.parent_id == Some(forum)) else {
        return ctx.respond(i, &ui::ephemeral(&format!("Use this inside a post in <#{forum}>."))).await;
    };
    if thread.owner_id != Some(user.id) && !ctx.is_admin(i.member.as_ref()) {
        return ctx.respond(i, &ui::ephemeral("Only the person who asked (or an admin) can mark this solved.")).await;
    }
    let solved_tag = match ctx.get(&format!("channels/{forum}")).await {
        Ok(c) => c["available_tags"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| t["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(&help.solved_tag)))
            .and_then(|t| chipsoc_discord::wire::id(&t["id"]))
            .and_then(|id| Id::<TagMarker>::new_checked(id.0)),
        Err(e) => {
            tracing::warn!(?e, "reading the help forum's tags failed");
            None
        }
    };
    let mut tags: Vec<Id<TagMarker>> = thread.applied_tags.clone().unwrap_or_default();
    if let Some(t) = solved_tag
        && !tags.contains(&t)
    {
        tags.push(t);
    }
    // Tag first, and say so only if that worked; archive last, as replying would reopen the post.
    let path = format!("channels/{}", thread.id);
    let tagged = ctx.call(Method::Patch, &path, Some(serde_json::json!({ "applied_tags": tags }))).await;
    if let Err(e) = tagged {
        tracing::warn!(?e, "tagging solved post failed");
        return ctx
            .respond(i, &ui::ephemeral("Couldn't update this post; is the bot allowed to manage threads?"))
            .await;
    }
    ctx.respond(i, &ui::reply(&solved_text(user.id.get(), solved_tag.is_some(), &help.solved_tag))).await;
    if solved_tag.is_none() {
        tracing::warn!(tag = help.solved_tag, "the help forum has no solved tag; posts are closed untagged");
    }
    let closed = ctx.call(Method::Patch, &path, Some(serde_json::json!({ "archived": true }))).await;
    ctx.log_err(closed, "closing solved post");
    ctx.audit(Some(&user.id.to_string()), "solved", Some(&thread.id.to_string()), None).await;
}

/// What `/solved` says, in the post: whether it could tag it too.
fn solved_text(by: u64, tagged: bool, tag: &str) -> String {
    let head = format!("✅ Marked solved by <@{by}>. Thanks for helping each other!");
    if tagged {
        head
    } else {
        format!("{head}\n-# Couldn't tag it **{}**: the forum has no such tag (an admin can add it).", ui::escape(tag))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_missing_tag_is_said() {
        assert!(!super::solved_text(1, true, "solved").contains("Couldn't"));
        let untagged = super::solved_text(1, false, "solved");
        assert!(untagged.starts_with("✅ Marked solved by <@1>.") && untagged.contains("no such tag"), "{untagged}");
    }
}
