//! Signed federation envelopes, exchanged between partner servers' bots.
//!
//! An envelope is a Discord message posted (by webhook) into the recipient's
//! private inbox channel:
//!
//! ```text
//! guildfed/2
//! {"body":{…},"from":"a","kid":"Ab12+d34","kind":"roles","nonce":"…","seq":17,"sig":"…","to":"b","ts":1700000000}
//! ```
//!
//! `sig` is ed25519 over `guildfed/2`, a newline, and the canonical JSON
//! (sorted keys) of every other field, so the version is signed too. `kid`
//! names the signing key, so a partner can list several while rotating.
//! `to` stops one partner replaying our envelope to another; `seq` orders
//! envelopes from one sender; `ts` + `nonce` reject stale and replayed ones.
//!
//! Version 1 envelopes (`guildfed/1`: no kid, magic not signed) are still
//! accepted, until every partner sends version 2.
//!
//! A full role sync is a snapshot: `as_of` is the sender's seq when it read
//! its members, so a receiver can tell it from deltas sent after it. Catalogues
//! and role syncs too big for one message are sent in parts.
//!
//! Shared content (`share`: a resource, question, feed item, event or
//! announcement; `share.remove` to take one back) is keyed by the sender's
//! `origin` id, so a later envelope for the same origin updates it. A long
//! text goes in parts, each carrying the rest of the item. A kind this
//! version doesn't know (from a newer partner) still verifies, as
//! [`Body::Unknown`], for the receiver to ignore; unknown fields are ignored.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub use ed25519_dalek::{SigningKey as SecretKey, VerifyingKey as PublicKey};

/// What we send.
pub const MAGIC: &str = "guildfed/2";
/// Still accepted from partners on older builds.
pub const MAGIC_V1: &str = "guildfed/1";
/// Accept envelopes up to this many seconds old (or ahead).
pub const MAX_SKEW: i64 = 24 * 3600;
/// Seqs are milliseconds since the epoch: refuse one more than this many
/// seconds ahead of when the envelope arrived, so a stolen key can't claim a
/// seq no real update ever beats.
pub const MAX_SEQ_AHEAD: i64 = 5 * 60;

/// One member's attested roles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Member {
    pub id: String,
    pub roles: Vec<String>,
}

/// One project in a shared catalogue.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Project {
    pub slug: String,
    pub name: String,
    #[serde(default)]
    pub blurb: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub github: Option<String>,
    #[serde(default)]
    pub open: bool,
}

/// What an envelope carries; `kind` and `body` on the wire.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body")]
pub enum Body {
    /// Part `part` (from 0) of `parts` of the project catalogue. Older senders
    /// send it whole, without `batch` and the part numbers.
    #[serde(rename = "catalog")]
    Catalog {
        projects: Vec<Project>,
        #[serde(default)]
        batch: String,
        #[serde(default)]
        part: u64,
        #[serde(default = "one")]
        parts: u64,
    },
    /// Part `part` (from 0) of `parts` of a full role sync, a snapshot taken
    /// at the sender's seq `as_of` (0 from older senders).
    #[serde(rename = "roles")]
    Roles {
        batch: String,
        part: u64,
        parts: u64,
        total: u64,
        members: Vec<Member>,
        #[serde(default)]
        as_of: u64,
    },
    #[serde(rename = "roles.delta")]
    RolesDelta { members: Vec<Member> },
    /// Part `part` (from 0) of a shared item; parts differ only in their text.
    #[serde(rename = "share")]
    Share(Share),
    /// The item shared from `origin` was taken down.
    #[serde(rename = "share.remove")]
    Unshare { what: ShareKind, origin: String },
    /// A kind this version doesn't know (its name): signed, but ignored.
    #[serde(skip)]
    Unknown(String),
}

/// The kinds this version reads.
const KINDS: &[&str] = &["catalog", "roles", "roles.delta", "share", "share.remove"];

