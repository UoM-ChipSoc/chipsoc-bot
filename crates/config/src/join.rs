//! `join.toml`: the bot's join flow. Discord's own onboarding asks every
//! question of everyone; this one branches. Each step is a question with
//! options (each giving roles and naming the next step) or a few text boxes.
//! Pure parts live here (walking the answers, which roles they give, the
//! channel gate) so `check`, the planner and the bot agree.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::schema::{Overwrite, Snapshot};

fn is_false(b: &bool) -> bool {
    !*b
}
fn yes() -> bool {
    true
}
fn remind_after() -> u32 {
    24
}
fn is_default_remind(h: &u32) -> bool {
    *h == remind_after()
}

/// `next = "end"` finishes the flow from a step that would otherwise go on.
pub const END: &str = "end";
/// Field keys the bot also knows: the nickname is made from `name` and
/// `surname`, and `company` is kept with the profile.
pub const NAME: &str = "name";
/// See [`NAME`].
pub const SURNAME: &str = "surname";
/// See [`NAME`].
pub const COMPANY: &str = "company";

const VIEW: &str = "VIEW_CHANNEL";

/// `join.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Join {
    /// Role key given when someone finishes the flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access_role: Option<String>,
    /// Only `open_channels` are visible to @everyone; `access_role` sees the rest.
    #[serde(default, skip_serializing_if = "is_false")]
    pub gate: bool,
    /// Channel keys @everyone still sees with the gate on (the one with the join button, say).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub open_channels: Vec<String>,
    /// With the gate on, give `access_role` to members who were here before it was.
    #[serde(default, skip_serializing_if = "is_false")]
    pub grandfather: bool,
    /// With the gate on: hours after joining before one reminder DM, and a note
    /// in the bot log (default 24; 0 = never).
    #[serde(default = "remind_after", skip_serializing_if = "is_default_remind")]
    pub remind_after_hours: u32,
    /// A date as `"MM-DD"`, e.g. `"09-15"`: members who answered a `yearly`
    /// step are asked each year whether it's still right.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub yearly_refresh: Option<String>,
    /// Key of the first step (default: the first one written).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<String>,
    /// Shown above the first question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intro: Option<String>,
    /// The DM `/server join notify` sends members who haven't been through the flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notify_message: Option<String>,
    /// The DM a new member gets if they haven't finished after `remind_after_hours`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reminder_message: Option<String>,
    /// Said after Confirm: what to do next (default with the gate on: "The
    /// rest of the server is open now.").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub done_message: Option<String>,
    /// `[[step]]` entries.
    #[serde(default, rename = "step")]
    pub steps: Vec<Step>,
}

/// One question in the flow: options to pick, or text boxes.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// How `next` and `start` refer to it; kept with the answers.
    pub key: String,
    /// The question shown.
    pub question: String,
    /// A line under the question.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Several options may be picked.
    #[serde(default, skip_serializing_if = "is_false")]
    pub multi: bool,
    /// A `multi` step may be left with nothing picked.
    #[serde(default, skip_serializing_if = "is_false")]
    pub optional: bool,
    /// Asked again every year on `yearly_refresh`.
    #[serde(default, skip_serializing_if = "is_false")]
    pub yearly: bool,
    /// The step after this one (options may say otherwise); unset or `end` finishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// `[[step.option]]` entries: the choices, as a menu (up to 25).
    #[serde(default, rename = "option", skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<JoinOption>,
    /// `[[step.field]]` entries: text boxes (a form of up to 5), instead of options.
    #[serde(default, rename = "field", skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<Field>,
}

