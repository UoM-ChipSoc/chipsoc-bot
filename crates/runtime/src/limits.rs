//! Per-member rate limits on forms anyone can send, so one person can't flood
//! the review channel or the feed fetcher. Each form has its own. A turn is
//! taken before any work: one refused by checks that cost nothing (a typo)
//! is given back, but once the bot has done work for it (read a forum,
//! fetched an address) it counts, refused or not (see [`Turn`]).

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What's limited, and how often each member may do it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    /// `/volunteer`.
    Volunteer,
    /// `/apply`.
    Apply,
    /// `/propose-project`.
    Idea,
    /// `/suggest-feed` (each one fetches the address).
    Feed,
    /// `/share-resource` (each one reads the resources forum).
    Resource,
    /// `/event` from members: a suggestion.
    Event,
    /// Confirming the join flow or sending `/profile` (each reads the member list and may rename namesakes).
    Profile,
}

impl Kind {
    fn every(self) -> Duration {
        match self {
            Self::Volunteer | Self::Apply => Duration::from_secs(60),
            Self::Idea | Self::Event => Duration::from_secs(5 * 60),
            Self::Feed | Self::Resource => Duration::from_secs(2 * 60),
            Self::Profile => Duration::from_secs(10),
        }
    }
}

/// A turn taken: given back when dropped, unless [`Turn::used`] says it
/// counts. So an early return before then (refused, failed to save) leaves
/// the member free to fix it and send again straight away. Call `used`
/// before work that costs (a fetch, a forum read), not after it.
#[must_use = "a turn is given back unless it's used"]
pub struct Turn<'a> {
    limits: &'a Limits,
    kind: Kind,
    user: u64,
    used: bool,
}

impl Turn<'_> {
    /// The submit went through, or is about to cost something: keep the turn.
    pub fn used(mut self) {
        self.used = true;
    }
}

impl Drop for Turn<'_> {
    fn drop(&mut self) {
        if !self.used {
            self.limits.forget(self.kind, self.user);
        }
    }
}

/// When each member last did each thing.
#[derive(Default)]
pub struct Limits {
    last: Mutex<HashMap<(Kind, u64), Instant>>,
}

impl Limits {
    /// Note that `user` does `kind` now, or say how many seconds until they may.
    pub fn take(&self, kind: Kind, user: u64) -> Result<(), u64> {
        self.take_at(kind, user, Instant::now())
    }

    fn take_at(&self, kind: Kind, user: u64, now: Instant) -> Result<(), u64> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(at) = last.get(&(kind, user)) {
            let since = now.saturating_duration_since(*at);
            if since < kind.every() {
                return Err((kind.every() - since).as_secs().max(1));
            }
        }
        // Forget anyone who's done waiting, so the map stays small.
        last.retain(|(k, _), at| now.saturating_duration_since(*at) < k.every());
        last.insert((kind, user), now);
        Ok(())
    }

    /// How many seconds until `user` may do `kind` again, if they must wait
    /// (asked when a form opens, so nobody fills one in only to be told to wait).
    pub fn wait(&self, kind: Kind, user: u64) -> Option<u64> {
        let last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        let since = Instant::now().saturating_duration_since(*last.get(&(kind, user))?);
        (since < kind.every()).then(|| kind.every().saturating_sub(since).as_secs().max(1))
    }

    /// Give back a turn taken for something that was refused, so fixing a
    /// typo doesn't mean waiting.
    fn forget(&self, kind: Kind, user: u64) {
        self.last.lock().unwrap_or_else(|e| e.into_inner()).remove(&(kind, user));
    }

    /// Take a turn (see [`Turn`]), or say as a message for the member how long to wait.
    pub fn turn(&self, kind: Kind, user: u64) -> Result<Turn<'_>, String> {
        match self.take(kind, user) {
            Ok(()) => Ok(Turn { limits: self, kind, user, used: false }),
            Err(s) => Err(waiting(s)),
        }
    }
}

/// What a member is told while they must wait `secs` seconds.
pub fn waiting(secs: u64) -> String {
    format!("You've just sent one; please wait {} and try again.", chipsoc_admin::fmt::about(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_at_a_time_per_member_and_kind() {
        let l = Limits::default();
        let t = Instant::now();
        assert!(l.take_at(Kind::Idea, 1, t).is_ok());
        assert!(l.take_at(Kind::Idea, 1, t + Duration::from_secs(10)).is_err());
        assert!(l.take_at(Kind::Idea, 2, t).is_ok(), "someone else");
        assert!(l.take_at(Kind::Feed, 1, t).is_ok(), "something else");
        assert!(l.take_at(Kind::Idea, 1, t + Kind::Idea.every()).is_ok(), "after the wait");
        l.forget(Kind::Idea, 1);
        assert!(l.take_at(Kind::Idea, 1, t + Kind::Idea.every()).is_ok(), "given back after a refusal");
    }

    #[test]
    fn each_form_has_its_own_and_only_a_submit_that_goes_through_counts() {
        let l = Limits::default();
        l.turn(Kind::Apply, 1).unwrap().used();
        assert!(l.turn(Kind::Volunteer, 1).is_ok(), "a position after a project: a different form");
        assert!(l.turn(Kind::Apply, 1).err().unwrap().contains("please wait"), "the same form again");
        {
            let _refused = l.turn(Kind::Feed, 1).unwrap();
            // dropped without being used: an unreadable feed, say
        }
        l.turn(Kind::Feed, 1).expect("the refused one was given back").used();
        assert!(l.turn(Kind::Feed, 1).is_err(), "a used one isn't");
        assert!(l.turn(Kind::Profile, 1).is_ok());
    }

    #[test]
    fn a_form_says_how_long_to_wait_before_it_opens() {
        let l = Limits::default();
        assert_eq!(l.wait(Kind::Idea, 1), None);
        l.turn(Kind::Idea, 1).unwrap().used();
        let secs = l.wait(Kind::Idea, 1).unwrap();
        assert!(secs > 290, "{secs}");
        assert!(l.turn(Kind::Idea, 1).err().unwrap().contains("please wait about 5 minutes"));
        assert_eq!(l.wait(Kind::Idea, 2), None, "someone else");
    }
}
