//! Who holds what: members.toml and project leads, with people's names, and
//! the roles of people who left, which admins can give back when they return.

use std::collections::{BTreeSet, HashMap};

use anyhow::Result;
use chipsoc_config::ops::Op;
use chipsoc_config::{Config, Snowflake};
use chipsoc_discord::Rest;
use chipsoc_discord::wire::id;
use serde::{Deserialize, Serialize};

use crate::{Actor, AuditAction, Host, Said, add_holder, audit, give_unrecorded, role_name, with_admin};

/// Someone, with the name people know them by (their id if it can't be found).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Person {
    pub id: Snowflake,
    pub name: String,
}

/// A role and who holds it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holding {
    pub role: String,
    pub name: String,
    /// The position it's for, if it's one (its label).
    pub position: Option<String>,
    /// The project it leads, if it's a project's Lead role (its slug).
    pub project: Option<String>,
    pub people: Vec<Person>,
}

/// A role by key and name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Named {
    pub key: String,
    pub name: String,
}

/// Someone who left holding roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormerMember {
    pub person: Person,
    pub roles: Vec<Named>,
    /// When they left.
    pub since: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Roster {
    /// Every role members.toml lists, in its order.
    pub holders: Vec<Holding>,
    /// Each project's leads.
    pub leads: Vec<Holding>,
    pub former: Vec<FormerMember>,
}

/// Who holds what, and who left holding something.
pub async fn roster<H: Host>(h: &H) -> Result<Roster> {
    let cfg = h.cfg();
    let former = h.db().former_members().await?;
    let mut ids: BTreeSet<Snowflake> = cfg.desired.holders.values().flatten().copied().collect();
    ids.extend(cfg.projects.iter().flat_map(|p| p.leads.iter().copied()));
    ids.extend(former.iter().filter_map(|f| f.user.parse::<Snowflake>().ok()));
    let names = names(h, &ids).await;
    let person = |id: &Snowflake| Person { id: *id, name: names.get(id).cloned().unwrap_or_else(|| id.to_string()) };
    let holders = cfg
        .desired
        .holders
        .iter()
        .map(|(key, users)| Holding {
            role: key.clone(),
            name: role_name(&cfg, key),
            position: cfg.bot.positions.iter().find(|p| &p.role == key).map(|p| label(&cfg, &p.role)),
            project: None,
            people: users.iter().map(person).collect(),
        })
        .collect();
    let leads = cfg
        .projects
        .iter()
        .map(|p| Holding {
            role: p.lead_role_key(),
            name: format!("{} lead", p.name),
            position: None,
            project: Some(p.slug.clone()),
            people: p.leads.iter().map(person).collect(),
        })
        .collect();
    let former = former
        .into_iter()
        .filter_map(|f| {
            let id: Snowflake = f.user.parse().ok()?;
            let roles = f.roles.iter().map(|k| Named { key: k.clone(), name: role_name(&cfg, k) }).collect();
            Some(FormerMember { person: person(&id), roles, since: f.since })
        })
        .collect();
    Ok(Roster { holders, leads, former })
}

/// A position's label, else its role's name.
fn label(cfg: &Config, role: &str) -> String {
    let p = cfg.bot.positions.iter().find(|p| p.role == role);
    p.and_then(|p| p.label.clone()).unwrap_or_else(|| role_name(cfg, role))
}

/// Names for `ids`, from the server's member list: nickname, else display
/// name, else username. Missing ones are left out.
async fn names<H: Host>(h: &H, ids: &BTreeSet<Snowflake>) -> HashMap<Snowflake, String> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let members = match h.rest().members(h.guild()).await {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("reading the member list failed: {e:#}");
            return HashMap::new();
        }
    };
    let name = chipsoc_discord::wire::shown_name;
    members
        .iter()
        .filter_map(|m| id(&m["user"]["id"]).filter(|uid| ids.contains(uid)).and_then(|uid| Some((uid, name(m)?))))
        .collect()
}

/// Give someone a role members.toml lists (and the admin role, for admin
/// positions), recorded in the config. `Err` says why not.
pub async fn appoint<H: Host>(h: &H, role: &str, user: Snowflake, by: &Actor) -> Result<Said, Said> {
    let cfg = h.cfg();
    if by.id == Some(user) && crate::grants_admin(&cfg, role) && !crate::is_owner(h, user).await {
        return Err(Said::new().text("Another admin has to appoint you to an admin role."));
    }
    if cfg.desired.holders.get(role).is_some_and(|u| u.contains(&user)) {
        return Err(Said::new().user(user).text(" already holds ").strong(role_name(&cfg, role)).text("."));
    }
    let said = add_holder(h, role, user).await?.said;
    audit(h, by, AuditAction::Appoint, Some(role), Some(&user.to_string())).await;
    Ok(said)
}

/// Take someone out of members.toml for `role`; the role is taken away from
/// them, and for an admin position the admin role too, unless something else
/// in the config still gives it to them. That takes away `/server`, web
/// sign-in and their Tailscale share as well.
pub async fn unappoint<H: Host>(h: &H, role: &str, user: Snowflake, by: &Actor) -> Result<Said, Said> {
    crate::only_owner(h, by, &chipsoc_engine::Ask::TakeRoles { user }).await?;
    if by.id == Some(user) && crate::grants_admin(&h.cfg(), role) && !crate::is_owner(h, user).await {
        return Err(Said::new().text("Another admin has to take an admin role from you."));
    }
    let op = Op::RemoveHolder { role: role.to_owned(), user };
    let said = record_op(h, &op, user, &with_admin(&h.cfg(), &[role.to_owned()])).await?;
    audit(h, by, AuditAction::Unappoint, Some(role), Some(&user.to_string())).await;
    Ok(Said::new().text("Taken out of the config.").then(said))
}

