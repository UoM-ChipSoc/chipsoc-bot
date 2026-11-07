//! `[recording]` in bot.toml: `/record` in voice channels, with a notice to
//! everyone in the call, a transcript made afterwards and a summary posted
//! for the committee. Recordings are kept until an admin deletes one.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};

/// What the bot posts in the call's chat when it starts recording.
/// `{server}` is the community's name.
pub const DEFAULT_NOTICE: &str = "🔴 This call is being recorded and transcribed by the bot for {server}; \
recordings are kept by the committee. If you don't want to be recorded, leave the call.";

/// What someone who joins while it's recording is sent.
pub const DEFAULT_JOIN_NOTICE: &str = "🔴 The call you joined in {server} ({channel}) is being recorded and \
transcribed by the bot; recordings are kept by the committee. If you don't want to be recorded, leave the call.";

/// `[recording]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recording {
    /// Role keys whose members may start and stop recordings (admins always can).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub who: Vec<String>,
    /// Channel key where each recording's summary and links are posted.
    pub channel: String,
    /// Stop after this many hours (default 3).
    #[serde(default = "max_hours")]
    pub max_hours: f64,
    /// Posted in the call's chat when recording starts (default [`DEFAULT_NOTICE`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// DM'd to whoever joins while it records (default [`DEFAULT_JOIN_NOTICE`]);
    /// `{channel}` is the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub join_notice: Option<String>,
    /// How transcripts are made: `whisper-cpp` (with `model`), `api`
    /// (`TRANSCRIBE_URL`) or `none`. Unset: `api` when `TRANSCRIBE_URL` is set, else none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcribe: Option<Engine>,
    /// whisper.cpp's model file (a `ggml-*.bin`): an absolute path outside
    /// the config folder, to a file owned by root that the bot can't change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// whisper.cpp's program, an absolute path outside the config folder to
    /// a program owned by root that the bot can't change (default
    /// `whisper-cli` in `/usr/local/bin`, `/usr/bin` or `/bin`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whisper: Option<String>,
    /// The language spoken, e.g. `en` (default: detected).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    /// Don't start with less free disk than this, in GB (default 2).
    #[serde(default = "min_free_gb")]
    pub min_free_gb: f64,
    /// Stop a recording once its files reach this size, in GB (default 4).
    #[serde(default = "max_gb")]
    pub max_gb: f64,
    /// All of this server's recordings together may take this much, in GB
    /// (default 20): none starts beyond it, and one running stops there.
    #[serde(default = "max_total_gb")]
    pub max_total_gb: f64,
}

/// Transcription engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Engine {
    /// whisper.cpp's command-line program, run on this machine.
    WhisperCpp,
    /// An OpenAI-compatible `/audio/transcriptions` endpoint (`TRANSCRIBE_URL`, `TRANSCRIBE_API_KEY`).
    Api,
    /// No transcripts; the audio is kept.
    None,
}

fn max_hours() -> f64 {
    3.0
}

fn min_free_gb() -> f64 {
    2.0
}

fn max_gb() -> f64 {
    4.0
}

fn max_total_gb() -> f64 {
    20.0
}

/// Where whisper.cpp's program is looked for when `whisper` isn't set.
pub const WHISPER_DIRS: &[&str] = &["/usr/local/bin", "/usr/bin", "/bin"];

impl Default for Recording {
    fn default() -> Self {
        Self {
            who: Vec::new(),
            channel: String::new(),
            max_hours: max_hours(),
            notice: None,
            join_notice: None,
            transcribe: None,
            model: None,
            whisper: None,
            language: None,
            min_free_gb: min_free_gb(),
            max_gb: max_gb(),
            max_total_gb: max_total_gb(),
        }
    }
}

impl Recording {
    /// The notice for the call's chat, for `server`.
    pub fn notice_for(&self, server: &str) -> String {
        self.notice.as_deref().unwrap_or(DEFAULT_NOTICE).replace("{server}", server)
    }

    /// The DM for someone joining `channel` (its name) mid-recording.
    pub fn join_notice_for(&self, server: &str, channel: &str) -> String {
        self.join_notice
            .as_deref()
            .unwrap_or(DEFAULT_JOIN_NOTICE)
            .replace("{server}", server)
            .replace("{channel}", channel)
    }

    /// The engine to use, given whether `TRANSCRIBE_URL` is set.
    pub fn engine(&self, api_set: bool) -> Engine {
        match self.transcribe {
            Some(e) => e,
            None if api_set => Engine::Api,
            None => Engine::None,
        }
    }

