//! What partners share: put back together from its parts, capped per hour
//! and kind, then filed for review or posted (see [`chipsoc_admin::shares`]).
//! The mass guards are for roles; content has its own caps.

use chipsoc_admin::shares::{Ignored, Share, ShareKind, Taken, a_noun, noun};

use super::*;

/// Items of one kind taken from one partner per hour (new ones and
/// updates); more of that kind are ignored. Per kind, so a flood of one
/// never shuts out the others.
pub(super) const MAX_SHARES_PER_HOUR: u32 = 30;
/// Items from one partner coming in parts at once.
pub(super) const MAX_GATHERING: usize = 10;
/// How long an item's parts are waited for (seconds); then what came is dropped.
pub(super) const GATHER_FOR: i64 = 30 * 60;

/// A shared item coming in parts.
pub(super) struct Gathering {
    parts: u64,
    got: BTreeMap<u64, Share>,
    /// The lowest envelope seq among the parts.
    seq: u64,
    pub(super) msgs: Vec<u64>,
    /// When its first part came.
    started: i64,
}

#[derive(Debug, PartialEq)]
pub(super) enum Gathered {
    Waiting,
    /// Too many items in parts at once from this partner.
    Dropped,
    /// Every part is in: the item with its whole text.
    Complete {
        item: Box<Share>,
        seq: u64,
        msgs: Vec<u64>,
    },
}

/// Drop `peer`'s items whose parts didn't all come within [`GATHER_FOR`]
/// (they gave up, or sent it again in other parts). The messages they were in.
pub(super) fn expire(all: &mut HashMap<(String, String), Gathering>, peer: &str, now: i64) -> Vec<u64> {
    let old: Vec<(String, String)> =
        all.iter().filter(|((p, _), g)| p == peer && now - g.started > GATHER_FOR).map(|(k, _)| k.clone()).collect();
    old.into_iter().filter_map(|k| all.remove(&k)).flat_map(|g| g.msgs).collect()
}

/// Add one part of a shared item.
pub(super) fn gather(
    all: &mut HashMap<(String, String), Gathering>,
    peer: &str,
    item: Share,
    seq: u64,
    msg: u64,
    now: i64,
) -> Gathered {
    if item.parts <= 1 {
        return Gathered::Complete { item: Box::new(item), seq, msgs: vec![msg] };
    }
    let batch = if item.batch.is_empty() { item.origin.clone() } else { item.batch.clone() };
    let key = (peer.to_owned(), batch);
    if !all.contains_key(&key) && all.keys().filter(|(p, _)| p == peer).count() >= MAX_GATHERING {
        return Gathered::Dropped;
    }
    let g = all.entry(key.clone()).or_insert_with(|| Gathering {
        parts: item.parts,
        got: BTreeMap::new(),
        seq,
        msgs: Vec::new(),
        started: now,
    });
    if g.parts != item.parts {
        return Gathered::Dropped;
    }
    g.seq = g.seq.min(seq);
    if !g.msgs.contains(&msg) {
        g.msgs.push(msg);
    }
    g.got.entry(item.part).or_insert(item);
    if (g.got.len() as u64) < g.parts {
        return Gathered::Waiting;
    }
    let Some(g) = all.remove(&key) else { return Gathered::Waiting };
    let mut parts = g.got.into_values();
    let Some(mut item) = parts.next() else { return Gathered::Dropped };
    for p in parts {
        item.text.push_str(&p.text);
    }
    (item.part, item.parts) = (0, 1);
    Gathered::Complete { item: Box::new(item), seq: g.seq, msgs: g.msgs }
}