/// What a shared item is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShareKind {
    /// A link from the resources forum.
    Resource,
    /// A post in the help forum.
    Question,
    /// An item a followed feed posted.
    Feed,
    /// A scheduled event.
    Event,
    /// A post in an announcement channel.
    Announcement,
    /// One a newer version knows.
    #[serde(other)]
    Other,
}

impl ShareKind {
    pub const ALL: [Self; 5] = [Self::Resource, Self::Question, Self::Feed, Self::Event, Self::Announcement];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resource => "resource",
            Self::Question => "question",
            Self::Feed => "feed",
            Self::Event => "event",
            Self::Announcement => "announcement",
            Self::Other => "other",
        }
    }

    /// As federation.toml's `send` and `accept` name it: `resources`, `events`…
    pub fn plural(self) -> &'static str {
        match self {
            Self::Resource => "resources",
            Self::Question => "questions",
            Self::Feed => "feeds",
            Self::Event => "events",
            Self::Announcement => "announcements",
            Self::Other => "other",
        }
    }

    /// From [`ShareKind::as_str`] or [`ShareKind::plural`].
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s || k.plural() == s)
    }
}

/// One shared item, or a part of it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Share {
    pub what: ShareKind,
    /// The sender's id for it (a post, message or event), the same in every update.
    pub origin: String,
    pub title: String,
    /// The text, or this part's piece of it.
    #[serde(default)]
    pub text: String,
    /// The link a resource or feed item shares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// The original, on the sender's server.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
    /// Who shared, asked or posted it, as their server shows them.
    #[serde(default)]
    pub by: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Events: when (Unix times) and where.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    /// The parts of one send share a batch.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub batch: String,
    #[serde(default)]
    pub part: u64,
    #[serde(default = "one")]
    pub parts: u64,
}

impl Share {
    /// An item with nothing but what it is, where it's from and its title.
    pub fn new(what: ShareKind, origin: &str, title: &str) -> Self {
        Self {
            what,
            origin: origin.to_owned(),
            title: title.to_owned(),
            text: String::new(),
            url: None,
            link: None,
            by: String::new(),
            tags: Vec::new(),
            start: None,
            end: None,
            location: String::new(),
            batch: String::new(),
            part: 0,
            parts: 1,
        }
    }
}

/// Most parts one shared item comes in.
pub const MAX_SHARE_PARTS: u64 = 8;
/// Longest origin id.
pub const MAX_ORIGIN: usize = 100;

fn one() -> u64 {
    1
}

/// More parts than any real sync needs.
pub const MAX_PARTS: u64 = 10_000;

impl Body {
    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Roles { part, parts, .. } | Self::Catalog { part, parts, .. }
                if part >= parts || *parts > MAX_PARTS =>
            {
                Err(format!("part {part} of {parts}"))
            }
            Self::Share(s) if s.part >= s.parts || s.parts > MAX_SHARE_PARTS => {
                Err(format!("part {} of {}", s.part, s.parts))
            }
            Self::Share(Share { origin, .. }) | Self::Unshare { origin, .. }
                if origin.is_empty() || origin.len() > MAX_ORIGIN =>
            {
                Err("bad origin".into())
            }
            _ => Ok(()),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Envelope {
    pub from: String,
    pub to: String,
    /// Strictly increasing per sender.
    pub seq: u64,
    pub ts: i64,
    pub nonce: String,
    pub body: Body,
}

impl Envelope {
    /// Fails only if the system has no randomness for the nonce.
    pub fn new(from: &str, to: &str, seq: u64, ts: i64, body: Body) -> Result<Self, String> {
        Ok(Self { from: from.into(), to: to.into(), seq, ts, nonce: nonce()?, body })
    }
}

/// Hands out increasing sequence numbers: milliseconds since the epoch, bumped
/// if two are requested in the same millisecond.
#[derive(Debug, Default)]
pub struct Seq(AtomicU64);

impl Seq {
    pub fn next(&self, now_ms: u64) -> u64 {
        let mut last = self.0.load(Ordering::Relaxed);
        loop {
            let next = now_ms.max(last + 1);
            match self.0.compare_exchange_weak(last, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return next,
                Err(seen) => last = seen,
            }
        }
    }
}

/// A fresh keypair as base64: (secret, public).
pub fn generate() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| e.to_string())?;
    let sk = SigningKey::from_bytes(&seed);
    Ok((B64.encode(sk.to_bytes()), public_of(&sk)))
}

