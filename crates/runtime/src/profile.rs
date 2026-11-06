//! Profiles: members give their name (and where they work, for some roles) and
//! the bot sets their server nickname to it. The start of their surname is
//! added only to tell apart people with the same name: "Ada L.", "Ada Lo.".

use std::collections::HashMap;

use chipsoc_admin::join::Unnameable;
use chipsoc_store::Profile;
use twilight_model::application::interaction::Interaction;
use twilight_model::channel::message::component::{ButtonStyle, TextInputStyle};
use twilight_model::gateway::payload::incoming::MemberUpdate;
use twilight_model::guild::MemberFlags;
use twilight_model::id::Id;
use twilight_model::id::marker::{RoleMarker, UserMarker};

use crate::apps::invoker;
use crate::commands::CustomId;
use crate::limits::Kind;
use crate::{Ctx, members, now, ui};

/// Discord's limit on nicknames.
const MAX_NICK: usize = 32;

/// After someone finishes onboarding, DM them once with the profile button;
/// later, ask once where they work when they take a role that needs it.
pub async fn on_member_update(ctx: &Ctx, m: &MemberUpdate) {
    let cfg = ctx.cfg();
    let Some(conf) = &cfg.bot.profile else { return };
    if cfg.join.is_some() {
        return; // The join flow asks for the name (and company) itself, and DMs only when told to.
    }
    let flags = m.flags.unwrap_or(MemberFlags::empty());
    let onboarded = flags.contains(MemberFlags::COMPLETED_ONBOARDING);
    let onboarding = flags.contains(MemberFlags::STARTED_ONBOARDING) && !onboarded;
    if m.user.bot || m.pending || onboarding {
        return;
    }
    if onboarded && first(ctx, m.user.id, "profile").await {
        let text = format!("Welcome to {}! 👋 Tell us your name so people know who you are:", ctx.name());
        return dm_button(ctx, m.user.id, &text).await;
    }
    let held = conf.company_roles.iter().filter(|k| ctx.role(k).is_some_and(|r| m.roles.contains(&r)));
    let held: Vec<&String> = held.collect();
    if held.is_empty() || has_company(ctx, m.user.id).await {
        return;
    }
    for key in held {
        if first(ctx, m.user.id, &format!("company:{key}")).await {
            let text =
                format!("You picked **{}**: tell us where you're working (or leave it blank).", ctx.role_name(key));
            dm_button(ctx, m.user.id, &text).await;
        }
    }
}

/// Whether this is the first time we'd prompt them about `kind` (and note that we have).
async fn first(ctx: &Ctx, user: Id<UserMarker>, kind: &str) -> bool {
    match ctx.db().first_prompt(&user.to_string(), kind, now()).await {
        Ok(first) => first,
        Err(e) => {
            tracing::warn!(?e, "checking a prompt failed");
            false
        }
    }
}

async fn has_company(ctx: &Ctx, user: Id<UserMarker>) -> bool {
    ctx.db().profile(&user.to_string()).await.ok().flatten().is_some_and(|p| !p.company.is_empty())
}

/// DM someone with the profile button.
async fn dm_button(ctx: &Ctx, user: Id<UserMarker>, text: &str) {
    let id = CustomId::Do("profile".into()).in_dm(ctx.guild);
    let row = [ui::row(vec![ui::button(&id, "Set up your profile", ButtonStyle::Primary)])];
    if let Err(why) = ctx.dm_with(user, text, &row).await {
        tracing::info!(%user, %why, "profile DM not sent");
    }
}

async fn member_roles(ctx: &Ctx, i: &Interaction, user: Id<UserMarker>) -> Vec<Id<RoleMarker>> {
    if let Some(m) = &i.member {
        return m.roles.clone();
    }
    match ctx.get(&format!("guilds/{}/members/{user}", ctx.guild)).await {
        Ok(m) => members::roles(&m),
        Err(e) => {
            tracing::warn!(?e, "reading member roles failed; not asking where they work");
            Vec::new()
        }
    }
}

