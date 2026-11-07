#!/usr/bin/env bash
# Deploy the bot to a server. Settings: deploy.env (copy deploy.env.example).
# More in docs/deploy.md.
#
#   scripts/deploy.sh setup      first time: user, folders, config checkout + deploy key, service
#   scripts/deploy.sh secrets    upload $SECRETS as the server's .env (first time, or when secrets change)
#                                and restart the bot and web UI so they use it
#   scripts/deploy.sh            test, build a new release (on the server as an unprivileged
#                                build user, or here: DEPLOY_BUILD), switch to it, restart;
#                                if it doesn't stay up and connected, switch back and fail;
#                                once it's up, move the git tag $DEPLOY_TAG to it and push the tag
#   scripts/deploy.sh rollback   switch back to the previous release (if it can load the config)
#
#   scripts/deploy.sh backup-setup  nightly encrypted off-site backups of the database, the
#                                server's .env and the config commit (BACKUP_* in deploy.env)
#   scripts/deploy.sh backup [--latest]  back up now (--latest: take the last nightly one) and
#                                download it into backups/ (encrypted once backup-setup ran;
#                                before that, the plain database)
#   scripts/deploy.sh backup-check  is the newest off-site backup recent, and does it decrypt?
#   scripts/deploy.sh restore [F] [--db-only] [--unsigned] [--renamed]  put backup F (default:
#                                the newest in backups/) on the server: database and secrets, then
#                                restart and check; F must carry the server's signature (--unsigned:
#                                older ones) and be the bundle it's named as (--renamed: asks instead)
#
#   scripts/deploy.sh monitor-setup [--off]  a timer on the server that checks the bot's heartbeat
#                                every minute and says when it's down (MONITOR_* in deploy.env and
#                                the server's .env)
#
#   scripts/deploy.sh migrate NEW   move the bot to server NEW (an ssh destination, root):
#                                about a minute offline; goes back by itself if it fails there
#   scripts/deploy.sh migrate-back  move it back to the server it came from
#
#   scripts/deploy.sh revoke P [K]  on the server, take back everything partner P gave
#                                (only what key id K signed, if given): `fed revoke` with the bot's settings
#   scripts/deploy.sh git ARGS   git in the server's config checkout, as the bot's user with its
#                                deploy key (e.g. `git status`, when the config can't sync)
#
# The config isn't deployed from here: the bot pulls it from CONFIG_REPO.
# ASSUME_YES=1 answers yes to every question (for scripts). After a migrate the
# old server refuses every command; --force (anywhere) uses it again all the same.
# shellcheck disable=SC2029 # remote commands are put together here, on purpose
set -euo pipefail
# Errors stop $(…) too (bash 4.4 and later).
shopt -s inherit_errexit 2> /dev/null || true
# Character ranges in the checks here mean ASCII (this shell's own matching only).
LC_ALL=C
cd "$(dirname "$0")/.."
FORCE=
args=()
for a in "$@"; do if [ "$a" = --force ]; then FORCE=1; else args+=("$a"); fi; done
set -- ${args[@]+"${args[@]}"}
[ -f deploy.env ] || { echo "copy deploy.env.example to deploy.env and fill it in" >&2; exit 1; }
# shellcheck disable=SC1091
source deploy.env
: "${HOST:?set HOST in deploy.env}" "${CONFIG_REPO:?set CONFIG_REPO in deploy.env}"
DIR=${DIR:-/opt/discord-bot}
SERVICE=${SERVICE:-discord-bot}
CONFIG_BRANCH=${CONFIG_BRANCH:-main}
SECRETS=${SECRETS:-.env.server}
WEB_PORT=${WEB_PORT:-8080}
# After a restart: seconds to wait for the bot to say it connected, then to watch that it stays up.
HEALTH_WAIT=${HEALTH_WAIT:-30}
HEALTH_STABLE=${HEALTH_STABLE:-30}
# Git tag (in this repo) moved to the release that runs, so the config repo's
# CI can check out the bot at `ref: deployed`. Empty: don't tag.
DEPLOY_TAG=${DEPLOY_TAG-deployed}
DEPLOY_TAG_REMOTE=${DEPLOY_TAG_REMOTE:-origin}
# Where releases are built: server (as its own unprivileged user there) or
# local (here, then copied over). BUILD_JOBS: cargo's -j (default: every CPU).
DEPLOY_BUILD=${DEPLOY_BUILD:-server}
BUILD_JOBS=${BUILD_JOBS:-}
# monitor-setup: minutes without a heartbeat before it says the bot is down,
# hours before it says so again (0: once), and optionally a mail address and
# a role to ping in the webhook's message.
MONITOR_STALE_MINUTES=${MONITOR_STALE_MINUTES:-5}
MONITOR_REPEAT_HOURS=${MONITOR_REPEAT_HOURS:-6}
MONITOR_EMAIL=${MONITOR_EMAIL:-}
MONITOR_ROLE=${MONITOR_ROLE:-}
# Off-site backups (backup-setup): who can decrypt them, where they go, how many are kept.
BACKUP_AGE_RECIPIENT=${BACKUP_AGE_RECIPIENT:-}
BACKUP_TARGET=${BACKUP_TARGET:-}
BACKUP_KEEP=${BACKUP_KEEP:-30}
BACKUP_CALENDAR=${BACKUP_CALENDAR:-daily}
BACKUP_MAX_AGE_HOURS=${BACKUP_MAX_AGE_HOURS:-26}
# 1: bundles carry the voice recordings too (else only a list of them).
BACKUP_RECORDINGS=${BACKUP_RECORDINGS:-}
# The server's backup signing key (backup-setup records it): restore and
# backup-check only trust bundles it signed.
BACKUP_SIGNING_KEY=${BACKUP_SIGNING_KEY:-}
# Settings, besides those the bot is known to use, a restored .env may hold.
BACKUP_ENV_KEYS=${BACKUP_ENV_KEYS:-}
# On this machine only: the age private key, for restore and backup-check.
BACKUP_AGE_IDENTITY=${BACKUP_AGE_IDENTITY:-}
BACKUP_AGE_IDENTITY=${BACKUP_AGE_IDENTITY/#\~/$HOME}

say() { echo "› $*"; }
note() { echo "note: $*" >&2; }
die() { echo "$*" >&2; exit 1; }

# These go into commands run on the servers, so they're held to plain characters.
valid() { [[ $2 =~ $3 ]] || die "$1 in deploy.env: \"$2\" isn't allowed here (${4:-letters, digits and . _ - only})"; }
valid SERVICE "$SERVICE" '^[a-z][a-z0-9-]*$' "lower-case letters, digits and -, starting with a letter"
valid DIR "$DIR" '^/[A-Za-z0-9._/-]*[A-Za-z0-9._-]$' "an absolute path of letters, digits and . _ - /"
valid HOST "$HOST" '^[A-Za-z0-9._@][A-Za-z0-9._@:-]*$' "an ssh destination such as root@my-server"
valid CONFIG_BRANCH "$CONFIG_BRANCH" '^[A-Za-z0-9._/-]+$'
valid WEB_PORT "$WEB_PORT" '^[0-9]+$' "a port number"
valid BACKUP_CALENDAR "$BACKUP_CALENDAR" '^[A-Za-z0-9 *:,./~-]+$' "a systemd OnCalendar time"
valid DEPLOY_BUILD "$DEPLOY_BUILD" '^(server|local)$' "server or local"
[ -z "$BUILD_JOBS" ] || valid BUILD_JOBS "$BUILD_JOBS" '^[1-9][0-9]*$' "a number"
[ "${#SERVICE}" -le 26 ] || die "SERVICE in deploy.env: at most 26 characters (its build user is SERVICE-build)"
[ -z "${WEB_URL:-}" ] || valid WEB_URL "$WEB_URL" '^https?://[A-Za-z0-9.-]+(:[0-9]+)?/?$' "https://MACHINE.TAILNET.ts.net"
[[ $DIR != */.* && $DIR != / ]] || die "DIR in deploy.env: $DIR isn't allowed here"

# On the server: the units' state folder (database and its copies), the
# off-site backup script and the encrypted bundles it keeps.
STATE=/var/lib/$SERVICE
DB=$STATE/bot.db
OFFSITE=$DIR/backup-offsite
MONITOR=$DIR/monitor
# Releases are built by their own user, in its own folder (sources, cargo,
# rustup, target), never as root; only the finished binary is copied out.
BUILDER=$SERVICE-build
BUILD=$DIR/build
BUNDLES=$DIR/backup/bundles
# A bundle's name, and the files it holds (see Backups below).
BUNDLE_RE="^$SERVICE-[0-9]{8}-[0-9]{6}\.tar\.gz\.age\$"
BUNDLE_GLOB="$SERVICE-[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]-[0-9][0-9][0-9][0-9][0-9][0-9].tar.gz.age"
# The ssh-keygen signature namespace of bundles.
SIGN_NS=bot-backup
# Here: how far a migration got, so it can carry on (and migrate-back knows where from).
MIGRATE_STATE=.deploy-state/migrate-$SERVICE
GIT_SSH="ssh -F /dev/null -i $DIR/.ssh/deploy_key -o UserKnownHostsFile=$DIR/.ssh/known_hosts -o IdentitiesOnly=yes"

# Commands run when this script exits (temporary folders here and on the server).
CLEANUP=()
cleanup() {
  local c
  for c in ${CLEANUP[@]+"${CLEANUP[@]}"}; do eval "$c" > /dev/null 2>&1 || true; done
}
trap cleanup EXIT
trap 'exit 130' INT TERM HUP
# Text from a server or a bundle, without control characters (terminal escapes).
clean() { tr -d '\000-\010\013-\037\177'; }

# A line from the terminal if there is one (not from a pipe), else from stdin.
ask() {
  local a=''
  if (exec < /dev/tty) 2> /dev/null; then read -r a < /dev/tty || true; else read -r a || true; fi
  echo "$a"
}
# Ask a yes/no question; ASSUME_YES=1 says yes.
confirm() {
  [ -z "${ASSUME_YES:-}" ] || return 0
  printf '%s [y/N] ' "$1" >&2
  case $(ask) in y | Y | yes) return 0 ;; *) return 1 ;; esac
}
pause() { [ -n "${ASSUME_YES:-}" ] || { printf '%s' "$1" >&2; ask > /dev/null; }; }

# Shell commands on host $1 (as root), or on HOST.
on_host() {
  local h=$1
  shift
  ssh "$h" "set -euo pipefail; $*"
}
on() { on_host "$HOST" "$@"; }

# ---- Run on the server: sent ahead of every `remote` script ----

# Restart service $1, and its web UI if that's set up (it needs the client secret
# in $2/.env). Never on a server the bot moved off ($2/MOVED-TO).
restart_units() {
  if [ -e "$2/MOVED-TO" ]; then
    echo "not starting the bot here: it $(head -n 1 "$2/MOVED-TO" | tr -cd '[:print:]') ($2/MOVED-TO)" >&2
    return 1
  fi
  systemctl restart "$1"
  if systemctl is-enabled -q "$1-web" 2> /dev/null && grep -q '^DISCORD_CLIENT_SECRET=' "$2/.env"; then
    systemctl restart "$1-web"
  fi
}

# The last "connected; bot running servers=N waiting=M" line service $1 logged since time $2.
connected_line() {
  journalctl -u "$1" --since "@$2" -q -o cat 2> /dev/null |
    sed 's/\x1b\[[0-9;]*m//g' | grep 'bot running' | tail -n 1 || true
}

# Whether service $1, (re)started at time $2, says it connected within $3
# seconds and then stays up (no restarts) for $4 more.
healthy() {
  local service=$1 since=$2 wait=$3 stable=$4 line='' restarts
  for _ in $(seq "$wait"); do
    sleep 1
    line=$(connected_line "$service" "$since")
    [ -n "$line" ] && break
    systemctl is-failed -q "$service" && break
  done
  if [ -z "$line" ]; then echo "the bot didn't say it connected within ${wait}s" >&2; return 1; fi
  case "$line" in
    *"servers=0"*) echo "the bot connected but runs no server ($line)" >&2; return 1 ;;
  esac
  restarts=$(systemctl show -p NRestarts --value "$service")
  sleep "$stable"
  if ! systemctl is-active -q "$service"; then echo "the bot stopped within ${stable}s" >&2; return 1; fi
  if [ "$(systemctl show -p NRestarts --value "$service")" != "$restarts" ]; then
    echo "the bot restarted within ${stable}s" >&2
    return 1
  fi
  case "$line" in *"waiting=0"*) ;; *) echo "note: some servers' configs don't load: $line" ;; esac
}

# Enable service $1's installed units and (re)start them ($2: its folder).
start_units() {
  systemctl enable -q "$1"
  systemctl enable -q --now "$1-backup.timer" 2> /dev/null || true
  # The monitor, once monitor-setup has turned it on.
  if [ -f "$2/monitor.env" ]; then systemctl enable -q --now "$1-monitor.timer" 2> /dev/null || true; fi
  if systemctl cat "$1-web" > /dev/null 2>&1; then systemctl enable -q "$1-web"; fi
  restart_units "$1" "$2"
}

# Stop and disable service $1's units, keeping everything on disk.
stop_units() {
  local u
  for u in "$1" "$1-web" "$1-backup.timer" "$1-monitor.timer"; do systemctl disable -q --now "$u" 2> /dev/null || true; done
  ! systemctl is-active -q "$1" && ! systemctl is-active -q "$1-web"
}

# Service $1's unit files.
unit_names() { echo "$1.service $1-web.service $1-backup.service $1-backup.timer $1-monitor.service $1-monitor.timer"; }

