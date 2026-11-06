//! Who's in a server: one list per server, read from Discord when it's
//! older than a while and kept up to date from the gateway's member events
//! meanwhile, so the jobs and handlers that need everyone (the join flow,
//! web access, nicknames, drift) don't each page through the member list.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use chipsoc_config::Snowflake;
use chipsoc_discord::Rest;
use serde_json::Value;
use twilight_model::id::Id;
use twilight_model::id::marker::RoleMarker;

/// How long a list read from Discord is used, events keeping it current.
/// Read again after, in case an event was missed.
pub(crate) const FRESH: Duration = Duration::from_secs(30 * 60);

/// A member event from the gateway.
#[derive(Debug)]
pub(crate) enum Seen {
    /// Joined: the member as Discord sends them.
    Joined(Value),
    /// Changed: the fields sent (roles, nickname, …), merged into what's kept.
    Changed(Value),
    Left(u64),
}

/// One server's member list (see the module docs).
pub(crate) struct Members {
    inner: Mutex<Inner>,
    /// One read from Discord at a time; the others wait for it.
    reading: tokio::sync::Mutex<()>,
    fresh: Duration,
}

#[derive(Default)]
struct Inner {
    /// When it was read, and the members (sorted by user id).
    list: Option<(Instant, Arc<Vec<Value>>)>,
    /// Being read: events meanwhile, applied once it's in.
    missed: Option<Vec<Seen>>,
}

impl Members {
    /// Kept for `fresh` (zero: read every time).
    pub fn new(fresh: Duration) -> Self {
        Self { inner: Mutex::default(), reading: tokio::sync::Mutex::new(()), fresh }
    }

    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Everyone in `guild`, from the list kept if it's fresh, else read from Discord.
    pub async fn get<R: Rest + ?Sized>(&self, rest: &R, guild: Snowflake) -> Result<Arc<Vec<Value>>> {
        let _one = self.reading.lock().await;
        {
            let mut inner = self.inner();
            if let Some((at, list)) = &inner.list
                && at.elapsed() < self.fresh
            {
                return Ok(list.clone());
            }
            inner.list = None;
            inner.missed = Some(Vec::new());
        }
        let read = chipsoc_discord::export::fetch_members(rest, guild).await;
        let mut inner = self.inner();
        let missed = inner.missed.take().unwrap_or_default();
        let mut list = read?;
        list.sort_by_key(user);
        for s in missed {
            apply(&mut list, s);
        }
        let list = Arc::new(list);
        if !self.fresh.is_zero() {
            inner.list = Some((Instant::now(), list.clone()));
        }
        Ok(list)
    }

    /// A member event: the list kept follows it.
    pub fn seen(&self, s: Seen) {
        let mut inner = self.inner();
        if let Some(missed) = &mut inner.missed {
            missed.push(s);
        } else if let Some((_, list)) = &mut inner.list {
            apply(Arc::make_mut(list), s);
        }
    }

    /// Read the list again next time (the gateway reconnected, say, and events may be lost).
    pub fn forget(&self) {
        self.inner().list = None;
    }
}

/// `s` applied to `list` (sorted by user id).
fn apply(list: &mut Vec<Value>, s: Seen) {
    let at = |list: &Vec<Value>, id: u64| list.binary_search_by_key(&Some(id), user);
    match s {
        Seen::Joined(m) => {
            let Some(id) = user(&m) else { return };
            match at(list, id) {
                Ok(i) => list[i] = m,
                Err(i) => list.insert(i, m),
            }
        }
        Seen::Changed(patch) => {
            let Some(id) = user(&patch) else { return };
            match at(list, id) {
                Ok(i) => {
                    if let (Some(m), Some(p)) = (list[i].as_object_mut(), patch.as_object()) {
                        // A join time Discord leaves out of an update isn't a lost one.
                        let kept = |(k, v): &(&String, &Value)| *k != "guild_id" && !(*k == "joined_at" && v.is_null());
                        m.extend(p.iter().filter(kept).map(|(k, v)| (k.clone(), v.clone())));
                    }
                }
                // Someone the list didn't have: they're in the server, so they're added.
                Err(i) => list.insert(i, patch),
            }
        }
        Seen::Left(id) => {
            if let Ok(i) = at(list, id) {
                list.remove(i);
            }
        }
    }
}

/// A member's user id.
pub(crate) fn user(m: &Value) -> Option<u64> {
    chipsoc_discord::wire::id(&m["user"]["id"]).map(|s| s.0)
}

pub(crate) fn is_bot(m: &Value) -> bool {
    m["user"]["bot"].as_bool().unwrap_or(false)
}

/// The roles a member holds.
pub(crate) fn roles(m: &Value) -> Vec<Id<RoleMarker>> {
    chipsoc_discord::wire::ids(&m["roles"]).into_iter().filter_map(|s| Id::new_checked(s.0)).collect()
}

/// When they joined, in Unix seconds (0 if Discord didn't say).
pub(crate) fn joined(m: &Value) -> i64 {
    m["joined_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map_or(0, |t| t.timestamp())
}

/// The name Discord shows for a member: nickname, else display name, else username.
pub(crate) fn shown(m: &Value) -> String {
    chipsoc_discord::wire::shown_name(m).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chipsoc_discord::fake::FakeDiscord;
    use serde_json::json;

    #[tokio::test]
    async fn the_list_follows_member_events_until_read_again() {
        let fake = FakeDiscord::new();
        for u in [7, 8] {
            fake.join(u);
        }
        let m = Members::new(FRESH);
        assert_eq!(m.get(&fake, Snowflake(1000)).await.unwrap().len(), 2);
        let reads = fake.requests();
        m.seen(Seen::Joined(json!({ "user": { "id": "9" }, "roles": [] })));
        m.seen(Seen::Changed(json!({ "guild_id": "1000", "user": { "id": "7" }, "roles": ["5"], "nick": "Ada" })));
        m.seen(Seen::Left(8));
        let list = m.get(&fake, Snowflake(1000)).await.unwrap();
        assert_eq!(fake.requests(), reads, "no second read");
        let ids: Vec<u64> = list.iter().filter_map(user).collect();
        assert_eq!(ids, [7, 9]);
        assert_eq!((shown(&list[0]), roles(&list[0])), ("Ada".to_owned(), vec![Id::new(5)]));
        assert!(list[0].get("guild_id").is_none());

        m.forget();
        assert_eq!(m.get(&fake, Snowflake(1000)).await.unwrap().len(), 2, "read again: the fake has 7 and 8");
        assert!(fake.requests() > reads);
    }

    #[test]
    fn names_and_join_times_read_as_discord_sends_them() {
        let m = json!({ "user": { "id": "1", "username": "ada", "global_name": "Ada L" }, "nick": null,
                        "joined_at": "2024-01-01T00:00:00.000000+00:00" });
        assert_eq!(shown(&m), "Ada L");
        assert_eq!(joined(&m), 1_704_067_200);
        assert!(!is_bot(&m));
    }
}