/// The company roles this member holds (they're asked where they work).
async fn company_roles(ctx: &Ctx, i: &Interaction, user: Id<UserMarker>) -> Vec<String> {
    let Some(keys) = ctx.cfg().bot.profile.as_ref().map(|p| p.company_roles.clone()) else { return Vec::new() };
    let roles = member_roles(ctx, i, user).await;
    keys.into_iter().filter(|k| ctx.role(k).is_some_and(|r| roles.contains(&r))).collect()
}

pub async fn open(ctx: &Ctx, i: &Interaction) {
    if ctx.cfg().join.is_some() {
        return crate::join::start(ctx, i, None).await;
    }
    let Some(user) = invoker(i) else { return };
    let saved = match ctx.db().profile(&user.id.to_string()).await {
        Ok(p) => p.unwrap_or_default(),
        Err(e) => {
            tracing::warn!(?e, "reading profile failed; the form starts empty");
            Profile::default()
        }
    };
    let surname = ui::input("surname", "Surname (optional)", TextInputStyle::Short, false, None);
    let why = "Only used if someone here has your name: we add the shortest bit that tells you apart, like Ada L.";
    let mut fields =
        vec![ui::input("name", "Name or nickname", TextInputStyle::Short, true, None), ui::described(surname, why)];
    if !company_roles(ctx, i, user.id).await.is_empty() {
        let q = ctx.cfg().bot.profile.as_ref().map(|p| p.company_question.clone()).unwrap_or_default();
        let company = ui::input("company", &q, TextInputStyle::Short, false, None);
        fields.push(ui::described(company, chipsoc_config::bot::COMPANY_HELP));
    }
    prefill(&mut fields, &saved);
    let form = CustomId::ProfileForm.in_dm(ctx.guild);
    ctx.respond(i, &ui::modal(&form, "Your profile", fields)).await;
}

/// Show what they entered last time.
fn prefill(fields: &mut [twilight_model::channel::message::Component], p: &Profile) {
    use twilight_model::channel::message::Component;
    for f in fields {
        if let Component::Label(l) = f
            && let Component::TextInput(t) = l.component.as_mut()
        {
            let v = match t.custom_id.as_str() {
                "name" => &p.name,
                "surname" => &p.surname,
                "company" => &p.company,
                _ => continue,
            };
            t.value = Some(v.clone()).filter(|v| !v.is_empty());
        }
    }
}

/// Ways to write a name, shortest first, with how much of the surname each
/// shows: "Ada", "Ada L.", "Ada Lo.", …, "Ada Lovelace".
fn spellings(name: &str, surname: &str) -> Vec<(usize, String)> {
    let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
    let surname = surname.split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = surname.chars().collect();
    let mut out = vec![(0, name.clone())];
    for k in (1..chars.len()).filter(|&k| chars[k - 1].is_alphanumeric()) {
        let mut start: String = chars[..k].iter().collect();
        start = start.chars().next().map(|c| c.to_uppercase().chain(start.chars().skip(1)).collect()).unwrap_or(start);
        out.push((k, format!("{name} {start}.")));
    }
    if !chars.is_empty() {
        out.push((chars.len(), format!("{name} {surname}")));
    }
    out.into_iter().map(|(k, s)| (k, s.chars().take(MAX_NICK).collect())).collect()
}

fn same(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}

/// The shortest spelling of their name that nobody else here shows (`taken`)
/// and whose start of surname no `rival` (someone else with this name) shares.
/// The name alone does only without rivals. Else the whole surname, if that's
/// free; none if even that's taken, so nobody can pass as someone else.
pub fn nickname(name: &str, surname: &str, taken: &[String], rivals: &[String]) -> Option<String> {
    let start = |s: &str, k: usize| s.trim().chars().take(k).collect::<String>().to_lowercase();
    let spellings = spellings(name, surname);
    let free = |(k, s): &&(usize, String)| {
        let apart = if *k == 0 { rivals.is_empty() } else { rivals.iter().all(|r| start(r, *k) != start(surname, *k)) };
        apart && !taken.iter().any(|t| same(t, s))
    };
    let last = spellings.last().filter(|(_, s)| !taken.iter().any(|t| same(t, s)));
    spellings.iter().find(free).or(last).map(|(_, s)| s.clone())
}