# Once the bot has moved off this server to $3: stop, disable and mask service
# $1's units (their files kept as *.moved; masked, a unit file written later
# goes to /dev/null) and say where it went in $2/MOVED-TO, which every
# deploy.sh command and the units themselves check. Fails if it still runs.
retire_units() {
  local u f
  printf 'moved to %s on %s\n' "$3" "$(date -u +%FT%TZ)" > "$2/MOVED-TO"
  for u in $(unit_names "$1"); do
    f=/etc/systemd/system/$u
    systemctl disable -q --now "$u" 2> /dev/null || true
    if [ -f "$f" ] && [ ! -L "$f" ]; then mv -f "$f" "$f.moved"; fi
    [ -L "$f" ] || systemctl mask -q "$u"
  done
  systemctl daemon-reload
  ! systemctl is-active -q "$1" && ! systemctl is-active -q "$1-web"
}

# Undo retire_units for service $1 (folder $2): unmasked, unit files back;
# $2/MOVED-TO goes too if $3 is `all` (else the units still can't start).
revive_units() {
  local u f
  for u in $(unit_names "$1"); do
    f=/etc/systemd/system/$u
    if [ -L "$f" ] && [ "$(readlink "$f")" = /dev/null ]; then systemctl unmask -q "$u" 2> /dev/null || rm -f "$f"; fi
    if [ -f "$f.moved" ] && [ ! -e "$f" ]; then mv "$f.moved" "$f"; fi
  done
  systemctl daemon-reload
  [ "$3" != all ] || rm -f "$2/MOVED-TO"
}

# Delete all but the newest $1 of files $2… (an unmatched glob is fine).
keep_newest() {
  local n=$1
  shift
  [ -e "$1" ] || return 0
  # shellcheck disable=SC2012 # our own names, without spaces
  ls -1t -- "$@" | tail -n +$((n + 1)) | xargs -r rm -f --
}

# Whether the member names on stdin (`tar -t`) are a bundle's: folder $1 with
# only the files a bundle has, each once, bot.db among them.
bundle_ok() {
  local top=$1 n seen=' '
  while IFS= read -r n; do
    case $n in
      "$top" | "$top/") ;;
      "$top/bot.db" | "$top/env" | "$top/info.txt" | "$top/config-unpushed.patch" | "$top/recordings.txt" | "$top/recordings.tar")
        case $seen in *" $n "*) echo "the backup holds $n twice" >&2; return 1 ;; esac
        seen="$seen$n "
        ;;
      *) echo "the backup holds something a backup doesn't: $(printf '%s' "$n" | tr -cd '[:print:]' | cut -c 1-200)" >&2; return 1 ;;
    esac
  done
  case $seen in *" $top/bot.db "*) ;; *) echo "there's no bot.db in the backup" >&2; return 1 ;; esac
}

# Whether env file $1 is safe to install as the server's .env: plain
# KEY=value lines (and comments), each value on its line, and only settings
# the bot uses (the known ones, those in $2, and those named in $3/.env and
# $3/config/federation.toml). Never ones that change how the bot, git or
# ssh run (GIT_SSH_COMMAND, LD_PRELOAD, PATH…) or that the units set.
env_ok() {
  local f=$1 dir=$3 allow line key val bad=0 i=0 dq='^"([^"\\]|\\.)*"$' sq="^'[^']*'\$"
  allow=" DISCORD_TOKEN APPLICATION_ID FED_SIGNING_KEY TAILSCALE_API_KEY DISCORD_CLIENT_SECRET WEB_TOKEN"
  allow="$allow TRANSCRIBE_URL TRANSCRIBE_API_KEY TRANSCRIBE_MODEL RECORDINGS_DIR RUST_LOG MONITOR_WEBHOOK MONITOR_PING_URL $2 "
  if [ -f "$dir/.env" ]; then
    allow="$allow $(sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' "$dir/.env" | tr '\n' ' ')"
  fi
  if [ -f "$dir/config/federation.toml" ]; then
    allow="$allow $(sed -n 's/^[[:space:]]*\(key_env\|webhook_env\|share_webhook_env\)[[:space:]]*=[[:space:]]*"\([A-Za-z_][A-Za-z0-9_]*\)".*/\2/p' \
      "$dir/config/federation.toml" | tr '\n' ' ') "
  fi
  while IFS= read -r line || [ -n "$line" ]; do
    i=$((i + 1))
    case $line in '' | '#'* | ';'*) continue ;; esac
    if ! [[ $line =~ ^[A-Za-z_][A-Za-z0-9_]*= ]]; then
      echo "  refused: line $i isn't KEY=value" >&2
      bad=1
      continue
    fi
    key=${line%%=*} val=${line#*=}
    case $key in
      PATH | HOME | SHELL | USER | LOGNAME | ENV | IFS | TMPDIR | LANG | LANGUAGE | TZ | EDITOR | VISUAL | PAGER | \
        DATABASE_URL | BOT_LOCAL | BOT_CONFIG | CONFIG_AUTOSAVE | WEB_URL | HEARTBEAT_FILE | NOTIFY_SOCKET | GNUPGHOME | \
        LD_* | GIT_* | SSH_* | SYSTEMD_* | DYLD_* | BASH_* | LC_* | XDG_* | SSL_* | CURL_* | \
        PYTHON* | PERL* | RUBY* | NODE_* | CARGO* | RUSTC* | RUST_BACKTRACE | RUST_MIN_STACK | *_PROXY | *_proxy)
        echo "  refused: $key (it would change how the bot, git or ssh run, or what the units set)" >&2
        bad=1
        continue
        ;;
    esac
    case $allow in
      *" $key "*) ;;
      *) echo "  refused: $key isn't a setting the bot uses (if it is, add it to BACKUP_ENV_KEYS in deploy.env)" >&2; bad=1; continue ;;
    esac
    # systemd joins a line ending in \ to the next, and a quoted value runs on until its quote.
    if [[ $val == \"* && ! $val =~ $dq ]] || [[ $val == \'* && ! $val =~ $sq ]] || [[ $val == *\\ ]]; then
      echo "  refused: $key's value runs past its line" >&2
      bad=1
    fi
  done < "$f"
  return "$bad"
}

REMOTE_FNS=(restart_units connected_line healthy start_units stop_units unit_names retire_units revive_units
  keep_newest bundle_ok env_ok)

# Run the bash script on stdin on host $1 with arguments $2…, after the functions above.
remote() {
  local h=$1
  shift
  # ssh joins arguments into one command line: quote them, or empty ones vanish.
  { echo 'set -euo pipefail; export LC_ALL=C'; declare -f "${REMOTE_FNS[@]}"; cat; } | ssh "$h" "bash -s -- $(printf '%q ' "$@")"
}

# Start the bot (and web UI) on host $1, enabled, and check it's healthy.
start_checked() {
  remote "$1" "$SERVICE" "$DIR" "$HEALTH_WAIT" "$HEALTH_STABLE" << 'REMOTE'
service=$1 dir=$2
since=$(date +%s)
start_units "$service" "$dir"
if ! healthy "$service" "$since" "$3" "$4"; then
  journalctl -u "$service" -n 20 --no-pager -q >&2 || true
  exit 1
fi
if systemctl is-enabled -q "$service-web" 2>/dev/null && ! systemctl is-active -q "$service-web"; then
  echo "note: the web UI isn't running: journalctl -u $service-web" >&2
fi
journalctl -u "$service" -n 3 --no-pager -q || true
REMOTE
}

# Install whichever of commands $2… host $1 lacks (asks first). Packages are
# named as the commands, but for sqlite3 off Debian.
ensure_pkgs() {
  local h=$1 missing
  shift
  missing=$(on_host "$h" "for c in $*; do command -v \$c >/dev/null || printf '%s ' \$c; done")
  [ -n "$missing" ] || return 0
  confirm "$h lacks: $missing. Install them?" || die "install $missing on $h first"
  on_host "$h" "p='$missing'
    if command -v apt-get >/dev/null; then
      export DEBIAN_FRONTEND=noninteractive
      apt-get install -y -q \$p >/dev/null 2>&1 || { apt-get update -q >/dev/null && apt-get install -y -q \$p >/dev/null; }
    elif command -v dnf >/dev/null; then dnf install -y -q \${p/sqlite3/sqlite}
    elif command -v pacman >/dev/null; then pacman -S --noconfirm --needed \${p/sqlite3/sqlite} >/dev/null
    else echo 'no apt-get, dnf or pacman here: install them by hand' >&2; exit 1; fi"
}

# ---- Setup and deploys ----

# The service user and folders on host $1.
prepare_host() {
  on_host "$1" "id $SERVICE >/dev/null 2>&1 || useradd --system --no-create-home --shell /usr/sbin/nologin $SERVICE
      mkdir -p $DIR/releases $DIR/src $DIR/.ssh
      chown $SERVICE:$SERVICE $DIR/.ssh && chmod 700 $DIR/.ssh"
}

# Host $1's deploy key for the config repo (made if it has none), added on GitHub or by you.
deploy_key() {
  local key
  on_host "$1" "[ -f $DIR/.ssh/deploy_key ] || ssh-keygen -q -t ed25519 -N '' -C '$SERVICE@config' -f $DIR/.ssh/deploy_key
      ssh-keyscan -q github.com > $DIR/.ssh/known_hosts 2>/dev/null
      chown -R $SERVICE:$SERVICE $DIR/.ssh && chmod 700 $DIR/.ssh && chmod 600 $DIR/.ssh/deploy_key"
  key=$(on_host "$1" "cat $DIR/.ssh/deploy_key.pub")
  if [ -n "${CONFIG_REPO_GH:-}" ] && command -v gh > /dev/null; then
    say "adding the server's deploy key (read + write) to $CONFIG_REPO_GH"
    printf '%s\n' "$key" | gh repo deploy-key add - --repo "$CONFIG_REPO_GH" --allow-write --title "$SERVICE on $1" || true
  else
    echo "Add this key to the config repo as a deploy key with write access:"
    echo "  $key"
    pause "Press Enter once it's added… "
  fi
}

# The config checkout on host $1 (cloned once; the bot keeps it up to date).
clone_config() {
  say "config checkout"
  # Clone next to the old folder, then swap, so a running bot never loses its config.
  on_host "$1" "if [ ! -d $DIR/config/.git ]; then
        rm -rf $DIR/config.new && install -d -o $SERVICE -g $SERVICE $DIR/config.new
        sudo -u $SERVICE env GIT_SSH_COMMAND='$GIT_SSH' \
          git clone -q --branch $(printf %q "$CONFIG_BRANCH") -- $(printf %q "$CONFIG_REPO") $DIR/config.new
        [ -e $DIR/config ] && mv $DIR/config $DIR/config.before-git.\$(date +%s)
        mv $DIR/config.new $DIR/config
      fi
      sudo -u $SERVICE git -C $DIR/config config user.name '$SERVICE'
      sudo -u $SERVICE git -C $DIR/config config user.email '$SERVICE@localhost'"
}

setup() {
  say "user, folders and service"
  prepare_host "$HOST"
  deploy_key "$HOST"
  clone_config "$HOST"
  install_units "$HOST"
  echo "Done. Next: scripts/deploy.sh secrets, then scripts/deploy.sh"
}

# $1 with the @…@ values filled in.
fill() {
  local k v args=()
  for k in SERVICE DIR USER WEB_URL WEB_PORT BACKUP_CALENDAR; do
    case $k in USER) v=$SERVICE ;; WEB_URL) v=${WEB_URL:-} ;; *) v=${!k} ;; esac
    # As a sed replacement: \, | and & mean something there.
    v=$(printf '%s' "$v" | sed 's/[\\|&]/\\&/g')
    args+=(-e "s|@$k@|$v|g")
  done
  sed "${args[@]}" "$1"
}

# Install (or refresh) the units and the off-site backup script on host $1.
# Then, unless $2 is `install`, enable them, as setup and deploys do (a
# migration's new server gets them enabled at the cut-over).
install_units() {
  local h=$1 mode=${2:-enable} u=/etc/systemd/system
  fill deploy/bot.service | ssh "$h" "cat > $u/$SERVICE.service"
  # Daily database backups on the server (/var/lib/$SERVICE/backups, 14 kept),
  # and off-site ones once backup-setup has run.
  fill deploy/backup.service | ssh "$h" "cat > $u/$SERVICE-backup.service"
  fill deploy/backup.timer | ssh "$h" "cat > $u/$SERVICE-backup.timer"
  fill deploy/backup-offsite.sh | ssh "$h" "umask 077 && cat > $OFFSITE.new && chmod 700 $OFFSITE.new && mv $OFFSITE.new $OFFSITE"
  # The heartbeat check: installed always, on once monitor-setup has run.
  fill deploy/monitor.service | ssh "$h" "cat > $u/$SERVICE-monitor.service"
  fill deploy/monitor.timer | ssh "$h" "cat > $u/$SERVICE-monitor.timer"
  fill deploy/monitor.sh | ssh "$h" "umask 077 && cat > $MONITOR.new && chmod 700 $MONITOR.new && mv $MONITOR.new $MONITOR"
  if [ -n "${WEB_URL:-}" ]; then
    # The web UI listens on loopback; tailscale serve publishes it on the tailnet over HTTPS.
    fill deploy/web.service | ssh "$h" "cat > $u/$SERVICE-web.service"
  fi
  on_host "$h" "systemctl daemon-reload"
  on_host "$h" "command -v sqlite3 >/dev/null" || note "install sqlite3 on $h, or the daily database backups fail"
  [ "$mode" != install ] || return 0
  on_host "$h" "systemctl enable -q $SERVICE && systemctl enable -q --now $SERVICE-backup.timer
      if [ -f $DIR/monitor.env ]; then systemctl enable -q --now $SERVICE-monitor.timer; fi"
  if [ -n "${WEB_URL:-}" ]; then
    on_host "$h" "systemctl enable -q $SERVICE-web && tailscale serve --bg --https=443 http://127.0.0.1:$WEB_PORT >/dev/null"
  fi
}