/// File, post or update a whole item from `peer`; says in the log what's
/// worth knowing (at most [`MAX_LOGS_PER_HOUR`] notes). `first`: this is
/// its first try, so it counts toward the hourly cap (a retry doesn't
/// count again).
pub(super) async fn take_share<E: Env>(e: &E, peer: &Peer, mut item: Share, seq: u64, first: bool) -> Outcome {
    if item.what == ShareKind::Other {
        tracing::info!(peer = peer.name, "ignored a shared item of a kind this version doesn't know");
        return Outcome::Done;
    }
    item.title = ui::truncate(&item.title, chipsoc_admin::shares::MAX_TITLE);
    item.text = ui::truncate(&item.text, chipsoc_admin::shares::MAX_TEXT);
    if fmt::partner_name(&item.title, chipsoc_admin::shares::MAX_TITLE).is_empty() {
        // Nothing to call it (Discord would refuse it every time).
        tracing::info!(peer = peer.name, "ignored a shared item with no title");
        return Outcome::Done;
    }
    let spent = if first {
        let key = (peer.id.clone(), item.what.plural());
        e.state().inbox.lock().await.shared.entry(key).or_default().spend(MAX_SHARES_PER_HOUR, now())
    } else {
        Spent::Ok
    };
    match spent {
        Spent::Ok => {}
        Spent::JustOver => {
            let text = format!(
                "{} **{} shared over {MAX_SHARES_PER_HOUR} {} this hour**, so the bot is leaving the rest of those out until the hour's up. Check with them.",
                fmt::ATTENTION,
                peer.name,
                item.what.plural()
            );
            log_about(e, peer, &text).await;
            return Outcome::Done;
        }
        Spent::Over => return Outcome::Done,
    }
    let what = item.what;
    match chipsoc_admin::shares::take(e, peer, item, seq).await {
        Ok(
            Taken::Filed(_) | Taken::Updated | Taken::Ignored(Ignored::Stale | Ignored::BadEvent | Ignored::Untitled),
        ) => Outcome::Done,
        Ok(Taken::Posted(link)) => {
            log_about(e, peer, &format!("📥 Posted {} {} shared: {link}", a_noun(what), peer.name)).await;
            Outcome::Done
        }
        Ok(Taken::Ignored(Ignored::NotTaken)) => {
            let first = e.state().inbox.lock().await.not_taken.insert((peer.id.clone(), what.plural()));
            if first {
                let text = format!(
                    "{} shared {}, but we don't take their {} (`accept` in federation.toml), so it was left out.",
                    peer.name,
                    a_noun(what),
                    what.plural()
                );
                log_about(e, peer, &text).await;
            }
            Outcome::Done
        }
        Ok(Taken::Ignored(Ignored::QueueFull)) => {
            let text = format!(
                "{} **{} has {} {} waiting for review already**, so the bot left out the {} they just shared. Accept or reject some first.",
                fmt::ATTENTION,
                peer.name,
                chipsoc_admin::shares::MAX_IN_REVIEW,
                what.plural(),
                noun(what)
            );
            log_about(e, peer, &text).await;
            Outcome::Done
        }
        Err(err) => {
            tracing::warn!(?err, peer = peer.name, "taking a partner's item failed; will retry");
            Outcome::Retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(origin: &str, part: u64, parts: u64, text: &str) -> Share {
        let mut s = Share::new(ShareKind::Announcement, origin, "News");
        (s.part, s.parts, s.text, s.batch) = (part, parts, text.into(), format!("b{origin}"));
        s
    }

    #[test]
    fn parts_are_put_back_together_in_order() {
        let mut all = HashMap::new();
        assert_eq!(gather(&mut all, "p", part("1", 1, 2, "world"), 20, 2, 0), Gathered::Waiting);
        let Gathered::Complete { item, seq, msgs } = gather(&mut all, "p", part("1", 0, 2, "hello "), 10, 1, 0) else {
            panic!()
        };
        assert_eq!((item.text.as_str(), item.parts, seq, msgs), ("hello world", 1, 10, vec![2, 1]));
        assert!(all.is_empty());
        let whole = Share::new(ShareKind::Resource, "2", "T");
        assert!(matches!(gather(&mut all, "p", whole, 1, 3, 0), Gathered::Complete { .. }));
        for i in 0..MAX_GATHERING {
            assert_eq!(gather(&mut all, "p", part(&format!("x{i}"), 0, 2, ""), 1, 9, 0), Gathered::Waiting);
        }
        assert_eq!(gather(&mut all, "p", part("y", 0, 2, ""), 1, 9, 0), Gathered::Dropped, "capped per partner");
        assert_eq!(gather(&mut all, "q", part("y", 0, 2, ""), 1, 9, 0), Gathered::Waiting, "others aren't");
        // Half-received items don't hold their places for ever.
        assert!(expire(&mut all, "p", GATHER_FOR).is_empty(), "not yet");
        assert_eq!(expire(&mut all, "p", GATHER_FOR + 1), vec![9; MAX_GATHERING]);
        assert_eq!(gather(&mut all, "p", part("y", 0, 2, ""), 1, 10, GATHER_FOR + 1), Gathered::Waiting, "room again");
        assert_eq!(all.len(), 2, "q's is kept");
    }
}
