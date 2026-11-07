# Running a server

For the committee. "Apply" means pressing **Apply** under the plan in
`log.bot` (or `/server apply`, the dashboard's Apply page, or `apply`).
`log.bot` and the review channel are the two places to watch: plans to
Apply and changes to Approve are posted in the first, applications and
suggestions to Accept or Reject in the second.

## Everyday jobs

**Start of the year.**
1. Appoint the new committee (below), and open this year's positions
   (`/server position open`); close the rest.
2. Then take last year's committee out of their positions, one at a time
   with `/server member remove` or on the Members page, or all at once by
   emptying their lists in members.toml in a pull request (an admin approves
   it in `log.bot` and applies). People in admin positions lose `admin_role`
   with them. Appoint first: taking someone out of an admin position needs
   another admin's Approve, so with the old committee gone first only the
   server owner could approve the new one.
3. If `join.toml` has `yearly_refresh`, from that date the bot DMs members
   whose answers include a `yearly` step (year of study, say), a few at a
   time, asking them to confirm or update it. `log.bot` says when it
   starts, lists anyone whose DMs are closed, and says when it's done.
   Otherwise, announce that members should press **Update my details**.
4. Retire finished projects (below) and refresh posts in `content/`.

**Appointing committee.** `/server member appoint <position> @someone` (or
Accept their `/volunteer` card, or the Members page). For an admin position
a second admin presses **Approve** in `log.bot`, and they get the role and
`admin_role`; nobody appoints themselves except the server owner. They get
a welcome DM saying what they can do and where (`/server help`, the review
channel, `/record` when recording is on, and a dashboard link when
Tailscale is set up). Someone who should be an admin without a position
goes under `admin_role` in members.toml.

**New project.** Add it on the dashboard's Projects page, with `project
new`, or in projects.toml (status `proposed` gets just a post). Make it
active (`/server project status`; it asks first), Apply, add leads
(dashboard or `project lead add`), then open applications
(`/server project applications`).

**Finishing a project.** `/server project status <project> past` → Confirm →
Apply. Its channels are archived (history kept), its roles renamed and kept,
and its applications closed.

**Onboarding a partner society.** Agree ids, which roles each side vouches
for and how they map; then follow [partners.md](partners.md), step 3. Allow
about an hour; it needs the maintainer (secrets file, deploy.sh) and an
admin to approve.

**Handing over.** When someone with access leaves:
1. Take them out of members.toml (`/server member remove`, or the Members
   page) and have another admin press **Approve** in `log.bot`: that takes
   their position and `admin_role`, so their `/server`, web and CLI access
   ends within 5 minutes and their Tailscale share is revoked. Removing the
   role in Discord alone is reported, and the next apply gives it back. If
   they own the Discord server, transfer ownership first (they'd stay an
   admin).
2. Remove their push access to the config repo, any deploy keys they added,
   and their ssh key from the server.
3. Rotate every secret they could see ([deploy.md](deploy.md#rotating-secrets)).
4. Remove them from the Developer Portal team and the tailnet.

## Who is an admin

One rule everywhere (`/server`, web sign-in, CLI login, Tailscale shares):
the server owner, anyone with Administrator or Manage Server, and anyone in
`admin_role` (the committee). Appointing someone to (or taking them out of)
an admin role or position, accepting their application for one, or
restoring their own roles takes an admin other than them; the server owner
may do it themselves.

**Only the server owner** (Discord's owner of the server), in every front
end: applying a plan that deletes for real (`--delete`) or sweeps
(`--sweep`), or that weakens the bot (takes permissions from its role,
denies its role something in a channel, or puts roles above it);
approving a change to `admin_role`, which positions are admin positions,
or partner sync's `key_env` and partners; approving a change to `[drift]
mode` or to how recordings are transcribed (`[recording]` `transcribe`,
`model`, `whisper`); reverting or rewinding config changes. Nobody else
can take a role from the owner (in members.toml, a plan, or `member
remove`); while Discord can't say who the owner is, nobody but the bot's
own token takes a role from anyone. Anyone else is told "Only the server
owner can …". The list is `owner_only` in `crates/engine/src/owner.rs`.

**Undoing what someone did.** `/server history` (the web's Apply →
History page, `history`, `GET /changes`) lists the config's changes with
who made them; `/server history @admin` (`history --admin <who>`) is what
one admin did this week, from the audit log and the config's history. The
owner undoes one with `/server revert <id>` (Revert on that page, `revert
<id>`, `POST /undo`), or puts the whole config back as it was right after
it with `rewind`. Either is a new commit (history is never rewritten;
lock.toml stays the bot's), refused if later changes clash or the result
wouldn't pass `check`, and then shown as a plan: applying it undoes the
change in Discord too, archiving what it removes. Every front end says it
the same way ("Saved as a new change: …" or "Not changed: …"), and
`log.bot` notes who did it. Once saved it stands: if pushing it to the
config repo fails, that's said as a warning and the bot pushes it on its
next sync. History names whoever the bot signed a change for, else the
commit's git author (for reading only: approvals never go by it; see
[security.md](security.md#what-a-push-cant-do)).

## What asks first

Removing, archiving, taking a role away, changing a project's status,
closing an idea, reverting or rewinding, and applying ask first in every
front end (**Confirm**/**Cancel** in Discord, a second button on the web;
`--yes` skips it in the CLI). On the web, a revert's result is shown on the
page it sends you to, so reloading never sends it again. An
**Apply** button under a posted plan applies exactly that plan; it lasts 24
hours or until the bot restarts, and an old one offers a fresh plan. Once
anything is applied, from Discord, the dashboard, the API or the CLI, the
bot log's open plans lose their buttons and say who applied ("✅ Applied by
… (from the CLI/web)" within half a minute for an apply outside Discord); a
newer plan replaces the one before it ("↪️ Replaced by a newer plan").

## In Discord

Commands appear only when their feature is on. `/server` is listed for
everyone and checks for admin (to hide it: Server Settings → Integrations →
the bot → allow the admin role only); `/event` makes events for admins and
takes suggestions from members.

- Every member form answers at once and always replies. Each has a
  per-member limit (a minute for applications, 2 minutes for feeds and
  links, 5 for ideas and event suggestions), checked when the form opens
  and said in words ("about 5 minutes"). A refused submit doesn't count,
  unless checking it cost a fetch (an unreadable feed, a link already
  shared).
- A refusal they can fix (the wait, an http link, a duplicate, an
  unreadable feed or start time, a name clash) keeps what they typed for 30
  minutes, with an **Edit and resend** button that opens the form filled in.
- One pending application per person per position or project, and none for
  a position they hold or a team they're on. An Accept that gives nothing
  (Discord refused, or the role is too powerful now) stays in the queue.

### Application windows

A position with `closes` takes applications quietly until then: each puts a
line in the review channel ("📨 @Ada applied for **Treasurer** (3 so far)"),
without buttons, and the applicant is told when applications close. When
the window closes (the date passes, or an admin closes the position), the
bot posts one card per position listing everyone who applied, with
**Accept N** / **Reject N** for each and **Reject the rest…** (one reason,
DMed to each). Each decision redraws the card; up to 8 applicants fit on
one, more go on several. Without `closes`, each application gets its own
card at once.

| Command | Who | Does |
|---|---|---|
| `/volunteer`, `/apply` | Anyone | Form → review card; Accept gives the role and DMs them. A position with `closes` collects applications first. |
| `/propose-project` | Anyone | A review card with every answer; once accepted, a tagged post in the ideas forum. A DM either way (with the reason, if given). What the form asks is `[ideas] questions` ([config.md](config.md#project-ideas)). |
| `/share-resource` | Anyone | Title, https link, what it is, the forum's tags; checked (not shared before), then a review card; once accepted, a post crediting them, and a DM. |
| `/suggest-feed` | Anyone | A feed's address, or a website's (its feed is found from the page). A review card; once accepted, new posts are shared; a DM either way. |
| `/profile` | Anyone | Sets their nickname (with `join.toml`: opens the join flow). |
| `/solved` | Asker, admins | Tags a help post solved and closes it. If the bot can't (it needs Manage Threads in the forum), the member is asked to try later and `log.bot` says why. |
| `/partners` | Anyone | Partners' project catalogues (with partners only). |
| `/event` | Anyone | Admins: a scheduled event at once (in a voice or stage channel if "Where" names one), announced, with reminders a day and an hour before. Members: a suggestion for review; Accept makes it, crediting them, and they get a DM either way. A start that can't be read or has passed, or a length outside 5–1440 minutes, is refused. |
| `/server help` | Anyone | Start here: what admins can do, by area, and where. |
| `/server status` | Admins | The server at a glance: config problems, the review queue. |
| `/server drift` | Admins | Does Discord match the config? What changed, and who. |
| `/server plan\|apply\|pull` | Admins | Like the CLI commands; plans come with an **Apply** button. |
| `/server history [admin]` | Admins | The config's latest changes and who made them, or what one admin did this week. |
| `/server revert <change> [rewind]` | Owner | Undo a config change (or rewind to it), after **Confirm**; then a plan to apply. |
| `/server approvals` | Admins | What waits for an **Approve**, with its card posted again in `log.bot` (for one lost or buried there). |
| `/server web` | Admins | DMs a fresh one-time Tailscale link to the dashboard. |
| `/server queue list\|accept\|reject\|history` | Admins | The review queue by number; a reject can give a reason (Reject on a card asks too). A member who sent something is told by DM either way, with the reason; a partner is never told (its reason is only kept in the history). Every front end says which. |
| `/server member list\|info\|appoint\|remove` | Admins | Positions and who holds them, and who left; what someone answered when they joined; appoint or take out. |
| `/server project status\|applications\|rename`, `/server position open\|close` | Admins | Config edits: saved, then shown as a plan with an **Apply** button. Closing a position with a window posts its applicants' card. |
| `/server project lead` | Admins | Make someone a project lead (the Lead role, and a DM). Refused, and nobody told, if the Lead role has moderator or admin permissions in Discord the config doesn't give it. |
| `/server feed list\|add\|pause\|resume\|remove`, `/server resource list\|add\|archive` | Admins | Feeds (by title, which the bot keeps as each one last read; a paused one isn't checked) and the resources forum. |
| `/server idea close <post> [reason]` | Admins | Close an idea's post, after **Confirm**: it gets `[ideas] closed_tag` if set (and loses the needs-lead tag), "🔒 **Closed:** <reason>" (or just "🔒 **Closed.**") is posted in it, then it's archived and locked (never deleted). The reply says whether it was tagged. Also on the dashboard's Resources page (**Close…**, a reason, then **Yes, close this idea**), the TUI's Resources tab, `idea close` and the API. |
| `/server join notify` | Admins | Says how many members haven't been through the join flow, then DMs them once with **Send to N**. |
| `/server share announcement\|event` | Admins | Share an announcement (its link) or one of our events with partners you pick from a menu; only partners whose `send` lists that kind are offered ([partners.md](partners.md#sharing-content)). Making an event with `/event` offers the same menu. |
| `/record start [channel]`, `/record stop` | Admins, `[recording] who` | Record a meeting in the call you're in (or the one named); see below. |

### Recording meetings

With `[recording]` in bot.toml, `/record start` records the call you're in
(or `channel:`, which must be one you can see and join yourself): audio
only, as Discord gives bots no video or screen share. The bot joins
(muted), sets the channel's status to "🔴 Recording" (or says so in the
notice where it can't) and posts in the call's chat, and only then starts
hearing the call:

> 🔴 This call is being recorded and transcribed by the bot for *(server)*;
> recordings are kept by the committee. If you don't want to be recorded,
> leave the call.

Whoever joins while it records gets that by DM (in the call's chat, to
them, if their DMs are closed; failing that, in the channel `/record
start` was run from). If none of those works, the recording stops and
`log.bot` says why. Only whoever started a recording, or an admin, stops
it. There's no opt-out button: someone who doesn't want to be recorded
leaves the call. Change the wording with `notice` and `join_notice`, but
keep it clear: who records, that it's transcribed, who keeps it, and how
not to be in it.

It stops with `/record stop`, when the call has been empty for 2 minutes,
at `max_hours` (default 3), at `max_gb`, when all the server's recordings
reach `max_total_gb` (default 20), or if the disk runs low; it won't start
with less than `min_free_gb` free, or with the recordings already at
`max_total_gb` (said in `log.bot` too). Each speaker gets their own track
(silent where they weren't talking, so all of them line up) and there's a
mix of everyone, with who was in the call and when. Then a transcript is
made per speaker and merged, "Ada [12:03]: …", with whisper.cpp or a
transcription service ([deploy.md](deploy.md#voice-recordings)); without
one the summary says so and the audio is kept. The summary goes to
`[recording] channel`: how long, who, and a link to the dashboard's
**Recordings** page, where admins play or download each track, the mix and
the transcript. If the bot stops while recording, what was saved is kept,
closed and reported when it starts again.

Recordings are never deleted by the bot. An admin deletes one on its page
(**Delete this recording**, then **Yes, delete it for good**); it's logged
in the audit log and `log.bot`. Do that when someone asks for their
recording to be erased, or when your retention period is up.

**Your duties (UK GDPR).** A recording of people is their personal data.
Before turning this on: say in the server's rules or privacy notice that
meetings may be recorded, why, who keeps the recordings and for how long,
and how to ask for a copy or for one to be deleted; agree a retention
period and keep to it (delete by hand). A suggested rules line:

> Meetings in voice channels may be recorded and transcribed by the
> committee. The bot says so in the call when it records; if you don't want
> to be recorded, leave the call. Ask the committee for a copy of, or to
> delete, a recording you're in.


## Web dashboard

`chipsoc-bot web` (or the web unit; setup in [deploy.md](deploy.md)). Pages:
Overview, Queue, Members, Projects, Positions, Feeds, Resources, Onboarding,
Channels, Roles, Recordings, Activity, Apply; your Sign-ins under your name.
Edits are checked like any other and saved to the config repo, then posted in
`log.bot` as a plan to apply. It needs no JavaScript. Only admins get in;
browser sessions last 12 hours, and someone who stops being an admin is out
within 5 minutes.

## Terminal UI and CLI

Every command takes `--config <dir>`. Commands that edit files keep comments.

| Command | Does |
|---|---|
| *(none)*, `tui [--demo]` | Terminal UI: the dashboard's areas as tabs (keys in the footer, `?` for help; posts, briefs, questions and permissions open in `$EDITOR`), except onboarding, project renames and questions, member info, recordings and the full activity log. `--demo` runs on a copy of the config against a pretend server. |
| `web [--addr]` | Web UI (default `127.0.0.1:8080`); on the bot's server only (`--local`). |
| `check` | Checks the config without touching Discord. |
| `plan`, `apply [--yes] [--dry-run]` | Shows changes, or makes them (asks first). `--delete` deletes instead of archiving; `--sweep` also archives unmanaged channels. |
| `pull [--permissions]` | Copies Discord-side changes into the files (alias `adopt`). |
| `export <dir>`, `template <code> <dir>` | Starts a config from the server (through the bot's server) or a public template. |
| `open <kind> [key]` | Opens the right file in `$EDITOR`. |
| `status`, `drift` | The server at a glance; does Discord match the config, by who changed what. |
| `role list\|new\|set\|move\|remove`, `category list\|new\|rename\|move\|remove\|perms`, `channel list\|new\|set\|move\|move-to\|archive\|perms` | Names, topics, colours, tags, permissions, order. Channels are archived, not removed. |
| `project list\|new\|show\|set\|status\|applications\|rename\|brief\|lead` | Projects. |
| `position list\|new\|set\|questions\|intro\|open\|close\|move\|remove` | Positions and their forms. |
| `member list\|info\|appoint\|remove\|restore\|forget` | Positions and who holds them (`add` still works for `appoint`). |
| `queue list\|show\|accept\|reject [--reason]\|lead\|history` | The review queue. |
| `feed`, `resource`, `idea list\|close <post> [--reason]`, `activity` | Feeds, the resources forum, ideas (closing asks first, as in Discord), the audit log. |
| `history [--admin <who>]`, `revert <id> [--rewind]` | Who changed the config, or what one admin (@username, name or id) did this week; undo a change (the server owner). |
| `join status\|notify` | The join flow's progress; the one-off DM (says how many and asks first). |
| `post list\|show\|set`, `onboarding …` | Bot posts (shown as Discord will show them) and onboarding questions. |
| `op '<json>'`, `schema` | Any config edit as an `Op`; what can be edited. |
| `run [dir...]` | Runs the bot; extra directories serve more servers. On the bot's server only (`--local`). |
| `fed keygen\|key\|revoke` | Partner sync ([partners.md](partners.md)). |
| `share announcement\|event <link or id> [--to a,b]` | Share with partners (default: every partner we send it to); the bot sends it within a minute. Also `POST /api/v1/share`. |
| `login [--server <url>] [--paste]`, `logout` | Signs in to the bot's web server with Discord (default: bot.toml's `[web] url`), or out. |

**Terminal UI keys:** ↑↓ or j/k move, Tab or the number keys switch tabs
(in the dashboard's order: 1 Overview, 2 Queue, 3 Members, 4 Projects,
5 Positions, 6 Feeds, 7 Resources, 8 Channels, 9 Roles, 0 Apply), `?` help,
`q` quit. On the Queue: `a` accept, `x` reject, `h` history, then `l` make
lead. On Apply: `f` plan, `a` apply, `p` pull. The footer lists the rest;
Enter in a form names what it does ("Enter  reject", "appoint", …).

**Text you edit.** Posts, briefs, questions and permissions open in
`$VISUAL` or `$EDITOR` (else nano); VS Code, Zed and Sublime Text are asked
to wait until you close the file. Questions go one per line: add `[long]` for
a big box, `[optional]` if it may be left blank, and a hint for the box after
` // `, as in `Why this position? [long] // a line or two`. Permissions go one
per line, as Discord writes them (`MANAGE_MESSAGES`) or in words (`manage
messages`), and the same goes for `--permissions`, `--allow` and `--deny`.
Lines starting with `#` are notes.

Every edit is checked first and refused (with the problems) if `check` would
fail. bot.toml sections, screening, AutoMod rules and partners are edited
only as files or with `op`. `--json` prints JSON for scripts. Without a
terminal, missing values must be given as flags. Times are shown in
bot.toml's `timezone` (JSON keeps Unix times).

**Sign in first.** Commands that reach Discord or the bot's data need
`chipsoc-bot login` (default server: `[web] url`). It opens the browser, you
Continue on the bot's page, and the token is saved in
`~/.config/chipsoc-bot/remote.toml` (30 days, until `logout`, revocation
under *Sign-ins*, or losing admin; a web restart keeps it). `--paste` shows
a code to paste instead. Offline: `check`, `open`, `template`, `tui --demo`,
`fed keygen`, `fed key`, and lists that only read the files (`project list`,
`position list`, `role list`, …) on a checkout. On the bot's server,
`--local` uses its token; `run`, `web`, `fed revoke` need it. Signed in,
the TUI and every command work on the server's copy of the config; changes
are pushed to the config repo (`git pull` to see them locally). The TUI's
`e` still opens the file in your local clone, so commit and push what you
change there.

## JSON API

The web server's `/api/v1`, with the CLI's bearer token. Edits are tried on
a copy first and refused, with the problems, if `check` would fail; commits
say who. A refusal comes as `{"error"}`: 403 for what only the server owner
may do, 409 for other admin work turned down, 422 for a bad request or an
edit with problems (listed in `problems`).

| Endpoint | Does |
|---|---|
| `GET /me`, `POST /logout`, `GET /tokens`, `DELETE /tokens/{id}` | Who you are; sign out; your CLI sign-ins (never the token), revoke one. |
| `GET /status`, `GET /drift`, `GET /check` | The server at a glance; how Discord differs; config problems. |
| `GET /plan` (`?delete=true&sweep=true`), `POST /apply` (`{"plan_id", "steps"?}`), `POST /pull` (`{"permissions"}`) | Plan, apply that plan (or some of its steps), pull. |
| `GET /export` | The server as it is now, as a new config's files. |
| `GET /ops/schema`, `POST /ops`, `POST /ops/preview` | What can be edited; one edit (an `Op`, e.g. `{"op": "set_role", "key": "member", "set": {"hoist": true}}`), or a list of them made as one change (one check, one commit, all or nothing); a post's markdown as Discord shows it. |
| `GET`/`POST /projects`, `POST /projects/{slug}/status`, `POST /projects/{slug}/applications` | Projects; add one; change status; open or close applications. |
| `POST /projects/{slug}/leads`, `DELETE /projects/{slug}/leads/{user}` | Make someone a lead, or not. |
| `GET /queue`, `POST /queue/{id}` (`{"accept", "reason", "seen"}`), `POST /queue/{id}/lead`, `GET /queue/history` | The review queue; entries carry `from`, who sent them by name. An Accept with `seen` (the entry's `version`) is refused if the entry changed since. |
| `GET`/`POST /members`, `DELETE /members/{role}/{user}`, `POST /members/former/{user}/restore`, `DELETE /members/former/{user}` | Holders; appoint (`{"role", "user"}`) or take out; restore or forget a returner. |
| `GET /members/find?q=`, `GET /members/about/{user}` | Members by name (`@ada`, `Ada Lovelace`), for picking someone; what someone told the bot (profile, join answers). |
| `GET`/`POST /feeds`, `POST /feeds/{id}/pause` (`{"paused"}`), `DELETE /feeds/{id}` | Feeds. |
| `GET`/`POST /resources`, `DELETE /resources/{id}`, `GET /ideas`, `POST /ideas/{id}/close` (`{"reason"?}`) | The resources forum (archiving closes and locks, never deletes); ideas, and closing one. |
| `POST /share` (`{"what": "announcement"\|"event", "item", "partners"}`) | Share an announcement (its message link) or an event (its link or id) with partners (`partners` empty: every partner we send that kind to). |
| `GET /activity`, `GET /join`, `GET`/`POST /join/notify` | The audit log; join-flow progress; the join-flow DM. |
| `GET /changes` (`?user=`), `POST /undo` (`{"commit", "how": "revert"\|"rewind"}`) | Who changed the config (or what one admin did this week); undo a change (the server owner). |

The `Op` list is in [`ops/mod.rs`](../crates/config/src/ops/mod.rs); the
queue and the rest are in [`crates/admin`](../crates/admin/src/lib.rs).
Audit-log actions are snake_case (`appoint`, `made_lead`,
`approve_config_change`, …).
