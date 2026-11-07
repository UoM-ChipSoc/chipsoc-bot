//! Plans shown in Discord with an Apply button. Each is kept here under a short
//! id until it's pressed, so the button applies exactly what was shown.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use chipsoc_admin::fmt;
use chipsoc_config::Config;
use chipsoc_discord::exec::Report;
use chipsoc_model::{Expiring, Plan};

/// How long an Apply button stays good.
const KEEP: Duration = Duration::from_secs(24 * 3600);
/// Plans held at once; the oldest goes first.
const MAX_HELD: usize = 50;
/// Discord lets a reply be edited for 15 minutes; after that its buttons
/// just find the plan gone.
const EDITABLE: Duration = Duration::from_secs(14 * 60);

pub struct Plans {
    held: Mutex<Expiring<Plan>>,
    made: AtomicU64,
    /// Plans shown in replies only one admin sees: (plan id, interaction token, when).
    replies: Mutex<Vec<(String, String, Instant)>>,
}

impl Default for Plans {
    fn default() -> Self {
        Self { held: Mutex::new(Expiring::new(KEEP, MAX_HELD)), made: AtomicU64::new(0), replies: Mutex::default() }
    }
}

impl Plans {
    /// Keep a plan; returns its id for the button.
    pub fn put(&self, plan: Plan) -> String {
        self.put_at(plan, Instant::now())
    }

    /// The plan behind a button, once: a second press finds nothing.
    pub fn take(&self, id: &str) -> Option<Plan> {
        self.take_at(id, Instant::now())
    }

    /// Note that plan `id` is shown in the reply to interaction `token`.
    pub fn in_reply(&self, id: &str, token: &str) {
        self.replies().push((id.to_owned(), token.to_owned(), Instant::now()));
    }

    /// Plan `id`'s reply was pressed: what it shows next is up to the press.
    pub fn forget_reply(&self, id: &str) {
        self.replies().retain(|(p, _, _)| p != id);
    }

    /// The replies (plan id, token) still showing a plan other than `keep`,
    /// forgotten here and their plans dropped, so their buttons do nothing.
    pub fn close_replies(&self, keep: Option<&str>) -> Vec<(String, String)> {
        self.close_replies_at(keep, Instant::now())
    }

    fn close_replies_at(&self, keep: Option<&str>, now: Instant) -> Vec<(String, String)> {
        let mut replies = self.replies();
        let (kept, closed): (Vec<_>, Vec<_>) = replies.drain(..).partition(|(id, _, _)| Some(id.as_str()) == keep);
        *replies = kept;
        drop(replies);
        let mut out = Vec::new();
        for (id, token, at) in closed {
            self.take_at(&id, now);
            if now.duration_since(at) < EDITABLE {
                out.push((id, token));
            }
        }
        out
    }

    fn replies(&self) -> std::sync::MutexGuard<'_, Vec<(String, String, Instant)>> {
        self.replies.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn held(&self) -> std::sync::MutexGuard<'_, Expiring<Plan>> {
        self.held.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put_at(&self, plan: Plan, now: Instant) -> String {
        let n = self.made.fetch_add(1, Ordering::Relaxed);
        let id =
            chipsoc_model::stable_hash(format!("{}:{n}:{:?}", plan.fingerprint, SystemTime::now()))[..10].to_owned();
        self.held().put(id.clone(), plan, now);
        id
    }

    fn take_at(&self, id: &str, now: Instant) -> Option<Plan> {
        self.held().take(id, now)
    }
}

/// What a plan would do, for a message with an Apply button: `head` in
/// bold, then the steps grouped by kind, then warnings.
pub fn summary(cfg: &Config, head: &str, plan: &Plan) -> String {
    fmt::plan(head, plan, &fmt::Names::new(cfg))
}

/// "3 changes to apply".
pub fn to_apply(plan: &Plan) -> String {
    format!("{} to apply", fmt::count(plan.changes(), "change", "changes"))
}

/// Who applied, and how each step went (failures first).
pub fn outcome(cfg: &Config, who: &str, report: &Report) -> String {
    fmt::outcome(who, report, &fmt::Names::new(cfg))
}

#[cfg(test)]
mod tests {
    use chipsoc_discord::exec::Outcome;
    use chipsoc_model::{Action, ObjectKind};

    use super::*;

    fn create(key: &str) -> Action {
        Action::Create { kind: ObjectKind::Channel, key: key.into() }
    }

    #[test]
    fn a_plan_is_taken_once_and_expires() {
        let plans = Plans::default();
        let t = Instant::now();
        let a = plans.put_at(Plan { fingerprint: "f".into(), ..Plan::default() }, t);
        let b = plans.put_at(Plan::default(), t);
        assert_ne!(a, b);
        assert!(plans.take_at(&a, t + Duration::from_secs(60)).is_some_and(|p| p.fingerprint == "f"));
        assert!(plans.take_at(&a, t).is_none(), "used up");
        assert!(plans.take_at(&b, t + KEEP).is_none(), "expired");
        assert!(plans.take_at("nonsense", t).is_none());
    }

    #[test]
    fn only_so_many_plans_are_held() {
        let plans = Plans::default();
        let t = Instant::now();
        let first = plans.put_at(Plan::default(), t);
        for n in 1..=MAX_HELD as u64 {
            plans.put_at(Plan::default(), t + Duration::from_secs(n));
        }
        assert!(plans.take_at(&first, t).is_none(), "the oldest made room");
    }

    #[test]
    fn replies_showing_older_plans_are_closed_and_their_plans_dropped() {
        let plans = Plans::default();
        let t = Instant::now();
        let (old, stale, new) =
            (plans.put_at(Plan::default(), t), plans.put_at(Plan::default(), t), plans.put(Plan::default()));
        plans.replies().push((old.clone(), "tok-old".into(), t));
        plans.replies().push((stale.clone(), "tok-stale".into(), t));
        plans.in_reply(&new, "tok-new");
        let closed = plans.close_replies_at(Some(&new), t + EDITABLE / 2);
        assert_eq!(closed.len(), 2, "both older ones, still editable");
        assert!(plans.take_at(&old, t).is_none(), "an old reply's Apply finds nothing");
        assert_eq!(plans.close_replies_at(None, t + EDITABLE * 2), [], "too late to edit the newest: just dropped");
        assert!(plans.take(&new).is_none());
    }

    #[test]
    fn outcomes_list_failures_first_and_fit() {
        let mut report = Report::default();
        for n in 0..300 {
            let result = if n == 299 { Err("no permission".into()) } else { Ok(()) };
            report.outcomes.push(Outcome { action: create(&format!("channel-{n}")), result });
        }
        let text = outcome(&Config::default(), "<@1>", &report);
        assert!(text.chars().count() <= fmt::MAX_CONTENT);
        assert!(
            text.starts_with("⚠️ **Applied by <@1>:** 299 done, 1 failed.\n❌ Create `channel-299`: no permission."),
            "{text}"
        );
        assert!(text.contains("**Channels** (299)") && text.contains("+293 more"), "{text}");
    }

    #[test]
    fn summaries_count_steps() {
        let plan = Plan { actions: vec![create("a"), create("b")], ..Plan::default() };
        assert_eq!(to_apply(&plan), "2 changes to apply");
        assert_eq!(
            summary(&Config::default(), &to_apply(&plan), &plan),
            "**2 changes to apply**\n**Channels** (2)\n• Create `a`\n• Create `b`"
        );
    }
}
