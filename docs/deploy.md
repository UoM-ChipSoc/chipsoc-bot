# Deploying

## Settings

From the environment or a `.env` in the working directory. On a server set
up with `deploy.sh`, the secrets file (`.env.server`) becomes the server's
`.env` and the systemd units set the rest.

| Variable | Used by | Meaning |
|---|---|---|
| `DISCORD_TOKEN` | bot, web | The bot's token. Required. |
| `APPLICATION_ID` | bot, web | The application's id. Required. |
| `BOT_CONFIG` | all | Config directory (same as `--config`; default `config`). |
| `DATABASE_URL` | bot, web | Bot state, shared by both (default `sqlite:data/bot.db?mode=rwc`; the units set `sqlite:/var/lib/<service>/bot.db?mode=rwc`). |
| `CONFIG_AUTOSAVE` | bot, web | `1`: commit and push the config after every change (the units set it). |
| `FED_SIGNING_KEY` | bot | Partner-sync signing key, written by `fed keygen` (renamed with `key_env` in federation.toml). |
| Webhook variables | bot | One for each `webhook_env` / `share_webhook_env` in federation.toml. |
| `TAILSCALE_API_KEY` | bot | With `[web] share_device`: new admins get a one-time Tailscale share of the dashboard's machine. Expires; rotate it. |
| `DISCORD_CLIENT_SECRET` | web | With `WEB_URL`: admins sign in with Discord. |
| `WEB_URL` | web | The dashboard's public address, e.g. `https://vps.example.ts.net` (the web unit sets it from deploy.env). |
| `WEB_TOKEN` | web | A shared sign-in code (12+ characters) for trying the UI out: only on 127.0.0.1, no names (commits say "shared token"), no revoking. Ignored once Discord sign-in is set up. |
| `TRANSCRIBE_URL` | bot | An OpenAI-compatible transcription endpoint for recordings, e.g. `https://api.openai.com/v1` (`/audio/transcriptions` is added). Audio is sent there: pick one your privacy notice covers. |
| `TRANSCRIBE_API_KEY` | bot | Its key, if it needs one. |
| `TRANSCRIBE_MODEL` | bot | Its model (default `whisper-1`). |
| `RECORDINGS_DIR` | bot, web | Where recordings are kept (default: `recordings` beside the database, so `/var/lib/<service>/recordings` under the units; elsewhere, add it to both units' `ReadWritePaths`, and know that backups and `migrate` don't take it along). |
| `BOT_LOCAL` | CLI | `1`: this is the bot's own server, so commands use the bot's token and database here instead of a sign-in (same as `--local`; the units set it). |
| `RUST_LOG` | all | Log level (the units set `info`). |

The web UI never gets the partner-sync key, partner webhooks or
`TAILSCALE_API_KEY`: the web unit unsets them and the web UI drops them on
start. Git runs without the bot's secrets in its environment.

## deploy.sh

`scripts/deploy.sh` runs one bot (and its web UI) for one config repo on a
Linux server. The server needs systemd, root over ssh, git, sudo, Rust for
root (rustup; `~/.cargo/bin/cargo`), `sqlite3` for backups while running,
and Tailscale (installed and logged in) for the web UI. Your machine needs
Rust (the tests run first; `SKIP_TESTS=1` skips them), ssh and, optionally,
the GitHub CLI.

1. Fill in `deploy.env` (copy `deploy.env.example`).
2. `scripts/deploy.sh setup`, once: a sandboxed service user, a deploy key
   with write access to the config repo (added with `gh` if
   `CONFIG_REPO_GH` is set, else it asks you to), the config checkout, and
   the systemd units. With `WEB_URL` set it also installs the web unit and
   runs `tailscale serve` to publish it on the tailnet over HTTPS.
3. Write `.env.server` (see *Settings*) and run `scripts/deploy.sh secrets`:
   it uploads it as the server's root-only `.env` and restarts the bot and
   web UI, then waits for the bot to connect.