/// A choice in a step's menu.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JoinOption {
    /// Kept with the answers; defaults to the label as a slug. Set it to rename labels freely.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key: String,
    /// The text shown in the menu.
    pub label: String,
    /// A line under the label in the menu.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// A Unicode emoji shown with it, e.g. `"🎓"`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub emoji: Option<String>,
    /// Role keys given when picked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roles: Vec<String>,
    /// The step after this option (single-choice steps only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    /// `[[step.option.ask]]` entries: text boxes asked when it's picked, e.g. "Which university?".
    #[serde(default, rename = "ask", skip_serializing_if = "Vec::is_empty")]
    pub ask: Vec<Field>,
}

impl JoinOption {
    pub fn key(&self) -> String {
        if self.key.is_empty() { crate::slug(&self.label) } else { self.key.clone() }
    }
}

/// A text box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    /// Kept with the answers; `name`, `surname` and `company` also set the
    /// profile (and nickname).
    pub key: String,
    /// The box's label (up to 45 characters).
    pub label: String,
    /// A line under the label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    /// Placeholder text in the box.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// Must be filled in (default true).
    #[serde(default = "yes", skip_serializing_if = "Clone::clone")]
    pub required: bool,
    /// A paragraph box instead of one line.
    #[serde(default, skip_serializing_if = "is_false")]
    pub long: bool,
}

impl Default for Field {
    fn default() -> Self {
        Self { key: String::new(), label: String::new(), help: None, hint: None, required: true, long: false }
    }
}

/// What someone answered: step key → their answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Answers(pub BTreeMap<String, Answer>);

/// One step's answer.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
    /// Option keys picked.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
    /// Text boxes: field key → text.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub fields: BTreeMap<String, String>,
}

impl Answers {
    pub fn parse(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// What a finished set of answers comes to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// Roles the answers give (the access role not included).
    pub give: BTreeSet<String>,
    /// Every text box on the way: field key → text.
    pub fields: BTreeMap<String, String>,
    /// The answers on the path taken; others (an old branch) dropped.
    pub answers: Answers,
}

/// Where a walk through the answers got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Walk {
    /// Steps answered, in order.
    pub done: Vec<String>,
    /// The first step still to answer, or none if finished.
    pub at: Option<String>,
}

impl Join {
    pub fn step(&self, key: &str) -> Option<&Step> {
        self.steps.iter().find(|s| s.key == key)
    }

    pub fn first(&self) -> Option<&str> {
        self.start.as_deref().or(self.steps.first().map(|s| s.key.as_str()))
    }

    /// The step after `step` given its answer; `None` finishes.
    pub fn next_after<'a>(&'a self, step: &'a Step, answer: &Answer) -> Option<&'a str> {
        let chosen = (!step.multi)
            .then(|| answer.options.first().and_then(|k| step.option(k)))
            .flatten()
            .and_then(|o| o.next.as_deref());
        chosen.or(step.next.as_deref()).filter(|n| *n != END)
    }

    /// Follow the answers from the first step until one is missing or doesn't fit.
    pub fn walk(&self, answers: &Answers) -> Walk {
        let mut done = Vec::new();
        let mut at = self.first();
        while let Some(key) = at {
            let Some(step) = self.step(key) else { break };
            match answers.0.get(key).filter(|a| step.accepts(a)) {
                Some(a) if !done.iter().any(|d| d == key) => {
                    done.push(key.to_owned());
                    at = self.next_after(step, a);
                }
                _ => return Walk { done, at: Some(key.to_owned()) },
            }
        }
        Walk { done, at: None }
    }

    /// Every role an option can give.
    pub fn option_roles(&self) -> BTreeSet<String> {
        self.steps.iter().flat_map(|s| &s.options).flat_map(|o| o.roles.iter().cloned()).collect()
    }

