//! Transcripts, made after a recording stops: each speaker's track is cut
//! into the stretches where they talk, each stretch goes to an engine
//! (whisper.cpp on this machine, or an OpenAI-compatible endpoint), and the
//! lines are merged in time order into `transcript.md`, "Name [mm:ss]: …".

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::store::{self, Meta, TRANSCRIPT, WORK};
use crate::{duration, md_name, opus, wav};

/// Longest piece of audio sent at once (5 min of 16 kHz WAV is about 10 MB).
const MAX_CHUNK_MS: u64 = 5 * 60_000;
/// Stretches of speech closer than this go in one piece.
const JOIN_GAP_MS: u64 = 10_000;
/// Audio kept either side of a stretch of speech.
const PAD_MS: u64 = 300;
/// How long one piece may take.
const PER_CHUNK: Duration = Duration::from_secs(10 * 60);

/// One line of the transcript.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    /// When it was said, in ms from the start.
    pub at_ms: u64,
    pub speaker: String,
    pub text: String,
}

/// What an engine heard in one piece, times from the piece's start.
#[derive(Debug, Clone, PartialEq)]
pub struct Seg {
    pub from_ms: u64,
    pub text: String,
}

/// How to turn speech into text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Engine {
    /// whisper.cpp's `whisper-cli`.
    WhisperCpp { program: String, model: PathBuf, language: Option<String> },
    /// An OpenAI-compatible `/audio/transcriptions` endpoint.
    Api { url: String, key: Option<String>, model: String, language: Option<String> },
}

impl Engine {
    /// The API engine `TRANSCRIBE_URL`, `TRANSCRIBE_API_KEY` and
    /// `TRANSCRIBE_MODEL` (default `whisper-1`) describe, if the URL is set.
    pub fn api_from_env(language: Option<String>) -> Option<Self> {
        let url = std::env::var("TRANSCRIBE_URL").ok().filter(|u| !u.trim().is_empty())?;
        let key = std::env::var("TRANSCRIBE_API_KEY").ok().filter(|k| !k.is_empty());
        let model =
            std::env::var("TRANSCRIBE_MODEL").ok().filter(|m| !m.is_empty()).unwrap_or_else(|| "whisper-1".into());
        Some(Self::Api { url: endpoint(&url), key, model, language })
    }

    /// whisper.cpp's `program` with `model`, once both are found safe to
    /// use ([`installed`]): what it says is wrong otherwise.
    pub fn whisper_cpp(program: &Path, model: &Path, language: Option<String>) -> Result<Self, String> {
        installed(program, true).map_err(|why| format!("whisper.cpp's program {why}"))?;
        installed(model, false).map_err(|why| format!("whisper.cpp's model {why}"))?;
        Ok(Self::WhisperCpp { program: program.display().to_string(), model: model.to_owned(), language })
    }

    /// What to call it in the transcript.
    pub fn label(&self) -> String {
        match self {
            Self::WhisperCpp { model, .. } => {
                format!("whisper.cpp ({})", model.file_name().map(|n| n.to_string_lossy()).unwrap_or_default())
            }
            Self::Api { model, .. } => format!("a transcription service ({model})"),
        }
    }

    /// The text in the 16 kHz WAV at `wav`.
    pub async fn run(&self, wav: &Path) -> Result<Vec<Seg>> {
        match self {
            Self::WhisperCpp { program, model, language } => whisper(program, model, language.as_deref(), wav).await,
            Self::Api { url, key, model, language } => api(url, key.as_deref(), model, language.as_deref(), wav).await,
        }
    }
}

/// The transcriptions endpoint for `url`, which may name only the API's base.
fn endpoint(url: &str) -> String {
    let u = url.trim().trim_end_matches('/');
    if u.ends_with("/audio/transcriptions") { u.to_owned() } else { format!("{u}/audio/transcriptions") }
}

/// The only environment whisper.cpp gets: none of the bot's (its token,
/// keys and database are there).
const WHISPER_ENV: &[(&str, &str)] = &[("PATH", "/usr/local/bin:/usr/bin:/bin"), ("LC_ALL", "C.UTF-8")];

/// Whether `path` is fit for the bot to run (a `program`) or read: given
/// as an absolute path, not a symlink, a regular file owned by root that
/// nobody else can change, in folders likewise (so the bot can't swap it
/// either); a program must be executable. `Err` ends a sentence starting
/// with the file: "isn't owned by root".
pub fn installed(path: &Path, program: bool) -> Result<(), String> {
    installed_by(path, program, 0)
}

