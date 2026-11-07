//! Events: a Discord scheduled event (in a voice or stage channel if "Where"
//! names one, else somewhere outside Discord), announced in the events
//! channel, with reminders a day and an hour before. Admins make them from
//! `/event` at once; a member's suggestion is made when an admin accepts it.

use anyhow::Result;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::{Method, Rest};
use chrono::{Days, NaiveDate, NaiveDateTime, NaiveTime, TimeZone};
use chrono_tz::Tz;
use serde_json::{Value, json};

use crate::{Answer, Host, Refusal, now};

/// The configured time zone, or a message for the admin saying what's wrong.
pub fn tz(cfg: &Config) -> Result<Tz, Refusal> {
    let name = &cfg.bot.timezone;
    name.parse().map_err(|_| Refusal::from(format!("bot.toml `timezone = \"{name}\"` isn't a time zone the bot knows (use a name like Europe/Paris), so it can't place events. Fix it and try again.")))
}

/// Why a start time can't be used.
#[derive(Debug, PartialEq, Eq)]
pub enum BadTime {
    Unreadable,
    /// In the hour the clocks skip when they go forward.
    Skipped,
}

/// A local date and time ("2026-11-12 18:00", "12/11/2026 18:00", "tomorrow
/// 18:00") in the configured time zone, as a Unix time. In the hour that
/// happens twice when the clocks go back, the first.
pub fn parse_time(s: &str, tz: Tz, today: NaiveDate) -> Result<i64, BadTime> {
    let local = local_time(s, today).ok_or(BadTime::Unreadable)?;
    at(tz, local).ok_or(BadTime::Skipped)
}

/// A start time as a member reads it back and the bot reads it again
/// later: "2026-11-12 18:00", local time.
pub fn local_text(tz: Tz, start: i64) -> String {
    chrono::DateTime::from_timestamp(start, 0)
        .map(|t| t.with_timezone(&tz).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_default()
}

/// A local time as a Unix time, if it exists there.
fn at(tz: Tz, local: NaiveDateTime) -> Option<i64> {
    tz.from_local_datetime(&local).earliest().map(|dt| dt.timestamp())
}

/// The same local time a day earlier (not always 24 hours, across a clock change).
pub fn day_before(tz: Tz, start: i64) -> Option<i64> {
    let local = chrono::DateTime::from_timestamp(start, 0)?.with_timezone(&tz).naive_local();
    at(tz, local.checked_sub_days(Days::new(1))?)
}

fn local_time(s: &str, today: NaiveDate) -> Option<NaiveDateTime> {
    let s = s.trim().to_lowercase();
    let relative = |rest: &str, add: u64| {
        let t = NaiveTime::parse_from_str(rest.trim(), "%H:%M").ok()?;
        Some(today.checked_add_days(Days::new(add))?.and_time(t))
    };
    if let Some(rest) = s.strip_prefix("today") {
        relative(rest, 0)
    } else if let Some(rest) = s.strip_prefix("tomorrow") {
        relative(rest, 1)
    } else {
        ["%Y-%m-%d %H:%M", "%d/%m/%Y %H:%M", "%Y-%m-%dT%H:%M", "%d-%m-%Y %H:%M"]
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(&s, f).ok())
    }
}

/// Shortest and longest events, in minutes.
pub const MINUTES: std::ops::RangeInclusive<i64> = 5..=24 * 60;

/// "Length in minutes" as given: blank is an hour; anything else must be a
/// whole number of minutes in [`MINUTES`]. `Err` says what to give instead.
pub fn minutes(s: &str) -> Result<i64, Refusal> {
    let s = s.trim().trim_end_matches("min").trim_end_matches("mins").trim();
    if s.is_empty() {
        return Ok(60);
    }
    match s.parse::<i64>() {
        Ok(n) if MINUTES.contains(&n) => Ok(n),
        _ => Err(format!(
            "Give the length as a number of minutes from {} to {} (24 hours), or leave it blank for an hour.",
            MINUTES.start(),
            MINUTES.end()
        )
        .into()),
    }
}

/// Where an event happens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Place {
    /// In one of the server's voice channels, or stage channels (`stage`).
    Room { id: u64, stage: bool },
    /// Anywhere else: a room, a link.
    Elsewhere(String),
}

