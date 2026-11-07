//! Turning what's heard into files: a track per speaker, each as long as the
//! recording (silent while they aren't talking, so all of them line up), and
//! a mix of everyone. Works on 20 ms ticks of decoded audio from whatever
//! receives the call (see [`Input`]), so it can be tested without Discord.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::opus;
use crate::store::{self, MIX, Meta, Participant, Span, Track};
use crate::{FRAME, TICK_MS};

/// Ticks held before writing, so audio heard just before Discord says who
/// sent it still goes on their track.
const LAG: usize = 50;
/// Silent ticks after which someone has stopped talking.
const HANG: u64 = 25;
/// Loudness (RMS) above which a frame is speech rather than background.
const VOICED: f64 = 150.0;
/// How often the metadata is saved while recording, in ticks (10 s).
const SAVE_EVERY: u64 = 500;
/// No audio for this long means the connection dropped: the gap is filled
/// with silence when it comes back, so tracks keep time.
const GAP: Duration = Duration::from_secs(2);

/// What the receiving side tells the recorder.
#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// 20 ms of the call: each sender's audio (by SSRC; 48 kHz mono, [`FRAME`] samples).
    Tick(Vec<(u32, Vec<i16>)>),
    /// Discord said who sends `ssrc`.
    Speaker {
        ssrc: u32,
        user: u64,
    },
    /// Someone came into the call (or was there when it started).
    Joined {
        user: u64,
        name: String,
    },
    Left {
        user: u64,
    },
}

/// Somewhere a track's frames go.
pub trait Sink: Send {
    fn push(&mut self, frame: Option<&[i16]>) -> io::Result<()>;
    fn bytes(&self) -> u64;
    fn finish(self: Box<Self>) -> io::Result<()>;
}

impl<W: io::Write + Send> Sink for opus::Writer<W> {
    fn push(&mut self, frame: Option<&[i16]>) -> io::Result<()> {
        opus::Writer::push(self, frame)
    }
    fn bytes(&self) -> u64 {
        opus::Writer::bytes(self)
    }
    fn finish(self: Box<Self>) -> io::Result<()> {
        opus::Writer::finish(*self).map(drop)
    }
}

/// Opens a track's sink by file name.
pub trait Sinks: Send {
    fn open(&mut self, file: &str) -> io::Result<Box<dyn Sink>>;
}

/// Ogg Opus files in a folder.
pub struct OggFiles {
    dir: PathBuf,
    serial: u32,
}

impl OggFiles {
    pub fn new(dir: &Path) -> Self {
        Self { dir: dir.to_owned(), serial: 1 }
    }
}

impl Sinks for OggFiles {
    fn open(&mut self, file: &str) -> io::Result<Box<dyn Sink>> {
        self.serial += 1;
        Ok(Box::new(opus::Writer::create(&self.dir.join(file), self.serial)?))
    }
}

/// Whose a track is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Who {
    User(u64),
    /// Audio from a sender Discord never named.
    Unknown(u32),
}

struct Open {
    sink: Box<dyn Sink>,
    /// Its entry in the metadata's tracks.
    index: usize,
    /// Talking since, and last heard, in ticks.
    talking: Option<(u64, u64)>,
}

/// Writes one recording.
pub struct Recorder {
    dir: PathBuf,
    meta: Meta,
    sinks: Box<dyn Sinks>,
    /// Ticks heard: the recording's clock.
    heard: u64,
    /// Ticks written.
    written: u64,
    window: VecDeque<Vec<(u32, Vec<i16>)>>,
    senders: HashMap<u32, u64>,
    tracks: BTreeMap<Who, Open>,
    mix: Box<dyn Sink>,
    /// Stop growing past this many bytes.
    cap: u64,
}

fn loud(f: &[i16]) -> bool {
    let sum: f64 = f.iter().map(|&s| (s as f64) * (s as f64)).sum();
    (sum / f.len().max(1) as f64).sqrt() > VOICED
}

fn ms(ticks: u64) -> u64 {
    ticks * TICK_MS
}