    /// How transcripts are made: the engine, model and program. A change to
    /// these is the server owner's to approve, as it decides what runs on the
    /// bot's machine.
    pub fn transcription(r: Option<&Self>) -> (Option<Engine>, Option<&str>, Option<&str>) {
        r.map_or((None, None, None), |r| (r.transcribe, r.model.as_deref(), r.whisper.as_deref()))
    }

    /// [`Recording::transcription`] in words, naming the program and model
    /// exactly, for whoever approves a change to it.
    pub fn transcription_text(r: Option<&Self>) -> String {
        let code = |s: &str| format!("`{}`", s.replace('`', "'"));
        match r.and_then(|r| r.transcribe) {
            None if r.is_none() => "off".to_owned(),
            None => "by the service in TRANSCRIBE_URL if that's set, else none".to_owned(),
            Some(Engine::None) => "none".to_owned(),
            Some(Engine::Api) => "by the service in TRANSCRIBE_URL".to_owned(),
            Some(Engine::WhisperCpp) => {
                let r = r.expect("set");
                let program = r
                    .whisper
                    .as_deref()
                    .map_or_else(|| format!("`whisper-cli` from {}", WHISPER_DIRS.join(", ")), code);
                let model = r.model.as_deref().map_or_else(|| "(none)".to_owned(), code);
                format!("whisper.cpp on the bot's machine, running {program} with the model {model}")
            }
        }
    }

    /// What's wrong with these settings (beyond unknown keys, which `check`
    /// finds), for a config in `dir`.
    pub fn problems(&self, dir: &Path) -> Vec<String> {
        let mut out = Vec::new();
        if !(self.max_hours > 0.0 && self.max_hours <= 24.0) {
            out.push("bot.toml recording.max_hours: give a number of hours above 0, at most 24".to_owned());
        }
        if self.max_gb.is_nan() || self.max_gb <= 0.0 {
            out.push("bot.toml recording.max_gb: give a size above 0".to_owned());
        }
        if self.min_free_gb.is_nan() || self.min_free_gb < 0.0 {
            out.push("bot.toml recording.min_free_gb: give a size of 0 or more".to_owned());
        }
        if self.max_total_gb.is_nan() || self.max_total_gb <= 0.0 {
            out.push("bot.toml recording.max_total_gb: give a size above 0".to_owned());
        }
        for (what, path) in [("model", &self.model), ("whisper", &self.whisper)] {
            if let Some(p) = path.as_deref()
                && let Err(why) = outside(dir, p)
            {
                out.push(format!("bot.toml recording.{what}: {why}"));
            }
        }
        if self.transcribe == Some(Engine::WhisperCpp) && self.model.as_deref().is_none_or(str::is_empty) {
            out.push(
                "bot.toml recording: `transcribe = \"whisper-cpp\"` needs `model`, the path of a ggml model file"
                    .to_owned(),
            );
        }
        for (what, text) in [("notice", &self.notice), ("join_notice", &self.join_notice)] {
            if text.as_deref().is_some_and(|t| t.trim().is_empty()) {
                out.push(format!(
                    "bot.toml recording.{what}: leave it out for the default rather than empty: people must be told"
                ));
            }
        }
        out
    }
}