secrets() {
  [ -f "$SECRETS" ] || { echo "$SECRETS not found" >&2; exit 1; }
  say "uploading $SECRETS as $DIR/.env"
  ssh "$HOST" "umask 077 && cat > $DIR/.env && chown root:root $DIR/.env" < "$SECRETS"
  # systemd reads .env only when a service starts, so restart them to use it.
  if ! on "[ -L $DIR/current ]"; then
    echo "No release yet; the bot starts with these secrets on the first scripts/deploy.sh."
    return
  fi
  say "restart the bot (and web UI) with them"
  remote "$HOST" "$DIR" "$SERVICE" "$HEALTH_WAIT" << 'REMOTE'
dir=$1 service=$2
since=$(date +%s)
restart_units "$service" "$dir"
if healthy "$service" "$since" "$3" 0; then
  echo "The bot is back up with the new secrets."
  exit 0
fi
journalctl -u "$service" -n 20 --no-pager -q >&2 || true
echo "The bot didn't come up with them: is DISCORD_TOKEN right? Look at: journalctl -u $service" >&2
exit 1
REMOTE
}

# Build commit $2 of this checkout for host $1, as release $3: on that
# server or here (DEPLOY_BUILD).
build_release() {
  say "code $3"
  if [ "$DEPLOY_BUILD" = local ]; then build_here "$@"; else build_there "$@"; fi
}

# A build on host $1 as its build user (BUILDER, made the first time), in a
# sandbox that can write only to its own folder (BUILD) and can't see the
# bot's secrets, database or config: the crates' build scripts never run as
# root. Root copies out the binary alone, reading it as the build user.
build_there() {
  local h=$1 commit=$2 name=$3 run
  # The Opus codec (for voice recordings) is built from source with CMake;
  # the gateway's TLS links to OpenSSL.
  ensure_pkgs "$h" cmake gcc make curl pkg-config
  on_host "$h" "pkg-config --exists openssl" ||
    note "$h lacks OpenSSL's headers, so the build fails: install them (Debian, Ubuntu: libssl-dev; Fedora: openssl-devel)"
  on_host "$h" "id $BUILDER >/dev/null 2>&1 ||
        useradd --system --home-dir $BUILD --no-create-home --shell /usr/sbin/nologin $BUILDER
      install -d -m 700 -o $BUILDER -g $BUILDER $BUILD
      # Older versions of this script built as root, in these.
      rm -rf $DIR/src $DIR/target
      mkdir -p $DIR/releases"
  run="systemd-run --quiet --wait --pipe --collect -p User=$BUILDER -p Group=$BUILDER \
    -p WorkingDirectory=$BUILD -p UMask=0077 -p NoNewPrivileges=yes -p ProtectSystem=strict \
    -p ReadWritePaths=$BUILD -p ProtectHome=yes -p PrivateTmp=yes -p PrivateDevices=yes \
    -p ProtectKernelTunables=yes -p ProtectKernelModules=yes -p ProtectControlGroups=yes \
    -p RestrictSUIDSGID=yes -p InaccessiblePaths=-$DIR/.env -p InaccessiblePaths=-$DIR/config \
    -p InaccessiblePaths=-$DIR/.ssh -p InaccessiblePaths=-$DIR/backup -p InaccessiblePaths=-$STATE \
    -E HOME=$BUILD -E CARGO_HOME=$BUILD/cargo -E RUSTUP_HOME=$BUILD/rustup -E CARGO_TARGET_DIR=$BUILD/target \
    -E PATH=$BUILD/cargo/bin:/usr/local/bin:/usr/bin:/bin"
  if ! on_host "$h" "[ -x $BUILD/cargo/bin/cargo ]"; then
    confirm "$h: install Rust (rustup, stable) for the build user $BUILDER, in $BUILD?" ||
      die "builds on $h need Rust for $BUILDER (or DEPLOY_BUILD=local)"
    say "Rust for $BUILDER"
    on_host "$h" "$run sh -c 'curl --proto =https --tlsv1.2 -sSf https://sh.rustup.rs |
      sh -s -- -y -q --profile minimal --default-toolchain stable --no-modify-path'"
  else
    on_host "$h" "$run $BUILD/cargo/bin/rustup update -q --no-self-update stable >/dev/null 2>&1" ||
      note "couldn't update Rust on $h; building with the one it has"
  fi
  git archive --format=tar "$commit" |
    on_host "$h" "$run sh -c 'rm -rf src && mkdir src && tar -x -C src'"
  say "build"
  # Its output without control characters (it comes from the crates' build scripts).
  on_host "$h" "$run $BUILD/cargo/bin/cargo build --manifest-path $BUILD/src/Cargo.toml --release --locked -q -p chipsoc-bot ${BUILD_JOBS:+-j $BUILD_JOBS} 2>&1 |
        tr -d '\\000-\\010\\013-\\037\\177'
      mkdir -p $DIR/releases/$name && rm -f $DIR/releases/$name/bot.new
      runuser -u $BUILDER -- cat $BUILD/target/release/chipsoc-bot > $DIR/releases/$name/bot.new
      chmod 755 $DIR/releases/$name/bot.new && mv -f $DIR/releases/$name/bot.new $DIR/releases/$name/bot"
}