impl Recorder {
    /// Start writing into `dir` (made by [`store::create`]), described by `meta`.
    pub fn new(dir: &Path, mut meta: Meta, mut sinks: Box<dyn Sinks>, cap: u64) -> io::Result<Self> {
        let mix = sinks.open(MIX)?;
        meta.mix = Some(MIX.to_owned());
        store::save(dir, &meta)?;
        Ok(Self {
            dir: dir.to_owned(),
            meta,
            sinks,
            heard: 0,
            written: 0,
            window: VecDeque::new(),
            senders: HashMap::new(),
            tracks: BTreeMap::new(),
            mix,
            cap,
        })
    }

    /// The recording so far.
    pub fn meta(&self) -> &Meta {
        &self.meta
    }

    /// How long it's been recording, in ms.
    pub fn elapsed_ms(&self) -> u64 {
        ms(self.heard)
    }

    /// The size of what's written.
    pub fn bytes(&self) -> u64 {
        self.mix.bytes() + self.tracks.values().map(|t| t.sink.bytes()).sum::<u64>()
    }

    /// Whether it has reached its size cap.
    pub fn full(&self) -> bool {
        self.bytes() >= self.cap
    }

    pub fn input(&mut self, i: Input) -> io::Result<()> {
        match i {
            Input::Tick(voices) => self.tick(voices),
            Input::Speaker { ssrc, user } => {
                self.senders.insert(ssrc, user);
                Ok(())
            }
            Input::Joined { user, name } => {
                self.joined(user, &name);
                Ok(())
            }
            Input::Left { user } => {
                self.left(user);
                Ok(())
            }
        }
    }

    /// 20 ms more of the call.
    pub fn tick(&mut self, voices: Vec<(u32, Vec<i16>)>) -> io::Result<()> {
        self.heard += 1;
        self.window.push_back(voices);
        if self.window.len() > LAG
            && let Some(oldest) = self.window.pop_front()
        {
            self.write(oldest)?;
        }
        if self.heard.is_multiple_of(SAVE_EVERY) {
            self.save()?;
        }
        Ok(())
    }

    /// Someone came in (again).
    pub fn joined(&mut self, user: u64, name: &str) {
        let now = self.elapsed_ms();
        let p = match self.meta.participants.iter_mut().find(|p| p.id == user) {
            Some(p) => p,
            None => {
                self.meta.participants.push(Participant { id: user, name: name.to_owned(), spans: Vec::new() });
                self.meta.participants.last_mut().expect("just pushed")
            }
        };
        if p.spans.last().is_none_or(|s| s.to_ms.is_some()) {
            p.spans.push(Span { from_ms: now, to_ms: None });
        }
    }

    pub fn left(&mut self, user: u64) {
        let now = self.elapsed_ms();
        if let Some(s) = self.meta.participants.iter_mut().find(|p| p.id == user).and_then(|p| p.spans.last_mut())
            && s.to_ms.is_none()
        {
            s.to_ms = Some(now);
        }
    }

    /// Write the oldest tick held.
    fn write(&mut self, voices: Vec<(u32, Vec<i16>)>) -> io::Result<()> {
        let at = self.written;
        let mut by: BTreeMap<Who, Vec<i32>> = BTreeMap::new();
        let mut mix = vec![0i32; FRAME];
        let mut any = false;
        for (ssrc, pcm) in &voices {
            let who = self.senders.get(ssrc).map_or(Who::Unknown(*ssrc), |u| Who::User(*u));
            let acc = by.entry(who).or_insert_with(|| vec![0; FRAME]);
            for (i, s) in pcm.iter().take(FRAME).enumerate() {
                acc[i] += *s as i32;
                mix[i] += *s as i32;
            }
            any = true;
        }
        for who in by.keys() {
            if !self.tracks.contains_key(who) {
                self.open(*who, at)?;
            }
        }
        let clamp =
            |v: &[i32]| -> Vec<i16> { v.iter().map(|&s| s.clamp(i16::MIN as i32, i16::MAX as i32) as i16).collect() };
        for (who, t) in &mut self.tracks {
            let frame = by.get(who).map(|v| clamp(v));
            let voiced = frame.as_deref().is_some_and(loud);
            t.sink.push(frame.as_deref())?;
            let track = &mut self.meta.tracks[t.index];
            match (&mut t.talking, voiced) {
                (Some((_, last)), true) => *last = at,
                (None, true) => t.talking = Some((at, at)),
                (Some((from, last)), false) if at - *last > HANG => {
                    track.speech.push([ms(*from), ms(*last + 1)]);
                    t.talking = None;
                }
                _ => {}
            }
        }
        self.mix.push(any.then(|| clamp(&mix)).as_deref())?;
        self.written += 1;
        Ok(())
    }

