#!/usr/bin/env bash
# Installed by scripts/deploy.sh as @DIR@/monitor (root only), which fills in
# the @…@ values. @SERVICE@-monitor.timer runs it every minute, once
# `deploy.sh monitor-setup` has written @DIR@/monitor.env.
#
# The bot writes the time to its heartbeat file (/var/lib/@SERVICE@/heartbeat)
# about once a minute while Discord's gateway talks to it. If that's older
# than MONITOR_STALE_MINUTES, or the bot isn't running, this says so once (and
# again every MONITOR_REPEAT_HOURS), and once more when it's back: to the
# Discord webhook MONITOR_WEBHOOK and/or by mail to MONITOR_EMAIL. While the
# bot is up it fetches MONITOR_PING_URL (an outside dead man's switch, such as
# healthchecks.io), which then warns you if the pings stop: the whole server
# down included. The two URLs are secrets, so they're read from @DIR@/.env.
#
#   monitor [check]   check once (what the timer runs)
#   monitor test      send a test message to each of them
set -euo pipefail
shopt -s inherit_errexit
umask 077
export LC_ALL=C
dir=@DIR@
service=@SERVICE@
[[ $service =~ ^[a-z][a-z0-9-]*$ ]] || { echo "SERVICE $service: lower-case letters, digits and - only" >&2; exit 1; }
beat=/var/lib/$service/heartbeat
# What this remembers between runs (since when it's down, when it last said so).
state=/var/lib/$service-monitor
conf=$dir/monitor.env
MONITOR_STALE_MINUTES=5 MONITOR_REPEAT_HOURS=6 MONITOR_EMAIL='' MONITOR_ROLE=''
# shellcheck disable=SC1090 # written by deploy.sh monitor-setup
if [ -f "$conf" ]; then . "$conf"; else
  echo "monitoring isn't set up ($conf): scripts/deploy.sh monitor-setup" >&2
  [ "${1:-check}" = check ]
  exit
fi
[[ $MONITOR_STALE_MINUTES =~ ^[1-9][0-9]*$ ]] || { echo "MONITOR_STALE_MINUTES should be a number" >&2; exit 1; }
[[ $MONITOR_REPEAT_HOURS =~ ^[0-9]+$ ]] || { echo "MONITOR_REPEAT_HOURS should be a number" >&2; exit 1; }
[[ -z $MONITOR_ROLE || $MONITOR_ROLE =~ ^[0-9]{17,20}$ ]] || { echo "MONITOR_ROLE should be a role id" >&2; exit 1; }

# Setting $1 from the server's .env (root only), without its quotes; never run as shell.
secret() {
  local v
  v=$(sed -n "s/^$1=//p" "$dir/.env" 2> /dev/null | tail -n 1)
  v=${v#[\"\']} v=${v%[\"\']}
  printf '%s' "$v"
}
webhook=$(secret MONITOR_WEBHOOK)
ping_url=$(secret MONITOR_PING_URL)
url_re='^https://[A-Za-z0-9.-]+(:[0-9]+)?(/[A-Za-z0-9._~/?&=%+-]*)?$'
if [ -n "$webhook" ] && ! [[ $webhook =~ $url_re ]]; then echo "MONITOR_WEBHOOK in $dir/.env isn't an https:// URL" >&2; webhook=''; fi
if [ -n "$ping_url" ] && ! [[ $ping_url =~ $url_re ]]; then echo "MONITOR_PING_URL in $dir/.env isn't an https:// URL" >&2; ping_url=''; fi

# The URL goes to curl through a pipe, so it isn't on a command line others
# can see; stdin is left for the data.
fetch() {
  local url=$1
  shift
  curl -K <(printf 'url = "%s"\n' "$url") -fsS --proto =https --max-time 20 --retry 2 -o /dev/null "$@"
}

# Say $1 everywhere set up; true if it reached at least one.
say() {
  local text=$1 sent=1 json mention='' allowed='[]'
  if [ -n "$webhook" ]; then
    if [ -n "$MONITOR_ROLE" ]; then mention="<@&$MONITOR_ROLE> " allowed="[\"$MONITOR_ROLE\"]"; fi
    json=$(printf '%s' "$mention$text" | sed 's/\\/\\\\/g; s/"/\\"/g' | awk '{ printf "%s%s", (NR > 1 ? "\\n" : ""), $0 }')
    if printf '{"content": "%s", "allowed_mentions": {"parse": [], "roles": %s}}' "$json" "$allowed" |
      fetch "$webhook" -H 'Content-Type: application/json' --data-binary @-; then
      sent=0
    else
      echo "couldn't post to MONITOR_WEBHOOK" >&2
    fi
  fi
  if [ -n "$MONITOR_EMAIL" ]; then
    if command -v sendmail > /dev/null &&
      printf 'To: %s\nSubject: %s\n\n%s\n' "$MONITOR_EMAIL" "$(head -n 1 <<< "$text" | tr -d '*')" "$text" |
      sendmail -i -- "$MONITOR_EMAIL"; then
      sent=0
    else
      echo "couldn't mail $MONITOR_EMAIL (is sendmail set up on this server?)" >&2
    fi
  fi
  return "$sent"
}

host=$(uname -n | tr -cd 'A-Za-z0-9.-')
case ${1:-check} in
  test)
    [ -n "$webhook$MONITOR_EMAIL$ping_url" ] || { echo "nothing to send to: MONITOR_WEBHOOK, MONITOR_PING_URL or MONITOR_EMAIL" >&2; exit 1; }
    ok=1
    if [ -n "$webhook$MONITOR_EMAIL" ]; then
      say "Test from $service on $host: if the bot goes down, a message like this one says so here." && ok=0
    fi
    if [ -n "$ping_url" ]; then
      if fetch "$ping_url"; then echo "pinged MONITOR_PING_URL"; ok=0; else echo "couldn't fetch MONITOR_PING_URL" >&2; fi
    fi
    exit "$ok"
    ;;
  check) ;;
  *) echo "usage: monitor [check|test]" >&2; exit 1 ;;