# A build here, of commit $2 as committed (in .deploy-state; cargo's cache is
# this checkout's target/), copied to host $1 as release $3 and checked there
# by its hash. It must run on the server: same machine type, and a C library
# no newer than the server's.
build_here() {
  local h=$1 commit=$2 name=$3 src=.deploy-state/build-$SERVICE out need have sum
  [ "$(uname -sm)" = "$(on_host "$h" uname -sm)" ] ||
    die "this machine ($(uname -sm)) isn't the same kind as $h: build there (DEPLOY_BUILD=server)"
  rm -rf "$src" && mkdir -p "$src"
  git archive --format=tar "$commit" | tar -x -C "$src"
  say "build (here)"
  CARGO_TARGET_DIR=$PWD/target cargo build --manifest-path "$src/Cargo.toml" --release --locked -q -p chipsoc-bot \
    ${BUILD_JOBS:+-j "$BUILD_JOBS"}
  rm -rf "$src"
  out=target/release/chipsoc-bot
  # The newest glibc version it needs, against the server's.
  need=$({ objdump -T "$out" 2> /dev/null || readelf -W --dyn-syms "$out" 2> /dev/null || true; } |
    grep -o 'GLIBC_[0-9][0-9.]*' | sed 's/^GLIBC_//' | sort -uV | tail -n 1)
  have=$(on_host "$h" "getconf GNU_LIBC_VERSION 2>/dev/null | cut -d' ' -f2" || true)
  if [ -n "$need" ] && [ -n "$have" ] && [ "$(printf '%s\n%s\n' "$need" "$have" | sort -V | tail -n 1)" != "$have" ]; then
    die "this build needs glibc $need, and $h has $have: build on a system no newer than the server's (docs/deploy.md, Where releases are built), or on the server (DEPLOY_BUILD=server)"
  fi
  sum=$(sha256sum < "$out" | cut -d' ' -f1)
  say "copying it to $h"
  on_host "$h" "mkdir -p $DIR/releases/$name && umask 022 && cat > $DIR/releases/$name/bot.new" < "$out"
  on_host "$h" "f=$DIR/releases/$name/bot
      [ \"\$(sha256sum < \$f.new | cut -d' ' -f1)\" = $sum ] || { rm -f \$f.new; echo 'the copy on $h differs from the build' >&2; exit 1; }
      chmod 755 \$f.new && mv -f \$f.new \$f"
}

deploy() {
  local rev
  rev=$(git rev-parse --short HEAD)
  git diff --quiet HEAD -- crates Cargo.toml Cargo.lock || echo "note: uncommitted changes aren't deployed (deploying $rev)"
  [ -n "${SKIP_TESTS:-}" ] || { say "tests"; cargo test -q --workspace --locked > /dev/null; }
  build_release "$HOST" HEAD "$rev"
  if ! check_config "$rev"; then
    [ "$DEPLOY_BUILD" != local ] ||
      echo "(built here: if it doesn't start there at all, build on a system like the server's, or DEPLOY_BUILD=server)" >&2
    die "the new release can't load the config; still running the old one"
  fi
  install_units "$HOST"
  switch "$rev"
  tag_deployed "$(git rev-parse HEAD)"
}

# Move $DEPLOY_TAG to commit $1 and push it. A failure here doesn't undo the deploy.
tag_deployed() {
  [ -n "$DEPLOY_TAG" ] || return 0
  git tag -f "$DEPLOY_TAG" "$1" > /dev/null
  if git remote get-url "$DEPLOY_TAG_REMOTE" > /dev/null 2>&1 &&
    git push -q -f "$DEPLOY_TAG_REMOTE" "refs/tags/$DEPLOY_TAG"; then
    say "tagged $DEPLOY_TAG at $(git rev-parse --short "$1") and pushed it"
  else
    echo "note: couldn't push tag $DEPLOY_TAG to $DEPLOY_TAG_REMOTE; the config repo's CI checks the bot at the old one" >&2
  fi
}

# Whether release $1 can load the config on host $2 (default HOST), pulled first if it can be.
check_config() {
  say "check the config loads with $1"
  on_host "${2:-$HOST}" "sudo -u $SERVICE env GIT_SSH_COMMAND='$GIT_SSH' git -C $DIR/config pull -q --ff-only ||
        echo 'note: couldn'\''t pull the config; checking the copy on the server'
      cd $DIR && sudo -u $SERVICE $DIR/releases/$1/bot check --config $DIR/config"
}

# Switch to release $1 and restart. `current` and `previous` (symlinks in $DIR)
# say which release runs and which ran before. If the bot doesn't connect, or
# doesn't stay up, switch back, restart that, and fail.
switch() {
  say "switch to $1 and restart"
  remote "$HOST" "$DIR" "$SERVICE" "$1" "$HEALTH_WAIT" "$HEALTH_STABLE" << 'REMOTE'
dir=$1 service=$2 target=$3 wait=$4 stable=$5
rel=$dir/releases/$target
[ -x "$rel/bot" ] || { echo "there's no release $target on the server" >&2; exit 1; }
was_current=$(readlink "$dir/current" 2>/dev/null || true)
was_previous=$(readlink "$dir/previous" 2>/dev/null || true)

ln -sfn "$rel" "$dir/current"
if [ -n "$was_current" ] && [ "$was_current" != "$rel" ]; then ln -sfn "$was_current" "$dir/previous"; fi
since=$(date +%s)
restart_units "$service" "$dir"
if healthy "$service" "$since" "$wait" "$stable"; then
  journalctl -u "$service" -n 5 --no-pager -q
  # Keep the five newest releases, and always the current and previous ones.
  keep_current=$(readlink -f "$dir/current")
  keep_previous=$(readlink -f "$dir/previous" 2>/dev/null || true)
  # shellcheck disable=SC2012 # release names are commit hashes
  for r in $(ls -1t "$dir/releases" | tail -n +6); do
    p=$(readlink -f "$dir/releases/$r")
    if [ "$p" != "$keep_current" ] && [ "$p" != "$keep_previous" ]; then rm -rf "$p"; fi
  done
  exit 0
fi

echo "› $target isn't healthy; switching back" >&2
journalctl -u "$service" -n 20 --no-pager -q >&2 || true
if [ -z "$was_current" ] || [ "$was_current" = "$rel" ]; then
  echo "there's no other release to go back to; look at: journalctl -u $service" >&2
  exit 1
fi
ln -sfn "$was_current" "$dir/current"
if [ -n "$was_previous" ]; then ln -sfn "$was_previous" "$dir/previous"; else rm -f "$dir/previous"; fi
since=$(date +%s)
restart_units "$service" "$dir"
if healthy "$service" "$since" "$wait" "$stable"; then
  echo "back on $(basename "$was_current")" >&2
else
  echo "$(basename "$was_current") isn't healthy either; look at: journalctl -u $service" >&2
fi
exit 1
REMOTE
}

rollback() {
  local prev
  # The release that ran before this one; older installs without `previous`
  # fall back to the newest release that isn't the current one.
  prev=$(on "if [ -L $DIR/previous ]; then basename \"\$(readlink $DIR/previous)\"; else
        cur=\$(readlink -f $DIR/current)
        for r in \$(ls -1t $DIR/releases); do
          [ \"\$(readlink -f $DIR/releases/\$r)\" = \"\$cur\" ] || { echo \"\$r\"; break; }
        done
      fi")
  [ -n "$prev" ] || { echo "no previous release" >&2; exit 1; }
  [[ $prev =~ ^[A-Za-z0-9._-]+$ ]] || die "the server's previous release has an odd name: $(printf %s "$prev" | clean)"
  check_config "$prev" ||
    { echo "release $prev can't load the config as it is now; not switching (deploy a fix instead)" >&2; exit 1; }
  switch "$prev"
  if git rev-parse -q --verify "$prev^{commit}" > /dev/null; then
    tag_deployed "$(git rev-parse "$prev^{commit}")"
  elif [ -n "$DEPLOY_TAG" ]; then
    echo "note: $prev isn't in this checkout; tag $DEPLOY_TAG wasn't moved" >&2
  fi
}

# ---- Backups ----
# A bundle is SERVICE-YYYYmmdd-HHMMSS.tar.gz.age (UTC), signed by the server
# (NAME.sig beside it: ssh-keygen -Y sign with its DIR/backup/sign_key). It
# holds one folder with bot.db, env (the server's .env), info.txt (when and
# where it was made, release, config repo and commit), recordings.txt (the
# voice recordings the server had) and, if there are any or it's asked for,
# config-unpushed.patch (config commits the bot hadn't pushed) and
# recordings.tar (BACKUP_RECORDINGS=1). The server makes them
# (deploy/backup-offsite.sh); this end sets that up, downloads, checks and
# restores them, and trusts only bundles signed by BACKUP_SIGNING_KEY.

need_age() {
  command -v age > /dev/null || die "install age on this machine (https://age-encryption.org; apt, brew, pacman: age)"
  [ -n "$BACKUP_AGE_IDENTITY" ] || die "set BACKUP_AGE_IDENTITY in deploy.env: your age private key file, on this machine"
  [ -f "$BACKUP_AGE_IDENTITY" ] || die "$BACKUP_AGE_IDENTITY (BACKUP_AGE_IDENTITY) not found"
}

# Whether $1 is one ssh-ed25519 public key, on one line.
valid_pub() {
  local re='^ssh-ed25519 [A-Za-z0-9+/]+={0,2}( [!-~]+)?$'
  [[ $1 =~ $re ]]
}

need_signer() {
  [ -n "$BACKUP_SIGNING_KEY" ] ||
    die "set BACKUP_SIGNING_KEY in deploy.env: the key the server signs its backups with (scripts/deploy.sh backup-setup shows it)"
  valid_pub "$BACKUP_SIGNING_KEY" || die "BACKUP_SIGNING_KEY in deploy.env isn't one ssh-ed25519 public key (ssh-ed25519 AAAA…)"
}

# Whether off-site backups are set up on HOST.
offsite_set_up() { on "[ -f $DIR/backup/backup.env ]"; }

# A new private folder for a bundle being looked at: in XDG_RUNTIME_DIR (in
# memory, this user's alone; /run/user/UID from cron) if there is one, else
# in backups/ (0700); never in the shared TMPDIR. The caller removes it
# (CLEANUP); ones that a killed run left are removed here.
private_dir() {
  local base=${XDG_RUNTIME_DIR:-/run/user/$(id -u)} d
  if [ -z "$base" ] || [ ! -d "$base" ] || [ ! -O "$base" ] || [ ! -w "$base" ]; then
    base=$PWD/backups
    mkdir -p "$base"
    chmod 700 "$base"
  fi
  find "$base" -maxdepth 1 -type d -name "$SERVICE-check.*" -mmin +60 -exec rm -rf {} + 2> /dev/null || true
  d=$(umask 077 && mktemp -d "$base/$SERVICE-check.XXXXXX")
  chmod 700 "$d"
  echo "$d"
}

# Whether bundle $1 carries the server's signature ($1.sig) by
# BACKUP_SIGNING_KEY, so it is as the server made it. $2: a private folder.
verify_bundle() {
  local f=$1 sig=$1.sig
  [ -f "$sig" ] || { echo "  ${f##*/} isn't signed (there's no ${sig##*/} beside it)" >&2; return 1; }
  printf 'backup namespaces="%s" %s\n' "$SIGN_NS" "$BACKUP_SIGNING_KEY" > "$2/allowed_signers"
  if ! ssh-keygen -Y verify -f "$2/allowed_signers" -I backup -n "$SIGN_NS" -s "$sig" < "$f" > /dev/null 2>&1; then
    echo "  ${f##*/}: its signature isn't by the server's backup key (BACKUP_SIGNING_KEY), or it was changed since" >&2
    return 1
  fi
}

# When bundle $1 was made, by its name (seconds since 1970).
bundle_time() {
  local t=${1##*/}
  t=${t#"$SERVICE"-}
  t=${t%%.*}
  date -u -d "${t:0:4}-${t:4:2}-${t:6:2} ${t:9:2}:${t:11:2}:${t:13:2}" +%s 2> /dev/null ||
    date -j -u -f '%Y%m%d-%H%M%S' "$t" +%s
}

# Whether bundle $1's name dates it later than now (give or take an hour's
# clock skew): no backup is made then, so it can't be the newest.
future_name() { [ "$(bundle_time "$1")" -gt $(($(date +%s) + 3600)) ]; }

# The newest of this service's bundles downloaded into backups/, if any.
newest_local() {
  local f best=''
  for f in backups/$BUNDLE_GLOB; do
    [ -f "$f" ] || continue
    if future_name "$f"; then
      note "passing over ${f##*/}: it's dated in the future"
      continue
    fi
    best=$f
  done
  [ -n "$best" ] && echo "$best"
}

# The newest bundle at the destination, as HOST lists it: "NAME BYTES".
newest_dest() {
  local n b
  while read -r n b; do
    [[ $n =~ $BUNDLE_RE && $b =~ ^[0-9]+$ ]] || continue
    if future_name "$n"; then
      note "passing over $n at the destination: it's dated in the future"
      continue
    fi
    echo "$n $b"
    return 0
  done <<< "$(on "$OFFSITE list")"
  return 1
}

# BACKUP_AGE_RECIPIENT must hold public keys only.
check_recipients() {
  local r
  [ -n "$BACKUP_AGE_RECIPIENT" ] ||
    die "set BACKUP_AGE_RECIPIENT in deploy.env to your age public key (age1…; docs/deploy.md, Backups)"
  case $BACKUP_AGE_RECIPIENT in
    *AGE-SECRET-KEY-*) die "BACKUP_AGE_RECIPIENT holds a private key! Put the public key (age1…) there; the private one never goes on the server." ;;
  esac
  for r in $BACKUP_AGE_RECIPIENT; do
    [[ $r =~ ^age1[a-z0-9]+$ ]] || die "BACKUP_AGE_RECIPIENT: $r isn't an age public key (age1…)"
  done
}

# BACKUP_TARGET's ssh destination, and its folder there (relative to that
# account's home, or absolute; a leading ~/ is the home).
dest_host() { echo "${BACKUP_TARGET%%:*}"; }
dest_path() {
  local p=${BACKUP_TARGET#*:}
  # shellcheck disable=SC2088  # a literal ~ the user typed, not an expansion
  case $p in '' | '~' | '~/') p=. ;; '~/'*) p=${p#\~/} ;; esac
  echo "$p"
}

backup_setup() {
  check_recipients
  case $BACKUP_TARGET in
    pull) ;;
    rclone:?*:*) valid BACKUP_TARGET "$BACKUP_TARGET" '^rclone:[A-Za-z0-9_.-]+:[A-Za-z0-9_./-]*$' "rclone:REMOTE:PATH, of letters, digits and . _ - /" ;;
    ?*:*)
      valid BACKUP_TARGET "$(dest_host)" '^([A-Za-z0-9._-]+@)?[A-Za-z0-9.-]+$' "USER@HOST:PATH"
      valid BACKUP_TARGET "$(dest_path)" '^[A-Za-z0-9._/-]+$' "USER@HOST:PATH, the path of letters, digits and . _ - / (~/ at the start)"
      case $(dest_path) in
        "$HOME"/*) note "BACKUP_TARGET's folder is your home folder here ($HOME): if you meant the destination's home, write it without ~ (USER@HOST:backups) or quote it ('USER@HOST:~/backups')" ;;
      esac
      ;;
    *) die "set BACKUP_TARGET in deploy.env: rclone:REMOTE:PATH, USER@HOST:PATH (ssh) or pull" ;;
  esac
  [[ $BACKUP_KEEP =~ ^[1-9][0-9]*$ ]] || die "BACKUP_KEEP should be a number"
  case $BACKUP_RECORDINGS in '' | 0 | 1) ;; *) die "BACKUP_RECORDINGS should be 1 (or nothing)" ;; esac
  local pkgs=(sqlite3 age sudo) out
  [[ $BACKUP_TARGET != rclone:* ]] || pkgs+=(rclone)
  ensure_pkgs "$HOST" "${pkgs[@]}"
  say "off-site backup settings on $HOST"
  printf '%s=%q\n' BACKUP_TARGET "$BACKUP_TARGET" BACKUP_AGE_RECIPIENT "$BACKUP_AGE_RECIPIENT" BACKUP_KEEP "$BACKUP_KEEP" \
    BACKUP_RECORDINGS "$BACKUP_RECORDINGS" |
    ssh "$HOST" "umask 077 && mkdir -p $DIR/backup && chmod 700 $DIR/backup && cat > $DIR/backup/backup.env"
  case $BACKUP_TARGET in
    pull) ;;
    rclone:*) backup_rclone_remote ;;
    *) backup_ssh_key ;;
  esac
  install_units "$HOST"
  signing_key
  case $BACKUP_TARGET in
    pull | rclone:*)
      on "$OFFSITE check" || die "the server can't write to $BACKUP_TARGET yet (above); fix that and run backup-setup again"
      ;;
    *)
      # Set up (again) unless the destination's forced command is there, with this BACKUP_KEEP.
      out=$(on "$OFFSITE check" 2>&1) || true
      if ! grep -qx "keep=$BACKUP_KEEP" <<< "$out"; then
        [ -z "$out" ] || printf '%s\n' "$out" | clean | sed 's/^/  /'
        { authorize_backup_key && on "$OFFSITE check"; } ||
          die "the server can't write to $BACKUP_TARGET yet (above); fix that and run backup-setup again"
      fi
      ;;
  esac
  say "a first backup"
  on "systemctl start $SERVICE-backup.service" || {
    on "journalctl -u $SERVICE-backup -n 20 --no-pager -q" >&2 || true
    die "the backup failed (above)"
  }
  on "$OFFSITE list | head -n 3"
  if [ "$BACKUP_TARGET" = pull ]; then
    echo "The server keeps the newest $BACKUP_KEEP. To have them off it, run scripts/deploy.sh backup --latest"
    echo "here every day (a cron job or systemd user timer; docs/deploy.md)."
  fi
  if [ -n "$BACKUP_AGE_IDENTITY" ]; then
    [ "$BACKUP_TARGET" != pull ] || backup --latest
    backup_check
  else
    note "set BACKUP_AGE_IDENTITY to your private key file here, then run: scripts/deploy.sh backup-check"
  fi
}

# The server's key for signing its backups (made once there), which must be
# BACKUP_SIGNING_KEY here; the first time, offers to put it in deploy.env.
signing_key() {
  local pub
  on "[ -f $DIR/backup/sign_key ] || ssh-keygen -q -t ed25519 -N '' -C '$SERVICE-backup-signing' -f $DIR/backup/sign_key"
  pub=$(on "cat $DIR/backup/sign_key.pub")
  valid_pub "$pub" || die "the server's backup signing key ($DIR/backup/sign_key.pub) isn't one ssh-ed25519 public key"
  pub=$(cut -d' ' -f1-2 <<< "$pub")
  if [ -n "$BACKUP_SIGNING_KEY" ]; then
    [ "$(cut -d' ' -f1-2 <<< "$BACKUP_SIGNING_KEY")" != "$pub" ] || return 0
    die "$HOST signs its backups with another key than BACKUP_SIGNING_KEY in deploy.env:
  $pub
If you know why (the server was set up afresh, or its key made again), put that in deploy.env; else find out first."
  fi
  echo "$HOST signs every backup with a key of its own; restore and backup-check trust only bundles it signed:"
  echo "  BACKUP_SIGNING_KEY='$pub'"
  if grep -q '^BACKUP_SIGNING_KEY=' deploy.env; then
    note "deploy.env sets BACKUP_SIGNING_KEY empty: put the key above there"
  elif confirm "Add that to deploy.env?"; then
    printf "\n# The server's backup signing key (backup-setup): restore and backup-check check it.\nBACKUP_SIGNING_KEY='%s'\n" "$pub" >> deploy.env
    echo "  deploy.env: BACKUP_SIGNING_KEY set"
  else
    note "put it in deploy.env yourself: until then restore and backup-check refuse the backups"
  fi
  BACKUP_SIGNING_KEY=$pub
}

# rclone target: the remote must be in the server's rclone.conf (root only).
backup_rclone_remote() {
  local remote=${BACKUP_TARGET#rclone:} name
  name=${remote%%:*}
  on "rclone --config $DIR/backup/rclone.conf listremotes 2>/dev/null | grep -qx '$name:'" && return 0
  if command -v rclone > /dev/null && rclone listremotes 2> /dev/null | grep -qx "$name:" &&
    confirm "Copy your rclone remote $name to the server? It gets all the access that remote has (best: a key for one bucket or folder that can't delete; docs/deploy.md)."; then
    rclone config show "$name" | ssh "$HOST" "umask 077 && cat >> $DIR/backup/rclone.conf"
  else
    die "set up rclone remote $name on the server: ssh -t $HOST rclone config --config $DIR/backup/rclone.conf, then backup-setup again"
  fi
}

# ssh target: the server's own key for it, and the destination's host key.
backup_ssh_key() {
  local host known
  host=$(dest_host)
  host=${host#*@}
  on "[ -f $DIR/backup/ssh_key ] || ssh-keygen -q -t ed25519 -N '' -C '$SERVICE-backup' -f $DIR/backup/ssh_key"
  # The host key this machine already trusts, if it knows the destination; else as the server sees it.
  known=$(ssh-keygen -F "$host" 2> /dev/null | grep -v '^#' || true)
  if [ -n "$known" ]; then
    printf '%s\n' "$known" | ssh "$HOST" "umask 077 && cat > $DIR/backup/known_hosts"
  elif ! on "[ -s $DIR/backup/known_hosts ]"; then
    on "ssh-keyscan -q $host > $DIR/backup/known_hosts 2>/dev/null && ssh-keygen -lf $DIR/backup/known_hosts"
    confirm "Is that $host's host key (ask its owner)?" || { on "rm -f $DIR/backup/known_hosts"; die "nothing trusted"; }
  fi
}

# Let the server's backup key in at the ssh destination, over this machine's
# ssh (asks first), but only for deploy/backup-dest.sh: a forced command that
# adds bundles to the folder (never replacing one), lists, reads and prunes
# them. No shell, no forwarding.
authorize_backup_key() {
  local dest path pub line script=.ssh/$SERVICE-backup-dest
  dest=$(dest_host) path=$(dest_path)
  pub=$(on "cat $DIR/backup/ssh_key.pub")
  valid_pub "$pub" || die "the server's backup key ($DIR/backup/ssh_key.pub) isn't one ssh-ed25519 public key"
  pub=$(cut -d' ' -f1-2 <<< "$pub")
  line="restrict,command=\"sh $script $path $BACKUP_KEEP\" $pub $SERVICE-backup"
  echo "At $dest, the server's backup key may only add bundles to $path, list and read them, and prune"
  echo "them as BACKUP_KEEP ($BACKUP_KEEP) says, through $script (deploy/backup-dest.sh); in ~/.ssh/authorized_keys:"
  echo "  $line"
  if ! confirm "Set that up there now, over your own ssh to $dest?"; then
    echo "By hand: copy deploy/backup-dest.sh to $dest:$script, with @SERVICE@ replaced by $SERVICE, and put that"
    echo "line in its ~/.ssh/authorized_keys instead of any other line with the same key."
    return 1
  fi
  fill deploy/backup-dest.sh |
    ssh "$dest" "umask 077 && mkdir -p .ssh && cat > $script.new && mv $script.new $script"
  # Any older line for this key goes (one without the forced command could do anything).
  printf '%s\n' "$line" | ssh "$dest" "umask 077 && f=.ssh/authorized_keys && touch \$f && k=\$(cat) &&
    { grep -vF '${pub#* }' \$f || true; printf '%s\n' \"\$k\"; } > \$f.new && mv \$f.new \$f"
}

backup() {
  mkdir -p backups && chmod 700 backups
  offsite_set_up || { backup_db; return; }
  if [ "${1:-}" != --latest ]; then
    say "a backup on $HOST now"
    on "systemctl start $SERVICE-backup.service" || {
      on "journalctl -u $SERVICE-backup -n 20 --no-pager -q" >&2 || true
      die "the backup failed (above)"
    }
  fi
  local name out all i
  name=$(on "$OFFSITE list local | head -n 1 | cut -d' ' -f1")
  [ -n "$name" ] || die "there's no backup on $HOST"
  [[ $name =~ $BUNDLE_RE ]] || die "$HOST lists a bundle with an odd name: $(clean <<< "$name")"
  out=backups/$name
  if [ -f "$out" ]; then
    echo "have $out already"
  else
    (umask 077 && ssh "$HOST" "cat $BUNDLES/$name" > "$out.part") || die "couldn't download $name"
    mv "$out.part" "$out"
    echo "saved $out (encrypted)"
  fi
  if [ ! -f "$out.sig" ]; then
    if (umask 077 && ssh "$HOST" "cat $BUNDLES/$name.sig" > "$out.sig.part" 2> /dev/null); then
      mv "$out.sig.part" "$out.sig"
    else
      rm -f "$out.sig.part"
      note "$name isn't signed (made before backups were signed? run scripts/deploy.sh backup-setup)"
    fi
  fi
  # Keep the newest BACKUP_KEEP here too (this service's alone: digits follow SERVICE-).
  # shellcheck disable=SC2206  # the glob is meant to expand to bundle files
  all=(backups/$BUNDLE_GLOB)
  for ((i = 0; i < ${#all[@]} - BACKUP_KEEP; i++)); do rm -f "${all[$i]}" "${all[$i]}.sig"; done
}

# The plain database, before backup-setup.
backup_db() {
  local out latest
  out="backups/$SERVICE-$(date +%Y%m%d-%H%M%S).db"
  if on "systemctl cat $SERVICE-backup.service >/dev/null 2>&1"; then
    # A fresh backup by the daily timer's service, then download the newest.
    on "systemctl start $SERVICE-backup.service"
    latest=$(on "ls -1t $STATE/backups/bot-[0-9]*.db | head -n 1")
    [[ $latest =~ ^$STATE/backups/bot-[0-9-]+\.db$ ]] || die "no database copy on $HOST"
    (umask 077 && ssh "$HOST" "cat $latest" > "$out") || die "couldn't download $latest"
  else
    # Older setups without the timer. sqlite's backup command gives a
    # consistent copy while the bot runs. The database is in WAL mode, so
    # without sqlite3 a plain copy is only safe with the bot (and web UI)
    # stopped. Into a private folder (an older version used a fixed /tmp name).
    (umask 077 && on "rm -f /tmp/$SERVICE-backup.db 2>/dev/null || true
        t=\$(mktemp -d) && trap 'rm -rf \"\$t\"' EXIT
        if command -v sqlite3 >/dev/null; then sqlite3 $DB \".backup \$t/bot.db\"
        else systemctl stop $SERVICE $SERVICE-web 2>/dev/null; cp $DB \$t/bot.db
          systemctl start $SERVICE $SERVICE-web 2>/dev/null; fi
        cat \$t/bot.db" > "$out") || die "couldn't copy the database"
  fi
  chmod 600 "$out"
  echo "saved $out"
  note "that's the database alone, unencrypted; for nightly encrypted off-site backups with the secrets: scripts/deploy.sh backup-setup"
}

# Decrypt bundle $1 once, into private folder $2, reading from that one
# stream: its member names into $2/members, and its env's "SETTINGS TOKENS"
# counts into $2/env (the secrets themselves are never written); bot.db,
# info.txt and recordings.txt are unpacked into $2/x ($3: the bundle's folder).
open_bundle() {
  local ct=$1 d=$2 top=$3 p1 p2 ok=0
  mkdir "$d/x"
  mkfifo "$d/names" "$d/secrets"
  { tar -tzf - > "$d/members" 2> /dev/null || true; cat > /dev/null; } < "$d/names" &
  p1=$!
  {
    tar -xzOf - "$top/env" 2> /dev/null |
      awk '/^DISCORD_TOKEN=./ { t++ } /^[A-Za-z_][A-Za-z0-9_]*=/ { n++ } END { print n + 0, t + 0 }' > "$d/env"
    cat > /dev/null
  } < "$d/secrets" &
  p2=$!
  if age -d -i "$BACKUP_AGE_IDENTITY" "$ct" | tee "$d/names" "$d/secrets" | {
    (cd "$d/x" && tar -xzf - --no-same-owner --no-same-permissions \
      "$top/bot.db" "$top/info.txt" "$top/recordings.txt" 2> /dev/null) || true
    cat > /dev/null
  }; then
    ok=1
  fi
  wait "$p1" "$p2" || true
  [ "$ok" = 1 ]
}

backup_check() {
  offsite_set_up || die "off-site backups aren't set up: scripts/deploy.sh backup-setup"
  need_age
  need_signer
  local problems=0 newest='' size='' ct made hours pd top x f n
  say "the nightly backup on $HOST"
  if on "systemctl is-failed -q $SERVICE-backup.service"; then
    echo "  its last run failed: ssh $HOST journalctl -u $SERVICE-backup -n 30"
    problems=1
  fi
  on "systemctl is-active -q $SERVICE-backup.timer" || { echo "  its timer is off (scripts/deploy.sh backup-setup)"; problems=1; }
  pd=$(private_dir)
  CLEANUP+=("rm -rf $(printf %q "$pd")")
  if [ "$BACKUP_TARGET" = pull ]; then
    say "the newest bundle here (backups/)"
    ct=$(newest_local) || die "  none: run scripts/deploy.sh backup"
    newest=${ct##*/}
    size=$(wc -c < "$ct" | tr -d ' ')
  else
    say "the newest bundle at $BACKUP_TARGET"
    read -r newest size <<< "$(newest_dest)" || true
    [ -n "$newest" ] || die "  none there"
    # Straight from the destination, through the server (still encrypted), into the private folder.
    ct=$pd/$newest
    ssh "$HOST" "$OFFSITE get $newest" > "$ct" || die "  couldn't download it"
    ssh "$HOST" "$OFFSITE get $newest.sig" > "$ct.sig" 2> /dev/null || rm -f "$ct.sig"
    [ "$(wc -c < "$ct" | tr -d ' ')" = "$size" ] || die "  it didn't download whole"
  fi
  echo "  $newest: $size bytes"
  verify_bundle "$ct" "$pd" || die "Backups need a look: the newest isn't signed by $HOST's backup key, so it isn't opened."
  echo "  signed by $HOST's backup key"
  say "decrypting it with $BACKUP_AGE_IDENTITY"
  top=${newest%.tar.gz.age}
  open_bundle "$ct" "$pd" "$top" || die "  it doesn't decrypt"
  x=$(head -n 1 "$pd/members")
  x=${x%%/*}
  [ "$x" = "$top" ] ||
    die "Backups need a look: $newest holds $(clean <<< "$x" | cut -c 1-80) (an older bundle renamed?)"
  x=$pd/x/$top
  bundle_ok "$top" < "$pd/members" || problems=1
  for f in bot.db info.txt; do
    if [ -f "$x/$f" ] && [ ! -L "$x/$f" ]; then echo "  $f: $(wc -c < "$x/$f" | tr -d ' ') bytes"; fi
  done
  [ -s "$x/bot.db" ] || { echo "  there's no database in it"; problems=1; }
  read -r n f < "$pd/env" || true
  n=${n:-0} f=${f:-0}
  if [ "$f" -gt 0 ]; then echo "  env: $n settings, DISCORD_TOKEN among them"; else
    echo "  there are no secrets (DISCORD_TOKEN) in it"
    problems=1
  fi
  if command -v sqlite3 > /dev/null && [ -s "$x/bot.db" ]; then
    if [ "$(sqlite3 "$x/bot.db" 'PRAGMA integrity_check' 2>&1 | head -n 1)" = ok ]; then
      echo "  the database is intact"
    else
      echo "  the database is damaged"
      problems=1
    fi
  fi
  if [ -s "$x/recordings.txt" ]; then
    n="(listed, not in the bundle)"
    if grep -qxF "$top/recordings.tar" "$pd/members"; then n="(in the bundle)"; fi
    echo "  recordings: $(awk '{ n++; s += $1 } END { printf "%d files, %d MB", n, s / 1048576 }' "$x/recordings.txt") $n"
  fi
  # How old it is by the time signed inside it, not by its name.
  made=$(sed -n 's/^made-at: \([0-9][0-9]*\)$/\1/p' "$x/info.txt" 2> /dev/null | head -n 1)
  if [ -z "$made" ]; then
    echo "  it doesn't say when it was made (an older bundle?)"
    problems=1
  else
    hours=$((($(date +%s) - made) / 3600))
    echo "  made ${hours}h ago"
    if [ "$hours" -lt -1 ]; then
      echo "  that's in the future: is a clock wrong?"
      problems=1
    elif [ "$hours" -ge "$BACKUP_MAX_AGE_HOURS" ]; then
      echo "  that's older than ${BACKUP_MAX_AGE_HOURS}h"
      problems=1
    fi
  fi
  if ! grep -qxF "bundle: $newest" "$x/info.txt" 2> /dev/null; then
    echo "  it says it's another bundle than $newest (renamed?)"
    problems=1
  fi
  [ ! -f "$x/info.txt" ] || clean < "$x/info.txt" | sed 's/^/  /'
  rm -rf "$pd"
  [ "$problems" = 0 ] || die "Backups need a look (above)."
  echo "Backups look fine."
}

# The settings names (not values) in this machine's secrets file and
# BACKUP_ENV_KEYS: these may be in a restored .env too.
known_env_keys() {
  local k out=''
  if [ -f "$SECRETS" ]; then out=$(sed -n 's/^\([A-Za-z_][A-Za-z0-9_]*\)=.*/\1/p' "$SECRETS" | tr '\n' ' '); fi
  for k in $BACKUP_ENV_KEYS; do
    [[ $k =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || die "BACKUP_ENV_KEYS: $k isn't a setting's name"
    out="$out $k"
  done
  echo "$out"
}

restore() {
  local f='' db_only='' unsigned='' renamed='' a t what pd why
  for a in "$@"; do
    case $a in
      --db-only) db_only=1 ;;
      --unsigned) unsigned=1 ;;
      --renamed) renamed=1 ;;
      -*) die "restore: unknown option $a" ;;
      *) f=$a ;;
    esac
  done
  [ -n "$f" ] || f=$(newest_local) ||
    die "usage: $0 restore [backup file] [--db-only] [--unsigned] [--renamed] (there's none in backups/: $0 backup)"
  [ -f "$f" ] || die "$f not found"
  case $f in *.age | *.db) ;; *) die "$f: expected a .tar.gz.age bundle or a .db file" ;; esac
  t=$(on "[ -d $DIR ] && mktemp -d $DIR/.restore.XXXXXX") || die "$HOST has no $DIR: run scripts/deploy.sh setup first"
  [[ $t =~ ^$DIR/\.restore\.[A-Za-z0-9]+$ ]] || die "odd folder from $HOST: $(clean <<< "$t")"
  CLEANUP+=("on_host $(printf %q "$HOST") $(printf %q "rm -rf $t")")
  # The checks below and the swap need it (a server set up afresh may lack it).
  ensure_pkgs "$HOST" sqlite3
  case $f in
    *.age)
      need_age
      if [ -n "$unsigned" ]; then
        note "not checking who made $f (--unsigned): only for a bundle made before backups were signed, that you know is yours"
        confirm "Restore $f without checking its signature?" || die "nothing changed"
      else
        need_signer
        pd=$(private_dir)
        CLEANUP+=("rm -rf $(printf %q "$pd")")
        verify_bundle "$f" "$pd" ||
          die "not restoring $f: it may not be from $HOST's backups (one made before they were signed: --unsigned)"
        say "$f is signed by the server's backup key"
      fi
      say "decrypting $f here, unpacking it on $HOST"
      # Decrypted in the pipe: the secrets aren't written to this machine's disk.
      age -d -i "$BACKUP_AGE_IDENTITY" "$f" | ssh "$HOST" "umask 077 && cat > $t/bundle.tar.gz" || die "couldn't decrypt $f"
      ;;
    *.db)
      ssh "$HOST" "umask 077 && cat > $t/bundle.db" < "$f"
      db_only=1
      ;;
  esac
  remote "$HOST" "$t" "$DIR" "$SERVICE" "$db_only" "$(known_env_keys)" "${f##*/}" "$unsigned" << 'REMOTE'