    /// A new speaker's track, silent up to tick `at`.
    fn open(&mut self, who: Who, at: u64) -> io::Result<()> {
        let (user, name, file) = match who {
            Who::User(u) => {
                let name = self.meta.name_of(u).map_or_else(|| format!("User {u}"), str::to_owned);
                (Some(u), name, format!("{u}.ogg"))
            }
            Who::Unknown(s) => (None, format!("Unknown speaker {s}"), format!("unknown-{s}.ogg")),
        };
        let mut sink = self.sinks.open(&file)?;
        for _ in 0..at {
            sink.push(None)?;
        }
        self.meta.tracks.push(Track { user, name, file, bytes: 0, speech: Vec::new() });
        self.tracks.insert(who, Open { sink, index: self.meta.tracks.len() - 1, talking: None });
        Ok(())
    }

    fn update(&mut self) {
        for t in self.tracks.values() {
            self.meta.tracks[t.index].bytes = t.sink.bytes();
        }
        self.meta.length_ms = self.elapsed_ms();
        self.meta.bytes = self.bytes();
    }

    /// Save the metadata as it is now (a crash keeps what's saved).
    pub fn save(&mut self) -> io::Result<()> {
        self.update();
        store::save(&self.dir, &self.meta)
    }

    /// Write what's held, close the files and save; `ended_at` in Unix seconds.
    pub fn finish(mut self, ended_at: i64, why: &str) -> io::Result<Meta> {
        while let Some(v) = self.window.pop_front() {
            self.write(v)?;
        }
        let end = self.written;
        for t in self.tracks.values_mut() {
            if let Some((from, last)) = t.talking.take() {
                self.meta.tracks[t.index].speech.push([ms(from), ms((last + 1).min(end))]);
            }
        }
        self.update();
        let length = self.meta.length_ms;
        for p in &mut self.meta.participants {
            for s in &mut p.spans {
                if s.to_ms.is_none() {
                    s.to_ms = Some(length);
                }
            }
        }
        self.meta.ended_at = Some(ended_at);
        self.meta.ended_because = Some(why.to_owned());
        let tracks = std::mem::take(&mut self.tracks);
        for t in tracks.into_values() {
            t.sink.finish()?;
        }
        let mix = std::mem::replace(&mut self.mix, Box::new(Nothing));
        mix.finish()?;
        self.meta.bytes = store::size(&self.dir);
        for t in &mut self.meta.tracks {
            t.bytes = std::fs::metadata(self.dir.join(&t.file)).map_or(t.bytes, |m| m.len());
        }
        store::save(&self.dir, &self.meta)?;
        Ok(self.meta)
    }
}

/// A sink that keeps nothing (what's left once the mix is closed).
struct Nothing;

impl Sink for Nothing {
    fn push(&mut self, _: Option<&[i16]>) -> io::Result<()> {
        Ok(())
    }
    fn bytes(&self) -> u64 {
        0
    }
    fn finish(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }
}

enum Msg {
    /// What was heard, and when it was sent: a stall on the recorder's
    /// thread then doesn't read as silence.
    In(Instant, Input),
    Finish {
        at: i64,
        why: String,
        done: mpsc::Sender<io::Result<Meta>>,
    },
}

/// A recorder on its own thread, fed through [`Feed`]s.
pub struct Running {
    tx: mpsc::Sender<Msg>,
    full: Arc<AtomicBool>,
    broken: Arc<AtomicBool>,
    bytes: Arc<AtomicU64>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Where the receiving side sends what it hears. Cheap to clone.
#[derive(Clone)]
pub struct Feed(mpsc::Sender<Msg>);

impl Feed {
    pub fn send(&self, i: Input) {
        self.send_at(Instant::now(), i);
    }