esac

mkdir -p "$state"
now=$(date +%s)
# Down since when, as far as can be told (the last heartbeat, or this run).
why='' from=$now
if [ -f "$beat" ]; then from=$(stat -c %Y "$beat"); fi
[ "$from" -le "$now" ] || from=$now
if ! systemctl is-active -q "$service"; then
  why="the bot isn't running ($(systemctl is-active "$service" 2> /dev/null || true))"
elif [ ! -f "$beat" ]; then
  # A release from before heartbeats: only a stopped bot can be told.
  echo "no heartbeat file yet ($beat); only checking that the bot runs" >&2
else
  if [ $((now - from)) -gt $((MONITOR_STALE_MINUTES * 60)) ]; then
    why="it runs, but hasn't heard from Discord for $(((now - from) / 60)) minutes"
  fi
fi

down=$state/down
if [ -n "$why" ]; then
  since='' last=0
  if [ -f "$down" ]; then read -r since last < "$down" || true; fi
  [[ $since =~ ^[0-9]+$ && $last =~ ^[0-9]+$ ]] || since=$from last=0
  [ "$from" -ge "$since" ] || since=$from
  echo "down: $why" >&2
  # Said once it has been down a while (not for a restart), then every MONITOR_REPEAT_HOURS.
  if { [ "$last" = 0 ] && [ $((now - since)) -ge $((MONITOR_STALE_MINUTES * 60)) ]; } ||
    { [ "$last" != 0 ] && [ "$MONITOR_REPEAT_HOURS" != 0 ] && [ $((now - last)) -ge $((MONITOR_REPEAT_HOURS * 3600)) ]; }; then
    what="is down"
    [ "$last" = 0 ] || what="is still down, since $(date -u -d "@$since" '+%a %H:%M UTC')"
    if say "**The bot ($service on $host) $what**: $why.
With the join gate on, nobody new gets in until it's back: give newcomers the join flow's access role by hand. What to do: docs/deploy.md, *When the bot is down*."; then
      last=$now
    fi
  fi
  echo "$since $last" > "$down"
  exit 0
fi

if [ -f "$down" ]; then
  read -r since last < "$down" || true
  [[ ${since:-} =~ ^[0-9]+$ ]] || since=$now
  # Only if it said it was down (else nobody needs to hear it's back).
  if [ "${last:-0}" = 0 ] || say "**The bot ($service on $host) is back** after $(((now - since) / 60)) minutes."; then
    rm -f "$down"
  fi
fi
if [ -n "$ping_url" ]; then fetch "$ping_url" || echo "couldn't fetch MONITOR_PING_URL" >&2; fi
