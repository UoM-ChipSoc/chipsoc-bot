//! `/partners`: the projects partners share.

use super::*;

/// Messages `/partners` answers with at most.
pub(super) const MAX_PAGES: usize = 5;

pub(super) use chipsoc_admin::fmt::{plain, safe_link};

/// `lines` as messages that each fit, at most `max` of them; the last says
/// how many lines didn't fit.
pub(super) fn pages(lines: &[String], max: usize) -> Vec<String> {
    // Room for "…and N more."
    const ROOM: usize = 30;
    let mut out = Vec::new();
    let mut cur = String::new();
    for (n, line) in lines.iter().enumerate() {
        let line = ui::truncate(line, ui::MAX_CONTENT - ROOM - 1);
        if !cur.is_empty() && cur.chars().count() + 1 + line.chars().count() > ui::MAX_CONTENT - ROOM {
            if out.len() + 1 == max {
                cur.push_str(&format!("\n…and {} more.", lines.len() - n));
                out.push(cur);
                return out;
            }
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(&line);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[derive(Deserialize)]
pub(super) struct Catalogue {
    pub(super) projects: Vec<proto::Project>,
}

/// `/partners`: partners' shared project catalogues, over several messages if need be.
pub async fn partners(ctx: &Ctx, i: &Interaction) {
    let cfg = ctx.cfg();
    let catalogs = match ctx.store.fed(&ctx.gkey).catalogs().await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(?e, "reading partner catalogues failed");
            return ctx.respond(i, &ui::ephemeral("Couldn't read partners' projects; try again later.")).await;
        }
    };
    if catalogs.is_empty() {
        return ctx.respond(i, &ui::ephemeral("No partner has shared its projects yet.")).await;
    }
    let mut lines = vec![String::from("**Partners' projects**")];
    for c in catalogs {
        let name =
            cfg.federation.peers.iter().find(|p| p.id == c.peer).map_or_else(|| plain(&c.peer, 50), |p| p.name.clone());
        lines.push(format!("\n__{name}__ · updated <t:{}:R>", c.ts));
        if c.body.len() > MAX_CATALOG {
            lines.push("• (too big to show)".to_owned());
            continue;
        }
        let projects = serde_json::from_str::<Catalogue>(&c.body).map(|c| c.projects).unwrap_or_default();
        for p in projects {
            let link = p.github.as_deref().and_then(safe_link).map(|l| format!(" · {l}")).unwrap_or_default();
            let open = if p.open { " · recruiting" } else { "" };
            let (name, blurb, status) = (plain(&p.name, 100), plain(&p.blurb, 400), plain(&p.status, 30));
            lines.push(format!("• **{name}** — {blurb} · {status}{open}{link}"));
        }
    }
    let mut pages = pages(&lines, MAX_PAGES).into_iter();
    let Some(first) = pages.next() else { return };
    ctx.respond(i, &ui::ephemeral(&first)).await;
    for page in pages {
        let body = serde_json::json!({
            "content": page,
            "flags": MessageFlags::EPHEMERAL.bits(),
            "allowed_mentions": AllowedMentions::default(),
        });
        let path = format!("webhooks/{}/{}", ctx.app, i.token);
        ctx.log_err(ctx.call(chipsoc_discord::Method::Post, &path, Some(body)).await, "sending more partner projects");
    }
}
