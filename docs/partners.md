# Partner sync

Partners exchange signed messages; roles change only through `role_map`,
never powerful roles, and large swings wait for an admin's Allow/Refuse in
the bot log. They can also share content: resources, questions, feed
items, announcements and events ([below](#sharing-content)).

## Setting up

Set up in `federation.toml` (fields: `Federation` and `Peer` in
[`schema.rs`](../crates/config/src/schema.rs)). Each level is optional.

1. **Follow.** List announcement channels in `publish`. The bot publishes
   their posts, so partners can press *Follow*.
2. **Relayed posts.** A partner gives you a webhook URL. Put it in the bot's
   `.env` under the name in `share_webhook_env`, and list your channels in
   `share`.
3. **Signed sync** of project catalogues and roles. Both sides run this bot.
   Each side:
   1. makes its key in the secrets file: `chipsoc-bot --config <config
      clone> fed keygen --env .env.server` (with deploy.sh; else `--env` the
      bot's `.env`) writes it and prints the public key; swap `name` and
      public key with the other;
   2. makes a webhook on its `inbox` channel and sends the URL privately;
      puts theirs in the secrets file under the peer's `webhook_env`;
   3. adds a `[[peer]]` with `id`, `public_key`, `webhook_env`, `catalog`,
      `send_roles` and the agreed `role_map` (making any mapped role first);
   4. runs `fed key --env .env.server` (with `--config` too: shows the
      public key, checks each partner's key and that its webhooks are set)
      and `check`;
   5. uploads the secrets (`scripts/deploy.sh secrets`, which restarts the
      bot), pushes, and has an admin **Approve** the new partner in
      `log.bot`.

   Role changes arrive within about 30 seconds, with a full sync daily.
4. **Shared content** (below): with signed sync set up, add `send` and
   `accept` to the `[[peer]]`.

## Sharing content

What we send each partner is its `send` list; what we take from it, and
where it lands, is its `accept`. Both sides need this version; a partner on
an older one refuses what it doesn't know (its bot log says "didn't check
out"), so agree first. From this version on, a kind a bot doesn't know yet
is ignored quietly. Nothing is shared or taken unless listed.

```toml
[[peer]]
name = "SHaRC"
# … id, public_key, webhook_env as for signed sync
send = ["projects", "resources", "questions", "feeds", "events", "announcements"]
[peer.accept]
resources = "partner-resources"                      # a forum; waits for review
questions = "partner-questions"                      # a forum
feeds = { to = "partner-feeds", review = false }     # a forum or a channel; posted at once
announcements = "partner-news"                       # a text or announcement channel
events = true                                        # or { review = false }
# resources = { to = "partner-resources", edits = "apply" }   # their edits skip review
```

- **`send`.** `projects` is the catalogue (the same as `catalog = true`,
  which still works; with neither, no catalogue goes). `resources`, `feeds`
  and `questions` go as they're posted: a link an admin accepts (or posts)
  in the resources forum, an item a followed feed posts, a new post in the
  help forum. `announcements` and `events` go only when an admin shares one:
  `/server share announcement <message link>` or `/server share event <link
  or id>` (also `/event` when an admin makes one, the web API `POST
  /api/v1/share`, and `chipsoc-bot share`), picking partners from those whose
  `send` lists it.
- **`accept`.** Each kind names our channel key to land in (resources and
  questions need a forum); events become events here (outside Discord, at
  their place or link). What arrives waits in our review queue
  (`partner-resource`, `partner-question`, `partner-feed`, `partner-event`,
  `partner-announcement`) unless that kind says `review = false`; Accept
  posts it, Reject drops it (and later updates of it). Accept posts exactly
  the version shown: if the partner changes it while it waits, its card is
  drawn again with the new version (and a note), and an Accept pressed on
  the old card, or from a queue page loaded before the change, is refused
  ("changed since — review again").
- **What's posted** always says whom it's from, as we name them, and who
  shared it there: "From **SHaRC** · shared by Ada · `<link to the
  original>`". Their text is escaped (no markdown tricks, masked links,
  headings or mentions), invisible characters are taken out, links show only
  if they're plain https, and nobody is ever pinged.
- **Questions** are mirrored as a post linking to the original thread
  ("Answer there"). Replies don't sync either way (in this version): answers
  belong in their thread, and the mirror says so.
- **Edits and take-downs** follow, best effort: when the original is edited
  (a question's or announcement's text, an event's details) partners who had
  it get the new version. Only edits by whoever shared it here (the member
  who asked, the admin who shared an announcement) or by an admin are sent,
  since what partners post says it's from us. On the receiving side, an edit
  of something waiting for review updates the entry; an edit of something
  already posted waits for review too, as a "changed" card listing what
  changed, and Accept updates the post (Reject leaves it as it was). Set
  `edits = "apply"` in a kind's table to apply a partner's edits at once;
  `edits = "review"` reviews them even for a kind posted with `review =
  false` (which otherwise applies them). When it's deleted (bulk deletes
  too), archived (a resource) or cancelled (an event), partners close and
  lock the post, replace the message with a note, or cancel the event;
  nothing is deleted. If it, or an edit of it, was still waiting for
  review, it comes off the queue.
- **What can be shared.** An announcement must be in one of our channels
  that every member can see (open to @everyone, or to the join gate's access
  role), not where partners' posts land, and the admin sharing it must be
  able to see it.
- **Caps.** Each item is keyed by the sender's id for it, so a repeat only
  updates; a late envelope never undoes a newer one. Besides the 1000
  messages an hour, a partner's items are capped per kind: 30 of each kind
  an hour and 10 of each kind (or edits of it) waiting for review, so a
  flood of questions never shuts out their announcements and events; past
  that they're left out, and `log.bot` says so. What we share as it's posted
  (resources, feed items, questions) is capped at 20 of each kind an hour,
  and 3 an hour from any one member's posts; announcements and events (which
  an admin shares) aren't capped and go out first. Items without a title are
  left out. One that fails here is tried again, waiting longer each time (5
  minutes, then up to an hour), without counting toward the hourly cap
  again, and given up on after 12 tries. Long texts go in up to 8 messages
  and are cut at 3000 characters; parts that don't all arrive within 30
  minutes are dropped. The mass-change guard is for roles only.
- Adding or changing `send` or `accept` is a partner change, so it waits for
  the server owner's **Approve** in `log.bot` like keys and `role_map`.
  `check` refuses kinds it doesn't know, channels that don't exist or don't
  suit (a text channel for resources), and reviewed kinds without a
  `[review]` queue.

## Rules

Incoming messages must be signed by a known key, addressed to you, under 24
hours old (by when Discord says they were posted) and not replayed; one
whose seq is more than 5 minutes in the future is refused. Each member's
newest state wins, and a full sync counts from when the partner took it, so
it never undoes a later change. Roles change only through `role_map`, and
never touch roles a member already had. Someone who leaves and comes back
gets back, within moments, the roles partners still vouch for (through
`role_map` entries still there). `check` refuses powerful roles:
`everyone`, `admin_role`, roles members.toml or project leads manage, roles
with a moderating permission (their own or from an overwrite), and roles
that can see a channel hidden from @everyone. A partner giving roles to, or
taking them from, many members within a day is held, and `log.bot` asks an
admin; *Allow* applies just what that question describes and *Refuse* drops
it (in the audit log; the partner isn't told, and its next full sync is
checked afresh). Held updates wait (with a daily reminder) until someone
decides. Inbox messages are saved as they arrive, so none is lost while
the bot is down; edited ones are ignored. Each partner is capped at 1000
messages an hour, 20 held updates and a 256 KB catalogue; its project
links show only if they're plain https.

## Taking back

Removing a `[[peer]]` takes back every role its word gave and deletes its
shared projects. What it shared is archived here (posts closed and locked,
messages replaced by a note, events cancelled; nothing deleted), what waits
for review comes off the queue, and the bot forgets what it shared and what
we shared with it, so adding the partner back starts afresh. Removing a key from its `public_keys` takes back what was
given on messages that key signed, drops what it has held, and resets the
partner's seqs; so to rotate, add the new key, wait a day for their full
sync, then remove the old one. Removing an entry from `role_map`, or a
mapped role gaining moderator or admin permissions in Discord, takes back
the grants of that role. Whatever fails is tried again every few minutes;
all of it is logged to `log.bot`. To take everything back at once (a stolen
key): `scripts/deploy.sh revoke <peer> [key id]` runs `fed revoke` on the
server with the bot's settings and database (by hand, it needs the bot's
`DATABASE_URL`, and refuses to start a new database). Then remove the peer
or key from federation.toml, or their next sync gives the roles back.