t=$1 dir=$2 service=$3 db_only=$4 keys=$5 file=$6 unsigned=$7
cd "$t"
# Why this bundle may not be the one its file name says (an older one renamed
# as the newest would roll the database back): into $t/renamed.
renamed() { echo "  $1" >&2; echo "$1" >> renamed; }
if [ -f bundle.db ]; then
  mkdir -p x/db && mv bundle.db x/db/bot.db
else
  tar -tzf bundle.tar.gz > members 2> /dev/null || { echo "the backup doesn't unpack" >&2; exit 1; }
  top=$(head -n 1 members)
  top=${top%%/*}
  [[ $top =~ ^$service-[0-9]{8}-[0-9]{6}$ ]] || { echo "that isn't a backup of $service" >&2; exit 1; }
  bundle_ok "$top" < members || exit 1
  [ "$top.tar.gz.age" = "$file" ] || renamed "inside, it's $top.tar.gz.age"
  want=()
  for m in bot.db env info.txt config-unpushed.patch recordings.txt recordings.tar; do
    if grep -qxF "$top/$m" members; then want+=("$top/$m"); fi
  done
  # Only those names, as plain files owned by root (0600), whatever the archive says.
  mkdir x
  tar -xzf bundle.tar.gz -C x --no-same-owner --no-same-permissions "${want[@]}"
  rm -f bundle.tar.gz
  mv "x/$top" x/db
fi
b=x/db
for m in "$b"/*; do
  [ -f "$m" ] && [ ! -L "$m" ] && [ "$(stat -c %h "$m")" = 1 ] || { echo "the backup's ${m##*/} isn't a plain file" >&2; exit 1; }