    /// The roles the answers give and their text, if they're finished.
    pub fn outcome(&self, answers: &Answers) -> Option<Outcome> {
        let walk = self.walk(answers);
        if walk.at.is_some() {
            return None;
        }
        let mut out = Outcome::default();
        for key in &walk.done {
            let (Some(step), Some(a)) = (self.step(key), answers.0.get(key)) else { continue };
            let mut kept = Answer { options: a.options.clone(), fields: BTreeMap::new() };
            let picked: Vec<&JoinOption> = a.options.iter().filter_map(|k| step.option(k)).collect();
            let boxes = step.fields.iter().chain(picked.iter().flat_map(|o| &o.ask));
            for f in boxes {
                if let Some(v) = a.fields.get(&f.key).filter(|v| !v.trim().is_empty()) {
                    kept.fields.insert(f.key.clone(), v.clone());
                    out.fields.insert(f.key.clone(), v.clone());
                }
            }
            for o in picked {
                out.give.extend(o.roles.iter().cloned());
            }
            out.answers.0.insert(key.clone(), kept);
        }
        Some(out)
    }

    /// Each answer on the path taken, as (question, what they said): the
    /// options' labels, then text boxes as "label: text". Answers off the
    /// path (an old branch) aren't shown.
    pub fn said(&self, answers: &Answers) -> Vec<(String, String)> {
        let mut out = Vec::new();
        for key in self.walk(answers).done {
            let (Some(step), Some(a)) = (self.step(&key), answers.0.get(&key)) else { continue };
            let mut said: Vec<String> =
                a.options.iter().filter_map(|k| step.option(k)).map(|o| o.label.clone()).collect();
            for f in step.boxes(a) {
                if let Some(v) = a.fields.get(&f.key).filter(|v| !v.trim().is_empty()) {
                    said.push(format!("{}: {v}", f.label));
                }
            }
            out.push((step.question.clone(), said.join(", ")));
        }
        out
    }

    /// The answers on the path taken as short parts for one line (a review
    /// card's "who is this"): each option picked, with what its boxes say in
    /// brackets; an answer whose options give no roles (yes or no, say) after
    /// its question, so it reads on its own; text boxes as "label value". The
    /// name boxes are left out (the card mentions them).
    pub fn summary(&self, answers: &Answers) -> Vec<String> {
        let labelled = |label: &str, value: &str| {
            let label = label.trim();
            if label.ends_with('?') || label.ends_with(':') {
                format!("{label} {value}")
            } else {
                format!("{label}: {value}")
            }
        };
        // What the boxes say, the name boxes left out.
        let said = |a: &Answer, f: &Field| {
            let v = a.fields.get(&f.key).map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
            v.filter(|_| f.key != NAME && f.key != SURNAME)
        };
        let mut out = Vec::new();
        for key in self.walk(answers).done {
            let (Some(step), Some(a)) = (self.step(&key), answers.0.get(&key)) else { continue };
            if step.is_text() {
                out.extend(step.fields.iter().filter_map(|f| said(a, f).map(|v| labelled(&f.label, &v))));
                continue;
            }
            let picked: Vec<&JoinOption> = a.options.iter().filter_map(|k| step.option(k)).collect();
            if picked.is_empty() {
                continue;
            }
            let answer: Vec<String> = picked
                .iter()
                .map(|o| {
                    let boxes: Vec<String> = o.ask.iter().filter_map(|f| said(a, f)).collect();
                    if boxes.is_empty() { o.label.clone() } else { format!("{} ({})", o.label, boxes.join(", ")) }
                })
                .collect();
            let answer = answer.join(", ");
            if picked.iter().all(|o| o.roles.is_empty()) {
                out.push(labelled(&step.question, &answer));
            } else {
                out.push(answer);
            }
        }
        out
    }

    /// Answers read off the roles someone already holds (e.g. from Discord's
    /// onboarding), so the flow starts with them picked.
    pub fn guess(&self, held: &BTreeSet<String>) -> Answers {
        let mut out = Answers::default();
        for step in self.steps.iter().filter(|s| !s.options.is_empty()) {
            let fits = |o: &&JoinOption| !o.roles.is_empty() && o.roles.iter().all(|r| held.contains(r));
            let mut picked: Vec<String> = step.options.iter().filter(fits).map(JoinOption::key).collect();
            if !step.multi {
                picked.truncate(1);
            }
            if !picked.is_empty() {
                out.0.insert(step.key.clone(), Answer { options: picked, fields: BTreeMap::new() });
            }
        }
        out
    }
}