    fn send_at(&self, at: Instant, i: Input) {
        let _ = self.0.send(Msg::In(at, i));
    }
}

impl Running {
    /// Run `rec` on a thread of its own.
    pub fn start(rec: Recorder) -> Self {
        let (tx, rx) = mpsc::channel::<Msg>();
        let (full, broken) = (Arc::new(AtomicBool::new(false)), Arc::new(AtomicBool::new(false)));
        let bytes = Arc::new(AtomicU64::new(0));
        let (f, w, b) = (full.clone(), broken.clone(), bytes.clone());
        let thread = std::thread::Builder::new()
            .name("recorder".into())
            .spawn(move || run(rec, rx, &f, &w, &b))
            .expect("starting the recorder thread");
        Self { tx, full, broken, bytes, thread: Some(thread) }
    }

    pub fn feed(&self) -> Feed {
        Feed(self.tx.clone())
    }

    /// Whether it reached its size cap.
    pub fn full(&self) -> bool {
        self.full.load(Ordering::Relaxed)
    }

    /// Whether writing the files failed, so it can't record any more.
    pub fn broken(&self) -> bool {
        self.broken.load(Ordering::Relaxed)
    }

    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// Stop and close the files (blocks: call it off the async threads).
    pub fn finish(mut self, at: i64, why: &str) -> io::Result<Meta> {
        let (done, wait) = mpsc::channel();
        self.tx
            .send(Msg::Finish { at, why: why.to_owned(), done })
            .map_err(|_| io::Error::other("the recorder stopped"))?;
        let r = wait.recv().map_err(|_| io::Error::other("the recorder stopped"))?;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        r
    }
}

fn run(mut rec: Recorder, rx: mpsc::Receiver<Msg>, full: &AtomicBool, failed: &AtomicBool, bytes: &AtomicU64) {
    let mut last_tick: Option<Instant> = None;
    let mut broken = false;
    loop {
        let msg = match rx.recv() {
            Ok(m) => m,
            Err(_) => {
                // Every sender is gone without a finish: save what's there.
                let _ = rec.save();
                return;
            }
        };
        match msg {
            Msg::In(now, i) => {
                if broken {
                    continue;
                }
                // Ticks missed (the call's audio stopped coming for a while)
                // are written as silence, measured by when each was sent.
                if matches!(i, Input::Tick(_)) {
                    if let Some(t) = last_tick
                        && now.saturating_duration_since(t) >= GAP
                    {
                        let missed = (now.saturating_duration_since(t).as_millis() as u64 / TICK_MS).saturating_sub(1);
                        for _ in 0..missed {
                            if rec.tick(Vec::new()).is_err() {
                                break;
                            }
                        }
                    }
                    last_tick = Some(now);
                }
                if let Err(e) = rec.input(i) {
                    tracing::error!("writing the recording failed: {e}");
                    broken = true;
                    failed.store(true, Ordering::Relaxed);
                }
                bytes.store(rec.bytes(), Ordering::Relaxed);
                if rec.full() {
                    full.store(true, Ordering::Relaxed);
                }
            }
            Msg::Finish { at, why, done } => {
                let _ = done.send(rec.finish(at, &why));
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::store::Person;

    /// Each file's frames.
    type Files = Arc<Mutex<BTreeMap<String, Vec<Option<Vec<i16>>>>>>;

    /// Frames each file got, in memory.
    #[derive(Clone, Default)]
    struct Mem(Files);

    struct MemSink(Files, String);

    impl Sink for MemSink {
        fn push(&mut self, frame: Option<&[i16]>) -> io::Result<()> {
            self.0.lock().unwrap().get_mut(&self.1).unwrap().push(frame.map(<[i16]>::to_vec));
            Ok(())
        }
        fn bytes(&self) -> u64 {
            // Two bytes a silent frame, a hundred a loud one.
            self.0.lock().unwrap()[&self.1].iter().map(|f| if f.is_some() { 100 } else { 2 }).sum()
        }
        fn finish(self: Box<Self>) -> io::Result<()> {
            Ok(())
        }
    }

    impl Sinks for Mem {
        fn open(&mut self, file: &str) -> io::Result<Box<dyn Sink>> {
            self.0.lock().unwrap().insert(file.to_owned(), Vec::new());
            Ok(Box::new(MemSink(self.0.clone(), file.to_owned())))
        }
    }

    fn frame(v: i16) -> Vec<i16> {
        vec![v; FRAME]
    }

    fn recorder(cap: u64) -> (tempfile::TempDir, Mem, Recorder) {
        let t = tempfile::tempdir().unwrap();
        let (id, dir) = store::create(t.path(), 1, 0, "meeting").unwrap();
        let meta = Meta::new(id, 1, 2, "meeting", 0, Person { id: 7, name: "Ada".into() });
        let mem = Mem::default();
        let rec = Recorder::new(&dir, meta, Box::new(mem.clone()), cap).unwrap();
        (t, mem, rec)
    }

    #[test]
    fn tracks_line_up_and_the_mix_is_everyone() {
        let (dir, mem, mut rec) = recorder(u64::MAX);
        rec.joined(7, "Ada");
        rec.input(Input::Speaker { ssrc: 100, user: 7 }).unwrap();
        // Ada talks for 1 s; Ben comes in at 3 s and they overlap from 4 s for 1 s.
        for t in 0..300u64 {
            let mut v = Vec::new();
            if t < 50 || (200..250).contains(&t) {
                v.push((100, frame(1000)));
            }
            if t == 150 {
                rec.joined(8, "Ben");
                rec.input(Input::Speaker { ssrc: 200, user: 8 }).unwrap();
            }
            if (200..250).contains(&t) {
                v.push((200, frame(32_000)));
            }
            rec.tick(v).unwrap();
        }
        rec.left(8);
        let meta = rec.finish(10, "stopped by Ada").unwrap();
        let files = mem.0.lock().unwrap();
        let (ada, ben, mix) = (&files["7.ogg"], &files["8.ogg"], &files[MIX]);
        assert_eq!((ada.len(), ben.len(), mix.len()), (300, 300, 300), "every track is the whole recording");
        assert_eq!(ada[0], Some(frame(1000)));
        assert_eq!(ada[100], None);
        assert!(ben[..200].iter().all(Option::is_none), "Ben's track is silent before he spoke");
        assert_eq!(ben[220], Some(frame(32_000)));
        assert_eq!(mix[220], Some(frame(i16::MAX)), "the mix adds them up, clipped");
        assert_eq!(mix[10], Some(frame(1000)));
        assert_eq!(mix[100], None);

        assert_eq!(meta.length_ms, 6000);
        assert_eq!(meta.ended_because.as_deref(), Some("stopped by Ada"));
        let names: Vec<&str> = meta.tracks.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["Ada", "Ben"]);
        assert_eq!(meta.tracks[0].speech, vec![[0, 1000], [4000, 5000]]);
        assert_eq!(meta.tracks[1].speech, vec![[4000, 5000]]);
        let ben = meta.participants.iter().find(|p| p.id == 8).unwrap();
        assert_eq!(ben.spans, vec![Span { from_ms: 3000, to_ms: Some(6000) }]);
        let ada = meta.participants.iter().find(|p| p.id == 7).unwrap();
        assert_eq!(ada.spans, vec![Span { from_ms: 0, to_ms: Some(6000) }], "still there at the end");
        let saved = store::list(dir.path(), 1);
        assert_eq!(saved[0], meta, "saved as finished");
    }

    #[test]
    fn audio_before_discord_names_the_sender_is_theirs_and_never_named_is_kept_apart() {
        let (_t, mem, mut rec) = recorder(u64::MAX);
        rec.joined(7, "Ada");
        for t in 0..120u64 {
            if t == 30 {
                // Named within the lag: the first 30 ticks are hers too.
                rec.input(Input::Speaker { ssrc: 100, user: 7 }).unwrap();
            }
            rec.tick(vec![(100, frame(500)), (300, frame(400))]).unwrap();
        }
        let meta = rec.finish(1, "stopped").unwrap();
        let files = mem.0.lock().unwrap();
        assert!(files["7.ogg"].iter().all(|f| f.is_some()), "all of it on Ada's track");
        assert_eq!(files["unknown-300.ogg"].len(), 120);
        assert!(meta.tracks.iter().any(|t| t.user.is_none() && t.name == "Unknown speaker 300"));
    }

    #[test]
    fn rejoining_adds_a_span_and_two_senders_for_one_person_share_a_track() {
        let (_t, mem, mut rec) = recorder(u64::MAX);
        rec.joined(7, "Ada");
        rec.input(Input::Speaker { ssrc: 1, user: 7 }).unwrap();
        for _ in 0..100 {
            rec.tick(vec![(1, frame(100))]).unwrap();
        }
        rec.left(7);
        rec.joined(7, "Ada");
        rec.input(Input::Speaker { ssrc: 2, user: 7 }).unwrap();
        for _ in 0..100 {
            rec.tick(vec![(2, frame(200))]).unwrap();
        }
        let meta = rec.finish(1, "stopped").unwrap();
        assert_eq!(meta.tracks.len(), 1);
        assert_eq!(mem.0.lock().unwrap()["7.ogg"][150], Some(frame(200)));
        let spans = &meta.participants[0].spans;
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].to_ms, Some(2000));
    }

    #[test]
    fn it_says_when_it_is_full() {
        let (_t, _mem, mut rec) = recorder(10_000);
        rec.input(Input::Speaker { ssrc: 1, user: 7 }).unwrap();
        let mut ticks = 0;
        while !rec.full() {
            rec.tick(vec![(1, frame(3000))]).unwrap();
            ticks += 1;
            assert!(ticks < 1000);
        }
        assert!(ticks > LAG);
    }

    #[test]
    fn a_gap_is_timed_by_when_the_audio_came_not_when_it_was_written() {
        let (_t, mem, rec) = recorder(u64::MAX);
        let running = Running::start(rec);
        let feed = running.feed();
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        feed.send_at(at(0), Input::Speaker { ssrc: 1, user: 7 });
        // 10 ticks, 3 s with nothing, 10 more; all taken by the recorder at
        // once, as after a stall.
        for i in 0..10 {
            feed.send_at(at(i * TICK_MS), Input::Tick(vec![(1, frame(100))]));
        }
        for i in 0..10 {
            feed.send_at(at(3180 + i * TICK_MS), Input::Tick(vec![(1, frame(100))]));
        }
        running.finish(1, "stopped").unwrap();
        let files = mem.0.lock().unwrap();
        let mix = &files[MIX];
        assert_eq!(mix.len(), 10 + 149 + 10, "3 s between the 10th tick and the 11th: 149 silent ones");
        assert!(mix[10..159].iter().all(Option::is_none));
        assert_eq!(mix[159], Some(frame(100)));
    }

    #[test]
    fn on_its_own_thread_with_real_files() {
        let t = tempfile::tempdir().unwrap();
        let (id, dir) = store::create(t.path(), 1, 0, "meeting").unwrap();
        let meta = Meta::new(id.clone(), 1, 2, "meeting", 0, Person { id: 7, name: "Ada".into() });
        let rec = Recorder::new(&dir, meta, Box::new(OggFiles::new(&dir)), u64::MAX).unwrap();
        let running = Running::start(rec);
        let feed = running.feed();
        feed.send(Input::Joined { user: 7, name: "Ada".into() });
        feed.send(Input::Speaker { ssrc: 5, user: 7 });
        for i in 0..200 {
            let f: Vec<i16> = (0..FRAME).map(|j| (((i * FRAME + j) as f32 * 0.05).sin() * 5000.0) as i16).collect();
            feed.send(Input::Tick(vec![(5, f)]));
        }
        let meta = running.finish(5, "stopped").unwrap();
        assert_eq!(meta.files(), ["7.ogg", MIX]);
        for f in meta.files() {
            let mut n = 0;
            opus::decode(&dir.join(f), |s| n += s.len()).unwrap();
            assert!(n >= 199 * FRAME, "{f}: {n}");
        }
        assert!(meta.bytes > 0);
        assert_eq!(store::find(t.path(), 1, &id).unwrap().1, meta);
    }
}
