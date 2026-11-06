#!/usr/bin/env bash
# Installed by scripts/deploy.sh as @DIR@/backup-offsite (root only), which
# fills in the @…@ values. @SERVICE@-backup.service runs it as root after its
# database copy: it bundles that copy, the secrets (@DIR@/.env), a list of the
# voice recordings (or, with BACKUP_RECORDINGS=1, the recordings) and a note
# of when and where it was made; encrypts the bundle to the owner's age key
# (the private key is never on the server), signs it with the server's own
# key (@DIR@/backup/sign_key, so a restore can tell it's the server's) and
# sends both off the server. Settings: @DIR@/backup/backup.env, written by
# `deploy.sh backup-setup`; without it, `run` does nothing.
#
# An ssh destination lets the server's key run only deploy/backup-dest.sh
# there (put, list, get, prune, check): see that file.
#
#   backup-offsite [run]        make, send and prune a bundle
#   backup-offsite list [local] bundles at the destination (or kept here), newest first: NAME BYTES
#   backup-offsite get NAME     a bundle (or NAME.sig) from the destination, to stdout (still encrypted)
#   backup-offsite check        whether the destination can be reached and written to
set -euo pipefail
# Errors stop $(…) too, such as make_bundle.
shopt -s inherit_errexit
umask 077
export LC_ALL=C
dir=@DIR@
service=@SERVICE@
[[ $service =~ ^[a-z][a-z0-9-]*$ ]] || { echo "SERVICE $service: lower-case letters, digits and - only" >&2; exit 1; }
# The database copies @SERVICE@-backup.service makes as the bot's user, and the recordings.
copies=/var/lib/$service/backups
recordings=/var/lib/$service/recordings
conf=$dir/backup/backup.env
# Encrypted bundles kept on the server (root only, outside the bot's folders).
bundles=$dir/backup/bundles
sign_key=$dir/backup/sign_key
# The ssh-keygen signature namespace (deploy.sh checks the same).
sign_ns=bot-backup
# A bundle's (or its signature's) name, and a line of `list`.
name_re="^$service-[0-9]{8}-[0-9]{6}\.tar\.gz\.age(\.sig)?\$"
line_re="^$service-[0-9]{8}-[0-9]{6}\.tar\.gz\.age [0-9]+\$"
cmd=${1:-run}

if [ ! -f "$conf" ]; then
  [ "$cmd" != run ] || exit 0
  echo "off-site backups aren't set up (scripts/deploy.sh backup-setup)" >&2
  exit 1
fi
# shellcheck disable=SC1090
. "$conf"
target=${BACKUP_TARGET:?BACKUP_TARGET is not set in $conf}
keep=${BACKUP_KEEP:-30}
[[ $keep =~ ^[1-9][0-9]*$ ]] || { echo "BACKUP_KEEP should be a number" >&2; exit 1; }
case $target in
  pull) kind=pull ;;
  rclone:*) kind=rclone remote=${target#rclone:} ;;
  *:*) kind=ssh dest=${target%%:*} ;;
  *) echo "BACKUP_TARGET: expected pull, rclone:REMOTE:PATH or USER@HOST:PATH, not $target" >&2; exit 1 ;;
esac

# The destination's forced command (backup-dest.sh), with the server's backup key.
dest_ssh() {
  ssh -F /dev/null -i "$dir/backup/ssh_key" -o UserKnownHostsFile="$dir/backup/known_hosts" \
    -o IdentitiesOnly=yes -o BatchMode=yes -o ConnectTimeout=20 -- "$dest" "$@"
}
rcl() { rclone --config "$dir/backup/rclone.conf" "$@"; }
# An rclone remote:path joined with a file name.
rjoin() { case $1 in *: | */) echo "$1$2" ;; *) echo "$1/$2" ;; esac }
# git in the config checkout as the bot's user (never as root in its folder).
cfg() { runuser -u "$service" -- git -C "$dir/config" "$@"; }