/// "Where" as given: a voice or stage channel here (mentioned, or by its
/// name, with or without `#`), else as written. `rooms`: the server's voice
/// and stage channels as (id, name, stage).
pub fn place(text: &str, rooms: &[(u64, String, bool)]) -> Place {
    let t = text.trim();
    let mentioned = t.strip_prefix("<#").and_then(|r| r.strip_suffix('>')).and_then(|id| id.parse::<u64>().ok());
    let found = match mentioned {
        Some(id) => rooms.iter().find(|(r, _, _)| *r == id),
        None => {
            let name = t.trim_start_matches('#').trim();
            rooms.iter().find(|(_, n, _)| n.eq_ignore_ascii_case(name))
        }
    };
    match found {
        Some((id, _, stage)) => Place::Room { id: *id, stage: *stage },
        None => Place::Elsewhere(t.to_owned()),
    }
}

/// Discord's channel types for voice and stage channels.
const VOICE: u64 = 2;
const STAGE: u64 = 13;

/// The server's voice and stage channels, for [`place`]; none if Discord doesn't say.
async fn rooms<H: Host>(h: &H) -> Vec<(u64, String, bool)> {
    let channels = match h.rest().call(Method::Get, &format!("guilds/{}/channels", h.guild()), None).await {
        Ok(Value::Array(c)) => c,
        Ok(_) => Vec::new(),
        Err(e) => {
            tracing::warn!("listing channels failed; the event's place is taken as written: {e:#}");
            Vec::new()
        }
    };
    channels
        .iter()
        .filter_map(|c| {
            let kind = c["type"].as_u64()?;
            let id = chipsoc_discord::wire::id(&c["id"])?;
            (kind == VOICE || kind == STAGE).then(|| (id.0, chipsoc_discord::wire::s(&c["name"]), kind == STAGE))
        })
        .collect()
}

/// An event to make: checked already (see [`check`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEvent {
    pub title: String,
    /// Unix time.
    pub start: i64,
    pub minutes: i64,
    /// "Where", as written.
    pub location: String,
    pub description: String,
}

/// The questions an event suggestion is kept under.
pub const EVENT_TITLE: &str = "Title";
pub const EVENT_START: &str = "Starts";
pub const EVENT_LENGTH: &str = "Length";
pub const EVENT_WHERE: &str = "Where";
pub const EVENT_DETAILS: &str = "Details";

impl NewEvent {
    /// As kept in the review queue: the start in local time, as it's read back.
    pub fn answers(&self, tz: Tz) -> Vec<Answer> {
        let a = |q: &str, v: String| Answer::new(q, Some(&v));
        vec![
            a(EVENT_TITLE, self.title.clone()),
            a(EVENT_START, local_text(tz, self.start)),
            a(EVENT_LENGTH, format!("{} min", self.minutes)),
            a(EVENT_WHERE, self.location.clone()),
            a(EVENT_DETAILS, self.description.clone()),
        ]
    }

    /// From the review queue (see [`NewEvent::answers`]); `None` if its start can't be read.
    pub fn from_answers(answers: &[Answer], tz: Tz) -> Option<Self> {
        let get = |q: &str| answers.iter().find(|a| a.question == q).map(|a| a.answer.clone()).unwrap_or_default();
        let start = parse_time(&get(EVENT_START), tz, NaiveDate::MIN).ok()?;
        Some(Self {
            title: get(EVENT_TITLE),
            start,
            minutes: minutes(&get(EVENT_LENGTH)).unwrap_or(60),
            location: get(EVENT_WHERE),
            description: get(EVENT_DETAILS),
        })
    }
}

/// The form's answers as an event, or what to tell whoever sent it.
pub fn check(
    tz: Tz,
    today: NaiveDate,
    title: &str,
    start: &str,
    length: &str,
    location: &str,
    description: &str,
) -> Result<NewEvent, Refusal> {
    let at = match parse_time(start, tz, today) {
        Ok(at) => at,
        Err(BadTime::Unreadable) => {
            let why = format!("Couldn't read the start time `{start}`. Try `2026-11-12 18:00` or `tomorrow 18:00`.");
            return Err(why.into());
        }
        Err(BadTime::Skipped) => {
            return Err(
                format!("`{start}` doesn't happen in {tz}: the clocks go forward then. Pick another time.").into()
            );
        }
    };
    if at <= now() {
        return Err("That start time is in the past.".into());
    }
    let minutes = minutes(length)?;
    Ok(NewEvent {
        title: title.trim().to_owned(),
        start: at,
        minutes,
        location: location.trim().to_owned(),
        description: description.trim().to_owned(),
    })
}