/// Whether `path` is fit to name a file on the bot's machine: absolute, with
/// no `..`, and not in the config folder `dir` (which anyone who can push to
/// the repo can fill). Whether the file is there, and safe to use, is checked
/// when it's used.
pub fn outside(dir: &Path, path: &str) -> Result<(), String> {
    let p = Path::new(path);
    if !p.is_absolute() || p.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err(format!("`{path}` must be an absolute path (starting with `/`, no `..`)"));
    }
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_owned());
    let (config, file) = (real(dir), real(p));
    if file.starts_with(&config) || p.starts_with(dir) {
        return Err(format!("`{path}` is in the config folder; it must be a file installed on the server"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Recording, toml::de::Error> {
        toml::from_str(s)
    }

    #[test]
    fn defaults_and_engines() {
        let r = parse("channel = \"committee\"\nwho = [\"committee\"]").unwrap();
        assert_eq!(r.max_hours, 3.0);
        assert_eq!(r.max_gb, 4.0);
        assert_eq!(r.engine(false), Engine::None);
        assert_eq!(r.engine(true), Engine::Api, "TRANSCRIBE_URL turns the API on");
        let w = parse("channel = \"c\"\ntranscribe = \"whisper-cpp\"\nmodel = \"/m.bin\"").unwrap();
        assert_eq!(w.engine(true), Engine::WhisperCpp);
        assert!(w.problems(Path::new("/srv/config")).is_empty());
        let off = parse("channel = \"c\"\ntranscribe = \"none\"").unwrap();
        assert_eq!(off.engine(true), Engine::None);
        assert!(parse("channel = \"c\"\ntranscribe = \"magic\"").is_err());
        assert!(parse("channel = \"c\"\nopt_out = true").is_err(), "unknown keys are refused");
    }

    #[test]
    fn notices_name_the_server_and_say_how_to_avoid_it() {
        let r = Recording::default();
        let n = r.notice_for("Test Society");
        assert!(n.contains("Test Society") && n.contains("recorded") && n.contains("leave the call"), "{n}");
        let j = r.join_notice_for("Test Society", "#meeting");
        assert!(j.contains("#meeting") && j.contains("leave the call"), "{j}");
        let own = Recording { notice: Some("Recording for {server}.".into()), ..Default::default() };
        assert_eq!(own.notice_for("X"), "Recording for X.");
    }

    #[test]
    fn problems_are_found() {
        let bad = Recording {
            max_hours: 0.0,
            max_gb: -1.0,
            transcribe: Some(Engine::WhisperCpp),
            notice: Some("  ".into()),
            ..Default::default()
        };
        let p = bad.problems(Path::new("/srv/config")).join("\n");
        assert!(p.contains("max_hours") && p.contains("max_gb") && p.contains("model") && p.contains("notice"), "{p}");
    }

    #[test]
    fn transcription_programs_must_be_on_the_server_not_in_the_config() {
        let d = tempfile::tempdir().unwrap();
        let dir = d.path();
        std::fs::write(dir.join("x.sh"), "#!/bin/sh\n").unwrap();
        let with = |whisper: &str, model: &str| Recording {
            transcribe: Some(Engine::WhisperCpp),
            whisper: Some(whisper.into()),
            model: Some(model.into()),
            ..Default::default()
        };
        let inside = dir.join("x.sh").display().to_string();
        let p = with(&inside, "/usr/share/whisper/ggml-base.bin").problems(dir).join("\n");
        assert!(p.contains("recording.whisper") && p.contains("in the config folder"), "{p}");
        let p = with("x.sh", "../m.bin").problems(dir).join("\n");
        assert!(p.contains("recording.whisper") && p.contains("recording.model") && p.contains("absolute"), "{p}");
        let sneaky = format!("/usr/../{}", inside.trim_start_matches('/'));
        assert!(!with(&sneaky, "/m.bin").problems(dir).is_empty(), "no `..`");
        // Through a symlink to the config folder is still in it.
        #[cfg(unix)]
        {
            let link = tempfile::tempdir().unwrap();
            std::os::unix::fs::symlink(dir, link.path().join("cfg")).unwrap();
            let via = link.path().join("cfg/x.sh").display().to_string();
            assert!(!with(&via, "/m.bin").problems(dir).is_empty(), "{via}");
        }
        assert!(with("/usr/local/bin/whisper-cli", "/usr/share/whisper/ggml-base.bin").problems(dir).is_empty());

        // The approval card says exactly what would run.
        let r = with("/usr/local/bin/whisper-cli", "/opt/m/ggml-base.bin");
        let t = Recording::transcription_text(Some(&r));
        assert!(t.contains("`/usr/local/bin/whisper-cli`") && t.contains("`/opt/m/ggml-base.bin`"), "{t}");
        assert_eq!(Recording::transcription_text(None), "off");
        assert_ne!(Recording::transcription(Some(&r)), Recording::transcription(Some(&Recording::default())));
    }

    #[test]
    fn check_finds_unknown_roles_and_channels() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path();
        std::fs::create_dir_all(p.join("categories")).unwrap();
        std::fs::write(p.join("server.toml"), "[guild]\nname = \"T\"\n").unwrap();
        std::fs::write(
            p.join("roles.toml"),
            "[[role]]\nkey = \"everyone\"\nname = \"@everyone\"\n\n[[role]]\nkey = \"committee\"\nname = \"Committee\"\n",
        )
        .unwrap();
        std::fs::write(
            p.join("categories/10-a.toml"),
            "[category]\nkey = \"a\"\nname = \"A\"\n\n[[channel]]\nkey = \"general\"\nname = \"general\"\n",
        )
        .unwrap();
        let errors = |bot: &str| {
            std::fs::write(p.join("bot.toml"), format!("name = \"T\"\n[recording]\n{bot}")).unwrap();
            crate::authoring::check(p).unwrap().errors.join("\n")
        };
        assert_eq!(errors("who = [\"committee\"]\nchannel = \"general\""), "");
        let e = errors("who = [\"ghost\"]\nchannel = \"nowhere\"\nmax_hours = 30");
        assert!(e.contains("recording.who: unknown role `ghost`"), "{e}");
        assert!(e.contains("recording.channel: unknown channel `nowhere`"), "{e}");
        assert!(e.contains("max_hours"), "{e}");
    }
}