list_here() {
  local f
  for f in "$bundles"/*.tar.gz.age; do
    [ ! -f "$f" ] || printf '%s %s\n' "${f##*/}" "$(stat -c %s "$f")"
  done | { grep -E "$line_re" || true; } | sort -r
}

list_dest() {
  case $kind in
    pull) list_here ;;
    ssh) dest_ssh list | { grep -E "$line_re" || true; } | sort -r ;;
    rclone) { rcl lsf --files-only --format ps --separator ' ' "$remote" 2> /dev/null || true; } |
      { grep -E "$line_re" || true; } | sort -r ;;
  esac
}

# What's in the bundle besides the database and secrets: which bundle it is,
# when (signed with it, so a renamed or old bundle shows), and where from.
info() {
  echo "bundle: $1.tar.gz.age"
  echo "made-at: $2"
  echo "made: $(date -u -d "@$2" '+%Y-%m-%d %H:%M:%S UTC') on $(hostname)"
  echo "database copy: ${3##*/}"
  echo "release: $(basename "$(readlink "$dir/current" || echo none)")"
  if [ -d "$dir/config/.git" ]; then
    echo "config repo: $(cfg remote get-url origin 2> /dev/null || echo '?')"
    echo "config branch: $(cfg rev-parse --abbrev-ref HEAD 2> /dev/null || echo '?')"
    echo "config commit: $(cfg rev-parse HEAD 2> /dev/null || echo '?')"
    echo "config unpushed commits: $(cfg rev-list --count '@{u}..HEAD' 2> /dev/null || echo '?')"
    echo "config uncommitted changes: $(cfg status --porcelain 2> /dev/null | wc -l) files"
  fi
}

# The server's signing key, made the first time (backup-setup shows its public half).
ensure_sign_key() {
  [ -f "$sign_key" ] || ssh-keygen -q -t ed25519 -N '' -C "$service-backup-signing" -f "$sign_key"
}