pub fn signing_key(b64: &str) -> Result<SigningKey, String> {
    let bytes: [u8; 32] =
        B64.decode(b64.trim()).map_err(|e| e.to_string())?.try_into().map_err(|_| "signing key must be 32 bytes")?;
    Ok(SigningKey::from_bytes(&bytes))
}

pub fn verifying_key(b64: &str) -> Result<VerifyingKey, String> {
    let bytes: [u8; 32] =
        B64.decode(b64.trim()).map_err(|e| e.to_string())?.try_into().map_err(|_| "public key must be 32 bytes")?;
    VerifyingKey::from_bytes(&bytes).map_err(|e| e.to_string())
}

pub fn public_of(sk: &SigningKey) -> String {
    B64.encode(sk.verifying_key().to_bytes())
}

/// A key's id: the first 8 characters of its SHA-256, in base64.
pub fn kid(k: &VerifyingKey) -> String {
    B64.encode(Sha256::digest(k.to_bytes())).chars().take(8).collect()
}

/// True if `content` looks like an envelope of any version.
pub fn is_envelope(content: &str) -> bool {
    content.starts_with("guildfed/")
}

fn nonce() -> Result<String, String> {
    let mut n = [0u8; 12];
    getrandom::fill(&mut n).map_err(|e| format!("no randomness for a nonce: {e}"))?;
    Ok(B64.encode(n))
}

/// Sorted keys, no whitespace.
fn canonical(v: &Value) -> String {
    fn sort(v: &Value) -> Value {
        match v {
            Value::Object(m) => {
                let mut keys: Vec<&String> = m.keys().collect();
                keys.sort();
                Value::Object(keys.into_iter().map(|k| (k.clone(), sort(&m[k]))).collect::<Map<_, _>>())
            }
            Value::Array(a) => Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
}

/// The bytes a version 2 signature covers: the magic and every field but `sig`.
fn signed_v2(fields: &Value) -> String {
    format!("{MAGIC}\n{}", canonical(fields))
}

/// The bytes a version 1 signature covers: only the fields version 1 knew.
fn signed_v1(v: &Value) -> String {
    let pick = |k: &str| v.get(k).cloned().unwrap_or(Value::Null);
    let old = json!({ "from": pick("from"), "to": pick("to"), "seq": pick("seq"), "ts": pick("ts"),
                      "nonce": pick("nonce"), "kind": pick("kind"), "body": pick("body") });
    canonical(&old)
}

/// Sign an envelope into message content.
pub fn seal(sk: &SigningKey, e: &Envelope) -> String {
    let mut v = serde_json::to_value(&e.body).unwrap_or_default();
    v["from"] = e.from.clone().into();
    v["to"] = e.to.clone().into();
    v["seq"] = e.seq.into();
    v["ts"] = e.ts.into();
    v["nonce"] = e.nonce.clone().into();
    seal_fields(sk, v)
}

/// Sign raw envelope fields (`from`, `to`, `seq`, `ts`, `nonce`, `kind`,
/// `body`) into message content, whatever the kind: how a newer version's
/// envelopes look to this one.
pub fn seal_fields(sk: &SigningKey, mut v: Value) -> String {
    v["kid"] = kid(&sk.verifying_key()).into();
    let sig = sk.sign(signed_v2(&v).as_bytes());
    v["sig"] = Value::String(B64.encode(sig.to_bytes()));
    format!("{MAGIC}\n{}", canonical(&v))
}

/// The keys to try: the one `want` names, or all of them if there's no kid.
pub fn candidates<'a>(keys: &'a [VerifyingKey], want: Option<&str>) -> Vec<&'a VerifyingKey> {
    keys.iter().filter(|k| want.is_none_or(|w| kid(k) == w)).collect()
}

/// A verified envelope and the id of the key that signed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub envelope: Envelope,
    pub kid: String,
}