/// Take someone off a project's leads; its Lead role is taken away from them.
pub async fn remove_lead<H: Host>(h: &H, slug: &str, user: Snowflake, by: &Actor) -> Result<Said, Said> {
    crate::only_owner(h, by, &chipsoc_engine::Ask::TakeRoles { user }).await?;
    let cfg = h.cfg();
    let p = cfg
        .projects
        .iter()
        .find(|p| p.slug == slug)
        .ok_or_else(|| Said::new().text("No project ").code(slug).text("."))?;
    let op = Op::RemoveLead { slug: slug.to_owned(), user };
    let said = record_op(h, &op, user, &[p.lead_role_key()]).await?;
    audit(h, by, AuditAction::RemoveLead, Some(slug), Some(&user.to_string())).await;
    Ok(Said::new().text("Taken off the project's leads.").then(said))
}

/// Make `op` through the host (checked like any other), then apply `user`'s holding of `roles`.
async fn record_op<H: Host>(h: &H, op: &Op, user: Snowflake, roles: &[String]) -> Result<Said, Said> {
    if let Some(why) = h.unwritable() {
        return Err(Said::new().text(why));
    }
    h.record(std::slice::from_ref(op), user, roles).await.map(|r| r.said).map_err(|e| {
        tracing::warn!("recording a change failed: {e:#}");
        Said::new().text(crate::fmt::error(&e))
    })
}

/// Give someone who came back the roles they held when they left, and forget
/// them. `Err` says why not.
pub async fn restore<H: Host>(h: &H, user: Snowflake, by: &Actor) -> Result<Said, Said> {
    if by.id == Some(user) {
        return Err(Said::new().text("Another admin has to restore your own roles."));
    }
    let roles = match h.db().former_roles(&user.to_string()).await {
        Ok(r) if r.is_empty() => return Err(Said::new().text("Already dealt with.")),
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("reading former roles failed: {e:#}");
            return Err(Said::new().text(format!("Couldn't read their old roles: {}", crate::fmt::error(&e))));
        }
    };
    let text = restore_roles(h, user, &roles).await;
    if let Err(e) = h.db().forget_roles(&user.to_string()).await {
        tracing::warn!("forgetting roles failed: {e:#}");
    }
    audit(h, by, AuditAction::Restore, Some(&user.to_string()), Some(&roles.join(", "))).await;
    Ok(text)
}

async fn restore_roles<H: Host>(h: &H, user: Snowflake, roles: &[String]) -> Said {
    let cfg = h.cfg();
    let roles = with_admin(&cfg, roles);
    if let Some(why) = h.unwritable() {
        return give_unrecorded(h, user, &roles, &Said::new().text(why)).await.0;
    }
    // Roles the config decides on go back in it (members.toml, project leads),
    // checked like any edit; others it no longer lists are given directly.
    let (mut ops, mut keys, mut direct) = (Vec::new(), Vec::new(), Vec::new());
    for key in &roles {
        match cfg.projects.iter().find(|p| p.lead_role_key() == *key) {
            Some(p) => ops.push(Op::AddLead { slug: p.slug.clone(), user }),
            None if cfg.members.contains_key(key) => ops.push(Op::AddHolder { role: key.clone(), user }),
            None if cfg.desired.holders.contains_key(key) => {}
            None => {
                direct.push(key.clone());
                continue;
            }
        }
        keys.push(key.clone());
    }
    let mut out = Said::new();
    if !direct.is_empty() {
        out = give_unrecorded(h, user, &direct, &Said::new().text("the config doesn't list it")).await.0;
    }
    if keys.is_empty() {
        return out;
    }
    if let Some(why) = crate::grown_too_powerful(h, &keys).await {
        return out.then(why);
    }
    // Only what they aren't listed for already (an op that changes nothing is refused).
    ops.retain(|op| match op {
        Op::AddLead { slug, .. } => !cfg.projects.iter().any(|p| &p.slug == slug && p.leads.contains(&user)),
        Op::AddHolder { role, .. } => !cfg.members.get(role).is_some_and(|u| u.contains(&user)),
        _ => true,
    });
    match h.record(&ops, user, &keys).await {
        Ok(r) => out.then(Said::new().text("Back in the config.").then(r.said)),
        Err(e) => {
            tracing::warn!("putting a returner back in the config failed: {e:#}");
            let why =
                format!("{} Couldn't put them back in the config: {}", crate::fmt::ATTENTION, crate::fmt::error(&e));
            out.then(why.as_str())
        }
    }
}

/// Don't offer someone's old roles back. False if none were remembered.
pub async fn forget_former<H: Host>(h: &H, user: Snowflake, by: &Actor) -> Result<bool> {
    let had = h.db().former_roles(&user.to_string()).await?;
    if had.is_empty() {
        return Ok(false);
    }
    h.db().forget_roles(&user.to_string()).await?;
    audit(h, by, AuditAction::ForgetFormer, Some(&user.to_string()), None).await;
    Ok(true)
}