# Bundle the newest database copy, the secrets and the note; encrypt; sign; print its name.
make_bundle() {
  local copies_all db now name work r rs=()
  copies_all=("$copies"/bot-[0-9]*.db)
  db=${copies_all[${#copies_all[@]} - 1]}
  [ -f "$db" ] || { echo "no database copy in $copies" >&2; exit 1; }
  now=$(date -u +%s)
  name=$service-$(date -u -d "@$now" +%Y%m%d-%H%M%S)
  mkdir -p "$bundles"
  # Plain copies a run that was killed left behind.
  rm -rf "$dir"/backup/.work.*
  work=$(mktemp -d "$dir/backup/.work.XXXXXX")
  # Expanded now: $work is local, gone by the time the trap runs.
  # shellcheck disable=SC2064
  trap "rm -rf '$work'" EXIT
  mkdir "$work/$name"
  # Read as the bot's user: the copies live in its folder.
  runuser -u "$service" -- cat "$db" > "$work/$name/bot.db"
  cp "$dir/.env" "$work/$name/env"
  info "$name" "$now" "$db" > "$work/$name/info.txt"
  if [ "$(cfg rev-list --count '@{u}..HEAD' 2> /dev/null || echo 0)" != 0 ]; then
    cfg format-patch --stdout '@{u}..HEAD' > "$work/$name/config-unpushed.patch"
  fi
  if [ -d "$recordings" ]; then
    # Which recordings there were ("BYTES PATH"), so a restore can say what it lacks.
    runuser -u "$service" -- find "$recordings" -type f -printf '%s %P\n' | sort -k 2 > "$work/$name/recordings.txt"
    if [ "${BACKUP_RECORDINGS:-}" = 1 ]; then
      runuser -u "$service" -- tar -C "$recordings" -cf - . > "$work/$name/recordings.tar"
    fi
  fi
  for r in $BACKUP_AGE_RECIPIENT; do rs+=(-r "$r"); done
  [ ${#rs[@]} -gt 0 ] || { echo "BACKUP_AGE_RECIPIENT isn't set in $conf" >&2; exit 1; }
  tar -C "$work" -czf - "$name" | age "${rs[@]}" -o "$bundles/$name.tar.gz.age.part"
  ensure_sign_key
  rm -f "$bundles/$name.tar.gz.age.part.sig"
  ssh-keygen -Y sign -q -f "$sign_key" -n "$sign_ns" "$bundles/$name.tar.gz.age.part" 2> /dev/null
  mv "$bundles/$name.tar.gz.age.part.sig" "$bundles/$name.tar.gz.age.sig"
  mv "$bundles/$name.tar.gz.age.part" "$bundles/$name.tar.gz.age"
  echo "$name.tar.gz.age"
}

# Keep the newest $1 of the bundles listed on stdin, here (and their signatures).
prune_here() {
  local f
  tail -n +$(($1 + 1)) | cut -d' ' -f1 | while read -r f; do
    if [[ $f =~ $name_re ]]; then rm -f -- "${bundles:?}/$f" "${bundles:?}/$f.sig"; fi
  done
}

# Keep the newest $keep at the destination. At an ssh one, its forced command
# does it (by its own rules); with rclone, failing is only a note: a key
# that can't delete is the safer one (old bundles expire by a lifecycle rule).
prune_dest() {
  local f
  case $kind in
    ssh) dest_ssh prune ;;
    rclone)
      list_dest | tail -n +$((keep + 1)) | cut -d' ' -f1 | while read -r f; do
        [[ $f =~ $name_re ]] || continue
        if ! rcl deletefile "$(rjoin "$remote" "$f")" 2> /dev/null; then
          echo "note: couldn't delete $f at $target (a key without delete rights? Then expire old ones there)" >&2
          break
        fi
        rcl deletefile "$(rjoin "$remote" "$f.sig")" 2> /dev/null || true
      done
      ;;
  esac
}

run() {
  local f sent
  f=$(make_bundle)
  case $kind in
    # The bundle first: a signature never names a bundle that isn't there.
    ssh) dest_ssh put "$f" < "$bundles/$f" && dest_ssh put "$f.sig" < "$bundles/$f.sig" ;;
    rclone) rcl copyto "$bundles/$f" "$(rjoin "$remote" "$f")" && rcl copyto "$bundles/$f.sig" "$(rjoin "$remote" "$f.sig")" ;;
  esac
  if [ "$kind" != pull ]; then
    sent=$(list_dest | awk -v f="$f" '$1 == f { print $2 }')
    [ "$sent" = "$(stat -c %s "$bundles/$f")" ] || { echo "$f didn't arrive whole at $target" >&2; exit 1; }
    prune_dest
    # The newest two stay here too, for a quick `deploy.sh backup`.
    list_here | prune_here 2
  else
    list_here | prune_here "$keep"
  fi
  echo "backed up $f to $target"
}

check() {
  case $kind in
    pull) echo "pull: bundles stay on the server for deploy.sh backup --latest" ;;
    ssh) dest_ssh check ;;
    rclone) rcl mkdir "$remote" && rcl lsf "$remote" > /dev/null && echo "$target: can be written to" ;;
  esac
}

case $cmd in
  run) run ;;
  list) if [ "${2:-}" = local ]; then list_here; else list_dest; fi ;;
  get)
    [[ ${2:-} =~ $name_re ]] || { echo "usage: backup-offsite get NAME (a bundle's name, or NAME.sig)" >&2; exit 1; }
    case $kind in
      pull) cat -- "$bundles/$2" ;;
      ssh) dest_ssh get "$2" ;;
      rclone) rcl cat "$(rjoin "$remote" "$2")" ;;
    esac
    ;;
  check) check ;;
  *) echo "usage: backup-offsite [run|list [local]|get NAME|check]" >&2; exit 1 ;;
esac