/// Why an envelope was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyError {
    NotEnvelope,
    /// Not shaped like an envelope: bad JSON, a field missing or the wrong type.
    Malformed(String),
    UnknownSender(String),
    UnknownKey {
        from: String,
        kid: String,
    },
    BadSignature,
    /// Signed, but its body doesn't make sense.
    BadBody(String),
    /// Addressed to someone else (this one's id).
    NotForUs(String),
    /// Older than [`MAX_SKEW`], or as far in the future.
    OutOfTime,
    SeqAhead,
    SnapshotAhead,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotEnvelope => f.write_str("not a federation envelope"),
            Self::Malformed(why) => f.write_str(why),
            Self::UnknownSender(from) => write!(f, "unknown sender `{from}`"),
            Self::UnknownKey { from, kid } => write!(f, "unknown key id `{kid}` for `{from}`"),
            Self::BadSignature => f.write_str("signature does not verify"),
            Self::BadBody(why) => write!(f, "bad body: {why}"),
            Self::NotForUs(to) => write!(f, "addressed to `{to}`, not us"),
            Self::OutOfTime => f.write_str("envelope too old or from the future"),
            Self::SeqAhead => f.write_str("seq from the future"),
            Self::SnapshotAhead => f.write_str("full sync taken in the future"),
        }
    }
}

impl std::error::Error for VerifyError {}

/// Parse and verify message content addressed to `me`. `peers` maps sender id → its keys.
pub fn open(
    content: &str,
    peers: &HashMap<String, Vec<VerifyingKey>>,
    me: &str,
    now: i64,
) -> Result<Envelope, VerifyError> {
    verify(content, peers, me, now).map(|o| o.envelope)
}

/// [`open`], also saying which key signed it.
pub fn verify(
    content: &str,
    peers: &HashMap<String, Vec<VerifyingKey>>,
    me: &str,
    now: i64,
) -> Result<Opened, VerifyError> {
    use VerifyError::Malformed;
    let (v2, json) = match (content.strip_prefix(MAGIC), content.strip_prefix(MAGIC_V1)) {
        (Some(rest), _) => (true, rest),
        (_, Some(rest)) => (false, rest),
        _ => return Err(VerifyError::NotEnvelope),
    };
    let missing = |k: &str| Malformed(format!("missing {k}"));
    let mut v: Value = serde_json::from_str(json.trim()).map_err(|e| Malformed(format!("bad json: {e}")))?;
    let sig = v.as_object_mut().and_then(|o| o.remove("sig")).ok_or_else(|| missing("sig"))?;
    let sig: [u8; 64] = B64
        .decode(sig.as_str().ok_or_else(|| missing("sig"))?)
        .map_err(|e| Malformed(e.to_string()))?
        .try_into()
        .map_err(|_| Malformed("bad signature length".into()))?;
    let str_field = |k: &str| v[k].as_str().map(str::to_owned).ok_or_else(|| missing(k));
    let from = str_field("from")?;
    let keys = peers.get(&from).ok_or_else(|| VerifyError::UnknownSender(from.clone()))?;
    let want = v.get("kid").and_then(Value::as_str);
    let keys = candidates(keys, want);
    if keys.is_empty() {
        return Err(VerifyError::UnknownKey { from, kid: want.unwrap_or_default().to_owned() });
    }
    let signed = if v2 { signed_v2(&v) } else { signed_v1(&v) };
    let sig = Signature::from_bytes(&sig);
    let Some(signer) = keys.into_iter().find(|k| k.verify(signed.as_bytes(), &sig).is_ok()) else {
        return Err(VerifyError::BadSignature);
    };
    let kind = v["kind"].as_str().ok_or_else(|| missing("kind"))?;
    let body: Body = if KINDS.contains(&kind) {
        serde_json::from_value(json!({ "kind": kind, "body": v["body"] }))
            .map_err(|e| VerifyError::BadBody(e.to_string()))?
    } else {
        Body::Unknown(kind.chars().take(40).collect())
    };
    body.validate().map_err(VerifyError::BadBody)?;
    let e = Envelope {
        from,
        to: str_field("to")?,
        seq: v["seq"].as_u64().ok_or_else(|| missing("seq"))?,
        ts: v["ts"].as_i64().ok_or_else(|| missing("ts"))?,
        nonce: str_field("nonce")?,
        body,
    };
    if e.to != me {
        return Err(VerifyError::NotForUs(e.to));
    }
    if (now - e.ts).abs() > MAX_SKEW {
        return Err(VerifyError::OutOfTime);
    }
    let latest = (now.saturating_add(MAX_SEQ_AHEAD).max(0) as u64).saturating_mul(1000);
    if e.seq > latest {
        return Err(VerifyError::SeqAhead);
    }
    if let Body::Roles { as_of, .. } = e.body
        && as_of > latest
    {
        return Err(VerifyError::SnapshotAhead);
    }
    Ok(Opened { envelope: e, kid: kid(signer) })
}

