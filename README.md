# chipsoc-bot

Run a Discord server from config files in git, plus the bot that runs it day
to day. Any community can use it.

## What it does

- The server lives as TOML and Markdown files in a git repo.
- `plan` lists every change, `apply` makes Discord match, and `pull` copies
  changes made in Discord back into the files.
- Config changes show a plan first. Removed channels and roles are archived
  (hidden, history kept), never deleted unless you pass `--delete`.
- `lock.toml` maps config keys to Discord ids, so renames are edits.
- File order is Discord order. Channels inherit their category's permissions.
- The bot (`run`) adds a join flow, forms, a review queue, events, feeds,
  logs, drift watch and partner sync, and posts each config change's plan
  for admins to apply. Changes made in Discord instead are reported in the
  bot log (`[drift] mode = "undo"` puts them back).
- Sensitive changes (who's an admin, what the bot trusts) wait for an
  admin's **Approve**; whoever made one through the bot can't approve it
  themselves (the server owner can). See [docs/security.md](docs/security.md).
- `/record` records meetings in voice channels (with a notice to everyone
  in the call), transcribes them and keeps them for the committee.
- Four front ends share one backend: Discord (`/server` and buttons), a web
  dashboard, a terminal UI and a CLI.

## Quick start

Build with `cargo build --release`.

1. In the Discord Developer Portal, create an application and a bot. Turn on
   the **Server Members** and **Message Content** intents. Set Install Link to
   *None* and turn **Public Bot** off.
2. Invite it with Administrator:
   `https://discord.com/oauth2/authorize?client_id=<APPLICATION_ID>&permissions=8&scope=bot+applications.commands`
3. The server owner drags the bot's role to the top, once (bots only manage
   roles below their own).
4. Write a `.env` (see [Settings](docs/deploy.md#settings); never commit it)
   with `BOT_LOCAL=1` on the machine that runs the bot. Elsewhere, people use
   `login`.
5. Run `chipsoc-bot export config` to write your existing server as files,
   or `chipsoc-bot template <code> config` to start from a discord.new
   template, or copy [`examples/minimal`](examples/minimal) (a category plus
   the archive, a log channel, a position, a short join flow; `check` passes
   on it). Add a `bot.toml`, and put the directory in its own git repo (keep
   it private with few writers, or protect its main branch:
   [security](docs/security.md)).
6. Run `check`, `plan` and `apply`, then commit everything, including
   `lock.toml`.
7. Run `chipsoc-bot run`, or deploy it ([docs/deploy.md](docs/deploy.md)).

You need a server owner who is around: some things only the owner may do
or approve (deleting, changing who's an admin, undoing changes; the list is
in [docs/admin.md](docs/admin.md#who-is-an-admin)). Turn on 2FA for the
GitHub organisation and Discord's 2FA requirement for moderation.

## Docs

| | |
|---|---|
| [docs/admin.md](docs/admin.md) | For the committee: everyday jobs, commands in Discord, the dashboard, the terminal UI and CLI. |
| [docs/config.md](docs/config.md) | The config files, bot.toml, the join flow, projects, who holds which roles, how plans work. |
| [docs/deploy.md](docs/deploy.md) | Settings, deploying, encrypted off-site backups and restoring, moving to another server (`deploy.sh migrate`), the dashboard over Tailscale, rotating secrets, recovering from failures. |
| [docs/security.md](docs/security.md) | Who can do what, what a push can't do, sign-ins, CI. |
| [docs/partners.md](docs/partners.md) | Partner sync between servers: roles, project catalogues, and shared resources, questions, feeds, announcements and events. |

## Development

Crates: `model` (shared types), `config` (files, `check`, edits and `ops`,
trust, the write lock), `diff` (pure planner), `discord` (REST, export,
fake), `engine` (plan/apply/pull, git), `store` (SQLite), `admin` (review
queue, people, feeds, resources, join, audit log: the shared backend), `api`
(the web API's types), `client` (the CLI and TUI's way to the web API, or to
local files), `fedproto` (signed messages),
`voice` (call capture, Opus/Ogg files, transcripts), `runtime` (the bot),
`tui`, `web`, `cli` (the `chipsoc-bot` binary).

`cargo test --workspace` runs against an in-memory fake Discord
(`crates/discord/src/fake.rs`), never the real one.