#[cfg(unix)]
fn installed_by(path: &Path, program: bool, owner: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let at = path.display();
    if !path.is_absolute() {
        return Err(format!("{at} must be an absolute path"));
    }
    let m = std::fs::symlink_metadata(path).map_err(|_| format!("{at} isn't there"))?;
    if m.file_type().is_symlink() {
        return Err(format!("{at} is a symlink; name the file it points to"));
    }
    if !m.is_file() {
        return Err(format!("{at} isn't a file"));
    }
    let safe = |m: &std::fs::Metadata| (m.uid() == owner || m.uid() == 0) && m.mode() & 0o022 == 0;
    if m.uid() != owner || !safe(&m) {
        return Err(format!("{at} must be owned by root and writable by nobody else"));
    }
    if program && m.mode() & 0o111 == 0 {
        return Err(format!("{at} isn't executable"));
    }
    for dir in path.ancestors().skip(1) {
        let d = std::fs::metadata(dir).map_err(|_| format!("{at}: {} can't be read", dir.display()))?;
        if !safe(&d) {
            return Err(format!("{at} is in {}, which others than root can change", dir.display()));
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn installed_by(path: &Path, _: bool, _: u32) -> Result<(), String> {
    Err(format!("{} can't be checked on this system, so it isn't used", path.display()))
}

async fn whisper(program: &str, model: &Path, language: Option<&str>, wav: &Path) -> Result<Vec<Seg>> {
    let out = wav.with_extension("");
    let mut cmd = tokio::process::Command::new(program);
    cmd.arg("-m").arg(model).arg("-f").arg(wav).arg("-oj").arg("-of").arg(&out).arg("-np");
    cmd.arg("-l").arg(language.unwrap_or("auto"));
    cmd.env_clear().envs(WHISPER_ENV.iter().copied());
    if let Some(dir) = wav.parent() {
        cmd.current_dir(dir);
    }
    cmd.kill_on_drop(true).stdin(std::process::Stdio::null());
    let done = tokio::time::timeout(PER_CHUNK, cmd.output())
        .await
        .context("whisper.cpp took too long")?
        .with_context(|| format!("running {program} (is whisper.cpp installed?)"))?;
    if !done.status.success() {
        let err = String::from_utf8_lossy(&done.stderr);
        let tail: String =
            err.lines().rev().take(3).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join(" / ");
        bail!("{program} failed ({}): {tail}", done.status);
    }
    let json = out.with_extension("json");
    let text = tokio::fs::read(&json).await.with_context(|| format!("reading {}", json.display()))?;
    parse_whisper(&text)
}

/// whisper.cpp's `-oj` output.
fn parse_whisper(json: &[u8]) -> Result<Vec<Seg>> {
    #[derive(Deserialize)]
    struct Out {
        transcription: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        offsets: Offsets,
        text: String,
    }
    #[derive(Deserialize)]
    struct Offsets {
        from: u64,
    }
    let out: Out = serde_json::from_slice(json).context("reading whisper.cpp's output")?;
    Ok(out.transcription.into_iter().map(|i| Seg { from_ms: i.offsets.from, text: i.text }).collect())
}

async fn api(url: &str, key: Option<&str>, model: &str, language: Option<&str>, wav: &Path) -> Result<Vec<Seg>> {
    let audio = tokio::fs::read(wav).await?;
    let client = reqwest::Client::builder().timeout(PER_CHUNK).build()?;
    let mut last = String::new();
    // Some services only answer plain JSON (no timings): asked for second.
    for format in ["verbose_json", "json"] {
        let part = reqwest::multipart::Part::bytes(audio.clone()).file_name("audio.wav").mime_str("audio/wav")?;
        let mut form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", model.to_owned())
            .text("response_format", format);
        if let Some(l) = language {
            form = form.text("language", l.to_owned());
        }
        let mut req = client.post(url).multipart(form);
        if let Some(k) = key {
            req = req.bearer_auth(k);
        }
        let res = req.send().await.context("sending audio to TRANSCRIBE_URL")?;
        let status = res.status();
        let body = res.bytes().await?;
        if status.is_success() {
            return parse_api(&body);
        }
        last = format!("{status}: {}", String::from_utf8_lossy(&body).chars().take(300).collect::<String>());
        if status != reqwest::StatusCode::BAD_REQUEST {
            break;
        }
    }
    bail!("the transcription service refused: {last}")
}

/// An `/audio/transcriptions` answer: its segments, else all its text at the start.
fn parse_api(json: &[u8]) -> Result<Vec<Seg>> {
    #[derive(Deserialize)]
    struct Out {
        #[serde(default)]
        text: String,
        #[serde(default)]
        segments: Vec<Item>,
    }
    #[derive(Deserialize)]
    struct Item {
        start: f64,
        text: String,
    }
    let out: Out = serde_json::from_slice(json).context("reading the transcription service's answer")?;
    if out.segments.is_empty() {
        return Ok(vec![Seg { from_ms: 0, text: out.text }]);
    }
    Ok(out.segments.into_iter().map(|s| Seg { from_ms: (s.start.max(0.0) * 1000.0) as u64, text: s.text }).collect())
}

/// What engines write for silence or noise, not speech.
fn is_noise(text: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || (t.starts_with('[') && t.ends_with(']'))
        || (t.starts_with('(') && t.ends_with(')'))
        || (t.starts_with('*') && t.ends_with('*'))
}

/// The pieces to transcribe of a track whose speech is `speech` (ms), in a
/// recording `length` ms long: stretches close together joined, padded,
/// none longer than [`MAX_CHUNK_MS`].
pub fn chunks(speech: &[[u64; 2]], length: u64) -> Vec<(u64, u64)> {
    let mut out: Vec<(u64, u64)> = Vec::new();
    for &[from, to] in speech {
        let (a, b) = (from.saturating_sub(PAD_MS), (to + PAD_MS).min(length.max(to)));
        match out.last_mut() {
            Some(c) if a <= c.1 + JOIN_GAP_MS && b - c.0 <= MAX_CHUNK_MS => c.1 = c.1.max(b),
            _ => out.push((a, b)),
        }
    }
    // A long stretch with no pause is cut up.
    out.into_iter()
        .flat_map(|(a, b)| {
            let mut v = Vec::new();
            let mut s = a;
            while s < b {
                let e = (s + MAX_CHUNK_MS).min(b);
                v.push((s, e));
                s = e;
            }
            v
        })
        .collect()
}

/// `ms` as `mm:ss`, or `h:mm:ss` past an hour.
pub fn stamp(ms: u64) -> String {
    let s = ms / 1000;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// The transcript: a heading, then every line in time order (speakers in
/// name order when two start together), "Name [mm:ss]: text".
pub fn merge(meta: &Meta, made_by: &str, mut lines: Vec<Line>) -> String {
    lines.sort_by(|a, b| a.at_ms.cmp(&b.at_ms).then_with(|| a.speaker.cmp(&b.speaker)));
    let when = chrono::DateTime::from_timestamp(meta.started_at, 0).unwrap_or_default().format("%Y-%m-%d %H:%M UTC");
    // Names are whatever people chose: escaped, so none passes for a line or a heading.
    let channel = match meta.channel_name.strip_prefix('#') {
        Some(n) => format!("#{}", md_name(n)),
        None => md_name(&meta.channel_name),
    };
    let mut out = format!("# Transcript: {channel}, {when}\n\n");
    let people: Vec<String> = meta.names().into_iter().map(md_name).collect();
    out.push_str(&format!("Recorded for {}. People: {}.\n", duration(meta.length_ms), people.join(", ")));
    out.push_str(&format!("Transcribed automatically by {made_by}; it may have mistakes.\n\n"));
    if lines.is_empty() {
        out.push_str("(Nothing was said that could be transcribed.)\n");
    }
    for l in lines {
        let text = l.text.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!("{} [{}]: {text}\n\n", md_name(&l.speaker), stamp(l.at_ms)));
    }
    out
}

/// Cut `track` into 16 kHz WAVs, one per piece in `pieces` (ms), in `work`.
fn extract(track: &Path, pieces: &[(u64, u64)], work: &Path, stem: &str) -> Result<Vec<PathBuf>> {
    let mut ds = wav::Downsampler::new();
    let mut out16 = Vec::new();
    let mut at = 0u64; // 16 kHz samples seen
    let mut next = 0usize;
    let mut buf: Vec<i16> = Vec::new();
    let mut files = Vec::new();
    let per_ms = (wav::RATE_16K / 1000) as u64;
    let mut failed = None;
    opus::decode(track, |f| {
        if next >= pieces.len() || failed.is_some() {
            return;
        }
        out16.clear();
        ds.push(f, &mut out16);
        for &s in &out16 {
            while next < pieces.len() && at >= pieces[next].1 * per_ms {
                let p = work.join(format!("{stem}-{next}.wav"));
                if let Err(e) = wav::write(&p, &buf) {
                    failed = Some(e);
                }
                files.push(p);
                buf.clear();
                next += 1;
            }
            if next < pieces.len() && at >= pieces[next].0 * per_ms {
                buf.push(s);
            }
            at += 1;
        }
    })?;
    if let Some(e) = failed {
        return Err(e.into());
    }
    // A piece running to the end of the audio.
    while next < pieces.len() {
        let p = work.join(format!("{stem}-{next}.wav"));
        wav::write(&p, &buf)?;
        buf.clear();
        files.push(p);
        next += 1;
    }
    Ok(files)
}

/// Transcribe the recording in `dir` with `engine`, writing `transcript.md`
/// there; its text is returned. Scratch files are removed either way.
pub async fn transcribe(dir: &Path, meta: &Meta, engine: &Engine) -> Result<String> {
    let work = dir.join(WORK);
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work)?;
    let r = lines(dir, &work, meta, engine).await;
    let _ = std::fs::remove_dir_all(&work);
    let text = merge(meta, &engine.label(), r?);
    std::fs::write(dir.join(TRANSCRIPT), &text)?;
    Ok(text)
}

async fn lines(dir: &Path, work: &Path, meta: &Meta, engine: &Engine) -> Result<Vec<Line>> {
    let mut lines = Vec::new();
    for (n, t) in meta.tracks.iter().enumerate() {
        let pieces = chunks(&t.speech, meta.length_ms);
        if pieces.is_empty() {
            continue;
        }
        let (track, w, p) = (dir.join(&t.file), work.to_owned(), pieces.clone());
        let stem = format!("t{n}");
        let wavs = tokio::task::spawn_blocking(move || extract(&track, &p, &w, &stem)).await??;
        let speaker = t.user.and_then(|u| meta.name_of(u)).unwrap_or(&t.name).to_owned();
        for (wav, (from, _)) in wavs.iter().zip(&pieces) {
            let segs = engine.run(wav).await.with_context(|| format!("transcribing {speaker}'s audio"))?;
            for s in segs.into_iter().filter(|s| !is_noise(&s.text)) {
                lines.push(Line { at_ms: from + s.from_ms, speaker: speaker.clone(), text: s.text });
            }
            let _ = std::fs::remove_file(wav);
        }
    }
    Ok(lines)
}

/// Mark `dir`'s recording as being transcribed, or how it ended up.
pub fn set_state(dir: &Path, state: store::Transcript) -> Result<Meta> {
    let mut meta = store::load(dir)?;
    meta.transcript = state;
    meta.bytes = store::size(dir);
    store::save(dir, &meta)?;
    Ok(meta)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Participant, Person, Span, Track};

    fn meta() -> Meta {
        let mut m = Meta::new("x".into(), 1, 2, "#meeting", 1_780_000_000, Person { id: 7, name: "Ada".into() });
        m.length_ms = 65 * 60_000;
        for (id, name) in [(7, "Ada"), (8, "Ben")] {
            m.participants.push(Participant { id, name: name.into(), spans: vec![Span { from_ms: 0, to_ms: None }] });
        }
        m
    }

    #[test]
    fn lines_merge_in_time_order_named_and_stamped() {
        let l = |at_ms, speaker: &str, text: &str| Line { at_ms, speaker: speaker.into(), text: text.into() };
        let text = merge(
            &meta(),
            "whisper.cpp (ggml-base.en.bin)",
            vec![
                l(5_000, "Ben", " Hi."),
                l(3_000, "Ada", " Hello everyone."),
                l(5_000, "Ada", "Same time."),
                l(3_725_000, "Ben", "Bye"),
            ],
        );
        let body: Vec<&str> = text.lines().filter(|l| l.contains("]: ")).collect();
        assert_eq!(
            body,
            ["Ada [00:03]: Hello everyone.", "Ada [00:05]: Same time.", "Ben [00:05]: Hi.", "Ben [1:02:05]: Bye"]
        );
        assert!(text.starts_with("# Transcript: #meeting, 2026-05-28 20:26 UTC"), "{text}");
        assert!(text.contains("Recorded for 1h 05m. People: Ada, Ben."), "{text}");
        assert!(text.contains("whisper.cpp (ggml-base.en.bin)"));
    }

    #[test]
    fn a_name_cant_pass_for_another_line_or_a_heading() {
        let mut m = meta();
        m.channel_name = "#meeting\n# Board decision".into();
        m.participants[0].name = "Ada [00:00]: I agree ·".into();
        let l = Line {
            at_ms: 5_000,
            speaker: "# Transcript: [x](https://e.example)".into(),
            text: "hi\nBen [00:09]: no".into(),
        };
        let text = merge(&m, "whisper.cpp", vec![l]);
        assert!(text.starts_with("# Transcript: #meeting # Board decision, "), "{text}");
        assert!(text.contains("People: Ada \\[00:00\\]: I agree ·, Ben."), "{text}");
        assert!(
            text.contains("\\# Transcript: \\[x\\]\\(https://e.example\\) [00:05]: hi Ben [00:09]: no\n"),
            "{text}"
        );
        assert_eq!(text.lines().filter(|l| l.starts_with("# ")).count(), 1, "one heading: {text}");
    }

    #[cfg(unix)]
    #[test]
    fn only_files_root_installed_are_run_or_read() {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let prog = t.path().join("whisper-cli");
        std::fs::write(&prog, "#!/bin/sh\n").unwrap();
        let me = std::fs::metadata(&prog).unwrap().uid();
        let mode = |m| std::fs::set_permissions(&prog, std::fs::Permissions::from_mode(m)).unwrap();
        mode(0o755);
        if me != 0 {
            let e = installed(&prog, true).unwrap_err();
            assert!(e.contains("must be owned by root"), "a file the bot (or a push) could change: {e}");
        }
        mode(0o775);
        assert!(installed_by(&prog, true, me).unwrap_err().contains("writable by nobody else"));
        mode(0o644);
        assert!(installed_by(&prog, true, me).unwrap_err().contains("isn't executable"));
        let link = t.path().join("link");
        std::os::unix::fs::symlink(&prog, &link).unwrap();
        assert!(installed_by(&link, false, me).unwrap_err().contains("symlink"));
        assert!(installed_by(Path::new("whisper-cli"), true, me).unwrap_err().contains("absolute"));
        assert!(installed_by(&t.path().join("nope"), true, me).unwrap_err().contains("isn't there"));
        assert!(installed_by(t.path(), false, me).unwrap_err().contains("isn't a file"));
        // A program root installed, if this system has one where expected.
        let env = Path::new("/usr/bin/env");
        let root_owned =
            std::fs::symlink_metadata(env).is_ok_and(|m| m.is_file() && m.uid() == 0 && m.mode() & 0o022 == 0);
        if root_owned {
            assert_eq!(installed(env, true), Ok(()));
            assert!(Engine::whisper_cpp(env, &prog, None).unwrap_err().starts_with("whisper.cpp's model"));
        }
    }

    /// whisper.cpp gets none of the bot's environment (its token and keys are there).
    #[cfg(unix)]
    #[tokio::test]
    async fn whisper_runs_with_a_bare_environment() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let dump = t.path().join("env.txt");
        let script = t.path().join("whisper-cli");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nenv > '{}'\nwhile [ $# -gt 0 ]; do case $1 in -of) o=$2; shift;; esac; shift; done\n\
                 printf '{{\"transcription\":[]}}' > \"$o.json\"\n",
                dump.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let wav = t.path().join("a.wav");
        wav::write(&wav, &[0i16; 160]).unwrap();
        assert!(std::env::var_os("HOME").is_some() || std::env::var_os("PATH").is_some(), "the test has some");
        let segs = whisper(&script.display().to_string(), Path::new("/m.bin"), None, &wav).await.unwrap();
        assert!(segs.is_empty());
        let env = std::fs::read_to_string(&dump).unwrap();
        let names: Vec<&str> = env.lines().filter_map(|l| l.split_once('=').map(|(k, _)| k)).collect();
        assert!(names.contains(&"PATH"), "{env}");
        let shell_own = ["PATH", "LC_ALL", "PWD", "OLDPWD", "SHLVL", "_"];
        assert!(names.iter().all(|n| shell_own.contains(n)), "nothing else: {env}");
    }

    #[test]
    fn speech_is_cut_into_padded_joined_pieces() {
        assert_eq!(chunks(&[], 1000), vec![]);
        // Close together: one piece; far apart: two; padded, not past the end.
        assert_eq!(
            chunks(&[[1000, 2000], [5000, 6000], [60_000, 61_000]], 61_100),
            vec![(700, 6300), (59_700, 61_100)]
        );
        assert_eq!(chunks(&[[0, 100]], 100), vec![(0, 100)]);
        // Twelve minutes without a pause: three pieces of at most five.
        let long = chunks(&[[0, 12 * 60_000]], 12 * 60_000);
        assert_eq!(long, vec![(0, 300_000), (300_000, 600_000), (600_000, 720_000)]);
    }

    #[test]
    fn engines_answers_are_read() {
        let w = br#"{"transcription":[{"timestamps":{"from":"00:00:01,000","to":"00:00:02,000"},"offsets":{"from":1000,"to":2000},"text":" Hello"},{"offsets":{"from":2500,"to":3000},"text":" [BLANK_AUDIO]"}]}"#;
        let segs = parse_whisper(w).unwrap();
        assert_eq!(segs[0], Seg { from_ms: 1000, text: " Hello".into() });
        assert!(is_noise(&segs[1].text));
        let v = parse_api(
            br#"{"text":"a b","segments":[{"start":0.5,"end":1,"text":"a"},{"start":2.25,"end":3,"text":"b"}]}"#,
        )
        .unwrap();
        assert_eq!(v[1], Seg { from_ms: 2250, text: "b".into() });
        let plain = parse_api(br#"{"text":"all of it"}"#).unwrap();
        assert_eq!(plain, vec![Seg { from_ms: 0, text: "all of it".into() }]);
        assert_eq!(endpoint("https://api.openai.com/v1/"), "https://api.openai.com/v1/audio/transcriptions");
        assert_eq!(endpoint("http://x/v1/audio/transcriptions"), "http://x/v1/audio/transcriptions");
    }

    #[test]
    fn stamps() {
        assert_eq!(stamp(0), "00:00");
        assert_eq!(stamp(61_999), "01:01");
        assert_eq!(stamp(3_600_000), "1:00:00");
    }

    /// A pretend whisper.cpp: a script that writes the JSON whisper.cpp
    /// would, saying how long the piece it was given is.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_recording_is_transcribed_per_speaker_with_whisper_cpp() {
        use std::os::unix::fs::PermissionsExt;
        let t = tempfile::tempdir().unwrap();
        let script = t.path().join("whisper-cli");
        std::fs::write(
            &script,
            "#!/bin/sh\nwhile [ $# -gt 0 ]; do case $1 in -f) f=$2; shift;; -of) o=$2; shift;; esac; shift; done\n\
             n=$(($(wc -c < \"$f\") - 44))\n\
             printf '{\"transcription\":[{\"offsets\":{\"from\":0,\"to\":1},\"text\":\" bytes %s\"},{\"offsets\":{\"from\":0,\"to\":1},\"text\":\"[BLANK_AUDIO]\"}]}' $n > \"$o.json\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let dir = t.path().join("rec");
        std::fs::create_dir_all(&dir).unwrap();
        let mut m = meta();
        m.length_ms = 4000;
        for (user, file, speech) in [(7, "7.ogg", [0u64, 1000]), (8, "8.ogg", [2000, 3000])] {
            let mut w = opus::Writer::create(&dir.join(file), 1).unwrap();
            for i in 0..200u64 {
                let on = (speech[0] / 20..speech[1] / 20).contains(&i);
                w.push(on.then(|| vec![3000i16; crate::FRAME]).as_deref()).unwrap();
            }
            w.finish().unwrap();
            m.tracks.push(Track {
                user: Some(user),
                name: String::new(),
                file: file.into(),
                bytes: 0,
                speech: vec![speech],
            });
        }
        let engine = Engine::WhisperCpp {
            program: script.to_string_lossy().into(),
            model: "/m/ggml-tiny.bin".into(),
            language: None,
        };
        let text = transcribe(&dir, &m, &engine).await.unwrap();
        // Ada's piece is 0–1.3 s, Ben's 1.7–3.3 s: 16 kHz, two bytes a sample.
        let body: Vec<&str> = text.lines().filter(|l| l.contains("]: ")).collect();
        assert_eq!(body, ["Ada [00:00]: bytes 41600", "Ben [00:01]: bytes 51200"], "{text}");
        assert_eq!(std::fs::read_to_string(dir.join(TRANSCRIPT)).unwrap(), text);
        assert!(!dir.join(WORK).exists(), "scratch files are gone");
    }
}