#[cfg(test)]
mod tests {
    use super::*;

    type Peers = HashMap<String, Vec<VerifyingKey>>;

    fn setup() -> (SigningKey, Peers) {
        let (secret, public) = generate().unwrap();
        let peers = HashMap::from([("a".to_owned(), vec![verifying_key(&public).unwrap()])]);
        (signing_key(&secret).unwrap(), peers)
    }

    fn delta() -> Body {
        Body::RolesDelta { members: vec![Member { id: "1".into(), roles: vec!["committee".into()] }] }
    }

    fn env(to: &str, ts: i64) -> Envelope {
        Envelope::new("a", to, 1, ts, delta()).unwrap()
    }

    fn fields(kind: &str, body: Value) -> Value {
        json!({ "from": "a", "to": "b", "seq": 1, "ts": 1000, "nonce": "n", "kind": kind, "body": body })
    }

    /// What a version 1 sender produced.
    fn seal_v1(sk: &SigningKey, e: &Envelope) -> String {
        let body = serde_json::to_value(&e.body).unwrap();
        let mut v = fields(body["kind"].as_str().unwrap(), body["body"].clone());
        v["nonce"] = json!(e.nonce);
        let sig = sk.sign(canonical(&v).as_bytes());
        v["sig"] = json!(B64.encode(sig.to_bytes()));
        format!("{MAGIC_V1}\n{}", canonical(&v))
    }

    #[test]
    fn round_trips_and_rejects_tampering() {
        let (sk, peers) = setup();
        let e = env("b", 1000);
        let msg = seal(&sk, &e);
        assert!(msg.starts_with("guildfed/2\n") && msg.contains("\"kid\""));
        assert_eq!(open(&msg, &peers, "b", 1000).unwrap(), e);
        let tampered = msg.replace("committee", "chair");
        assert_eq!(open(&tampered, &peers, "b", 1000).unwrap_err(), VerifyError::BadSignature);
    }

    #[test]
    fn the_version_is_signed() {
        let (sk, peers) = setup();
        let downgraded = seal(&sk, &env("b", 1000)).replacen(MAGIC, MAGIC_V1, 1);
        assert_eq!(open(&downgraded, &peers, "b", 1000).unwrap_err(), VerifyError::BadSignature);
    }

    #[test]
    fn version_1_is_still_accepted() {
        let (sk, peers) = setup();
        let e = env("b", 1000);
        assert_eq!(open(&seal_v1(&sk, &e), &peers, "b", 1000).unwrap(), e);
    }

    #[test]
    fn rejects_wrong_recipient_stale_unknown_and_wrong_key() {
        let (sk, peers) = setup();
        let msg = seal(&sk, &env("b", 1000));
        assert!(open(&msg, &peers, "c", 1000).unwrap_err().to_string().contains("not us"), "replay to another peer");
        assert!(open(&msg, &peers, "b", 1000 + MAX_SKEW + 1).unwrap_err().to_string().contains("too old"));
        let (_, other) = generate().unwrap();
        let wrong = HashMap::from([("a".to_owned(), vec![verifying_key(&other).unwrap()])]);
        assert!(open(&msg, &wrong, "b", 1000).unwrap_err().to_string().contains("unknown key id"));
        assert!(open(&msg, &HashMap::new(), "b", 1000).unwrap_err().to_string().contains("unknown sender"));
        assert!(open("hello", &peers, "b", 0).is_err());
    }

