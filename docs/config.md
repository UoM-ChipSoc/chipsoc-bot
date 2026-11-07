# Config

| File | Controls |
|---|---|
| `server.toml` | Server settings and system channels. |
| `roles.toml` | Roles, top of the hierarchy first. |
| `categories/NN-name.toml` | One category and its channels, in order. |
| `channels.toml` | Channels outside any category. |
| `content/*.md` | Text of bot posts and project briefs. |
| `onboarding.toml`, `screening.toml`, `automod.toml` | Discord's onboarding questions → roles; rules accepted on joining; AutoMod rules. |
| `join.toml` | The bot's join flow (see *Join flow*), instead of onboarding questions. |
| `projects.toml` | Projects (see *Projects*). |
| `members.toml` | Who holds which positions and roles: role key = [user ids] (see *Who holds which roles*). |
| `bot.toml` | Bot features. |
| `federation.toml` | Partner sync ([partners.md](partners.md)). |
| `lock.toml` | Generated ids and last-applied state. Commit it; don't edit it. |
| `lock.toml.bak` | The previous `lock.toml`, kept on each save. Don't commit it. |

Put `.env*`, `lock.toml.bak`, `.apply.lock`, `.write.lock`, `*.tmp` and
`*.moving` in the config repo's `.gitignore`. The bot only ever commits
`lock.toml` and the config files it wrote itself (never `git add -A`), and
warns about a `.env` that isn't ignored.

All fields are in [`schema.rs`](../crates/config/src/schema.rs),
[`bot.rs`](../crates/config/src/bot.rs) and
[`join.rs`](../crates/config/src/join.rs). `check` catches typos and unknown
keys.

- **Keys:** every role, category and channel has a permanent `key` of
  lower-case letters, digits and dashes. Change `name` freely; a new `key`
  makes a new object. Removed roles stay until `--delete`. Text,
  announcement and forum channel names are planned as Discord keeps them
  (lower case, dashes for spaces); `check` says when. A project's `slug` is
  its key: rename it with `project rename`, the dashboard or
  `/server project rename` (see *Projects*), not in projects.toml.
- **Overwrites:** a channel's own entry replaces its category's entry for the
  same role; `inherit = false` drops the category's.
- **Posts:** `post = "content/x.md"` on a channel keeps a bot message there,
  edited in place. `{#key}` and `{@key}` become mentions. A line
  `{button:action|Label}` adds a button; the actions are `volunteer`,
  `apply:<slug>`, `idea` (or `propose-project`), `resource` (or
  `share-resource`), `feed` (or `suggest-feed`), `event` (the `/event`
  form: members suggest, admins create), `profile`, `join` and
  `join:update` (both start the join flow); a post can't have two buttons
  with the same action. An `apply:<slug>` button is left off while that
  project's applications are closed. On a forum, the post is a pinned "How to
  post here" thread (the bot needs Manage Threads to pin it). A forum that
  denies @everyone Send Messages should have a post with the button of the
  form that posts there; `check` warns otherwise, and errors if the bot's own
  role couldn't post there.