impl Step {
    pub fn option(&self, key: &str) -> Option<&JoinOption> {
        self.options.iter().find(|o| o.key() == key)
    }

    /// Whether the step is text boxes rather than options.
    pub fn is_text(&self) -> bool {
        self.options.is_empty()
    }

    /// The text boxes this answer needs filled: the step's own, or those of the options picked.
    pub fn boxes<'a>(&'a self, a: &'a Answer) -> Vec<&'a Field> {
        if self.is_text() {
            return self.fields.iter().collect();
        }
        a.options.iter().filter_map(|k| self.option(k)).flat_map(|o| &o.ask).collect()
    }

    /// Whether an answer is a complete one for this step.
    pub fn accepts(&self, a: &Answer) -> bool {
        let known = a.options.iter().all(|k| self.option(k).is_some());
        let mut unique = BTreeSet::new();
        let count_ok = match (self.is_text(), self.multi) {
            (true, _) => a.options.is_empty(),
            (false, false) => a.options.len() == 1,
            (false, true) => self.optional || !a.options.is_empty(),
        };
        let filled =
            self.boxes(a).iter().all(|f| !f.required || a.fields.get(&f.key).is_some_and(|v| !v.trim().is_empty()));
        known && count_ok && filled && a.options.iter().all(|k| unique.insert(k))
    }
}

/// Role changes for someone whose finished answers come to `out`: what to
/// add, and which roles the flow gave that no longer apply. Only roles the
/// flow's options give are ever taken; `keep` (e.g. roles partner sync
/// gives) never are.
pub fn role_changes(
    join: &Join,
    out: &Outcome,
    held: &BTreeSet<String>,
    keep: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut add: BTreeSet<String> = out.give.difference(held).cloned().collect();
    if let Some(a) = &join.access_role
        && !held.contains(a)
    {
        add.insert(a.clone());
    }
    let remove = join
        .option_roles()
        .into_iter()
        .filter(|r| held.contains(r) && !out.give.contains(r) && !keep.contains(r))
        .filter(|r| join.access_role.as_ref() != Some(r))
        .collect();
    (add, remove)
}

/// The channel gate, as permissions: @everyone loses View Channel, the access
/// role gains it, and each open channel lets @everyone view it. Channels
/// hidden from @everyone already stay hidden.
pub fn gate(snap: &mut Snapshot, join: &Join) {
    let Some(access) = join.access_role.as_deref().filter(|_| join.gate) else { return };
    for r in snap.roles.roles.iter_mut() {
        if r.key == "everyone" {
            r.permissions.retain(|p| p != VIEW);
        } else if r.key == access && !r.permissions.iter().any(|p| p == VIEW) {
            r.permissions.push(VIEW.into());
        }
    }
    let channels =
        snap.categories.iter_mut().flat_map(|c| c.channels.iter_mut()).chain(snap.top_level.channels.iter_mut());
    for ch in channels.filter(|c| join.open_channels.contains(&c.key)) {
        match ch.overwrites.iter_mut().find(|o| o.role.as_deref() == Some("everyone")) {
            Some(o) => {
                o.deny.retain(|p| p != VIEW);
                if !o.allow.iter().any(|p| p == VIEW) {
                    o.allow.push(VIEW.into());
                }
            }
            None => ch.overwrites.push(Overwrite {
                role: Some("everyone".into()),
                allow: vec![VIEW.into()],
                ..Default::default()
            }),
        }
    }
}

