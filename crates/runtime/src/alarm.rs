//! A problem that lasts is said again every so often, not once and then
//! never; and when it clears, that's said too. A problem that may be a
//! hiccup (the network, GitHub) is said only once it has happened a few
//! times in a row.

use std::time::{Duration, Instant};

/// How often a lasting problem is repeated in the bot log.
pub const REPEAT: Duration = Duration::from_secs(60 * 60);

#[derive(Debug)]
pub struct Alarm {
    /// Failures in a row before it's said.
    after: u32,
    streak: u32,
    said: Option<Instant>,
}

impl Default for Alarm {
    fn default() -> Self {
        Self::after(1)
    }
}

impl Alarm {
    /// One that speaks up only after `n` failures in a row.
    pub fn after(n: u32) -> Self {
        Self { after: n.max(1), streak: 0, said: None }
    }

    /// It failed: whether to say so now (once it has failed `after` times
    /// in a row, then every [`REPEAT`]).
    pub fn failed(&mut self, now: Instant) -> bool {
        self.streak = self.streak.saturating_add(1);
        if self.streak < self.after || self.said.is_some_and(|t| now.duration_since(t) < REPEAT) {
            return false;
        }
        self.said = Some(now);
        true
    }

    /// It worked: whether the problem had been said (so it's worth saying it's fixed).
    pub fn ok(&mut self) -> bool {
        self.streak = 0;
        self.said.take().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repeats_hourly_and_says_when_fixed() {
        let mut a = Alarm::default();
        let t = Instant::now();
        assert!(!a.ok(), "nothing to clear");
        assert!(a.failed(t));
        assert!(!a.failed(t + Duration::from_secs(120)));
        assert!(a.failed(t + REPEAT));
        assert!(a.ok());
        assert!(!a.ok());
        assert!(a.failed(t + REPEAT + Duration::from_secs(1)), "a new failure is said at once");
    }

    #[test]
    fn hiccups_arent_said_but_a_lasting_problem_is() {
        let min = |m: u64| Duration::from_secs(60 * m);
        let mut a = Alarm::after(3);
        let t = Instant::now();
        // GitHub refuses now and then: never three times running.
        for n in 0..10 {
            assert!(!a.failed(t + min(4 * n)), "a single failure stays in the process log");
            assert!(!a.ok(), "nothing was said, so nothing to clear");
        }
        assert!(!a.failed(t) && !a.failed(t + min(2)));
        assert!(a.failed(t + min(4)), "the third in a row is said");
        for n in 3..30 {
            assert!(!a.failed(t + min(2 * n)), "not again within the hour");
        }
        assert!(a.failed(t + min(4) + REPEAT), "said again an hour later");
        assert!(a.ok(), "and once when it works again");
        assert!(!a.ok());
    }
}