- **Forum posts in config:** on a forum (or media) channel, each
  `[[channel.thread]]` is a post the bot starts and keeps in step, alongside
  the pinned "How to post here" post (it isn't pinned itself):

  ```toml
  [[channel.thread]]
  key = "toolchain"                 # unique in this forum
  title = "Toolchain: HDL, FPGA and ASIC flows"   # at most 100 characters
  post = "content/toolchain.md"     # its text, in parts like any post
  tags = ["tools"]                  # optional; each one of the forum's tags, at most 5
  ```

  Its text is edited part by part in place, and its title and tags follow
  the config (the bot deletes the "changed the post title" notice Discord
  leaves when it renames one, as it does for project posts, and at start
  any older ones of its own). lock.toml keeps it among the posts as `"<forum>/<key>"`
  (older versions of the bot pass it by). Taken out of the config, it's
  archived and locked, never deleted; with lock.toml lost, it's found
  again by its title and text. `check` errors on a missing title or one
  over 100 characters, a tag the forum lacks, and the usual post problems.
- **Forum order:** `sort = "activity"` (latest activity first) or
  `"creation"` (newest post first) on a forum or media channel sets how
  Discord lists its posts; unset, it's left as Discord has it.
- **Long posts:** a post or project brief longer than Discord's 2000
  characters is sent as several messages (at most 10): a line holding only
  `<!-- part -->` starts the next one, and a part still too long is split at
  its last heading or blank line that fits (never inside a ``` block;
  mentions counted at their longest). The first part is the post (on a forum,
  the thread's first message), the rest follow it in order, and buttons go
  under the last. `check` names a part with nowhere to split, and errors on a
  button written before the last part. Each part is edited in place; one
  added is posted after the last (after any replies), and ones no longer
  needed are deleted. lock.toml keeps the later parts' ids as `parts`.
- **Lists:** `{positions}` becomes a bullet per position open in
  `/volunteer` ("- **Label**: first line of its intro", plus "applications
  close <date>" for one with `closes`) and `{projects:open}` one per
  project taking applications ("- **Name**: blurb"), or "Nothing open right
  now.". Opening or closing one changes the post on the next apply.

## bot.toml

At the top: `name`, `timezone` (default `Europe/London`), `admin_role` and
`archive` (the category key for removed channels). Each section turns a
feature on; leave it out to turn it off.

| Section | Turns on |
|---|---|
| `[log]` | `bot`: plans to apply, approvals, drift, partner sync, errors (called `log.bot` in these docs; the committee should read it). `people`: edits, deletes, joins, leaves, review decisions (text kept `keep_days`, default 90; text from a channel that keeps out any role or member is left out). |
| `[[position]]` | A role for `/volunteer`. `admin = true` also grants `admin_role` (the position's own role can't be `admin_role`); `open = false` leaves it out of `/volunteer`; `label`; `intro` is shown above `questions` (each a string, or `{ ask, long, optional, hint }`; default: hours per week, committee or organising experience (optional), electronics / chip design experience (optional), why this position; course and year aren't asked, as the card shows the applicant's join answers). Discord forms hold 5 boxes, the intro taking one. `closes = "2026-11-20"` (the end of that day) or `"2026-11-20 18:00"`, in `timezone`, gives it an application window ([admin.md](admin.md#application-windows)). |
| `[review]` | `channel` where applications, project ideas, feeds, links, event suggestions and rejoin cards wait for Accept or Reject. |
| `[events]` | `channel` for `/event` announcements. `suggest` (default true): members' `/event` is a suggestion for review (needs `[review]`); admins' events are made at once. |
| `[feeds]` | `channel` for feed posts (feeds are checked every 15 minutes). |
| `[ideas]` | `forum` where accepted `/propose-project` ideas are posted (needs `[review]`); `needs_lead_tag` marks ideas nobody leads yet; `closed_tag` marks ones closed with `/server idea close` (`check` errors if it isn't one of the forum's `tags`); `questions` and `post` make the form and the post your own (see *Project ideas*). |
| `[help]` | `forum` for `/solved`; `solved_tag` (default `solved`). |
| `[projects]` | `forum` for project posts; `areas` suggested in `project new`; `default_channels` for new projects; `pinned = "<slug>"` pins that project's post at the top of the forum (Discord pins one post per forum, so the forum can't have its own `post` too; changing it unpins the old one). |
| `[resources]` | `forum` for shared links: `/share-resource` from members (needs `[review]`), and straight from admins (`/server resource add`, web, CLI, API). The same link can't be shared twice; tags must be the forum's. |
| `[profile]` | `/profile` and a DM after onboarding (with `join.toml`, the join flow does both): the nickname is their name, plus the start of their surname only if someone else has the same name ("Ada L."); invisible characters are dropped and a name nobody can be told apart from is refused. Members with `company_roles` are also asked `company_question`. |
| `[web]` | `url`: the dashboard address shown in DMs and CLI hints (the web server itself uses `WEB_URL`); `share_device`: the Tailscale machine shared with new admins. |
| `[drift]` | `mode = "report"` (default) only reports changes made in Discord, in `log.bot`; `"undo"` puts them back. |
| `[recording]` | `/record` for meetings in voice and stage channels ([admin.md](admin.md#recording-meetings)). `channel`: where each recording's summary goes; `who`: role keys that may record besides admins; `max_hours` (default 3); `notice` (posted in the call's chat; `{server}`) and `join_notice` (DMed to late joiners; `{server}`, `{channel}`); `transcribe = "whisper-cpp"` (with `model`, a ggml file, and optionally `whisper`, the program, default `whisper-cli` in `/usr/local/bin`, `/usr/bin` or `/bin`; both absolute paths outside the config folder, to files owned by root that nobody else can change), `"api"` (`TRANSCRIBE_URL`) or `"none"` (unset: the API when `TRANSCRIBE_URL` is set); `language` (e.g. `"en"`; default detected); `min_free_gb` (default 2), `max_gb` per recording (default 4) and `max_total_gb` for all of them (default 20). A pushed change to this section waits for an admin's Approve; a change to `transcribe`, `model` or `whisper`, the server owner's. |

## Join flow

`join.toml` (fields: [`join.rs`](../crates/config/src/join.rs)). Discord's
onboarding asks every question of everyone; this one branches.

- `{button:join|Get started}` opens it, one step at a time, in a reply only
  they see (in the DM if pressed there); `/profile` too. Drafts last a day.
  Someone who has been through it before starts at their answers, with a
  **Change an answer…** menu and Confirm; a changed answer goes straight back
  to the summary unless it opens new questions. The yearly DM's **Update**
  starts at the yearly question.
- Steps are a menu (`multi`, `optional`) or up to 5 boxes; each option gives
  `roles` and may set `next`; `[[step.option.ask]]` adds boxes for that pick.
  `name`/`surname` set the nickname; `company` is saved with the profile.
- Confirm gives the answers' roles and `access_role`, takes back the flow's
  roles that no longer apply (never others, nor partner-sync ones), sets the
  nickname (not the owner's or anyone above the bot: they're told to) and
  saves the answers. Admins see them in `member info`. The summary lists the
  roles they'll get, but not `access_role`. The last message says
  `done_message` (default with the gate on: "The rest of the server is open
  now.").
- `check` refuses loops, steps nothing leads to, unknown roles, menus over 25
  options, forms over 5 boxes, and roles that are Discord's, the admin role,
  managed by members.toml or project leads, moderator-powered, able to see
  private channels, or given by partner sync (`role_map`). It warns when no
  post has the join button. Who holds the flow's roles isn't managed, so
  drift watch leaves them alone.
- With `join.toml`, onboarding.toml can't have questions; without an
  onboarding.toml, plan turns Discord's onboarding questions off (add an
  onboarding.toml without questions to keep its other settings). Screening
  (rules) is unchanged.
- `gate = true`: @everyone sees only `open_channels` (they must include the
  post with the button) and `access_role` sees the rest, as permissions: the
  plan takes View Channel from @everyone, gives it to `access_role`, and lets
  @everyone view each open channel; hidden categories stay hidden. Discord's
  onboarding must be off (`enabled = false`). New members who haven't
  finished after `remind_after_hours` (default 24) get one reminder DM, and
  `log.bot` lists them. `grandfather = true` gives `access_role` to members
  who were here before the gate.
- Nobody is DMed when the flow or the gate is turned on. Members already
  here hear about it from an announcement post (any post can have the
  button) and `/server join notify` (or `join notify`), once each.
- `yearly_refresh = "09-15"`: from that date each year, members whose answers
  include a `yearly = true` step get a DM to confirm it or update it (not in
  the year it's turned on, if the date has passed).

**Going live:** try it with `gate = false` (everyone still sees everything);
check the flow from a test account; then set `gate = true`, apply (the plan
says who stops seeing what), post the announcement, and run
`/server join notify`. `join status` shows who has finished and who waits.

## Projects

| Status | Gets |
|---|---|
| `proposed` | A post in the projects forum only. |
| `active` | `<Name> Team` and `<Name> Lead` roles, its own category of channels, and applications if `application.open`. |
| `past` | Roles kept as a record (renamed "· Contributor", "· Lead (past)"); channels archived. |

- A project's post gets its `tags`, its status (`active`, `past`, …) and
  its `kind` as forum tags, where the forum has a tag of that name (any
  case); `check` warns about a project tag the forum lacks.
- Leads (`leads`) hold the Lead role while active and keep it, renamed, once
  past. Accepted applicants get the Team role; those who said they'd like to
  lead get a **Make lead** button too (anyone else: `/server project lead`,
  the dashboard or `project lead add`). A new lead is DMed.
- Only active projects take applications: leaving `active` closes them, and
  accepting one for a project that isn't active is refused (`check` warns
  about open applications on one that isn't).
- Every status change asks first, in every front end. Active → past archives
  the channels; active → proposed archives them too, and making it active
  again starts new channels (past → active brings the old ones back).
- **Renaming** (`project rename <slug> <new>`, the dashboard, or
  `/server project rename`) keeps its roles, category, channels and post:
  their keys move in lock.toml, members.toml, mentions and `apply:` buttons
  in posts, partner role maps, the bot's own lock, pending applications and
  partner grants. Its text channels' names follow the slug on the next
  apply. Do it through the bot (signed in, the dashboard or `/server`): the
  bot keeps its own lock.toml, so a rename made on a laptop and pushed would
  make the project anew (the plan warns that lock.toml changes ids).

## Project ideas

`/propose-project` asks, by default, the idea's name, a one-line
description, its area (a menu of the ideas forum's tags, if it has any),
what it would do, and whether they'd like to lead it. `[ideas] questions`
sets the form instead, in order, 5 boxes at most:

```toml
[ideas]
forum = "ideas"
questions = [
  "name",
  "blurb",
  { key = "scope", ask = "What would it build?" },
  { key = "budget", ask = "Rough budget?", optional = true, hint = "£0 ok" },
  "lead",
]
post = "content/idea-post.md"   # optional
```

- Each question is written as a position's are: a string, or
  `{ ask, long, optional, hint }`, plus a `key` naming its answer (from a
  string, the key is made from the text: "Rough budget?" → `rough_budget`).
- The form's own questions are named by key alone: `name` (required: with
  `blurb`, the post's title), `blurb`, `area` (tags the post), `scope` (what
  it would do) and `lead` (whether they'd lead it; leave it out and every
  idea gets `needs_lead_tag`). Give them `ask`, `hint` and so on to word them
  your way.
- Every answer is on the review card and in the dashboard's queue. The
  accepted post shows the area, who proposed it, what it would do, each added
  question with its answer, and who'd lead it.
- `post` (optional) is a Markdown file the post is written from instead:
  `{answer:<key>}` is an answer (as they wrote it, links and headings shown
  as text), `{by}` mentions whoever proposed it, and `{lead}` says who'd
  lead it.
- `check` refuses a form over 5 boxes, one without `name` (or with it
  optional), two questions with one key, a key that isn't lowercase letters,
  digits and `_`, a missing `post` file, and a placeholder in it nothing
  fills.

## Who holds which roles

`members.toml` (role key → user ids) and project `leads` say who holds those
roles; roles not listed are never given or taken by the config.

- **Appointing** (Accept on a `/volunteer` card, `/server member appoint`,
  the web Members page, `member appoint`, or Restore) records them in the
  config and gives the role at once: only that person's roles change. An
  admin position (`admin = true`) also gives `admin_role`.
- **Taking someone out** takes the role away at once, and `admin_role` too
  unless another admin position (or `admin_role` in members.toml) still
  gives it. That ends their `/server`, web and CLI access and their
  Tailscale share.
- Who holds an admin position or `admin_role` is a sensitive setting
  ([security.md](security.md#what-a-push-cant-do)): appointing to or taking
  out of one is saved at once but takes effect, roles and all, when another
  admin presses **Approve** in `log.bot` (the reply says so).
- **`admin_role`** is managed only when members.toml lists it or a position
  has `admin = true`. Then a full apply makes its holders exactly the people
  in admin positions plus any listed under `admin_role`: anyone given it by
  hand loses it then. The plan names them in a warning, never takes it from
  everyone, and drift watch only reports changes to who holds it. So list
  every admin in members.toml.
- Someone who leaves holding listed roles is taken out; when they come back,
  a rejoin card (in `[review]`, else `log.bot`) offers **Restore** or
  **Forget** (also on the web Members page, `member restore|forget` and the
  API).
- A role members.toml manages isn't given directly when the config can't be
  saved, since drift watch (or the next apply) would take it back. No role
  is given that Discord has made more powerful than the config says.

## How changes are reconciled

`plan` lists every step and warns about what it leaves alone. Each edit says
who changed it since the last apply:

| Label | Meaning | Then |
|---|---|---|
| Changed in config | The files changed. | `apply` sends it. |
| Changed in Discord | Someone edited Discord. | `apply` puts it back; `pull` keeps it. |
| Both | Both sides changed. | `apply` keeps the config's; `pull` keeps Discord's. |

- `apply` refuses stale or tampered plans, runs one at a time, and saves
  after each step, so a failed apply resumes when run again.
- `pull` copies only Discord-side changes (names, colours, topics, tags,
  order, settings; permissions with `--permissions`) and lists the rest.
- **Plans in `log.bot`.** A merged commit is pulled within 2 minutes; an
  edit from the web or `/server` within 30 seconds. Its plan is posted once,
  with an **Apply** button, under "📥 Config changed: <commit subject>":
  grouped by Roles, Channels, Posts, Members and Server settings, with role
  moves summed up ("Treasurer above Member"). A config that doesn't load is
  reported and the bot keeps the last good one.
- **The bot's role on top.** Discord only lets a bot manage roles below its
  own. If any role it manages sits above the bot's role, the plan names
  them and leaves them alone until the server owner drags the bot's role to
  the top in Server Settings → Roles.
- **Syncing.** Its own commits that couldn't be pushed are rebased onto the
  repo's and pushed; if both sides changed `lock.toml`, the two are merged
  id by id (the bot's wins). A clash in any other file is left for a person
  ([deploy.md](deploy.md#recovering-from-failures)). A failed sync is said in
  `log.bot` after three in a row (about 6 minutes), then every hour until
  it's fixed, and once when it syncs again.
- `plan` and `apply` from a git checkout elsewhere fetch first and refuse if
  it's behind its remote, or lacks the commit `lock.toml` says was applied
  last: an old copy would undo changes others applied. Offline, they go
  ahead with a note.
- **Drift watch** checks about 15 seconds after Discord changes (and every
  20 minutes). With `mode = "undo"` it puts back a change made in Discord
  only when the audit log (its last 100 entries) shows a person, not the
  bot, changed that object, or gave or took that role; everything else is
  reported. If a config push or another apply lands meanwhile, it plans
  again 15 seconds later (up to three times). If it can't check, `log.bot`
  is told (again every hour). To change something for good, edit the
  config.
- **A lost lockfile:** if `lock.toml` is emptied, `plan` says so when
  `lock.toml.bak` knows more (`cp lock.toml.bak lock.toml`, or take it from
  git history). Without it, apply matches roles, categories and channels by
  name and the bot's posts and project threads by their text; a name several
  unknown things share is refused, and `--sweep` is refused while the lock
  knows less than half the config.
- **Not managed:** emojis and stickers, scheduled events, invites, webhooks,
  the onboarding Server Guide, role icons, forum layout (beyond `sort`) and default
  reactions, soundboard, vanity URL, widget, who holds roles not listed in
  members.toml or as project leads (join-flow roles included), and Discord's
  built-in AutoMod rules.