/// An event made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Made {
    /// The scheduled event's id, if Discord said it.
    pub id: Option<Snowflake>,
    /// Its page in Discord, if Discord said its id.
    pub link: Option<String>,
    /// The channel it was announced in, if the events channel is set up.
    pub announced: Option<Snowflake>,
}

impl Made {
    /// What making it did, the same for an admin's event and an accepted
    /// suggestion: "Put **Social** on the calendar for <time>, announced in #events."
    pub fn said(&self, title: &str, start: i64) -> crate::Said {
        let mut said = crate::Said::new().text("Put ").strong(title).text(" on the calendar for ").time(start);
        if let Some(c) = self.announced {
            said = said.text(", announced in ").channel(c);
        }
        said.text(".")
    }
}

/// An RFC 3339 time for Discord.
fn iso(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|t| t.to_rfc3339()).unwrap_or_default()
}

/// Make the scheduled event, announce it and save its reminders.
/// `suggested_by`: the member whose suggestion it was, credited in the post.
pub async fn create<H: Host>(h: &H, e: &NewEvent, suggested_by: Option<Snowflake>) -> Result<Made> {
    let cfg = h.cfg();
    let tz = tz(&cfg)?;
    let place = place(&e.location, &rooms(h).await);
    let end = e.start + e.minutes * 60;
    let mut body = json!({
        "name": crate::truncate(&e.title, 100),
        "privacy_level": 2,
        "scheduled_start_time": iso(e.start),
        "scheduled_end_time": iso(end),
    });
    match &place {
        Place::Room { id, stage } => {
            body["entity_type"] = json!(if *stage { 1 } else { 2 });
            body["channel_id"] = json!(id.to_string());
        }
        Place::Elsewhere(at) => {
            body["entity_type"] = json!(3);
            body["entity_metadata"] = json!({ "location": crate::truncate(at, 100) });
        }
    }
    if !e.description.is_empty() {
        body["description"] = json!(crate::truncate(&e.description, 1000));
    }
    let made = h.rest().call(Method::Post, &format!("guilds/{}/scheduled-events", h.guild()), Some(body)).await?;
    let event = chipsoc_discord::wire::id(&made["id"]);
    let link = event.map(|id| format!("https://discord.com/events/{}/{id}", h.guild()));
    // A member's words, in the bot's post, are shown as written; an admin's may use markdown.
    let shown = |s: &str| if suggested_by.is_some() { crate::fmt::escape(s) } else { s.to_owned() };
    let location = match &place {
        Place::Room { id, .. } => format!("<#{id}>"),
        Place::Elsewhere(at) => shown(at),
    };
    let (title, start, minutes) = (shown(&e.title), e.start, e.minutes);
    let shown_link = link.as_ref().map(|l| format!("\n{l}")).unwrap_or_default();
    let mut post = format!("📅 **{title}**\n🕒 <t:{start}:F> (<t:{start}:R>) · {minutes} min\n📍 {location}");
    if !e.description.is_empty() {
        post.push_str(&format!("\n\n{}", shown(&e.description)));
    }
    if let Some(who) = suggested_by {
        post.push_str(&format!("\n💡 Suggested by <@{who}>"));
    }
    post.push_str(&shown_link);
    let channel = cfg.bot.events.as_ref().and_then(|ev| cfg.desired.lock.channel(&ev.channel));
    if let Some(ch) = channel {
        if let Err(e) = crate::say(h, ch, &post).await {
            tracing::warn!("announcing an event failed: {e:#}");
        }
        let event_id = event.map(|e| e.to_string());
        let tomorrow = day_before(tz, start).unwrap_or(start - 24 * 3600);
        for due in [tomorrow, start - 3600] {
            if due > now() {
                let text = format!("⏰ **{title}** starts <t:{start}:R> (<t:{start}:F>) · {location}{shown_link}");
                if let Err(e) = h.db().add_reminder(&ch.to_string(), due, &text, event_id.as_deref()).await {
                    tracing::warn!("saving a reminder failed: {e:#}");
                }
            }
        }
    }
    Ok(Made { id: event, link, announced: channel })
}