    #[test]
    fn rotation_picks_the_key_by_id() {
        let (sk, mut peers) = setup();
        let (_, old) = generate().unwrap();
        peers.get_mut("a").unwrap().insert(0, verifying_key(&old).unwrap());
        let e = env("b", 1000);
        assert_eq!(open(&seal(&sk, &e), &peers, "b", 1000).unwrap(), e);

        let keys = &peers["a"];
        assert_eq!(candidates(keys, Some(&kid(&sk.verifying_key()))), vec![&keys[1]]);
        assert_eq!(candidates(keys, None).len(), 2, "no kid: try every key");
        assert!(candidates(keys, Some("nope")).is_empty());
        // Version 1 envelopes carry no kid, so every key is tried.
        assert_eq!(open(&seal_v1(&sk, &e), &peers, "b", 1000).unwrap(), e);
    }

    #[test]
    fn kid_is_short_and_stable() {
        let (sk, _) = setup();
        let k = kid(&sk.verifying_key());
        assert_eq!(k.len(), 8);
        assert_eq!(k, kid(&sk.verifying_key()));
        let (_, other) = generate().unwrap();
        assert_ne!(k, kid(&verifying_key(&other).unwrap()));
    }

    #[test]
    fn bodies_are_typed_and_checked() {
        let (sk, peers) = setup();
        let bad_part = json!({ "batch": "x", "part": 2, "parts": 2, "total": 0, "members": [] });
        let msg = seal_fields(&sk, fields("roles", bad_part));
        assert!(open(&msg, &peers, "b", 1000).unwrap_err().to_string().contains("part 2 of 2"));
        let msg = seal_fields(&sk, fields("share", json!({ "what": "resource" })));
        assert!(open(&msg, &peers, "b", 1000).unwrap_err().to_string().starts_with("bad body"));
        let msg = seal_fields(&sk, fields("roles.delta", json!({ "members": [{ "id": 1 }] })));
        assert!(open(&msg, &peers, "b", 1000).unwrap_err().to_string().starts_with("bad body"));
    }

    #[test]
    fn a_kind_from_a_newer_version_verifies_as_unknown() {
        let (sk, peers) = setup();
        let msg = seal_fields(&sk, fields("poll", json!({ "question": "?" })));
        assert_eq!(open(&msg, &peers, "b", 1000).unwrap().body, Body::Unknown("poll".into()));
        let tampered = msg.replace("?", "!");
        assert_eq!(open(&tampered, &peers, "b", 1000).unwrap_err(), VerifyError::BadSignature, "still signed");
        // A share of a kind this version doesn't know, with fields it doesn't know.
        let newer = json!({ "what": "poll", "origin": "9", "title": "T", "votes": 3 });
        let Body::Share(s) = open(&seal_fields(&sk, fields("share", newer)), &peers, "b", 1000).unwrap().body else {
            panic!()
        };
        assert_eq!((s.what, s.parts), (ShareKind::Other, 1));
    }

    #[test]
    fn shares_round_trip_and_are_checked() {
        let (sk, peers) = setup();
        let mut item = Share::new(ShareKind::Resource, "123", "A guide");
        item.url = Some("https://example.com".into());
        item.by = "Ada".into();
        let e = Envelope::new("a", "b", 1, 1000, Body::Share(item.clone())).unwrap();
        assert_eq!(open(&seal(&sk, &e), &peers, "b", 1000).unwrap(), e);
        let gone = Envelope::new("a", "b", 2, 1000, Body::Unshare { what: ShareKind::Event, origin: "7".into() });
        assert_eq!(open(&seal(&sk, gone.as_ref().unwrap()), &peers, "b", 1000).unwrap(), gone.unwrap());
        for bad in [
            Share { part: 2, parts: 2, ..item.clone() },
            Share { parts: MAX_SHARE_PARTS + 1, ..item.clone() },
            Share { origin: String::new(), ..item.clone() },
            Share { origin: "x".repeat(MAX_ORIGIN + 1), ..item.clone() },
        ] {
            let e = Envelope::new("a", "b", 1, 1000, Body::Share(bad)).unwrap();
            assert!(open(&seal(&sk, &e), &peers, "b", 1000).unwrap_err().to_string().starts_with("bad body"));
        }
        assert_eq!(ShareKind::parse("announcements"), Some(ShareKind::Announcement));
        assert_eq!(ShareKind::parse("event"), Some(ShareKind::Event));
        assert_eq!(ShareKind::parse("projects"), None);
    }

