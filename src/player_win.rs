//! The background work behind the window, on Media Foundation: playback
//! (pictures and sound), the still shown while paused, the timeline's
//! thumbnails and the waveform. Each keeps its own reader open and seeks in
//! place, on its own thread, and wakes the window when it has something new.
//!
//! The readers' times are the file's media timeline, whatever the system's
//! MP4 source does with edit lists (`crate::mf`), so a time on the timeline is
//! `t + zero` there ([`crate::mp4::Info::zero`]).

use crate::audio::{self, Output};
use crate::mf::{AudioReader, Clock, Decoded, VideoReader};
use crate::mp4::Info;
use crate::paint::Picture;
use crate::shell::Wake;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// What to say when pictures and sound can't be decoded here (Windows "N").
pub const NO_DECODER_TOAST: &str = "Media Foundation is not installed: no pictures or sound, but splitting works. Add the Media Feature Pack to see the video.";
pub const NO_DECODER_HINT: &str = "Media Foundation is not installed: add the Media Feature Pack to see pictures and hear sound";
pub const NO_DECODER_PREVIEW: &str = "No preview: Media Foundation is not installed";

/// Waveform peaks a second.
pub const PEAKS_PER_SEC: u32 = 100;

/// Can pictures and sound be decoded here?
pub fn available() -> bool {
    crate::mf::available()
}

fn hns(t: Duration) -> i64 {
    (t.as_nanos() / 100) as i64
}

fn from_hns(v: i64) -> Duration {
    Duration::from_nanos(v.max(0) as u64 * 100)
}

/// The file being worked on, as the workers need it.
#[derive(Clone)]
pub struct Source {
    pub available: bool,
    pub path: PathBuf,
    pub has_audio: bool,
    has_frames: bool,
    zero: i64,
    audio_zero: i64,
    /// How the readers relate their clock to the file's.
    clock: Clock,
    turn: u8,
    /// Half a frame (100 ns): how near a picture's time must be.
    half: i64,
    /// The one decoder, shared by the still, the thumbnails and playback
    /// (one at a time: a hardware decoder holds a lot of memory).
    decoder: Arc<Decoder>,
    /// Try the GPU first (tests turn it off, to check the software path too).
    gpu: bool,
}

/// The decoder and where it has read to; dropped after a while unused.
#[derive(Default)]
struct Decoder {
    slot: Mutex<Slot>,
}

#[derive(Default)]
struct Slot {
    v: Option<VideoReader>,
    /// The last time read, while the reader is there: the next read follows it.
    at: Option<i64>,
    used: Option<Instant>,
}

/// An idle decoder gives its memory back after this long.
const DECODER_IDLE: Duration = Duration::from_secs(10);

impl Source {
    pub fn new(path: PathBuf, info: &Info) -> Source {
        Source {
            available: available(),
            path,
            has_audio: info.has_audio,
            has_frames: !info.frames.is_empty(),
            zero: hns(info.zero),
            audio_zero: hns(info.audio_zero),
            clock: Clock {
                zero: hns(info.zero),
                key: info.keys.iter().find(|k| !k.is_zero()).map(|k| hns(*k) + hns(info.zero)),
                hidden: info.hidden,
                frame: (10_000_000.0 / info.fps()) as i64,
            },
            turn: info.turn,
            half: (5_000_000.0 / info.fps()) as i64,
            decoder: Arc::default(),
            gpu: true,
        }
    }