done
[ -f "$b/bot.db" ] || { echo "there's no bot.db in the backup" >&2; exit 1; }
ok=$(sqlite3 "$b/bot.db" 'PRAGMA integrity_check' 2>&1 | head -n 1)
rm -f "$b/bot.db-wal" "$b/bot.db-shm"
[ "$ok" = ok ] || { echo "the backup's database is damaged: $(printf '%s' "$ok" | tr -cd '[:print:]')" >&2; exit 1; }
echo "  database: $(du -h "$b/bot.db" | cut -f1)"
if [ "$db_only" != 1 ]; then
  [ -f "$b/env" ] || { echo "there are no secrets in the backup (--db-only restores the database alone)" >&2; exit 1; }
  grep -q '^DISCORD_TOKEN=' "$b/env" || { echo "the backup's secrets have no DISCORD_TOKEN" >&2; exit 1; }
  env_ok "$b/env" "$keys" "$dir" ||
    { echo "not restoring these secrets (above); --db-only restores the database alone" >&2; exit 1; }
  echo "  secrets: $(grep -c '^[A-Za-z_][A-Za-z0-9_]*=' "$b/env") settings"
  if [ -f "$b/recordings.tar" ]; then
    tar -tvf "$b/recordings.tar" > rec-members 2> /dev/null || { echo "the backup's recordings don't unpack" >&2; exit 1; }
    # Plain files and folders (GNU tar -tv: six fields), relative names of plain characters, no "..".
    if ! awk '{ t = substr($1, 1, 1); n = $NF
        if ((t != "-" && t != "d") || NF != 6 || n ~ /^\// || n ~ /(^|\/)\.\.(\/|$)/ || n !~ /^[A-Za-z0-9._\/-]+$/) bad = 1 }
        END { exit bad }' rec-members; then
      echo "the backup's recordings hold something recordings don't (links, odd names)" >&2
      exit 1
    fi
    echo "  recordings: $(grep -c '^-' rec-members) files (added to the server's)"
  elif [ -s "$b/recordings.txt" ]; then
    echo "  recordings: not in the backup ($(wc -l < "$b/recordings.txt") files were on the server then)"
  fi
fi
[ ! -f "$b/info.txt" ] || tr -d '\000-\010\013-\037\177' < "$b/info.txt" | sed 's/^/  /'
if [ -n "${top:-}" ]; then
  named=$(sed -n 's/^bundle: //p' "$b/info.txt" 2>/dev/null | head -n 1 | tr -cd '[:print:]')
  if [ -z "$named" ]; then
    # Every signed bundle says which it is; only older, unsigned ones may not.
    [ -n "$unsigned" ] || renamed "it doesn't say which bundle it is"
  elif [ "$named" != "$file" ]; then
    renamed "it says it's $named"
  fi
  made=$(sed -n 's/^made-at: \([0-9][0-9]*\)$/\1/p' "$b/info.txt" 2>/dev/null | head -n 1)
  [ -z "$made" ] || echo "  made $((($(date +%s) - made) / 3600))h ago"
fi
if [ -f "$b/info.txt" ] && [ -d "$dir/config/.git" ]; then
  want=$(sed -n 's/^config commit: //p' "$b/info.txt" | tr -cd '[:alnum:]')
  have=$(sudo -u "$service" git -C "$dir/config" rev-parse HEAD 2>/dev/null || true)
  [ "$want" = "$have" ] || echo "  (the server's config is at ${have:-nothing}; the bot pulls the config repo's newest itself)"
fi
REMOTE
  why=$(on "cat $t/renamed 2>/dev/null" | clean | tr '\n' ';' | sed 's/;$//; s/;/; /g') || why=''
  if [ -n "$why" ]; then
    [ -n "$renamed" ] ||
      die "not restoring ${f##*/}: it isn't the bundle it's named as ($why). An older backup renamed as the newest would roll the database back. If you renamed it yourself, run this again with --renamed."
    printf '%s is named as another bundle than it is (%s).\n' "${f##*/}" "$why" >&2
    confirm "Restore it all the same (you renamed it yourself)?" || die "nothing changed"
  fi
  what="database"
  [ -n "$db_only" ] || what="database and secrets"
  confirm "Replace the $what on $HOST with these? The bot stops for a moment (the current ones are kept)." ||
    die "nothing changed"
  remote "$HOST" "$t" "$DIR" "$SERVICE" "$db_only" "$DB" << 'REMOTE'
t=$1 dir=$2 service=$3 db_only=$4 db=$5
state=${db%/*}
b=$t/x/db
ts=$(date +%Y%m%d-%H%M%S)
for u in "$service" "$service-web"; do systemctl stop "$u" 2>/dev/null || true; done
mkdir -p "$state/backups"
chown -R "$service:$service" "$state"
if [ -f "$db" ]; then
  sudo -u "$service" sqlite3 "$db" ".backup $state/backups/before-restore-$ts.db"
  echo "  kept the replaced database as $state/backups/before-restore-$ts.db"
  keep_newest 3 "$state"/backups/before-restore-*.db
fi
rm -f "$db" "$db-wal" "$db-shm"
mv "$b/bot.db" "$db"
chown -R "$service:$service" "$state"
chmod 700 "$state"
chmod 600 "$db"
if [ "$db_only" != 1 ]; then
  if [ -f "$dir/.env" ]; then
    cp -p "$dir/.env" "$dir/.env.before-restore-$ts"
    echo "  kept the replaced secrets as $dir/.env.before-restore-$ts"
    keep_newest 3 "$dir"/.env.before-restore-*
  fi
  install -m 600 -o root -g root "$b/env" "$dir/.env"
  if [ -f "$b/recordings.tar" ]; then
    # As the bot's user, into its own folder (checked above: files and folders only).
    install -d -m 700 -o "$service" -g "$service" "$state/recordings"
    runuser -u "$service" -- tar -xf - -C "$state/recordings" --no-same-owner --no-same-permissions < "$b/recordings.tar"
    echo "  recordings put back in $state/recordings"
  fi
fi
if [ -f "$b/config-unpushed.patch" ]; then
  install -m 600 "$b/config-unpushed.patch" "$dir/config-unpushed-$ts.patch"
  echo "  the bot had config commits it hadn't pushed: $dir/config-unpushed-$ts.patch (git am them in the config repo if still wanted)"
fi
REMOTE
  if on "[ -L $DIR/current ]"; then
    say "starting the bot"
    start_checked "$HOST" || die "restored, but the bot isn't healthy (above)"
  else
    echo "Restored. There's no release on $HOST yet: run scripts/deploy.sh to start the bot."
  fi
}

# ---- Watching that it's up ----

# The heartbeat check on HOST (deploy/monitor.sh, every minute): its settings
# in DIR/monitor.env, the two URLs in the server's .env. --off turns it off.
monitor_setup() {
  if [ "${1:-}" = --off ]; then
    on "systemctl disable -q --now $SERVICE-monitor.timer 2>/dev/null || true; rm -f $DIR/monitor.env"
    echo "The monitor is off."
    return
  fi
  [ -z "${1:-}" ] || die "usage: $0 monitor-setup [--off]"
  valid MONITOR_STALE_MINUTES "$MONITOR_STALE_MINUTES" '^[1-9][0-9]*$' "a number of minutes"
  valid MONITOR_REPEAT_HOURS "$MONITOR_REPEAT_HOURS" '^[0-9]+$' "a number of hours (0: say it once)"
  [ -z "$MONITOR_EMAIL" ] || valid MONITOR_EMAIL "$MONITOR_EMAIL" '^[A-Za-z0-9._%+-]+@[A-Za-z0-9.-]+\.[A-Za-z]+$' "one mail address"
  [ -z "$MONITOR_ROLE" ] || valid MONITOR_ROLE "$MONITOR_ROLE" '^[0-9]{17,20}$' "a role's id (Developer Mode: right-click the role, Copy Role ID)"
  on "[ -f $DIR/.env ]" || die "there's no $DIR/.env on $HOST yet: scripts/deploy.sh secrets first"
  # Only the names are looked at, on the server.
  if ! on "grep -Eq '^(MONITOR_WEBHOOK|MONITOR_PING_URL)=.' $DIR/.env" && [ -z "$MONITOR_EMAIL" ]; then
    die "nothing to tell: put MONITOR_WEBHOOK (a Discord webhook, in a channel the committee reads) and/or
MONITOR_PING_URL (an outside check, such as healthchecks.io) in $SECRETS and run scripts/deploy.sh secrets,
or set MONITOR_EMAIL in deploy.env; then monitor-setup again (docs/deploy.md, Watching that it's up)"
  fi
  ensure_pkgs "$HOST" curl
  say "monitor settings on $HOST"
  printf '%s=%q\n' MONITOR_STALE_MINUTES "$MONITOR_STALE_MINUTES" MONITOR_REPEAT_HOURS "$MONITOR_REPEAT_HOURS" \
    MONITOR_EMAIL "$MONITOR_EMAIL" MONITOR_ROLE "$MONITOR_ROLE" |
    ssh "$HOST" "umask 077 && cat > $DIR/monitor.env.new && mv $DIR/monitor.env.new $DIR/monitor.env"
  install_units "$HOST"
  on "systemctl enable -q --now $SERVICE-monitor.timer"
  say "a test message"
  on "$MONITOR test" || die "the test didn't reach anything (above): check the URLs in $SECRETS (then secrets, and this again)"
  on "systemctl start $SERVICE-monitor.service" || {
    on "journalctl -u $SERVICE-monitor -n 10 --no-pager -q" >&2 || true
    die "the check itself failed (above)"
  }
  echo "The monitor checks the bot every minute: it says so after $MONITOR_STALE_MINUTES minutes down."
  on "[ -f /var/lib/$SERVICE/heartbeat ]" ||
    note "the bot hasn't written a heartbeat yet (a release from before them?): until the next deploy, only a stopped bot is noticed"
}

# `fed revoke` as the bot runs it: its user, .env and database (never a new
# one), so it changes the state the running bot uses.
revoke() {
  [ -n "${1:-}" ] || { echo "usage: $0 revoke <partner id or name> [key id]" >&2; exit 1; }
  local args
  args="fed revoke $(printf '%q' "$1")"
  [ -z "${2:-}" ] || args="$args --key $(printf '%q' "$2")"
  on "systemd-run --quiet --wait --pipe --collect -p User=$SERVICE -p Group=$SERVICE \\
        -p EnvironmentFile=$DIR/.env -p WorkingDirectory=$DIR \\
        -E 'DATABASE_URL=sqlite:$DB?mode=rw' -E BOT_LOCAL=1 \\
        $DIR/current/bot --config $DIR/config $args"
}

# git in the server's config checkout, as the bot's user with its deploy key.
config_git() {
  [ $# -gt 0 ] || { echo "usage: $0 git <git arguments>" >&2; exit 1; }
  on "cd $DIR/config && sudo -u $SERVICE env GIT_SSH_COMMAND='$GIT_SSH' git $(printf '%q ' "$@")"
}

# ---- Moving to another server ----
# Progress is appended to $MIGRATE_STATE as key=value lines (the last of each
# key counts): kind (migrate, or back), from, to, and step, the last step done.

STEPS=(start preflight prepared cutting cutover tailnet host finished)
state_get() { [ ! -f "$MIGRATE_STATE" ] || sed -n "s/^$1=//p" "$MIGRATE_STATE" | tail -n 1; }
state_set() { echo "$1=$2" >> "$MIGRATE_STATE"; }
state_new() {
  mkdir -p "${MIGRATE_STATE%/*}"
  printf 'started=%s\nkind=%s\nfrom=%s\nto=%s\nstep=start\n' "$(date -u +%FT%TZ)" "$1" "$2" "$3" >> "$MIGRATE_STATE"
}
# Whether the move got as far as step $1.
past() {
  local i now=0 want=0 step
  step=$(state_get step)
  for i in "${!STEPS[@]}"; do
    [ "${STEPS[$i]}" != "$step" ] || now=$i
    [ "${STEPS[$i]}" != "$1" ] || want=$i
  done
  [ "$now" -ge "$want" ]
}

# Refuse to work on HOST once the bot has moved off it: a second copy would
# run. Known here (.deploy-state) and, for every other checkout, on the
# server itself ($DIR/MOVED-TO, written by the move). --force uses it again.
guard_moved() {
  local m
  if [ -z "$FORCE" ] && [ "$(state_get from)" = "$HOST" ] && past cutover; then
    die "the bot moved from $HOST to $(state_get to): set HOST=$(state_get to) in deploy.env (or run $0 migrate-back)"
  fi
  m=$(on "head -n 1 $DIR/MOVED-TO 2>/dev/null" 2> /dev/null | clean) || true
  [ -n "$m" ] || return 0
  [ -n "$FORCE" ] ||
    die "the bot $m, so $HOST doesn't run it any more (two copies would both answer): set HOST in deploy.env to where it runs. To use $HOST again all the same: $0 $cmd --force"
  confirm "The bot $m. Use $HOST again (make sure it doesn't run anywhere else)?" || die "nothing changed"
  # shellcheck disable=SC2016 # run there
  remote "$HOST" "$SERVICE" "$DIR" all <<< 'revive_units "$1" "$2" "$3"'
  note "$HOST is in use again; its units are enabled by the next deploy"
}

# An ssh destination given on the command line.
valid_host() { [[ $1 =~ ^[A-Za-z0-9._@][A-Za-z0-9._@:-]*$ ]] || die "$1: expected an ssh destination such as root@new-server"; }

migrate() {
  local new=${1:-}
  [ -n "$new" ] || die "usage: $0 migrate NEW_HOST (an ssh destination you can log into as root)"
  valid_host "$new"
  [ "$new" != "$HOST" ] || die "$new is HOST already"
  if [ -f "$MIGRATE_STATE" ] && ! past finished && past preflight; then
    if [ "$(state_get kind)" != migrate ] || [ "$(state_get from)" != "$HOST" ] || [ "$(state_get to)" != "$new" ]; then
      die "a move from $(state_get from) to $(state_get to) isn't finished: carry it on first (or, if you've sorted it out by hand, delete $MIGRATE_STATE)"
    fi
    say "carrying on the move to $new (done: $(state_get step))"
  else
    state_new migrate "$HOST" "$new"
  fi
  move_bot "$HOST" "$new" "$new"
}

migrate_back() {
  [ -f "$MIGRATE_STATE" ] || die "there's no move to undo ($MIGRATE_STATE isn't here)"
  local kind from to
  kind=$(state_get kind) from=$(state_get from) to=$(state_get to)
  if [ "$kind" = back ] && ! past finished; then
    say "carrying on the move back to $to (done: $(state_get step))"
    move_bot "$from" "$to" "$from"
    return
  fi
  [ "$kind" = migrate ] || die "the last move was already back to $to"
  past cutover || die "the move to $to never cut over: the bot still runs on $from (delete $MIGRATE_STATE to forget it)"
  [ "$HOST" = "$to" ] || note "HOST is $HOST, but the bot moved to $to; moving it back from $to"
  confirm "Move the bot back from $to to $from?" || die "nothing changed"
  state_new back "$to" "$from"
  move_bot "$to" "$from" "$to"
}

# Move the bot from host $1 to host $2, step by step (each done once, even
# over several runs). The Tailscale API calls run on host $3, the newer server.
move_bot() {
  local from=$1 to=$2 api=$3
  # (Not `step && state_set`: errors inside a step must stop it.)
  if ! past preflight; then
    preflight "$from" "$to"
    state_set step preflight
  fi
  if ! past prepared; then
    prepare_target "$from" "$to"
    state_set step prepared
  fi
  if ! past cutover; then
    cutover "$from" "$to"
    state_set step cutover
  fi
  if ! past tailnet; then
    move_tailnet_name "$from" "$to" "$api"
    state_set step tailnet
  fi
  if ! past host; then
    set_host "$to"
    state_set step host
  fi
  summary "$from" "$to"
  state_set step finished
}

# Free disk (KB) on host $1 where path $2 is (or would be).
free_kb() {
  on_host "$1" "d=$2; while [ ! -e \$d ]; do d=\$(dirname \$d); done; df -Pk \$d | awk 'NR == 2 { print \$4 }'"
}

# Where the voice recordings are, on every server (RECORDINGS_DIR elsewhere isn't moved).
REC=$STATE/recordings

preflight() {
  local from=$1 to=$2 rev db_kb rel_kb rec_kb need free pkgs=(sqlite3 git sudo curl jq tar gzip)
  say "checking $from and $to"
  on_host "$from" true || die "can't ssh to $from"
  on_host "$to" true || die "can't ssh to $to"
  on_host "$to" "[ \"\$(id -u)\" = 0 ] && command -v systemctl >/dev/null" ||
    die "$to: log in as root (HOST-style root@…), on a server with systemd"
  [ "$(on_host "$from" cat /etc/machine-id)" != "$(on_host "$to" cat /etc/machine-id)" ] ||
    die "$from and $to are the same machine"
  on_host "$from" "[ ! -e $DIR/MOVED-TO ]" ||
    die "the bot $(on_host "$from" "head -n 1 $DIR/MOVED-TO" | clean) already: $from doesn't run it"
  rev=$(on_host "$from" "basename \"\$(readlink $DIR/current)\"") || die "$from runs no release ($DIR/current)"
  [[ $rev =~ ^[A-Za-z0-9._-]+$ ]] || die "$from runs a release with an odd name: $(clean <<< "$rev")"
  on_host "$from" "[ -f $DB ] && [ -f $DIR/.env ] && [ -f $DIR/.ssh/deploy_key ]" ||
    die "$from lacks the database ($DB), secrets ($DIR/.env) or deploy key"
  on_host "$to" "! systemctl is-active -q $SERVICE" ||
    die "the bot runs on $to already: stop it there first (two copies would both answer)"
  if [ "$(state_get kind)" = migrate ] &&
    on_host "$to" "[ -e $DIR ] || [ -e $DB ] || [ -e /etc/systemd/system/$SERVICE.service ]"; then
    confirm "$to has some of a $SERVICE install already ($DIR, $DB or its units). Its database and secrets will be replaced (the old ones kept beside them). Carry on?" ||
      die "nothing changed"
  fi
  ensure_pkgs "$from" sqlite3 sudo
  if on_host "$from" "[ -f $DIR/backup/backup.env ]"; then pkgs+=(age); fi
  if on_host "$from" "[ -f $DIR/backup/rclone.conf ]"; then pkgs+=(rclone); fi
  ensure_pkgs "$to" "${pkgs[@]}"
  if [ -n "${WEB_URL:-}" ]; then
    on_host "$to" "tailscale status >/dev/null 2>&1" ||
      die "Tailscale isn't up on $to: install it (https://tailscale.com/download), run tailscale up there, then migrate again"
  fi
  if on_host "$from" "grep -q '^RECORDINGS_DIR=.' $DIR/.env"; then
    note "$from keeps recordings in its own RECORDINGS_DIR: they aren't moved, copy them to $to yourself"
  fi
  db_kb=$(on_host "$from" "du -sk $DB | cut -f1")
  rel_kb=$(on_host "$from" "du -sk $DIR/current/ | cut -f1")
  rec_kb=$(on_host "$from" "if [ -d $REC ]; then du -sk $REC | cut -f1; else echo 0; fi")
  # Room for the database, its copy on the way in, the release and some to spare.
  need=$((2 * db_kb + rel_kb + 262144))
  free=$(free_kb "$to" "$DIR")
  [ "$free" -ge "$need" ] || die "$to has $((free / 1024)) MB free at $DIR, needs about $((need / 1024)) MB"
  # And there, the recordings too.
  need=$((need + rec_kb))
  free=$(free_kb "$to" "$STATE")
  [ "$free" -ge "$need" ] || die "$to has $((free / 1024)) MB free at $STATE, needs about $((need / 1024)) MB (recordings: $((rec_kb / 1024)) MB)"
  [ "$(free_kb "$from" "$STATE")" -ge $((db_kb + 65536)) ] || die "$from has too little disk free for a copy of the database"
  echo "  $from runs release $rev, database $((db_kb / 1024)) MB, recordings $((rec_kb / 1024)) MB; $to has $((free / 1024)) MB free"
}

# Everything but the database onto host $2, ready to start (not enabled),
# and a first copy of the recordings.
prepare_target() {
  local from=$1 to=$2 rev
  rev=$(on_host "$from" "basename \"\$(readlink $DIR/current)\"")
  [[ $rev =~ ^[A-Za-z0-9._-]+$ ]] || die "$from runs a release with an odd name: $(clean <<< "$rev")"
  say "setting up $to"
  prepare_host "$to"
  # Moving back to a server the bot left: its units unmasked (MOVED-TO stays until the cut-over).
  # shellcheck disable=SC2016 # run there
  remote "$to" "$SERVICE" "$DIR" keep <<< 'revive_units "$1" "$2" "$3"'
  copy_private "$from" "$to"
  clone_config "$to"
  copy_release "$from" "$to" "$rev"
  install_units "$to" install
  if ! check_config "$rev" "$to"; then
    note "release $rev doesn't run on $to; building it there"
    on_host "$to" "rm -rf $DIR/releases/$rev"
    build_on "$to" "$rev"
    check_config "$rev" "$to" || die "release $rev can't load the config on $to (above)"
  fi
  copy_recordings "$from" "$to" first
}

# The secrets and keys (.env, the deploy key, the off-site backup settings
# and keys) from host $1 to host $2, through this machine's ssh connections:
# they're never written here.
copy_private() {
  local from=$1 to=$2 files
  files=$(on_host "$from" "cd $DIR && for f in .env .ssh backup monitor.env; do [ ! -e \$f ] || echo \$f; done" | tr '\n' ' ')
  [[ $files =~ ^[a-z.\ ]*$ ]] || die "odd files on $from: $(clean <<< "$files")"
  say "copying the secrets and keys from $from (${files% })"
  ssh "$from" "tar -C $DIR -czf - --exclude=backup/bundles --exclude='backup/.work.*' $files" |
    ssh "$to" "set -e; umask 077
      [ ! -f $DIR/.env ] || cp -p $DIR/.env $DIR/.env.before-migrate-\$(date +%Y%m%d-%H%M%S)
      { ls -1t $DIR/.env.before-migrate-* 2>/dev/null || true; } | tail -n +4 | xargs -r rm -f --
      tar -C $DIR -xzf - --no-same-owner
      chown root:root $DIR/.env && chmod 600 $DIR/.env
      chown -R $SERVICE:$SERVICE $DIR/.ssh && chmod 700 $DIR/.ssh && chmod 600 $DIR/.ssh/deploy_key
      if [ -d $DIR/backup ]; then chown -R root:root $DIR/backup && chmod 700 $DIR/backup; fi"
}

# Release $3 onto host $2 (copied from host $1, or built there if the
# machines differ), as its `current`.
copy_release() {
  local from=$1 to=$2 rev=$3
  if on_host "$to" "[ -x $DIR/releases/$rev/bot ]"; then
    echo "  $to has release $rev"
  elif [ "$(on_host "$from" uname -m)" = "$(on_host "$to" uname -m)" ]; then
    say "copying release $rev"
    ssh "$from" "tar -C $DIR/releases -czf - $rev" | ssh "$to" "tar -C $DIR/releases -xzf - --no-same-owner"
  else
    build_on "$to" "$rev"
  fi
  on_host "$to" "c=\$(readlink $DIR/current 2>/dev/null || true)
    if [ -n \"\$c\" ] && [ \"\$c\" != $DIR/releases/$rev ]; then ln -sfn \"\$c\" $DIR/previous; fi
    ln -sfn $DIR/releases/$rev $DIR/current"
}

# Build release $2 for host $1 from this checkout.
build_on() {
  git rev-parse -q --verify "$2^{commit}" > /dev/null || die "can't build $2 for $1: it isn't in this checkout (git fetch?)"
  build_release "$1" "$2" "$2"
}

# A hash of host $1's database files as they are (not opened, so not changed).
db_sum() { on_host "$1" "{ cat $DB; [ ! -f $DB-wal ] || cat $DB-wal; } | sha256sum | cut -d' ' -f1"; }

# Stop on host $1 (and mark it as moved), copy the database, config and
# recordings over, start on host $2. If it isn't healthy there, stop it there
# and start host $1 again.
cutover() {
  local from=$1 to=$2 resume='' copied=''
  if [ "$(state_get step)" = cutting ]; then
    resume=1
    say "the last run stopped during the cut-over; doing it again"
  else
    confirm "Stop the bot on $from and start it on $to now? It's offline for about a minute." ||
      die "nothing changed; run it again to carry on"
    state_set step cutting
  fi
  say "stopping the bot on $from (kept there: disabled, masked, and marked as moved to $to)"
  # shellcheck disable=SC2016 # run there
  remote "$from" "$SERVICE" "$DIR" "$to" <<< 'retire_units "$1" "$2" "$3"' ||
    die "couldn't stop the bot on $from; it may still run there"
  if on_host "$to" "systemctl is-active -q $SERVICE"; then
    # Started there by the run that stopped? Only if $from's database is still the one copied.
    if [ -n "$resume" ] && [ -n "$(state_get copied)" ] && [ "$(db_sum "$from")" = "$(state_get copied)" ]; then
      say "the bot was started on $to already, with $from's database as it still is"
      copied=1
    else
      die "the bot runs on $to, but not with the database $from has now. Both have changes: stop one of them by hand (ssh $to systemctl stop $SERVICE to keep $from's), then run this again."
    fi
  fi
  if [ -z "$copied" ] && ! { copy_db "$from" "$to" && copy_config "$from" "$to" && copy_recordings "$from" "$to" last; }; then
    rollback_cutover "$from" "$to"
  fi
  on_host "$to" "rm -f $DIR/MOVED-TO"
  say "starting the bot on $to"
  start_checked "$to" || rollback_cutover "$from" "$to"
  echo "The bot runs on $to."
}

rollback_cutover() {
  local from=$1 to=$2
  echo "› it didn't work on $to; going back to $from" >&2
  # shellcheck disable=SC2016 # run there
  if ! remote "$to" "$SERVICE" <<< 'stop_units "$1"'; then
    echo "!! Couldn't make sure the bot stopped on $to, so it isn't started on $from again: two copies would run." >&2
    echo "!! See for yourself: ssh $to systemctl status $SERVICE. Then run this again: it carries on." >&2
    exit 1
  fi
  # From here $from runs again with its own database, so the next cut-over copies it afresh.
  state_set step prepared
  # shellcheck disable=SC2016 # run there
  remote "$from" "$SERVICE" "$DIR" all <<< 'revive_units "$1" "$2" "$3"' || true
  if start_checked "$from"; then
    echo "Back on $from as it was (its database as when it stopped). Fix the problem above and migrate again." >&2
  else
    echo "It isn't healthy on $from either: ssh $from journalctl -u $SERVICE -n 50" >&2
  fi
  exit 1
}

# The database from host $1 (stopped) to host $2: a consistent copy, checked
# on arrival. Host $2's old database, if any, is kept beside it (the newest 3).
copy_db() {
  local from=$1 to=$2 snap=$STATE/backups/migrate.db in=$DIR/.incoming.db sum
  say "copying the database"
  sum=$(on_host "$from" "sudo -u $SERVICE mkdir -p $STATE/backups && rm -f $snap
      sudo -u $SERVICE sqlite3 $DB '.backup $snap' && sudo -u $SERVICE sha256sum $snap | cut -d' ' -f1") || return 1
  ssh "$from" "sudo -u $SERVICE cat $snap | gzip -1" | ssh "$to" "umask 077 && gunzip > $in" || return 1
  on_host "$to" "[ \"\$(sha256sum < $in | cut -d' ' -f1)\" = $sum ] || { echo 'the copy differs from the original' >&2; exit 1; }
      [ \"\$(sqlite3 $in 'PRAGMA integrity_check')\" = ok ] || { echo 'the copy is damaged' >&2; exit 1; }
      rm -f $in-wal $in-shm
      mkdir -p $STATE/backups && chown -R $SERVICE:$SERVICE $STATE && chmod 700 $STATE
      if [ -f $DB ]; then sudo -u $SERVICE sqlite3 $DB \".backup $STATE/backups/before-migrate-\$(date +%Y%m%d-%H%M%S).db\"; fi
      { ls -1t $STATE/backups/before-migrate-*.db 2>/dev/null || true; } | tail -n +4 | xargs -r rm -f --
      rm -f $DB $DB-wal $DB-shm && mv $in $DB && chown $SERVICE:$SERVICE $DB && chmod 600 $DB" || return 1
  on_host "$from" "rm -f $snap" || true
  # What $from's database was when copied: a later run checks it's still so.
  state_set copied "$(db_sum "$from")"
}

# The config checkout from host $1 to host $2 as it is, with any commits the
# bot hasn't pushed yet.
copy_config() {
  local from=$1 to=$2 in=$DIR/.incoming-config
  say "copying the config checkout"
  ssh "$from" "tar -C $DIR -czf - config" |
    ssh "$to" "rm -rf $in && mkdir $in && tar -C $in -xzf - --no-same-owner && rm -rf $DIR/config &&
      mv $in/config $DIR/config && rmdir $in && chown -R $SERVICE:$SERVICE $DIR/config" || return 1
}

# The voice recordings ($REC) from host $1 to host $2, streamed through here.
# $3 `first`: all of them, while the bot still runs (it can take a while).
# `last`: with the bot stopped, those changed since, and the ones deleted
# since deleted there too; then both must hold the same files.
copy_recordings() {
  local from=$1 to=$2 mode=$3 mark=$STATE/.recordings-copied a b
  on_host "$from" "[ -d $REC ]" || return 0
  if [ "$mode" = first ] || ! on_host "$from" "[ -f $mark ]"; then
    say "copying the recordings ($(on_host "$from" "du -sh $REC | cut -f1"))"
    on_host "$from" "touch $mark"
    ssh "$from" "tar -C $STATE -cf - recordings" |
      ssh "$to" "mkdir -p $STATE && tar -C $STATE -xf - --no-same-owner && chown -R $SERVICE:$SERVICE $STATE && chmod 700 $STATE" ||
      return 1
    [ "$mode" = first ] && return 0
  else
    say "copying the recordings made since"
    ssh "$from" "cd $REC && find . -newer $mark \\( -type f -o -type d \\) -print0 | tar --null --no-recursion -T - -cf -" |
      ssh "$to" "mkdir -p $REC && tar -C $REC -xf - --no-same-owner && chown -R $SERVICE:$SERVICE $REC" || return 1
    # Deleted on $from since the first copy: deleted on $to too.
    ssh "$from" "cd $REC && find . -mindepth 1 -print0" | ssh "$to" "cd $REC && find . -mindepth 1 -print0 > ../.recordings-here
      tr '\\0' '\\n' | sort > ../.recordings-there; tr '\\0' '\\n' < ../.recordings-here | sort |
      comm -13 ../.recordings-there - | sort -r | while IFS= read -r p; do rm -rf -- \"\$p\"; done
      rm -f ../.recordings-here ../.recordings-there" || return 1
  fi
  a=$(on_host "$from" "cd $REC && find . -type f -printf '%P %s\n' | sort | sha256sum")
  b=$(on_host "$to" "cd $REC && find . -type f -printf '%P %s\n' | sort | sha256sum")
  [ "$a" = "$b" ] || { echo "the recordings on $to aren't the same as on $from" >&2; return 1; }
  on_host "$from" "rm -f $mark" || true
}

# Host $1's tailnet name and node id, read with host $2's jq: "NAME ID".
ts_self() {
  ssh "$1" "tailscale status --json" | ssh "$2" "jq -r '.Self.DNSName + \" \" + .Self.ID'" | sed 's/\. / /'
}

# Give the tailnet name $5 to machine $4 and call machine $2 (which has it)
# $3, with the Tailscale API key in host $1's .env (the key stays on that
# server). The two renames go straight after each other, so the name is free
# for as short a time as can be, and afterwards the API must say that $4 has
# it. Fails, saying what's what, otherwise.
ts_swap() {
  remote "$1" "$DIR" "$2" "$3" "$4" "$5" << 'REMOTE'
dir=$1 from_id=$2 spare=$3 to_id=$4 name=$5
key=$(sed -n 's/^TAILSCALE_API_KEY=//p' "$dir/.env" | tail -n 1)
key=${key#[\"\']} key=${key%[\"\']}
[ -n "$key" ] || { echo "there's no TAILSCALE_API_KEY in $dir/.env" >&2; exit 1; }
api() { printf 'user = "%s:"\n' "$key" | curl -fsS -K - "$@"; }
rename() {
  api -X POST -H 'Content-Type: application/json' -d "{\"name\": \"$2\"}" \
    "https://api.tailscale.com/api/v2/device/$1/name" > /dev/null
}
rename "$from_id" "$spare" || { echo "couldn't rename the old machine to $spare" >&2; exit 1; }
if ! rename "$to_id" "$name"; then
  echo "!! couldn't rename the new machine to $name" >&2
  rename "$from_id" "$name" ||
    echo "!! nor give $name back to the old machine: $name is free, and anyone on the tailnet can take it" >&2
  exit 1
fi
got=$(api "https://api.tailscale.com/api/v2/device/$to_id" | jq -r .name) || got=''
case $got in
  "$name" | "$name".*) ;;
  *) echo "!! the API says the new machine is ${got:-unknown}, not $name: another machine may have taken $name" >&2; exit 1 ;;
esac
REMOTE
}

# Move the dashboard's tailnet name (WEB_URL's machine) from host $1 to host
# $2, so WEB_URL and the Discord sign-in redirect stay valid; then publish
# the web UI there. The Tailscale API calls run on host $3.
TAILNET_NOTE=
move_tailnet_name() {
  local from=$1 to=$2 api=$3 want name spare from_dns='' from_id='' to_dns='' to_id='' i
  [ -n "${WEB_URL:-}" ] || return 0
  want=${WEB_URL#*://} want=${want%%/*} want=${want%%:*}
  name=${want%%.*}
  read -r from_dns from_id <<< "$(ts_self "$from" "$api")" || true
  read -r to_dns to_id <<< "$(ts_self "$to" "$api")" || true
  if [ "$to_dns" = "$want" ]; then
    echo "  $to is $want already"
  elif [ "$from_dns" != "$want" ]; then
    note "WEB_URL's machine ($want) isn't $from (${from_dns:-not on the tailnet}), so it isn't moved: make sure $want reaches $to"
  elif ! [[ $from_id =~ ^[A-Za-z0-9]+$ && $to_id =~ ^[A-Za-z0-9]+$ && $name =~ ^[a-z0-9-]+$ ]]; then
    TAILNET_NOTE=manual
  else
    spare=$name-old
    [ "${to_dns%%.*}" != "$spare" ] || spare=$name-new
    say "moving $want to $to (and calling $from $spare)"
    if ts_swap "$api" "$from_id" "$spare" "$to_id" "$name"; then
      TAILNET_NOTE="$from is $spare on the tailnet now"
      for i in $(seq 30); do
        [ "$(ts_self "$to" "$api" | cut -d' ' -f1)" != "$want" ] || break
        [ "$i" != 30 ] || note "$to doesn't call itself $want yet: check tailscale status there"
        sleep 1
      done
    else
      TAILNET_NOTE=manual
    fi
  fi
  if [ "$TAILNET_NOTE" = manual ]; then
    echo "!! The dashboard's name ($want) wasn't moved to $to (above; is TAILSCALE_API_KEY in the secrets, and current?)." >&2
    echo "!! See to it now, in the Tailscale admin console (Machines): until $to is $name, whoever holds $want gets" >&2
    echo "!! the dashboard's sign-ins." >&2
    TAILNET_NOTE="CHECK NOW in the Tailscale admin console (Machines) that $name is $to: rename $from to ${spare:-$name-old} and $to to $name if not"
  fi
  on_host "$to" "tailscale serve --bg --https=443 http://127.0.0.1:$WEB_PORT >/dev/null"
}

# Point deploy.env's HOST at $1 (asks first).
set_host() {
  local to=$1
  [ "$HOST" != "$to" ] || return 0
  if grep -q '^HOST=' deploy.env && confirm "Set HOST=$to in deploy.env (it's $HOST now)?"; then
    awk -v h="$to" '!d && /^HOST=/ { print "HOST=" h; d = 1; next } 1' deploy.env > .deploy-state/deploy.env.new
    cat .deploy-state/deploy.env.new > deploy.env && rm .deploy-state/deploy.env.new
    HOST=$to
    echo "  deploy.env: HOST=$to"
  else
    echo "  Set HOST=$to in deploy.env yourself: until then the other commands refuse to run."
  fi
}

summary() {
  local from=$1 to=$2
  echo
  echo "Done: the bot runs on $to."
  echo "  $from: stopped, disabled and masked ($DIR/MOVED-TO says where it went), with everything still there."
  echo "  To go back: $0 migrate-back"
  [ -z "$TAILNET_NOTE" ] || echo "  Dashboard: $TAILNET_NOTE."
  if [ -n "${WEB_URL:-}" ]; then
    echo "  Admins' Tailscale shares were of $from's machine: each gets a new link with /server web."
  fi
  if on_host "$to" "[ -f $DIR/backup/backup.env ]"; then
    echo "  Off-site backups run from $to now (check them: $0 backup-check)."
  fi
  echo "  The config repo's deploy key is the same one (\"$SERVICE on <first host>\" on GitHub)."
  echo "  Recordings were copied too: look at the Recordings page on $to's dashboard before deleting anything on $from."
  echo "  Once you're happy (a few days): ssh $from 'rm -rf $DIR $STATE /etc/systemd/system/$SERVICE*'"
  echo "  and remove $from from the tailnet. This move's record: $MIGRATE_STATE"
}

cmd=${1:-deploy}
case $cmd in migrate | migrate-back) ;; *) guard_moved ;; esac
case $cmd in
  setup) setup ;;
  secrets) secrets ;;
  deploy) deploy ;;
  rollback) rollback ;;
  backup-setup) backup_setup ;;
  backup) backup "${2:-}" ;;
  backup-check) backup_check ;;
  restore) shift; restore "$@" ;;
  monitor-setup) monitor_setup "${2:-}" ;;
  migrate) migrate "${2:-}" ;;
  migrate-back) migrate_back ;;
  revoke) revoke "${2:-}" "${3:-}" ;;
  git) shift; config_git "$@" ;;
  *) echo "usage: $0 [setup|secrets|deploy|rollback|backup-setup|backup [--latest]|backup-check|restore [FILE] [--db-only] [--unsigned] [--renamed]|monitor-setup [--off]|migrate NEW_HOST|migrate-back|revoke PARTNER [KEY]|git ARGS] [--force]" >&2; exit 1 ;;
esac
