# Security

- The bot's token can do anything an Administrator can, and never expires.
  It lives only in the server's root-only `.env` (read by systemd; the
  service user can't read the file) and the secrets file on the maintainer's
  machine. Admins work through Discord, the dashboard or `login`, never with
  the token.
- Keep `.env.server` in the committee's password manager: it's the only
  copy of the bot's secrets.
- **Protect the config repo's main branch.** Require a pull request with a
  review (a GitHub ruleset), and let the bot's deploy key bypass it (rulesets
  → bypass list → Deploy keys): the bot pushes `lock.toml` after applies and
  the edits made in Discord and on the dashboard. Pushes can't take over
  the bot (below), but a review keeps surprises out of the plans admins are
  asked to apply.
- Who counts as an admin, and what needs a second one:
  [admin.md](admin.md#who-is-an-admin).

## What a push can't do

Anyone who can push to the config repo can propose anything, but:

- The bot keeps its own `lock.toml` (ids, what was applied and from which
  commit) in its database and uses that, not the repo's: a pushed lock.toml
  may add keys for ids no key has, a changed id is refused (and said in
  `log.bot`), and a dropped key waits for an admin's approval. So a pushed
  change always shows as "changed in config", which needs an admin's Apply.
- Sensitive settings wait for an admin's **Approve** in `log.bot`:
  `admin_role`, which positions are `admin`, who members.toml lists for those
  roles, `[drift] mode`, `[web]`, the log channels, `[recording]` (whether
  calls may be recorded, by whom, and what people are told), and federation
  `key_env` and partners (ids, keys, `role_map`, and what we `send` and
  `accept`). Until then the bot and the web UI keep the approved ones; other
  changes go ahead as usual. Approving takes an admin under the approved
  settings other than whoever the commits name ("Made by alice": the name
  the bot signed them with, else git's author); the server owner may approve
  their own. A change to `admin_role`, admin positions, `key_env`, partners,
  `[drift] mode` or how recordings are transcribed (`[recording]`
  `transcribe`, `model`, `whisper`), or one taking a role from the owner,
  only the server owner approves; the card names the exact program and
  model. Approved changes to who holds admin roles are applied at once.
  **Reject** keeps the approved settings until the files are changed back.
- The first run (and a fresh database) approves what's there only if the
  config's admins are who Discord says: each admin role (`admin_role`,
  admin positions' roles) is in lock.toml and in Discord, and whoever
  members.toml lists for it is who holds it there. Otherwise everything
  sensitive waits for the server owner's Approve, and until then the bot
  uses only the files' log channels and `[web]`: no admin role, partners or
  recording, and drift is only reported.
- When Discord can't say who owns the server, whatever only the owner may
  do is refused ("Couldn't check who owns the server just now").
- Nothing in the config runs code: no hooks, templates are plain text, and
  git only ever gets commit ids from the bot. A config folder with a symlink
  in it is refused (it could make the bot read or write a file outside the
  config, `.git/config` say). `[recording] whisper` and `model` must be
  absolute paths outside the config folder, to files (not symlinks) owned
  by root that nobody else can change, in folders likewise; whisper.cpp
  runs with none of the bot's environment (no token or keys).

## Sign-ins

`login` waits up to 5 minutes. After Discord, the bot's page says which
computer is asking and as whom, with Continue and Cancel. Continue sends a
one-time code to 127.0.0.1 on the browser's own machine, worth a token only
with a secret the CLI that started the sign-in keeps, so a `login` link
someone else sends you signs nobody in for them (unless you send them the
address your browser lands on). `--paste` shows the code instead, valid 5
minutes, with a warning to paste it only into a terminal you started. The
token file is mode 0600; each new sign-in is noted in `log.bot`.

## CI

- **CI checks with the bot that runs.** Check out this repo at `ref:
  deployed` (the tag `deploy.sh` moves), not its default branch, so CI
  accepts the keys the running bot accepts. Use `fetch-depth: 0` for the
  config, since `plan` looks for the commit lock.toml says was applied.
  Commits the bot makes that change only lock.toml say `[skip ci]`; to skip
  more, add `paths-ignore: [lock.toml]` to the workflow's `push` trigger.
- A CI that only runs `check` needs no token.
- **If CI runs `plan`,** give it a second bot invited with
  `permissions=66560&scope=bot` (View Channel, Read Message History) and
  both intents, its role just below the bot's: anyone who can push a branch
  can make a workflow print its token. Without Manage Server it can't read
  AutoMod; the plan says what it left out.