/// "MM-DD" as (month, day).
pub fn month_day(s: &str) -> Option<(u32, u32)> {
    let (m, d) = s.split_once('-')?;
    let (m, d): (u32, u32) = (m.parse().ok()?, d.parse().ok()?);
    let days = [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    ((1..=12).contains(&m) && (1..=days[m as usize - 1]).contains(&d)).then_some((m, d))
}

/// Steps in an order that ends at a step reachable twice, if the steps loop.
pub fn cycle(join: &Join) -> Option<Vec<String>> {
    fn visit<'a>(
        join: &'a Join,
        key: &'a str,
        path: &mut Vec<&'a str>,
        done: &mut BTreeSet<&'a str>,
    ) -> Option<Vec<String>> {
        if let Some(i) = path.iter().position(|k| *k == key) {
            let mut c: Vec<String> = path[i..].iter().map(|s| s.to_string()).collect();
            c.push(key.to_owned());
            return Some(c);
        }
        if !done.insert(key) {
            return None;
        }
        let step = join.step(key)?;
        path.push(key);
        for n in successors(step) {
            if let Some(c) = visit(join, n, path, done) {
                return Some(c);
            }
        }
        path.pop();
        None
    }
    let mut done = BTreeSet::new();
    join.steps.iter().find_map(|s| visit(join, &s.key, &mut Vec::new(), &mut done))
}

/// Steps reachable from this one in one move.
pub fn successors(step: &Step) -> Vec<&str> {
    let mut out: Vec<&str> = step.options.iter().filter_map(|o| o.next.as_deref()).collect();
    let every_option_says = !step.options.is_empty() && !step.multi && step.options.iter().all(|o| o.next.is_some());
    if !every_option_says {
        out.extend(step.next.as_deref());
    }
    out.retain(|n| *n != END);
    out.dedup();
    out
}

