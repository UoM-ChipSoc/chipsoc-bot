#!/bin/sh
# At an ssh backup destination: the only thing the bot server's backup key
# may run there. `scripts/deploy.sh backup-setup` installs it as
# ~/.ssh/@SERVICE@-backup-dest and names it in ~/.ssh/authorized_keys:
#
#   restrict,command="sh .ssh/@SERVICE@-backup-dest DIR KEEP" ssh-ed25519 AAAA… @SERVICE@-backup
#
# So the server, or whoever takes it over, can add bundles to DIR but never
# replace or delete one, can list and read them, and can prune: of the
# bundles past the newest KEEP (by when they arrived here), those more than
# KEEP days old go. No shell, no other files, no forwarding. Plain sh, for
# a NAS too.
#
#   put NAME    stdin as DIR/NAME (a new bundle, or its .sig)
#   list        NAME BYTES of each bundle, newest first
#   get NAME    DIR/NAME to stdout
#   prune       as above
#   check       whether DIR can be written to; then keep=KEEP
set -eu
umask 077
LC_ALL=C
export LC_ALL
service=@SERVICE@
usage="usage: sh .ssh/$service-backup-dest DIR KEEP (in authorized_keys)"
dir=${1:?$usage}
keep=${2:?$usage}
die() {
  echo "$*" >&2
  exit 1
}
case $keep in '' | *[!0-9]* | 0*) die "KEEP should be a number" ;; esac
# The command the server asked for, as words (never globbed).
set -f
# shellcheck disable=SC2086
set -- ${SSH_ORIGINAL_COMMAND:-}
set +f
[ $# -le 2 ] || die "refused: only put, list, get, prune and check, with one name"
cmd=${1:-}
name=${2:-}

d8='[0-9][0-9][0-9][0-9][0-9][0-9][0-9][0-9]'
d6='[0-9][0-9][0-9][0-9][0-9][0-9]'
# Whether $1 is a bundle's name (or its signature's).
# shellcheck disable=SC2254 # the patterns are meant
bundle_name() {
  case $1 in
    "$service"-$d8-$d6.tar.gz.age | "$service"-$d8-$d6.tar.gz.age.sig) return 0 ;;
  esac
  return 1
}

mkdir -p -- "$dir"
cd -- "$dir"
case $cmd in
  put)
    bundle_name "$name" || die "refused: put $name"
    [ ! -e "$name" ] || die "$name is there already (bundles are never replaced)"
    tmp=.$name.part.$$
    trap 'rm -f -- "$tmp"' EXIT
    cat > "$tmp"
    # A hard link fails if the name was taken meanwhile; not every disk has them.
    if ! ln -- "$tmp" "$name" 2> /dev/null; then
      [ ! -e "$name" ] || die "$name is there already (bundles are never replaced)"
      mv -- "$tmp" "$name"
    fi
    ;;
  list)
    for f in "$service"-*.tar.gz.age; do
      if bundle_name "$f" && [ -f "$f" ]; then printf '%s %s\n' "$f" "$(wc -c < "$f" | tr -d ' ')"; fi
    done | sort -r
    ;;
  get)
    bundle_name "$name" && [ -f "$name" ] || die "there's no $name"
    cat -- "$name"
    ;;
  prune)
    n=0
    # shellcheck disable=SC2012 # names are checked one by one
    ls -1t | while IFS= read -r f; do
      case $f in *.sig) continue ;; esac
      bundle_name "$f" || continue
      n=$((n + 1))
      [ "$n" -gt "$keep" ] || continue
      [ -n "$(find "$f" -prune -mtime +"$((keep - 1))")" ] || continue
      rm -f -- "$f" "$f.sig"
    done
    # Uploads cut off, and signatures whose bundle is gone.
    for f in ."$service"-*.part.*; do
      if [ -f "$f" ] && [ -n "$(find "$f" -prune -mtime +0)" ]; then rm -f -- "$f"; fi
    done
    for f in "$service"-*.tar.gz.age.sig; do
      if bundle_name "$f" && [ ! -e "${f%.sig}" ]; then rm -f -- "$f"; fi
    done
    ;;
  check)
    : > .write-test
    rm -f .write-test
    echo "$dir at $(hostname 2> /dev/null || echo the destination) can be written to"
    echo "keep=$keep"
    ;;
  *) die "refused: only put, list, get, prune and check" ;;
esac