    #[test]
    fn verify_names_the_signing_key() {
        let (sk, mut peers) = setup();
        let (_, old) = generate().unwrap();
        peers.get_mut("a").unwrap().insert(0, verifying_key(&old).unwrap());
        let opened = verify(&seal(&sk, &env("b", 1000)), &peers, "b", 1000).unwrap();
        assert_eq!(opened.kid, kid(&sk.verifying_key()));
    }

    #[test]
    fn older_senders_bodies_get_defaults() {
        let (sk, peers) = setup();
        // A catalogue sent whole, and a role sync without `as_of`.
        let msg = seal_fields(&sk, fields("catalog", json!({ "projects": [] })));
        let body = open(&msg, &peers, "b", 1000).unwrap().body;
        assert_eq!(body, Body::Catalog { projects: vec![], batch: String::new(), part: 0, parts: 1 });
        let roles = json!({ "batch": "x", "part": 0, "parts": 1, "total": 0, "members": [] });
        let body = open(&seal_fields(&sk, fields("roles", roles)), &peers, "b", 1000).unwrap().body;
        assert!(matches!(body, Body::Roles { as_of: 0, .. }));
        let bad = json!({ "projects": [], "batch": "x", "part": 3, "parts": 3 });
        assert!(
            open(&seal_fields(&sk, fields("catalog", bad)), &peers, "b", 1000)
                .unwrap_err()
                .to_string()
                .contains("part 3")
        );
        let huge = json!({ "batch": "x", "part": 0, "parts": MAX_PARTS + 1, "total": 0, "members": [] });
        assert!(open(&seal_fields(&sk, fields("roles", huge)), &peers, "b", 1000).is_err());
    }

    #[test]
    fn seqs_from_the_future_are_refused() {
        // Red-team PoC: seq = i64::MAX, or a snapshot `as_of` far ahead, froze members' state for good.
        let (sk, peers) = setup();
        let now = 1_700_000_000;
        let at = |seq: u64, body: Body| seal(&sk, &Envelope::new("a", "b", seq, now, body).unwrap());
        let ok = (now as u64 + 60) * 1000;
        assert!(open(&at(ok, delta()), &peers, "b", now).is_ok(), "a minute of clock skew is fine");
        for seq in [i64::MAX as u64, u64::MAX, (now as u64 + MAX_SEQ_AHEAD as u64 + 1) * 1000] {
            assert_eq!(open(&at(seq, delta()), &peers, "b", now).unwrap_err(), VerifyError::SeqAhead);
        }
        let snapshot = |as_of| Body::Roles { batch: "x".into(), part: 0, parts: 1, total: 0, members: vec![], as_of };
        assert!(open(&at(ok, snapshot(ok)), &peers, "b", now).is_ok());
        let far = open(&at(ok, snapshot(i64::MAX as u64)), &peers, "b", now).unwrap_err();
        assert_eq!(far, VerifyError::SnapshotAhead);
        // Older senders counting from 1 are still fine.
        assert!(open(&at(17, delta()), &peers, "b", now).is_ok());
    }

    #[test]
    fn seq_strictly_increases() {
        let s = Seq::default();
        assert_eq!(s.next(5), 5);
        assert_eq!(s.next(5), 6);
        assert_eq!(s.next(3), 7);
        assert_eq!(s.next(100), 100);
    }
}