/// Steps the first can't lead to.
pub fn unreachable(join: &Join) -> Vec<String> {
    let mut seen = BTreeSet::new();
    let mut todo: Vec<&str> = join.first().into_iter().collect();
    while let Some(k) = todo.pop() {
        if !seen.insert(k) {
            continue;
        }
        if let Some(s) = join.step(k) {
            todo.extend(successors(s));
        }
    }
    join.steps.iter().filter(|s| !seen.contains(s.key.as_str())).map(|s| s.key.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Placement and work experience asked only on some years' path.
    fn experience_flow() -> Join {
        toml::from_str(
            r#"
[[step]]
key = "year"
question = "Year?"
[[step.option]]
key = "year-3"
label = "Year 3"
next = "placement-done"
[[step.option]]
key = "staff"
label = "Staff"
[[step]]
key = "placement-done"
question = "Placement?"
next = "experience"
[[step.option]]
key = "yes"
label = "Yes"
[[step.option.ask]]
key = "placement_company"
label = "Where was it?"
required = false
[[step.option]]
key = "no"
label = "No"
[[step]]
key = "experience"
question = "Other work?"
[[step.option]]
key = "yes"
label = "Yes"
[[step.option.ask]]
key = "experience"
label = "Where?"
required = false
[[step.option]]
key = "no"
label = "Not yet"
"#,
        )
        .unwrap()
    }

    fn choose(a: &mut Answers, step: &str, opt: &str, field: Option<(&str, &str)>) {
        let e = a.0.entry(step.to_owned()).or_default();
        e.options = vec![opt.to_owned()];
        if let Some((k, v)) = field {
            e.fields.insert(k.to_owned(), v.to_owned());
        }
    }

    #[test]
    fn placement_and_experience_are_kept_shown_and_cleared_off_the_path() {
        let j = experience_flow();
        let mut a = Answers::default();
        choose(&mut a, "year", "year-3", None);
        choose(&mut a, "placement-done", "yes", Some(("placement_company", "Arm")));
        choose(&mut a, "experience", "yes", Some(("experience", "Graphcore, Imagination")));
        let out = j.outcome(&a).unwrap();
        assert_eq!(out.fields["placement_company"], "Arm");
        assert_eq!(out.answers.0["experience"].fields["experience"], "Graphcore, Imagination", "stored");
        let said = j.said(&out.answers);
        assert_eq!(said[1], ("Placement?".to_owned(), "Yes, Where was it?: Arm".to_owned()));
        assert_eq!(said[2], ("Other work?".to_owned(), "Yes, Where?: Graphcore, Imagination".to_owned()));

        // No placement after all: its company goes, the rest stays.
        let mut b = out.answers.clone();
        choose(&mut b, "placement-done", "no", None);
        let out = j.outcome(&b).unwrap();
        assert!(!out.fields.contains_key("placement_company") && out.fields.contains_key("experience"));
        assert!(out.answers.0["placement-done"].fields.is_empty());

        // Staff now: neither question is on their path, so neither answer is kept.
        let mut c = out.answers.clone();
        choose(&mut c, "year", "staff", None);
        let out = j.outcome(&c).unwrap();
        assert_eq!(out.answers.0.keys().collect::<Vec<_>>(), ["year"]);
        assert!(out.fields.is_empty());
        assert_eq!(j.said(&c).len(), 1, "what's off the path isn't shown either");
    }

    #[test]
    fn the_summary_reads_on_its_own_in_one_line() {
        let mut j = experience_flow();
        j.steps[0].options[0].roles = vec!["year-3".into()];
        j.steps.push(Step {
            key: "name".into(),
            question: "Name?".into(),
            fields: vec![
                Field { key: NAME.into(), label: "Name".into(), ..Field::default() },
                Field { key: "pronouns".into(), label: "Pronouns".into(), required: false, ..Field::default() },
            ],
            ..Step::default()
        });
        j.steps[2].next = Some("name".into());
        let mut a = Answers::default();
        choose(&mut a, "year", "year-3", None);
        choose(&mut a, "placement-done", "yes", Some(("placement_company", "Arm")));
        choose(&mut a, "experience", "no", None);
        a.0.entry("name".into())
            .or_default()
            .fields
            .extend([("name".into(), "Ada".into()), ("pronouns".into(), "she/her".into())]);
        assert_eq!(
            j.summary(&a),
            ["Year 3", "Placement? Yes (Arm)", "Other work? Not yet", "Pronouns: she/her"],
            "a role-giving pick alone, a yes/no after its question, the name left out"
        );
        choose(&mut a, "year", "staff", None);
        assert!(j.summary(&a)[0].starts_with("Year? Staff"), "off the path: dropped; {:?}", j.summary(&a));
        assert_eq!(j.summary(&a).len(), 1);
    }

    fn flow() -> Join {
        toml::from_str(
            r#"
access_role = "access"
[[step]]
key = "uni"
question = "Which university?"
[[step.option]]
label = "Ours"
roles = ["ours"]
next = "year"
[[step.option]]
label = "Elsewhere"
roles = ["other-uni"]
next = "year"
[[step.option.ask]]
key = "uni_name"
label = "Which one?"
[[step.option]]
label = "Working"
roles = ["industry"]
next = "name"
[[step.option.ask]]
key = "company"
label = "Where?"
required = false

[[step]]
key = "year"
question = "Which year?"
yearly = true
next = "extra"
[[step.option]]
label = "Year 1"
roles = ["y1"]
[[step.option]]
label = "Graduated"
roles = ["alumni"]
next = "name"
[[step.option.ask]]
key = "company"
label = "Where do you work?"
required = false

[[step]]
key = "extra"
question = "Anything else?"
multi = true
optional = true
next = "name"
[[step.option]]
label = "Mentor"
roles = ["mentor"]
[[step.option]]
label = "Hardware fan"
roles = ["fan"]

[[step]]
key = "name"
question = "What should we call you?"
[[step.field]]
key = "name"
label = "Name"
[[step.field]]
key = "surname"
label = "Surname"
required = false
"#,
        )
        .unwrap()
    }

    fn pick(a: &mut Answers, step: &str, options: &[&str]) {
        a.0.entry(step.into()).or_default().options = options.iter().map(|s| s.to_string()).collect();
    }
    fn fill(a: &mut Answers, step: &str, field: &str, v: &str) {
        a.0.entry(step.into()).or_default().fields.insert(field.into(), v.into());
    }
    fn set(v: &[&str]) -> BTreeSet<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn walking_follows_the_branch_taken() {
        let j = flow();
        let mut a = Answers::default();
        assert_eq!(j.walk(&a).at.as_deref(), Some("uni"));
        pick(&mut a, "uni", &["elsewhere"]);
        assert_eq!(j.walk(&a).at.as_deref(), Some("uni"), "the option's question isn't answered yet");
        fill(&mut a, "uni", "uni_name", "Leeds");
        assert_eq!(j.walk(&a).at.as_deref(), Some("year"));
        pick(&mut a, "year", &["year-1"]);
        assert_eq!(j.walk(&a).at.as_deref(), Some("extra"));
        pick(&mut a, "extra", &[]);
        assert_eq!(j.walk(&a).at.as_deref(), Some("name"), "an optional multi step may be left empty");
        fill(&mut a, "name", "name", "Ada");
        let w = j.walk(&a);
        assert_eq!((w.done, w.at), (vec!["uni".into(), "year".into(), "extra".into(), "name".into()], None));
    }

    #[test]
    fn answers_give_their_roles_and_drop_the_old_branch() {
        let j = flow();
        let mut a = Answers::default();
        pick(&mut a, "uni", &["ours"]);
        pick(&mut a, "year", &["graduated"]);
        fill(&mut a, "year", "company", "Arm");
        fill(&mut a, "name", "name", "Ada");
        // Left over from an earlier time through: not on the path any more.
        pick(&mut a, "extra", &["hardware-fan"]);
        fill(&mut a, "uni", "uni_name", "Leeds");
        let out = j.outcome(&a).unwrap();
        assert_eq!(out.give, set(&["ours", "alumni"]));
        assert_eq!(out.fields.get("company").map(String::as_str), Some("Arm"));
        assert!(!out.answers.0.contains_key("extra"), "a step off the path is forgotten");
        assert!(!out.answers.0["uni"].fields.contains_key("uni_name"), "a box for an option not picked is forgotten");

        // They used to be a first year with a fan role: those go, others stay.
        let held = set(&["ours", "y1", "fan", "committee", "sharc"]);
        let (add, remove) = role_changes(&j, &out, &held, &set(&[]));
        assert_eq!(add, set(&["alumni", "access"]));
        assert_eq!(remove, set(&["y1", "fan"]), "only roles the flow gives are taken");
    }

    #[test]
    fn several_picks_give_all_their_roles_and_unpicking_takes_them() {
        let j = flow();
        let mut a = Answers::default();
        pick(&mut a, "uni", &["ours"]);
        pick(&mut a, "year", &["year-1"]);
        pick(&mut a, "extra", &["mentor", "hardware-fan"]);
        fill(&mut a, "name", "name", "Ada");
        let out = j.outcome(&a).unwrap();
        assert_eq!(out.give, set(&["ours", "y1", "mentor", "fan"]));
        let held = set(&["ours", "y1", "mentor", "fan", "access"]);
        let (add, remove) = role_changes(&j, &out, &held, &set(&[]));
        assert!(add.is_empty() && remove.is_empty(), "nothing changed");
        pick(&mut a, "extra", &["hardware-fan"]);
        let (_, remove) = role_changes(&j, &j.outcome(&a).unwrap(), &held, &set(&[]));
        assert_eq!(remove, set(&["mentor"]));
        let (_, remove) = role_changes(&j, &j.outcome(&a).unwrap(), &held, &set(&["mentor"]));
        assert!(remove.is_empty(), "unless something else gives it");
    }

    #[test]
    fn unfinished_answers_give_nothing() {
        let j = flow();
        let mut a = Answers::default();
        pick(&mut a, "uni", &["ours", "working"]);
        assert_eq!(j.outcome(&a), None, "two picks on a single-choice step");
        pick(&mut a, "uni", &["nope"]);
        assert_eq!(j.outcome(&a), None, "an option that isn't there");
    }

    #[test]
    fn roles_held_become_the_starting_answers() {
        let j = flow();
        let a = j.guess(&set(&["other-uni", "y1", "fan"]));
        assert_eq!(a.0["uni"].options, ["elsewhere"]);
        assert_eq!(a.0["year"].options, ["year-1"]);
        assert_eq!(a.0["extra"].options, ["hardware-fan"]);
    }

    #[test]
    fn loops_and_dead_ends_are_found() {
        let mut j = flow();
        assert_eq!(cycle(&j), None);
        assert!(unreachable(&j).is_empty());
        j.steps[3].next = Some("uni".into());
        assert_eq!(cycle(&j), Some(vec!["uni".into(), "year".into(), "name".into(), "uni".into()]));
        j.steps[3].next = None;
        j.steps[0].options.iter_mut().for_each(|o| o.next = Some("name".into()));
        assert_eq!(unreachable(&j), ["year", "extra"]);
    }

    #[test]
    fn the_gate_is_permissions() {
        use crate::schema::{Category, CategoryFile, Channel, Role};
        let ow = |role: &str, allow: &[&str], deny: &[&str]| Overwrite {
            role: Some(role.into()),
            allow: allow.iter().map(|s| s.to_string()).collect(),
            deny: deny.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };
        let ch = |k: &str, ows: Vec<Overwrite>| Channel {
            key: k.into(),
            name: k.into(),
            overwrites: ows,
            ..Default::default()
        };
        let mut snap = Snapshot::default();
        snap.roles.roles = vec![
            Role { key: "access".into(), name: "Access".into(), ..Default::default() },
            Role {
                key: "everyone".into(),
                name: "@everyone".into(),
                permissions: vec![VIEW.into(), "SEND_MESSAGES".into()],
                ..Default::default()
            },
        ];
        snap.categories = vec![
            CategoryFile {
                category: Category { key: "meta".into(), name: "Meta".into(), overwrites: vec![] },
                channels: vec![ch("welcome", vec![ow("everyone", &[], &["SEND_MESSAGES"])]), ch("general", vec![])],
            },
            CategoryFile {
                category: Category {
                    key: "staff".into(),
                    name: "Staff".into(),
                    overwrites: vec![ow("everyone", &[], &[VIEW])],
                },
                channels: vec![ch("secret", vec![ow("everyone", &[], &[VIEW])])],
            },
        ];
        snap.top_level.channels = vec![ch("rules", vec![ow("everyone", &[], &[VIEW])])];
        let before = crate::public_channels(&snap);
        assert_eq!(before, ["welcome", "general"]);
        let mut j = flow();
        j.gate = true;
        j.open_channels = vec!["welcome".into(), "rules".into()];
        gate(&mut snap, &j);
        assert_eq!(crate::public_channels(&snap), ["welcome", "rules"], "only the open channels");
        assert!(snap.role("access").unwrap().permissions.contains(&VIEW.to_string()));
        assert!(!snap.role("everyone").unwrap().permissions.contains(&VIEW.to_string()));
        assert_eq!(snap.channel("welcome").unwrap().overwrites, vec![ow("everyone", &[VIEW], &["SEND_MESSAGES"])]);
        assert!(!snap.sees_private("access"), "a hidden category stays hidden from the access role");
        let again = snap.clone();
        gate(&mut snap, &j);
        assert_eq!(snap, again, "gating twice changes nothing");
    }
}
