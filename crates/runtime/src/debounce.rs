//! When to run a check after a burst of events: a while after the last one
//! (but not forever after the first), and not at all for events the bot caused.

use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct Debounce {
    /// Wait this long after the latest event…
    wait: Duration,
    /// …but no longer than this after the first.
    max_wait: Duration,
    first: Option<Instant>,
    due: Option<Instant>,
    quiet_until: Option<Instant>,
}

impl Debounce {
    pub fn new(wait: Duration, max_wait: Duration) -> Self {
        Self { wait, max_wait, first: None, due: None, quiet_until: None }
    }

    /// An event happened. Returns whether it counts (it doesn't while quiet).
    pub fn poke(&mut self, now: Instant) -> bool {
        if self.quiet_until.is_some_and(|q| now < q) {
            return false;
        }
        let first = *self.first.get_or_insert(now);
        self.due = Some((now + self.wait).min(first + self.max_wait));
        true
    }

    /// When the check should run, if one is waiting.
    pub fn due(&self) -> Option<Instant> {
        self.due
    }

    /// The check ran: forget the events before it.
    pub fn done(&mut self) {
        (self.first, self.due) = (None, None);
    }

    /// Check again in a while even if quiet: the last check couldn't finish.
    pub fn again(&mut self, now: Instant) {
        let at = now + self.wait;
        self.due = Some(self.quiet_until.map_or(at, |q| q.max(at)));
    }

    /// Ignore events until `until` (the bot is changing things itself).
    pub fn quiet(&mut self, until: Instant) {
        self.quiet_until = Some(until);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn a_burst_runs_once_after_the_last_event() {
        let t = Instant::now();
        let mut d = Debounce::new(15 * S, 60 * S);
        assert_eq!(d.due(), None);
        assert!(d.poke(t));
        assert!(d.poke(t + 5 * S));
        assert_eq!(d.due(), Some(t + 20 * S));
        d.done();
        assert_eq!(d.due(), None);
    }

    #[test]
    fn a_steady_stream_still_runs() {
        let t = Instant::now();
        let mut d = Debounce::new(15 * S, 60 * S);
        for n in 0..10 {
            d.poke(t + n * 10 * S);
        }
        assert_eq!(d.due(), Some(t + 60 * S), "capped at max_wait after the first");
    }

    #[test]
    fn quiet_ignores_the_bots_own_changes() {
        let t = Instant::now();
        let mut d = Debounce::new(15 * S, 60 * S);
        d.quiet(t + 10 * S);
        assert!(!d.poke(t + 9 * S));
        assert_eq!(d.due(), None);
        assert!(d.poke(t + 10 * S));
        assert_eq!(d.due(), Some(t + 25 * S));
    }

    #[test]
    fn again_runs_even_while_quiet() {
        let t = Instant::now();
        let mut d = Debounce::new(15 * S, 60 * S);
        d.quiet(t + 30 * S);
        d.again(t);
        assert_eq!(d.due(), Some(t + 30 * S), "once the quiet ends");
        d.done();
        d.quiet(t);
        d.again(t);
        assert_eq!(d.due(), Some(t + 15 * S), "the usual wait once not quiet");
    }
}