    /// The decoder: wait for it, and open it if it is closed.
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        let mut g = self.decoder.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.v.is_none() {
            // On the GPU where it can be.
            g.v = VideoReader::open(&self.path, self.gpu, self.turn, self.clock).or_else(|_| VideoReader::open(&self.path, false, self.turn, self.clock)).ok();
            g.at = None;
        }
        g.used = Some(Instant::now());
        g
    }

    /// A reader for the sound: on the clock the pictures' reader found out.
    fn sound(&self) -> Option<AudioReader> {
        if self.has_frames {
            // Opening the picture's reader finds it out; another thread may be at it.
            for _ in 0..200 {
                if crate::mf::edits_found_out() {
                    break;
                }
                match self.decoder.slot.try_lock() {
                    Ok(g) => {
                        drop(g);
                        drop(self.slot());
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(5)),
                }
            }
        }
        AudioReader::open(&self.path, self.audio_zero).ok()
    }

    /// Close the decoder if nothing has used it for a while; true when it has.
    fn rest(&self) -> bool {
        if let Ok(mut g) = self.decoder.slot.try_lock() {
            if g.v.is_some() && g.used.is_some_and(|u| u.elapsed() > DECODER_IDLE) {
                g.v = None;
                g.at = None;
                return true;
            }
        }
        false
    }
}

// --- playing ------------------------------------------------------------------

pub struct Frame {
    pub t: Duration,
    pub pic: Picture,
}

/// Playing from `t0`: the pictures arrive on `frames`; time comes from the
/// sound played, or the wall clock without sound.
pub struct Playback {
    pub t0: Duration,
    started: Instant,
    audio_clock: bool,
    /// When the sound ran out before the picture, and how much had played.
    sound_end: Arc<OnceLock<(Instant, Duration)>>,
    rx: Option<Receiver<Frame>>,
    next: Option<Frame>,
    /// No more pictures will come.
    pub video_done: bool,
}