/// A name as typed, without invisible or control characters (zero-width
/// spaces, direction marks, fillers), its spaces tidied.
pub fn clean(s: &str) -> String {
    let invisible = |c: char| {
        c.is_control()
            || matches!(c,
                '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}'
                | '\u{180B}'..='\u{180F}' | '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}' | '\u{3164}' | '\u{FE00}'..='\u{FE0F}' | '\u{FEFF}' | '\u{FFA0}'
                | '\u{E0000}'..='\u{E007F}')
    };
    let kept: String = s.chars().filter(|c| !invisible(*c)).collect();
    kept.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Whether `shown` is a spelling the bot would make of this name, not one they chose.
fn is_spelling(shown: &str, name: &str, surname: &str) -> bool {
    spellings(name, surname).iter().any(|(_, s)| same(s, shown))
}

/// Their nickname, told apart from others here with the same name (none if
/// it can't be). Those others, if they set their name here too, are told
/// apart from them in turn.
pub(crate) async fn choose(ctx: &Ctx, me: Id<UserMarker>, p: &Profile) -> Result<Option<String>, ()> {
    let members = match ctx.members().await {
        Ok(m) => m,
        Err(e) => {
            // Without the member list, clashes can't be ruled out.
            tracing::warn!(?e, "listing members failed; nickname not set");
            return Err(());
        }
    };
    let named: HashMap<String, Profile> = match ctx.db().profiles_named(&p.name).await {
        Ok(v) => v.into_iter().map(|x| (x.user_id.clone(), x)).collect(),
        Err(e) => {
            tracing::warn!(?e, "reading profiles failed");
            HashMap::new()
        }
    };
    // Others here (not bots), with the name each shows.
    let others: Vec<(u64, String)> = members
        .iter()
        .filter(|m| !members::is_bot(m))
        .filter_map(|m| Some((members::user(m)?, members::shown(m))))
        .filter(|(id, _)| *id != me.get())
        .collect();
    let profile = |id: u64| named.get(&id.to_string());
    // Others with this name whose nickname the bot made, so it may change it.
    let rivals: Vec<(&(u64, String), &Profile)> = others
        .iter()
        .filter_map(|m| profile(m.0).filter(|r| is_spelling(&m.1, &r.name, &r.surname)).map(|r| (m, r)))
        .collect();
    let taken: Vec<String> = others.iter().map(|(_, shown)| shown.clone()).collect();
    let surnames: Vec<String> = rivals.iter().map(|(_, r)| r.surname.clone()).filter(|s| !s.is_empty()).collect();
    let Some(nick) = nickname(&p.name, &p.surname, &taken, &surnames) else { return Ok(None) };
    let clash = !rivals.is_empty() || taken.iter().any(|t| same(t, &p.name));
    if clash {
        for ((id, shows), r) in &rivals {
            let taken: Vec<String> =
                others.iter().filter(|(o, _)| o != id).map(|(_, s)| s.clone()).chain([nick.clone()]).collect();
            let surnames: Vec<String> = rivals
                .iter()
                .filter(|((o, _), _)| o != id)
                .map(|(_, o)| o.surname.clone())
                .chain([p.surname.clone()])
                .filter(|s| !s.is_empty())
                .collect();
            let Some(theirs) = nickname(&r.name, &r.surname, &taken, &surnames) else { continue };
            if theirs != *shows {
                ctx.log_err(set_nick(ctx, *id, &theirs).await, "telling a namesake apart");
            }
        }
    }
    Ok(Some(nick))
}

pub async fn submit(ctx: &Ctx, i: &Interaction, v: &HashMap<String, String>) {
    let Some(user) = invoker(i) else { return };
    if ctx.cfg().bot.profile.is_none() {
        // An old button or form: say so rather than leave Discord saying it failed.
        let text = if ctx.cfg().join.is_some() {
            "That form isn't used here any more: use `/profile` to update your details."
        } else {
            "Profiles aren't set up here."
        };
        return ctx.respond(i, &ui::ephemeral(text)).await;
    }
    let get = |k: &str| clean(v.get(k).map_or("", String::as_str));
    let p =
        Profile { user_id: user.id.to_string(), name: get("name"), surname: get("surname"), company: get("company") };
    if !p.name.chars().any(char::is_alphanumeric) {
        return ctx.respond(i, &ui::ephemeral("Please give a name (letters or digits).")).await;
    }
    // Reading the member list and renaming namesakes is costly: one at a time,
    // refused or not (a name clash too).
    if let Err(wait) = ctx.limits.turn(Kind::Profile, user.id.get()).map(crate::limits::Turn::used) {
        let again = crate::review::resend_row(ctx, i, &CustomId::ProfileForm);
        return ctx.respond(i, &ui::ephemeral_with(&wait, vec![again])).await;
    }
    ctx.respond(i, &ui::defer()).await;
    let saved = ctx.db().set_profile(&p, now()).await;
    ctx.log_err(saved.as_ref(), "saving profile");
    // They've been asked where they work; don't ask again for these roles.
    for key in company_roles(ctx, i, user.id).await {
        first(ctx, user.id, &format!("company:{key}")).await;
    }
    let nick = match choose(ctx, user.id, &p).await {
        Ok(Some(nick)) => nick,
        Ok(None) => {
            let text = format!(
                "Someone here already goes by **{}**. Add your surname (or more of it) so people can tell you apart, then send it again.",
                p.name
            );
            // What they sent is saved, so the form opens again with it.
            return ctx.edit_response(i, &text, &[crate::review::resend_row(ctx, i, &CustomId::ProfileForm)]).await;
        }
        Err(()) => {
            return ctx
                .edit_response(i, "Couldn't check your name against others just now; please try again.", &[])
                .await;
        }
    };
    let renamed = rename(ctx, user.id, &nick).await;
    ctx.edit_response(i, &submit_reply(&nick, saved.is_ok(), &renamed), &[]).await;
}

fn submit_reply(nick: &str, saved: bool, renamed: &Renamed) -> String {
    match (saved, set_it_yourself(renamed, nick)) {
        (true, None) => format!("Done: you're **{nick}** here. Change it any time with `/profile`."),
        (false, None) => {
            format!("You're **{nick}** here, but saving your details failed, so the form won't remember them.")
        }
        (true, Some(why)) => format!("Saved. {why}"),
        (false, Some(why)) => format!("Sorry, saving your details failed; please try again later. {why}"),
    }
}

/// How setting someone's nickname went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Renamed {
    Done,
    /// Discord doesn't let bots: not tried.
    Refused(Unnameable),
    Failed,
}