/// Why `e` needn't be suggested: the same event is waiting for review already.
pub async fn already_suggested<H: Host>(h: &H, e: &NewEvent) -> Option<String> {
    let tz = tz(&h.cfg()).ok()?;
    let waiting = h.db().pending_applications().await.unwrap_or_default();
    let start = local_text(tz, e.start);
    let same = |a: &chipsoc_store::Application| {
        a.kind == chipsoc_store::AppKind::Event
            && a.target.eq_ignore_ascii_case(&e.title)
            && Answer::parse(&a.answers).iter().any(|x| x.question == EVENT_START && x.answer == start)
    };
    waiting.iter().any(same).then(|| {
        format!(
            "Someone has already suggested **{}** at that time; it's waiting for an admin to look at it.",
            crate::fmt::escape(&e.title)
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_times_across_dst_and_relative_days() {
        let london = chrono_tz::Europe::London;
        let today = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        assert_eq!(parse_time("2026-11-12 18:00", london, today), Ok(1_794_506_400));
        assert_eq!(parse_time("12/07/2026 18:00", london, today), Ok(1_783_875_600));
        assert_eq!(parse_time("Tomorrow 18:00", london, today), parse_time("2026-11-05 18:00", london, today));
        assert_eq!(parse_time("not a date", london, today), Err(BadTime::Unreadable));
        assert_eq!(parse_time("2027-03-28 01:30", london, today), Err(BadTime::Skipped), "clocks go forward");
        let twice = parse_time("2026-10-25 01:30", london, today).unwrap();
        assert_eq!(twice, 1_792_888_200, "the first 01:30 (BST) when the clocks go back");
        assert_eq!(local_text(london, 1_794_506_400), "2026-11-12 18:00");
    }

    #[test]
    fn a_length_is_minutes_or_refused() {
        assert_eq!(minutes(""), Ok(60));
        assert_eq!(minutes(" 90 "), Ok(90));
        assert_eq!(minutes("45 min"), Ok(45));
        for bad in ["an hour", "1.5", "0", "3", "2000", "-30"] {
            assert!(minutes(bad).unwrap_err().contains("number of minutes"), "{bad}");
        }
    }

    #[test]
    fn where_finds_a_voice_or_stage_channel_else_is_as_written() {
        let rooms = [(10, "Lounge".to_owned(), false), (11, "talks".to_owned(), true)];
        assert_eq!(place("<#10>", &rooms), Place::Room { id: 10, stage: false });
        assert_eq!(place("#lounge", &rooms), Place::Room { id: 10, stage: false });
        assert_eq!(place(" Talks ", &rooms), Place::Room { id: 11, stage: true });
        assert_eq!(place("<#99>", &rooms), Place::Elsewhere("<#99>".into()), "not a voice channel here");
        assert_eq!(place("Room 2.15", &rooms), Place::Elsewhere("Room 2.15".into()));
    }

    #[test]
    fn the_day_before_reminder_keeps_the_local_time() {
        let london = chrono_tz::Europe::London;
        let today = NaiveDate::from_ymd_opt(2026, 10, 1).unwrap();
        // The clocks go back on 25 October: 18:00 on the 25th is 25 hours after 18:00 on the 24th.
        let start = parse_time("2026-10-25 18:00", london, today).unwrap();
        assert_eq!(day_before(london, start), parse_time("2026-10-24 18:00", london, today).ok());
        assert_eq!(start - day_before(london, start).unwrap(), 25 * 3600);
    }

    #[test]
    fn a_suggestion_keeps_its_time_as_a_local_time_it_reads_back() {
        let london = chrono_tz::Europe::London;
        let e = NewEvent {
            title: "Social".into(),
            start: 1_794_506_400,
            minutes: 90,
            location: "Room 2.15".into(),
            description: "Pizza".into(),
        };
        let answers = e.answers(london);
        assert_eq!(answers[1].answer, "2026-11-12 18:00");
        assert_eq!(NewEvent::from_answers(&answers, london), Some(e));
        let today = NaiveDate::from_ymd_opt(2026, 11, 4).unwrap();
        let refused = check(london, today, "T", "2020-01-01 10:00", "", "x", "").unwrap_err();
        assert_eq!(refused, "That start time is in the past.");
        assert!(check(london, today, "T", "soon", "", "x", "").unwrap_err().contains("Couldn't read"));
    }
}