4. `scripts/deploy.sh` tests, builds on the server, checks the new release
   loads the config (pulled first), and switches to it. It then waits
   `HEALTH_WAIT` seconds for the bot to log that it connected and watches it
   stay up for `HEALTH_STABLE`; if not, it switches back to the release
   before and fails. The five newest releases are kept. Once it's up, it
   moves the git tag `deployed` (`DEPLOY_TAG`) to the commit it shipped and
   pushes it (`DEPLOY_TAG_REMOTE`); `rollback` moves it back (a failed push
   is a note, not a failed deploy). The config repo's CI checks the bot out
   at that tag ([security.md](security.md#ci)).

| Command | Does |
|---|---|
| `scripts/deploy.sh rollback` | Back to the release that ran before, if it loads the config as it is now. |
| `scripts/deploy.sh backup-setup` | Nightly encrypted off-site backups ([Backups](#backups)). |
| `scripts/deploy.sh backup [--latest]` | Backs up now (`--latest`: takes the last nightly one) and downloads it into `backups/`. |
| `scripts/deploy.sh backup-check` | Whether the newest off-site backup is the server's, recent, and decrypts. |
| `scripts/deploy.sh restore [FILE] [--db-only] [--unsigned]` | Puts a backup (default: the newest in `backups/`) back on the server, then restarts and checks the bot. |
| `scripts/deploy.sh migrate NEW_HOST` | Moves the bot to another server, about a minute offline ([Moving to another server](#moving-to-another-server)). |
| `scripts/deploy.sh migrate-back` | Moves it back. |
| `scripts/deploy.sh revoke PEER [KEY]` | `fed revoke` on the server ([partners.md](partners.md)). |
| `scripts/deploy.sh git ARGS` | git in the server's config checkout, as the bot's user with its deploy key. |
| `ssh HOST journalctl -u discord-bot -f` | The bot's logs (`discord-bot-web` for the web UI; names follow `SERVICE`). |

After a `migrate`, every command refuses to run on the server the bot left
(it says where the bot went); `--force`, anywhere on the command line, uses
that server again all the same.

You never deploy the config: the server pulls it every 2 minutes and pushes
back. The database (WAL mode, shared by the bot and web UI) holds the review
queue, audit and message logs, feeds, profiles and join answers, people who
left, partner state, Tailscale shares and what the bot trusts (its lock and
the approved settings).

A daily systemd timer, `<SERVICE>-backup.timer` (installed by `setup` and
every deploy; needs `sqlite3` on the server), copies the database into
`/var/lib/<SERVICE>/backups/` and keeps the 14 newest. Those copies live on
the same disk: set up [off-site backups](#backups) too. To run several
servers from one machine, give each its own `DIR` and `SERVICE` in a
separate deploy.env. `SERVICE` is lower-case letters, digits and `-`; `DIR`
an absolute path of plain characters (they go into commands on the server).

## Backups

`scripts/deploy.sh backup-setup` sets up nightly backups, kept off the
server, of everything the bot can't get back from elsewhere: the database,
the server's `.env` (its secrets), a list of the voice recordings and a note
of when, where, the release and config commit (the config itself is in git;
commits the bot hadn't pushed yet go in as a patch). Each one is encrypted
with [age](https://age-encryption.org) to your public key: the server, and
wherever the backups are kept, can't read them. Only your private key can,
and it never goes on the server.

Each one is also signed by the server, with a key of its own
(`DIR/backup/sign_key`, made by backup-setup; `NAME.sig` beside each
bundle). Your public age key is no secret, so anyone who can write where
the backups are kept could make a bundle that decrypts; the signature is
how restore and backup-check know a bundle is the server's. They check it
against `BACKUP_SIGNING_KEY` in deploy.env before decrypting anything.

1. Make a key on your own machine:
   ```sh
   mkdir -p ~/.config/age
   age-keygen -o ~/.config/age/discord-bot.key    # prints the public key, age1…
   ```
   Or with a passphrase (age asks for it on each restore and check):
   `age-keygen | age -p -a -o ~/.config/age/discord-bot.key` (the public
   key is printed). Keep a copy of the private key somewhere safe and
   separate, such as your password manager (it's one line): without it the
   backups can't be read, and with it anyone can read the bot's secrets.
   Never on the server or in a repo. A co-maintainer can make their own:
   list both public keys, separated by spaces.
2. In deploy.env: `BACKUP_AGE_RECIPIENT` (the public key),
   `BACKUP_AGE_IDENTITY` (the private key's file, on your machine),
   `BACKUP_TARGET` (below), and optionally `BACKUP_KEEP` (default 30),
   `BACKUP_CALENDAR` (systemd `OnCalendar`, quoted, e.g.
   `'*-*-* 04:00'`; default `daily`, plus up to an hour's random delay) and
   `BACKUP_RECORDINGS=1` (put the voice recordings in each bundle too; each
   bundle then holds all of them, and the server needs room for them twice
   while it makes one).
3. `scripts/deploy.sh backup-setup`: installs `age` (and `rclone`) on the
   server if they're missing (it asks), writes the settings to
   `DIR/backup/` (root only), makes the signing key and shows it (it offers
   to add `BACKUP_SIGNING_KEY` to deploy.env; keep that line with your
   private key's copy), sets up the destination, makes a first backup and
   checks it with your key. Run it again after changing these settings. If
   the server's signing key ever differs from `BACKUP_SIGNING_KEY`, it stops
   and says so: find out why before you trust the new one.

| `BACKUP_TARGET` | Kept | backup-setup |
|---|---|---|
| `rclone:REMOTE:PATH` | Anywhere [rclone](https://rclone.org) reaches: S3, B2, Google Drive, SFTP… | Uses REMOTE from the server's own `DIR/backup/rclone.conf`; if it isn't there, offers to copy your machine's remote of that name. The server keeps these credentials, so whoever takes the server over has them: give it a key for one bucket or folder that can write, list and read but not delete or overwrite (B2: an application key without `deleteFiles`; S3: a policy without `s3:DeleteObject`, with versioning or Object Lock), and expire old bundles with the bucket's lifecycle rules. With such a key, pruning only says it couldn't. For remotes that sign in through a browser, make it on the server: `ssh -t HOST rclone config --config DIR/backup/rclone.conf`. |
| `USER@HOST:PATH` | A machine you can ssh to (a home server, a NAS) | PATH is relative to that account's home, or absolute (a leading `~/` works quoted: `'nas@home:~/backups'`; unquoted, deploy.env makes it your own home). Makes the server a key of its own (`DIR/backup/ssh_key`) and trusts the destination's host key as your machine knows it (else it shows the one the server sees, and asks). Then, over your own ssh, it puts [`deploy/backup-dest.sh`](../deploy/backup-dest.sh) at the destination (`~/.ssh/<SERVICE>-backup-dest`) and lets the key in only for that (`restrict,command="…"` in `~/.ssh/authorized_keys`, replacing any older line for the key): the server can add bundles to PATH but not replace or delete them, list and read them, and prune, which deletes only bundles past the newest `BACKUP_KEEP` that are also more than `BACKUP_KEEP` days old. No shell there. Port 22; an account just for backups is best (with a disk quota: the key can still add bundles). |
| `pull` | On the server, for your machine to download | Nothing. Download them daily, e.g. with cron on your machine: `30 9 * * * cd /path/to/this/repo && scripts/deploy.sh backup --latest >/dev/null` (ssh has to work unattended). |

Each night `<SERVICE>-backup.service` copies the database as before, then,
as root, `DIR/backup-offsite` bundles that copy with the `.env`, a note
(`info.txt`: which bundle, when, release, config commit) and the list of
recordings (`recordings.txt`; with `BACKUP_RECORDINGS=1` the recordings
too, `recordings.tar`) into `<SERVICE>-<UTC time>.tar.gz.age`, signs it,
sends both, checks it arrived whole and keeps the newest `BACKUP_KEEP` there.
The server keeps the newest two in `DIR/backup/bundles` (`BACKUP_KEEP` with
`pull`).

- `scripts/deploy.sh backup` backs up now and downloads the bundle and its
  `.sig` into `backups/` (still encrypted; the newest `BACKUP_KEEP` are kept
  there).
  `--latest` downloads the newest there is, without making one. Before
  backup-setup it downloads the plain database, as it always did.
- `scripts/deploy.sh backup-check` says whether the last run worked and
  the timer is on. Then it fetches the newest bundle from the destination
  itself (names dated in the future are passed over), checks the server's
  signature, and decrypts it with your key once, in a private folder in
  memory (`XDG_RUNTIME_DIR`, or `/run/user/<uid>`; else in `backups/`, never
  the shared temporary folder; removed afterwards, and leftovers of a killed
  run the next time). From that it unpacks only the database, to check its
  integrity, and `info.txt`; the secrets are read as they stream past (that
  `DISCORD_TOKEN` is there) and never written. The bundle must be younger
  than `BACKUP_MAX_AGE_HOURS` (26) by the time signed inside it, not by its
  name, and say it is the bundle it's named as. It fails otherwise, so it
  can run from cron too.
- `scripts/deploy.sh restore [FILE] [--db-only]` (FILE: the newest in
  `backups/` if not given) needs `FILE.sig` beside it and checks it first;
  `--unsigned` restores one without (only one made before backups were
  signed, that you know is yours: it asks). It decrypts on your machine
  straight into ssh (the secrets aren't written to your disk) and unpacks
  on the server: only the files a bundle has, as plain root-owned files
  (links and anything else refuse the whole restore). It checks the
  database, and that the secrets have `DISCORD_TOKEN` and only settings the
  bot uses, one plain `KEY=value` a line: the ones in *Settings*, the ones
  in your `.env.server` and the server's current `.env`, those
  `federation.toml` names, and any in `BACKUP_ENV_KEYS` (deploy.env). Never
  ones that change how the bot, git or ssh run (`GIT_SSH_COMMAND`,
  `LD_PRELOAD`, `PATH`…) or that the units set (`DATABASE_URL`…). It shows
  what the backup holds and asks. Then it stops the bot and web UI, keeps
  the current database (`/var/lib/<SERVICE>/backups/before-restore-*.db`)
  and `.env` (`DIR/.env.before-restore-*`; the newest 3 of each), puts the
  backup's in place (files 0600, the state folder 0700) and its recordings,
  if it has them, beside the server's (as the bot's user), starts them and
  runs the deploy health check. `--db-only` leaves the secrets alone; a
  `.db` file (an older plain backup) restores the database alone. Config
  commits the bot hadn't pushed are left in `DIR/config-unpushed-*.patch`
  for you to `git am`.

## Voice recordings

`/record` ([admin.md](admin.md#recording-meetings)) joins calls with
[songbird](https://github.com/serenity-rs/songbird) 0.6, which speaks
Discord's end-to-end encrypted voice (DAVE, required for every call since
March 2026). Building it needs `cmake` (for the Opus codec; `deploy.sh`
asks to install it) or the system's `libopus-dev` and `pkg-config`.

Recordings are Ogg Opus files (about 15 MB an hour per speaker) under
`/var/lib/<service>/recordings/<server id>/<date>-<time>-<channel>/`, with
`meta.json` and `transcript.md`. They stay on the server until an admin
deletes them. The nightly backups list them (`recordings.txt`) but hold them
only with `BACKUP_RECORDINGS=1`; `migrate` moves them.

Transcripts, optional, either way:

- **whisper.cpp on the server** (nothing leaves it). Install by hand, as
  root: build [whisper.cpp](https://github.com/ggml-org/whisper.cpp)
  (`cmake -B build && cmake --build build -j --config Release`), copy
  `build/bin/whisper-cli` to `/usr/local/bin`, and download a model
  (`models/download-ggml-model.sh base.en`) to somewhere the bot's sandbox
  can read, not under `/home`: say `/opt/whisper/ggml-base.en.bin`. Both
  must be owned by root and writable by nobody else (folders too), or the
  bot won't use them. Then in bot.toml: `transcribe = "whisper-cpp"`,
  `model = "/opt/whisper/ggml-base.en.bin"`; the server owner approves it.
  `base.en` is a good start on a small server (an hour of speech takes
  some minutes of CPU); `small.en` is better and slower.
- **A transcription service**: set `TRANSCRIBE_URL` (and
  `TRANSCRIBE_API_KEY`) in the secrets file (`deploy.sh secrets`).

ffmpeg isn't needed: the bot encodes and cuts the audio itself. Install it
only to convert downloads (`ffmpeg -i mix.ogg mix.mp3`).

Known limits: songbird's DAVE support is new, and its tracker reports calls
where receiving stalls after the encryption re-keys (someone joining) until
the bot rejoins (serenity-rs/songbird#310), and distorted audio from some
clients (#315). If a track comes out silent, stop and start again.

## Moving to another server

`scripts/deploy.sh migrate NEW_HOST` moves the bot from `HOST` to
`NEW_HOST` (an ssh destination you can log into as root, like `HOST`),
with the bot offline for about a minute. Your machine must reach both; they
needn't reach each other. Everything goes through your ssh connections,
the secrets host to host without touching your disk.

1. **Checks** both: reachable, `NEW_HOST` is another machine with systemd,
   the bot doesn't already run there, enough disk (the recordings
   included), `sqlite3`, `git`,
   `sudo`, `curl` and `jq` (and `age`, `rclone` if backups use them) there
   or installed (it asks), and Tailscale up on `NEW_HOST` if `WEB_URL` is
   set. If `NEW_HOST` has some of an install already, it asks first.
2. **Sets up `NEW_HOST`** while the bot still runs: user and folders; the
   `.env`, deploy key and backup settings copied over; the config cloned
   (which proves the deploy key works there); the running release copied
   (or built there, if it won't run); the units installed but not enabled;
   the config checked; and a first copy of the voice recordings
   (`/var/lib/<SERVICE>/recordings`), which can take a while.
3. **Cuts over** once you say so: stops, disables and masks the bot, web UI
   and backup units on `HOST` (their files kept as `*.moved`) and leaves
   `DIR/MOVED-TO` there saying where the bot went; copies the database
   (sqlite's backup, checked by hash and integrity check), the config
   checkout as it is (with any commits not pushed yet) and the recordings
   changed or deleted since the first copy (then both must hold the same
   files); then enables and starts them on `NEW_HOST` with the deploy
   health check (`HEALTH_WAIT`, `HEALTH_STABLE`). If the bot isn't healthy
   there, it stops it there and, only once that's sure, starts `HOST` again
   as it was, and fails: fix the cause and run migrate again (it copies
   everything again). If it can't make sure the bot stopped on `NEW_HOST`,
   it starts nothing and says so: look, then run migrate again.
4. **Moves the dashboard's name**: if `WEB_URL` is `HOST`'s tailnet name,
   it renames `HOST` to `<name>-old` and straight after `NEW_HOST` to
   `<name>` with the Tailscale API, run on `NEW_HOST` with the
   `TAILSCALE_API_KEY` in its `.env` (the key never comes to your machine),
   checks with the API that `NEW_HOST` has the name, then runs `tailscale
   serve` there. `WEB_URL` and the Discord redirect stay the same. Without
   a working key, or if anything else has the name, it says so loudly and
   how to rename them by hand (admin console → Machines). Between the two
   renames (well under a second) the name is free, and any machine on the
   tailnet could take it, and with it the dashboard's sign-ins: do it when
   you trust the tailnet's members, or keep the name out of members' reach
   (a Tailscale Service, or a tagged machine with `tagOwners` only admins
   are in).
5. Asks to set `HOST` in deploy.env to `NEW_HOST`, and sums up.

It notes how far it got in `.deploy-state/migrate-<SERVICE>` on your
machine: if it's interrupted, run the same command again and it carries on
(an unfinished cut-over starts again from stopping `HOST`; if the last run
had started the bot on `NEW_HOST` already, it carries on only if `HOST`'s
database is still the one it copied, and else stops and asks you to choose
which to keep). Once the bot has moved, every command refuses to run on
the old `HOST`, from any checkout: the server itself has `DIR/MOVED-TO`, its
units are masked (an older deploy.sh writing them writes into `/dev/null`)
and won't start while that file is there. `--force` on a command uses that
server again (it asks, unmasks the units and removes the file).

`scripts/deploy.sh migrate-back` moves it back the same way, with the
newer database (nothing since the move is lost), and swaps the names back
(the newer machine becomes `<name>-new`).

After a move:
- Admins' Tailscale shares were of the old machine: each gets a new link
  with `/server web`. Dashboard and CLI sign-ins move with the database.
- The deploy key on GitHub is the same one, still named "<service> on
  <old host>".
- Look at the Recordings page on the new server's dashboard before you
  delete anything on the old one.
- When you're sure (after a few days), remove the old install,
  `ssh OLD 'rm -rf DIR /var/lib/SERVICE /etc/systemd/system/SERVICE*'`,
  and the old machine from the tailnet. migrate-back can't work after that.

## Web dashboard over Tailscale

The web UI listens on 127.0.0.1 and `tailscale serve` publishes it at
`https://<machine>.<tailnet>.ts.net` (`WEB_URL`, and `[web] url`).

**Sign in with Discord.** Set `DISCORD_CLIENT_SECRET` and `WEB_URL`. In the
Developer Portal, under **OAuth2**, add `{WEB_URL}/auth/discord` to
Redirects and copy the Client Secret. Admin access is checked again at most
every 5 minutes. An `http://` `WEB_URL` is only allowed on a loopback
`--addr`. Without Discord sign-in the web UI listens only on 127.0.0.1
(`WEB_TOKEN` optional). Browser sessions and CLI sign-ins are kept in the
bot's database (only a hash of each token; cookies are encrypted with a
key kept there too), so restarting the web UI (every deploy) signs nobody
out. The shared-code sign-in is still kept in memory only.

**Sharing it with new admins.**
1. In the tailnet policy, let people the machine is shared with reach port
   443 on it and nothing else, e.g. with a grant
   `{"src": ["autogroup:shared"], "dst": ["<the machine's tag or host>"], "ip": ["tcp:443"]}`
   and no rule giving `autogroup:shared` (or `*`) more. Without that, a
   shared machine is reachable on every port, ssh included.
2. Make an API access token (Settings → Keys) and put it in `.env.server` as
   `TAILSCALE_API_KEY`. These expire (90 days at most): note the date and
   rotate it. A missing or refused key is said once in `log.bot`, and every
   share that couldn't be revoked each time.
3. Set `[web] share_device` to the machine's name.

Then the welcome DM every new admin gets also carries a one-time link that
shares the machine, and `/server web` sends a fresh one. The share is
revoked when they stop being an admin or leave. An hourly sweep catches
changes the bot missed: it revokes, and sends links to admins who have none
(those who became admins through a role's permissions). If more than five
would get a link at once, it sends none and says so in `log.bot` (a role,
or @everyone, probably has Manage Server by mistake). After the first
revoke, check Machines → Share in the Tailscale console and revoke by hand
if they're still listed; sign-in still checks admin either way.

## Rotating secrets

Put the new value in `.env.server` and run `scripts/deploy.sh secrets` (it
restarts the bot and web UI).

| Secret | Where to get a new one | Also |
|---|---|---|
| `DISCORD_TOKEN` | Developer Portal → Bot → Reset Token (the old one stops at once) | Then `secrets` at once. |
| `DISCORD_CLIENT_SECRET` | Developer Portal → OAuth2 → Reset Secret | Web sign-ins carry on. |
| `WEB_TOKEN` | Any new 12+ character code | Only without Discord sign-in. |
| `TAILSCALE_API_KEY` | Tailscale admin console → Settings → Keys | Before it expires. |
| `FED_SIGNING_KEY` | `fed keygen --force --env .env.server` (with `--config`) | Send partners the new public key and ask them to keep the old one for a day (add, don't replace), or what you vouched for is revoked until the next full sync. |
| Partner webhooks | Ask the partner | |
| The server's deploy key | Delete it on GitHub, then `ssh HOST rm DIR/.ssh/deploy_key*` and `scripts/deploy.sh setup` | |
| The backups' age key | `age-keygen`; the new public key in `BACKUP_AGE_RECIPIENT`, then `backup-setup` | Keep the old private key as long as backups made with it are kept. A leaked one exposes the secrets in every backup made with it: rotate those too. |
| The server's backup signing key | `ssh HOST rm DIR/backup/sign_key*`, take `BACKUP_SIGNING_KEY` out of deploy.env, then `backup-setup` | Bundles made before are signed by the old key: keep it (commented) to restore them, or `--unsigned`. |
| The server's backup ssh key | `ssh HOST rm DIR/backup/ssh_key*`, then `backup-setup` (it replaces the destination's line) | Remove the old line at the destination if the server is gone. |
| CI's token (if CI plans), `DISCORD_WEBHOOK` | The CI bot's Reset Token; a new webhook | The config repo's Actions secrets. |

**A new maintainer** gets `deploy.env`, `.env.server` (or rotates
everything into a new one), root ssh to the server, admin of the config
repo (its Actions secrets and deploy keys: the bot's is "<service> on
<host>"), ownership of the Developer Portal application (or its team), and
ownership of the tailnet (and with it the Tailscale API key), and the
backups' age private key (or they make their own and run `backup-setup`).
Then they rotate the secrets and remove the old maintainer's access.

## Recovering from failures

- *The bot won't start or connect:* `scripts/deploy.sh` already switched
  back if a new release didn't come up. Look at
  `ssh HOST journalctl -u discord-bot -n 50`. A refused token means
  rotating it; a config that doesn't load is said in `log.bot` (push a fix);
  `scripts/deploy.sh rollback` goes back one release.
- *The config can't sync (`log.bot` says so every hour):* the bot's
  unpushed commit clashes with the repo's. See it with
  `scripts/deploy.sh git fetch` and
  `scripts/deploy.sh git log --oneline origin/main..HEAD`, and save it with
  `scripts/deploy.sh git diff origin/main HEAD > clash.patch`. Make the same
  change in the repo by hand (if it's still wanted), then
  `scripts/deploy.sh git reset --hard origin/main`; the bot syncs again
  within 2 minutes. Its lock is kept in its database, so no ids are lost.
- *Commands are missing in Discord:* `log.bot` says why Discord refused them;
  fix the cause (often a config change) and restart the bot.
- *A lost or broken `lock.toml`* in the repo: the bot uses its own copy and
  writes it back on its next apply; for a checkout elsewhere, see *A lost
  lockfile* in [config.md](config.md#how-changes-are-reconciled).
- *The bot's role was moved below others:* plans warn; the server owner
  drags it back to the top.
- *A lost server:* set `HOST` to a new one, then `scripts/deploy.sh setup`
  (add its new deploy key), `scripts/deploy.sh restore` (database and
  secrets from the newest bundle in `backups/`: with `pull` your machine has
  them; else copy the newest and its `.sig` from the destination there
  first, with `rclone copy` or `scp`; restore checks the signature against
  `BACKUP_SIGNING_KEY`), and `scripts/deploy.sh`. Then `backup-setup`: the
  new server makes a new signing key, and backup-setup stops because it
  differs from `BACKUP_SIGNING_KEY`; put the new one there (the old one
  still checks the old bundles: keep it in a comment). Without a backup,
  `secrets` instead of restore:
  the bot starts afresh, the queue, logs and partner state are gone, and it
  approves the repo's sensitive settings as they are, so check them first.
  A server that still runs: `migrate` ([Moving to another server](#moving-to-another-server)).
- *A lost `.env.server`:* rotate every secret above, make a new partner key
  and ask partners for new webhooks.