/// Set `user`'s nickname, unless Discord wouldn't let the bot (the server
/// owner, or someone ranked at or above it). That's expected, so it's
/// neither tried nor logged.
pub(crate) async fn rename(ctx: &Ctx, user: Id<UserMarker>, nick: &str) -> Renamed {
    match chipsoc_admin::join::unnameable(ctx, chipsoc_config::Snowflake(user.get())).await {
        Ok(Some(why)) => return Renamed::Refused(why),
        Ok(None) => {}
        // Try anyway: Discord says no if it must.
        Err(e) => tracing::debug!("couldn't tell whether the bot may rename {user}: {e:#}"),
    }
    match set_nick(ctx, user.get(), nick).await {
        Ok(()) => Renamed::Done,
        Err(e) => {
            tracing::warn!(?e, "setting nickname failed");
            Renamed::Failed
        }
    }
}

async fn set_nick(ctx: &Ctx, user: u64, nick: &str) -> anyhow::Result<()> {
    let path = format!("guilds/{}/members/{user}", ctx.guild);
    ctx.call(chipsoc_discord::Method::Patch, &path, Some(serde_json::json!({ "nick": nick }))).await.map(drop)
}

/// What to tell someone whose nickname the bot couldn't set.
pub(crate) fn set_it_yourself(r: &Renamed, nick: &str) -> Option<String> {
    let whose = match r {
        Renamed::Done => return None,
        Renamed::Refused(Unnameable::Owner) => "Discord doesn't let bots change the server owner's nickname",
        Renamed::Refused(Unnameable::Above) => {
            "Discord doesn't let bots change the nickname of anyone ranked above the bot"
        }
        Renamed::Failed => "Couldn't set your nickname just now",
    };
    Some(format!("{whose} — set it yourself to **{nick}**: server name menu → Edit Per-server Profile."))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_name_alone_unless_someone_else_has_it() {
        assert_eq!(nickname(" Ada ", "lovelace", &v(&["Grace"]), &[]).unwrap(), "Ada");
        assert_eq!(nickname("Ada", "lovelace", &v(&["ada"]), &[]).unwrap(), "Ada L.");
        assert_eq!(nickname("Ada", "", &v(&["Ada"]), &[]), None, "nothing to tell them apart by: not the same name");
        assert_eq!(nickname("Ada", "", &[], &v(&["Lovelace"])), Some("Ada".into()), "a rival, but nobody shows it");
    }

    #[test]
    fn the_shortest_start_of_surname_that_tells_them_apart() {
        assert_eq!(nickname("Ada", "Lovelace", &v(&["Ada"]), &v(&["Lamb"])).unwrap(), "Ada Lo.");
        assert_eq!(nickname("Ada", "Lovelace", &v(&["Ada", "ADA L."]), &[]).unwrap(), "Ada Lo.");
        assert_eq!(nickname("Ada", "Lovelace", &[], &v(&["lovelace"])).unwrap(), "Ada Lovelace", "same surname");
        assert_eq!(nickname("Ada", "de Vere", &v(&["Ada"]), &v(&["Dent"])).unwrap(), "Ada De V.", "skips the space");
    }

    #[test]
    fn nicknames_fit_discords_limit() {
        assert_eq!(nickname(&"x".repeat(40), "", &[], &[]).unwrap().chars().count(), MAX_NICK);
        let long = nickname("Ada", &"y".repeat(40), &v(&["Ada"]), &v(&["y".repeat(40).as_str()])).unwrap();
        assert!(long.chars().count() <= MAX_NICK);
    }

    #[test]
    fn invisible_characters_cannot_make_a_name_look_like_another() {
        assert_eq!(clean("A\u{200B}da\u{FEFF}"), "Ada");
        assert_eq!(clean("  Ada \u{202E} \t Lovelace "), "Ada Lovelace");
        assert_eq!(clean("\u{3164}\u{200D}"), "", "fillers only: no name at all");
        let surname = clean("\u{200B}");
        assert_eq!(nickname("Ada", &surname, &v(&["Ada"]), &[]), None, "an invisible surname tells nobody apart");
    }

    #[test]
    fn owners_and_higher_ranks_are_told_to_set_it_themselves() {
        let owner = submit_reply("Ada", true, &Renamed::Refused(Unnameable::Owner));
        assert!(owner.contains("server owner's nickname") && owner.contains("Edit Per-server Profile"), "{owner}");
        assert!(owner.contains("**Ada**"), "{owner}");
        let above = set_it_yourself(&Renamed::Refused(Unnameable::Above), "Ada").unwrap();
        assert!(above.contains("ranked above the bot"), "{above}");
        assert_eq!(set_it_yourself(&Renamed::Done, "Ada"), None);
        assert!(submit_reply("Ada", true, &Renamed::Done).starts_with("Done: you're **Ada**"));
    }

    #[test]
    fn only_spellings_the_bot_makes_are_changed() {
        assert!(is_spelling("ada lo.", "Ada", "Lovelace"));
        assert!(is_spelling("Ada", "Ada", "Lovelace"));
        assert!(!is_spelling("Ada the Great", "Ada", "Lovelace"));
    }
}