impl Playback {
    pub fn start(src: &Source, t0: Duration, size: (u32, u32), out: Option<&Output>, wake: Wake) -> Playback {
        let generation = out.map(|o| o.reset()).unwrap_or(0);
        let mut pb = Playback { t0, started: Instant::now(), audio_clock: false, sound_end: Arc::default(), rx: None, next: None, video_done: true };
        if !src.available {
            return pb;
        }
        let (w, h) = size;
        if w > 0 && h > 0 && src.has_frames {
            let (tx, rx): (SyncSender<Frame>, _) = sync_channel(3);
            let (s, wake2) = (src.clone(), wake.clone());
            std::thread::spawn(move || {
                let mut g = s.slot();
                g.at = None;
                if let Some(v) = &mut g.v {
                    let want = hns(t0) + s.zero;
                    let _ = v.seek(want);
                    while let Some(d) = v.next() {
                        if d.ts + s.half < want {
                            continue;
                        }
                        let Some(pic) = v.picture(&d, (w, h)) else { continue };
                        if tx.send(Frame { t: from_hns(d.ts - s.zero), pic }).is_err() {
                            g.used = Some(Instant::now());
                            return;
                        }
                        wake2();
                    }
                }
                g.used = Some(Instant::now());
                drop(g);
                drop(tx);
                wake2();
            });
            pb.rx = Some(rx);
            pb.video_done = false;
        }
        if let (true, Some(out)) = (src.has_audio, out) {
            let sh = out.shared.clone();
            let (rate, channels) = (out.rate, out.channels);
            let cap = audio::max_queued(rate, channels);
            let (s, want, end) = (src.clone(), hns(t0) + src.audio_zero, pb.sound_end.clone());
            std::thread::spawn(move || {
                let Some(mut a) = s.sound() else {
                    let _ = end.set((Instant::now(), Duration::ZERO));
                    return;
                };
                let _ = a.seek(want);
                let mut mix = Resample::new(a.rate, a.channels, rate);
                while let Some((ts, mut pcm)) = a.next() {
                    let ch = a.channels.max(1) as usize;
                    // What is before the start, on whole frames.
                    let early = ((want - ts).max(0) as u128 * a.rate as u128 / 10_000_000) as usize * ch;
                    if early >= pcm.len() {
                        continue;
                    }
                    pcm.drain(..early);
                    let samples = mix.push(&pcm);
                    let Ok(q) = sh.queue.lock() else { return };
                    let mut q = sh
                        .room
                        .wait_while(q, |q| q.len() + samples.len() > cap && sh.generation.load(Ordering::SeqCst) == generation)
                        .unwrap_or_else(|e| e.into_inner());
                    if sh.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    q.extend(samples);
                }
                // The sound ran out: once it has played, the wall clock takes over.
                loop {
                    if sh.generation.load(Ordering::SeqCst) != generation {
                        return;
                    }
                    if sh.queue.lock().map_or(true, |q| q.is_empty()) {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                let frames = sh.played.load(Ordering::SeqCst);
                let _ = end.set((Instant::now(), Duration::from_nanos((frames as u128 * 1_000_000_000 / rate.max(1) as u128) as u64)));
            });
            pb.audio_clock = true;
        }
        pb
    }

    /// Where playback is now.
    pub fn clock(&self, out: Option<&Output>) -> Duration {
        if let Some((at, played)) = self.sound_end.get() {
            return self.t0 + *played + at.elapsed();
        }
        match (self.audio_clock, out) {
            (true, Some(o)) => self.t0 + o.played(),
            _ => self.t0 + self.started.elapsed(),
        }
    }

    /// The newest picture due by `now`, if one came since the last call.
    pub fn due(&mut self, now: Duration) -> Option<Frame> {
        let mut shown = None;
        loop {
            if self.next.is_none() {
                match self.rx.as_ref().map(|r| r.try_recv()) {
                    Some(Ok(f)) => self.next = Some(f),
                    Some(Err(std::sync::mpsc::TryRecvError::Disconnected)) => {
                        self.video_done = true;
                        break;
                    }
                    _ => break,
                }
            }
            match &self.next {
                Some(f) if f.t <= now + Duration::from_millis(4) => shown = self.next.take(),
                _ => break,
            }
        }
        shown
    }

    pub fn stop(self, out: Option<&Output>) {
        if let Some(o) = out {
            o.reset();
        }
    }
}

/// Interleaved floats of any shape to stereo at the output's rate (linear:
/// this is a preview).
struct Resample {
    step: f64,
    channels: usize,
    at: f64,
    last: [f32; 2],
    first: bool,
    same: bool,
}

impl Resample {
    fn new(rate: u32, channels: u16, out_rate: u32) -> Resample {
        Resample { step: rate as f64 / out_rate as f64, channels: channels.max(1) as usize, at: 0.0, last: [0.0; 2], first: true, same: rate == out_rate }
    }

    fn push(&mut self, pcm: &[f32]) -> Vec<f32> {
        let frames = pcm.chunks_exact(self.channels).map(|f| if self.channels == 1 { [f[0], f[0]] } else { [f[0], f[1]] });
        if self.same {
            return frames.flatten().collect();
        }
        let mut out = Vec::with_capacity((pcm.len() as f64 / self.channels as f64 / self.step) as usize * 2 + 4);
        for f in frames {
            if self.first {
                self.last = f;
                self.first = false;
                continue;
            }
            // Target positions between `last` (0) and `f` (1).
            while self.at < 1.0 {
                let k = self.at as f32;
                out.push(self.last[0] + (f[0] - self.last[0]) * k);
                out.push(self.last[1] + (f[1] - self.last[1]) * k);
                self.at += self.step;
            }
            self.at -= 1.0;
            self.last = f;
        }
        out
    }
}

// --- the still while paused ---------------------------------------------------

#[derive(Clone, Copy)]
struct StillJob {
    t: Duration,
    size: (u32, u32),
    seq: u64,
}

/// How many pictures before the last one asked for stay decoded: stepping
/// back through them needs no decoding at all.
const KEPT: usize = 10;

/// Finds the picture at a time: reading on when it is just ahead, seeking
/// otherwise, and keeping the last few for stepping back.
struct Finder {
    zero: i64,
    half: i64,
    size: (u32, u32),
    kept: VecDeque<(i64, Picture)>,
}

impl Finder {
    fn new(zero: i64, half: i64) -> Finder {
        Finder { zero, half, size: (0, 0), kept: VecDeque::new() }
    }

    fn keep(&mut self, ts: i64, p: Picture) {
        if self.kept.back().is_none_or(|k| k.0 < ts) {
            self.kept.push_back((ts, p));
            while self.kept.len() > KEPT {
                self.kept.pop_front();
            }
        }
    }

    /// The picture at `t`, `size`. `newer` is asked as it reads: a request
    /// that came in meanwhile takes over, and this one is given up.
    fn find(&mut self, src: &Source, t: Duration, size: (u32, u32), newer: impl Fn() -> bool) -> Option<Picture> {
        if size != self.size {
            self.size = size;
            self.kept.clear();
        }
        let want = hns(t) + self.zero;
        if let Some((_, p)) = self.kept.iter().find(|k| (k.0 - want).abs() <= self.half) {
            return Some(p.clone());
        }
        let mut g = src.slot();
        let Slot { v, at, used } = &mut *g;
        let v = v.as_mut()?;
        // Reading on is cheaper than seeking, for up to about a second ahead.
        let mut seek = !at.is_some_and(|a| want > a && want < a + 10_000_000);
        loop {
            if seek {
                v.seek(want).ok()?;
                self.kept.clear();
            }
            let mut last: Option<Decoded> = None;
            while let Some(d) = v.next() {
                *at = Some(d.ts);
                *used = Some(Instant::now());
                if d.ts + self.half >= want {
                    let p = v.picture(&d, size)?;
                    self.keep(d.ts, p.clone());
                    return Some(p);
                }
                // Near enough to be stepped back to.
                if d.ts + 2 * self.half * KEPT as i64 >= want {
                    if let Some(p) = v.picture(&d, size) {
                        self.keep(d.ts, p);
                    }
                }
                if newer() {
                    return None;
                }
                last = Some(d);
            }
            // At the end of the file. Past the last picture: the last one.
            *at = None;
            if let Some(d) = last {
                return v.picture(&d, size);
            }
            // Read on from the very end: nothing came, so seek back for it.
            if seek {
                return None;
            }
            seek = true;
        }
    }
}

/// Decodes the exact picture at a time; a newer request replaces an older
/// one still waiting.
pub struct Stills {
    job: Arc<(Mutex<Option<StillJob>>, Condvar)>,
    pub done: Arc<Mutex<Option<(u64, Duration, Picture)>>>,
    seq: u64,
}

impl Stills {
    pub fn new(src: Source, wake: Wake) -> Stills {
        let job: Arc<(Mutex<Option<StillJob>>, Condvar)> = Arc::new((Mutex::new(None), Condvar::new()));
        let done = Arc::new(Mutex::new(None));
        let (j2, d2) = (Arc::downgrade(&job), done.clone());
        std::thread::spawn(move || {
            if !src.available || !src.has_frames {
                return;
            }
            let mut f = Finder::new(src.zero, src.half);
            loop {
                let Some(job) = j2.upgrade() else { return };
                let next = {
                    let g = job.0.lock().unwrap_or_else(|e| e.into_inner());
                    let mut g = job.1.wait_timeout_while(g, Duration::from_millis(500), |j| j.is_none()).unwrap_or_else(|e| e.into_inner()).0;
                    g.take()
                };
                let Some(StillJob { t, size, seq }) = next else {
                    if src.rest() {
                        f.kept.clear();
                    }
                    continue;
                };
                let newer = || job.0.lock().is_ok_and(|g| g.is_some());
                if let Some(p) = f.find(&src, t, size, newer) {
                    *d2.lock().unwrap_or_else(|e| e.into_inner()) = Some((seq, t, p));
                    wake();
                }
            }
        });
        Stills { job, done, seq: 0 }
    }

    /// Ask for the picture at `t`; its request number.
    pub fn request(&mut self, t: Duration, size: (u32, u32)) -> u64 {
        self.seq += 1;
        *self.job.0.lock().unwrap_or_else(|e| e.into_inner()) = Some(StillJob { t, size, seq: self.seq });
        self.job.1.notify_one();
        self.seq
    }
}

// --- thumbnails ---------------------------------------------------------------

/// The asks waiting (newest last) and every key asked for, not yet made.
type Queue = Arc<(Mutex<(Vec<u64>, HashSet<u64>)>, Condvar)>;

/// Thumbnails by time (milliseconds), made on demand, newest asks first.
pub struct Thumbs {
    pub width: u32,
    pub cache: Arc<Mutex<HashMap<u64, Picture>>>,
    queue: Queue,
}

impl Thumbs {
    pub fn new(src: Source, size: (u32, u32), wake: Wake) -> Thumbs {
        let cache: Arc<Mutex<HashMap<u64, Picture>>> = Arc::new(Mutex::new(HashMap::new()));
        let queue: Queue = Arc::new((Mutex::new((Vec::new(), HashSet::new())), Condvar::new()));
        let (c, q) = (Arc::downgrade(&cache), Arc::downgrade(&queue));
        std::thread::spawn(move || {
            if !src.available || !src.has_frames {
                return;
            }
            loop {
                let Some(queue) = q.upgrade() else { return };
                let key = {
                    let g = queue.0.lock().unwrap_or_else(|e| e.into_inner());
                    let mut g = queue.1.wait_timeout_while(g, Duration::from_millis(500), |q| q.0.is_empty()).unwrap_or_else(|e| e.into_inner()).0;
                    g.0.pop()
                };
                drop(queue);
                let Some(key) = key else { continue };
                let pic = {
                    let mut g = src.slot();
                    g.at = None;
                    g.v.as_mut().and_then(|v| {
                        v.seek(hns(Duration::from_millis(key)) + src.zero).ok()?;
                        let d = v.next()?;
                        v.picture(&d, size)
                    })
                };
                let Some(cache) = c.upgrade() else { return };
                if let Some(p) = pic {
                    cache.lock().unwrap_or_else(|e| e.into_inner()).insert(key, p);
                }
                if let Some(q) = q.upgrade() {
                    q.0.lock().unwrap_or_else(|e| e.into_inner()).1.remove(&key);
                }
                wake();
            }
        });
        Thumbs { width: size.0, cache, queue }
    }

    /// The thumbnail for `key`, asking for it when there is none yet.
    pub fn get(&self, key: u64) -> Option<Picture> {
        if let Some(p) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
            return Some(p.clone());
        }
        let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        if q.1.insert(key) {
            q.0.push(key);
            self.queue.1.notify_one();
        }
        None
    }

    /// Drop the asks not yet started (the view moved on).
    pub fn forget_pending(&self) {
        let mut q = self.queue.0.lock().unwrap_or_else(|e| e.into_inner());
        let (list, set) = &mut *q;
        for k in list.drain(..) {
            set.remove(&k);
        }
    }
}

// --- waveform -----------------------------------------------------------------

/// The waveform, filled in as the sound is decoded.
pub fn load_peaks(src: &Source, wake: Wake) -> Arc<Mutex<Vec<f32>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    if src.available && src.has_audio {
        let (o, s, zero) = (Arc::downgrade(&out), src.clone(), src.audio_zero);
        std::thread::spawn(move || {
            let Some(mut a) = s.sound() else { return };
            let per = (a.rate / PEAKS_PER_SEC).max(1) as usize * a.channels.max(1) as usize;
            let mut peaks = Vec::<f32>::new();
            let (mut peak, mut n) = (0f32, 0usize);
            let ch = a.channels.max(1) as usize;
            let mut last = Instant::now();
            while let Some((ts, pcm)) = a.next() {
                // What the edit list hides at the start.
                let early = ((zero - ts).max(0) as u128 * a.rate as u128 / 10_000_000) as usize * ch;
                let from = early.min(pcm.len());
                for v in &pcm[from..] {
                    peak = peak.max(v.abs());
                    n += 1;
                    if n == per {
                        peaks.push(peak);
                        (peak, n) = (0.0, 0);
                    }
                }
                if last.elapsed() > Duration::from_millis(150) {
                    last = Instant::now();
                    let Some(o) = o.upgrade() else { return };
                    *o.lock().unwrap_or_else(|e| e.into_inner()) = peaks.clone();
                    wake();
                }
            }
            if let Some(o) = o.upgrade() {
                *o.lock().unwrap_or_else(|e| e.into_inner()) = peaks;
                wake();
            }
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// A video whose frames say which they are: eight blocks across, bright
    /// or dark for the bits of the frame's number, on a colour that shows the
    /// chroma plane is where it should be.
    fn numbered(ff: &Path, dir: &Path, name: &str, (w, h): (u32, u32), seconds: u32) -> PathBuf {
        let p = dir.join(name);
        let block = w / 8;
        let video =
            format!("color=c=black:s={w}x{h}:r=30:d={seconds},format=yuv420p,geq=lum='if(mod(floor(N/pow(2,floor(X/{block}))),2),200,40)':cb=90:cr=200");
        let ok = Command::new(ff)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", &video, "-f", "lavfi", "-i"])
            .arg(format!("sine=frequency=440:duration={seconds}"))
            .args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "12", "-g", "30", "-bf", "2", "-c:a", "aac", "-shortest"])
            .args(["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709"])
            .arg(&p)
            .status()
            .unwrap()
            .success();
        assert!(ok, "ffmpeg made no fixture");
        p
    }

    fn fixture(ff: &Path, dir: &Path) -> PathBuf {
        numbered(ff, dir, "src.mp4", (320, 240), 4)
    }

    fn info(p: &Path) -> Info {
        crate::mp4::info(&mut std::io::BufReader::new(std::fs::File::open(p).unwrap())).unwrap()
    }

    /// How bright a pixel of a BGRA picture is.
    fn level(p: &Picture, x: u32, y: u32) -> i32 {
        let i = (y as usize * p.w as usize + x as usize) * 4;
        p.bgra[i] as i32 + p.bgra[i + 1] as i32 + p.bgra[i + 2] as i32
    }

    /// The number a picture shows.
    fn number(p: &Picture) -> u32 {
        let block = p.w / 8;
        let levels: Vec<i32> = (0..8).map(|b| level(p, b * block + block / 2, p.h / 2)).collect();
        let (lo, hi) = (*levels.iter().min().unwrap(), *levels.iter().max().unwrap());
        // All one level: bit 7 is never set in these short clips, so it is dark.
        levels.iter().enumerate().map(|(b, l)| u32::from(hi - lo > 60 && *l > (lo + hi) / 2) << b).sum()
    }

    /// Asked in any order, on the GPU and off it, each picture is exactly the
    /// frame asked for: `first` is the number of the file's frame 0.
    fn check_frames(ff: &Path, p: &Path, size: (u32, u32), first: u32) {
        let i = info(p);
        let n = i.frames.len();
        // What ffmpeg shows, for the record: it says which side a mismatch is on.
        for k in [0, 1, 2] {
            if let Ok(pic) = crate::ffmpeg::picture_at(ff, p, i.frames[k], size.0, size.1, false) {
                eprintln!("{}: ffmpeg at frame {k} shows number {}", p.display(), number(&pic));
            }
        }
        // Both decoder paths are tried before anything fails: the report says which.
        let mut wrong = Vec::new();
        for gpu in [true, false] {
            let mut src = Source::new(p.to_path_buf(), &i);
            src.gpu = gpu;
            let mut f = Finder::new(src.zero, src.half);
            // The first frames, stepping back through and past what stays
            // decoded, a jump, and to the end.
            for k in [0, 1, 2, 40, 41, 40, 39, 38, 37, 25, 24, n / 2, n - 2, 3, 0] {
                let got = f.find(&src, i.frames[k], size, || false).unwrap_or_else(|| panic!("{} (gpu {gpu}): no picture for frame {k} of {n}", p.display()));
                assert_eq!((got.w, got.h), size);
                let shown = number(&got);
                if shown != first + k as u32 {
                    wrong.push(format!("gpu {gpu}: frame {k} shows {shown}, want {}", first + k as u32));
                }
            }
        }
        assert!(wrong.is_empty(), "{}: {}", p.display(), wrong.join("; "));
    }

    /// Prints how this system's readers stamp a file, on the raw clock: what
    /// the fixes for other Windows versions are worked out from. Run with
    /// --nocapture; asserts nothing.
    fn report_clock(p: &Path, size: (u32, u32)) {
        let i = info(p);
        eprintln!(
            "CLOCK {}: zero {} audio_zero {} frames[..3] {:?} keys[..3] {:?} ({} frames)",
            p.file_name().unwrap().to_string_lossy(),
            hns(i.zero),
            hns(i.audio_zero),
            i.frames.iter().take(3).map(|t| hns(*t)).collect::<Vec<_>>(),
            i.keys.iter().take(3).map(|t| hns(*t)).collect::<Vec<_>>(),
            i.frames.len()
        );
        for gpu in [true, false] {
            // Zero and no key: no shift, the source's own clock.
            let mut v = match VideoReader::open(p, gpu, 0, Clock::default()) {
                Ok(v) => v,
                Err(e) => {
                    eprintln!("CLOCK   gpu {gpu}: no reader: {e:#}");
                    continue;
                }
            };
            eprintln!("CLOCK   gpu {gpu}: {}", v.describe());
            let read = |v: &mut VideoReader, from: &str, n: usize| {
                let mut out = Vec::new();
                for _ in 0..n {
                    let Some(d) = v.next() else {
                        out.push("END".to_string());
                        break;
                    };
                    let shown = v.picture(&d, size).map_or(u32::MAX, |pic| number(&pic));
                    out.push(format!("{}:#{shown}", d.ts));
                }
                eprintln!("CLOCK   gpu {gpu} {from}: {}", out.join(" "));
            };
            read(&mut v, "from open", 20);
            let targets = [
                ("seek 0", 0),
                ("seek zero", hns(i.zero)),
                ("seek key[1]", i.keys.get(1).map_or(0, |k| hns(*k))),
                ("seek key[1]+zero", i.keys.get(1).map_or(0, |k| hns(*k) + hns(i.zero))),
            ];
            for (name, at) in targets {
                let _ = v.seek(at);
                read(&mut v, &format!("{name} ({at})"), 6);
            }
        }
        if let Ok(mut a) = AudioReader::open(p, 0) {
            let first: Vec<i64> = (0..3).filter_map(|_| a.next().map(|x| x.0)).collect();
            eprintln!("CLOCK   audio {} Hz {} ch: first packets {first:?}", a.rate, a.channels);
        }
    }

    #[test]
    fn report_how_this_system_stamps_files() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-report-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = fixture(&ff, &dir);
        report_clock(&src, (320, 240));
        let cut = dir.join("cut.mp4");
        let mut r = std::io::BufReader::new(std::fs::File::open(&src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&cut).unwrap());
        crate::mp4::trim(&mut r, &mut w, Duration::from_secs_f64(0.5), Duration::from_secs_f64(3.5)).unwrap();
        drop(w);
        report_clock(&cut, (320, 240));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The readers' clock is the file's on any system: the first picture is
    /// where the file says, read straight away or after going back to the
    /// start, and so is a key frame.
    #[test]
    fn readers_keep_the_files_time() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-clock-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = fixture(&ff, &dir);
        let i = info(&p);
        let (zero, key) = (hns(i.zero), hns(i.keys[1]) + hns(i.zero));
        let clock = Clock { zero, key: Some(key), hidden: i.hidden, frame: 333_333 };
        for gpu in [true, false] {
            let mut v = VideoReader::open(&p, gpu, 0, clock).unwrap();
            let near = |got: i64, want: i64, what: &str| assert!((got - want).abs() <= 2, "gpu {gpu}, {what}: {got}, want {want}");
            near(v.next().unwrap().ts, zero, "from the start");
            v.seek(zero).unwrap();
            near(v.next().unwrap().ts, zero, "after seeking to it");
            v.seek(key).unwrap();
            near(v.next().unwrap().ts, key, "at a key frame");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn stills_are_exact_with_and_without_a_hidden_lead_in() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-stills-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = fixture(&ff, &dir);
        check_frames(&ff, &src, (320, 240), 0);
        let cut = dir.join("cut.mp4");
        let mut r = std::io::BufReader::new(std::fs::File::open(&src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&cut).unwrap());
        crate::mp4::trim(&mut r, &mut w, Duration::from_secs_f64(0.5), Duration::from_secs_f64(3.5)).unwrap();
        drop(w);
        check_frames(&ff, &cut, (320, 240), 15);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Decoder surfaces are taller than 1080 rows: the frames stay exact and
    /// the colours right, in the top rows too, on the GPU and off it.
    #[test]
    fn stills_are_right_at_1080p() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-1080-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = numbered(&ff, &dir, "hd.mp4", (1920, 1080), 2);
        check_frames(&ff, &src, (1920, 1080), 0);
        let i = info(&src);
        for gpu in [true, false] {
            let mut s = Source::new(src.clone(), &i);
            s.gpu = gpu;
            let mut f = Finder::new(s.zero, s.half);
            let pic = f.find(&s, i.frames[5], (1920, 1080), || false).unwrap();
            // Frame 5: blocks 0 and 2 bright. Same colour at the top, middle and bottom.
            let at = |y: u32| {
                let k = ((y * 1920 + 120) * 4) as usize;
                [pic.bgra[k] as i32, pic.bgra[k + 1] as i32, pic.bgra[k + 2] as i32]
            };
            for y in [2, 8, 300, 1070] {
                let (a, b) = (at(y), at(540));
                assert!(a.iter().zip(&b).all(|(p, q)| (p - q).abs() <= 8), "gpu {gpu}: row {y} is {a:?}, the middle {b:?}");
            }
            // BT.709 for Y 200, Cb 90, Cr 200: red well above blue.
            let [b, g, r] = at(540);
            assert!(r > g && r - b > 60, "gpu {gpu}: {b}, {g}, {r}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_waveform_covers_the_sound() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-peaks-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = fixture(&ff, &dir);
        let i = info(&p);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let tx = Mutex::new(tx);
        let peaks = load_peaks(
            &Source::new(p.clone(), &i),
            Arc::new(move || {
                let _ = tx.lock().unwrap().send(());
            }),
        );
        let t = Instant::now();
        let want = (PEAKS_PER_SEC * 4) as usize;
        while peaks.lock().unwrap().len() < want - 5 && t.elapsed() < Duration::from_secs(10) {
            let _ = rx.recv_timeout(Duration::from_millis(100));
        }
        let got = peaks.lock().unwrap().clone();
        assert!(got.len().abs_diff(want) <= 5, "{} peaks, want about {want}", got.len());
        assert!(got.iter().copied().fold(0.0f32, f32::max) > 0.05, "the sine is there");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resampling_keeps_length_and_shape() {
        let mut same = Resample::new(48000, 2, 48000);
        assert_eq!(same.push(&[0.1, 0.2, 0.3, 0.4]), vec![0.1, 0.2, 0.3, 0.4]);
        let mut mono = Resample::new(48000, 1, 48000);
        assert_eq!(mono.push(&[0.5]), vec![0.5, 0.5]);
        let mut up = Resample::new(24000, 1, 48000);
        let out = up.push(&vec![0.25; 1000]);
        assert!(out.len().abs_diff(2 * 2000) <= 8, "{}", out.len());
        assert!(out.iter().all(|v| (v - 0.25).abs() < 1e-6));
    }
}
