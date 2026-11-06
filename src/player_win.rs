//! The background work behind the window, on Media Foundation: playback
//! (pictures and sound), the still shown while paused, the timeline's
//! thumbnails and the waveform. Each keeps its own reader open and seeks in
//! place, on its own thread, and wakes the window when it has something new.
//!
//! Media Foundation ignores edit lists: it gives a frame its composition time
//! in the file, so a time on the timeline is `t + zero` in its clock
//! ([`crate::mp4::Info::zero`]).

use crate::audio::{self, Output};
use crate::mf::{AudioReader, Decoded, VideoReader};
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
    turn: u8,
    /// Half a frame (100 ns): how near a picture's time must be.
    half: i64,
    /// The one decoder, shared by the still, the thumbnails and playback
    /// (one at a time: a hardware decoder holds a lot of memory).
    decoder: Arc<Decoder>,
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
            turn: info.turn,
            half: (5_000_000.0 / info.fps()) as i64,
            decoder: Arc::default(),
        }
    }

    /// The decoder: wait for it, and open it if it is closed.
    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        let mut g = self.decoder.slot.lock().unwrap_or_else(|e| e.into_inner());
        if g.v.is_none() {
            // On the GPU where it can be.
            g.v = VideoReader::open(&self.path, true, self.turn).or_else(|_| VideoReader::open(&self.path, false, self.turn)).ok();
            g.at = None;
        }
        g.used = Some(Instant::now());
        g
    }

    /// Close the decoder if nothing has used it for a while.
    fn rest(&self) {
        if let Ok(mut g) = self.decoder.slot.try_lock() {
            if g.v.is_some() && g.used.is_some_and(|u| u.elapsed() > DECODER_IDLE) {
                g.v = None;
                g.at = None;
            }
        }
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
            let (path, want, end) = (src.path.clone(), hns(t0) + src.audio_zero, pb.sound_end.clone());
            std::thread::spawn(move || {
                let Ok(mut a) = AudioReader::open(&path) else {
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
                    src.rest();
                    continue;
                };
                let newer = || job.0.lock().map_or(false, |g| g.is_some());
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
        let (o, path, zero) = (Arc::downgrade(&out), src.path.clone(), src.audio_zero);
        std::thread::spawn(move || {
            let Ok(mut a) = AudioReader::open(&path) else { return };
            let per = (a.rate / PEAKS_PER_SEC).max(1) as usize * a.channels.max(1) as usize;
            let mut peaks = Vec::<f32>::new();
            let (mut peak, mut n) = (0f32, 0usize);
            // What the edit list hides at the start.
            let mut skip = (zero.max(0) as u128 * a.rate as u128 / 10_000_000) as usize * a.channels.max(1) as usize;
            let mut last = Instant::now();
            while let Some((_, pcm)) = a.next() {
                let from = skip.min(pcm.len());
                skip -= from;
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

    fn fixture(ff: &Path, dir: &Path) -> PathBuf {
        let p = dir.join("src.mp4");
        let ok = Command::new(ff)
            .args(["-hide_banner", "-loglevel", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=320x240:rate=30:duration=4", "-f", "lavfi", "-i"])
            .args(["sine=frequency=440:duration=4", "-c:v", "libx264", "-g", "30", "-bf", "2", "-pix_fmt", "yuv420p", "-c:a", "aac", "-shortest"])
            .arg(&p)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        p
    }

    fn info(p: &Path) -> Info {
        crate::mp4::info(&mut std::io::BufReader::new(std::fs::File::open(p).unwrap())).unwrap()
    }

    fn diff(a: &Picture, b: &Picture) -> f64 {
        a.bgra.iter().zip(b.bgra.iter()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum::<u64>() as f64 / a.bgra.len() as f64
    }

    /// Asked in any order, the picture is the one ffmpeg shows at that time,
    /// and not its neighbour.
    fn check_frames(ff: &Path, p: &Path) {
        let i = info(p);
        let src = Source::new(p.to_path_buf(), &i);
        let mut f = Finder::new(src.zero, src.half);
        let n = i.frames.len();
        for k in [0, 1, 2, 40, 41, 40, 39, 38, 37, n / 2, n - 2, n - 1, 3] {
            let t = i.frames[k];
            let got = f.find(&src, t, (320, 240), || false).unwrap_or_else(|| panic!("{}: no picture for frame {k} of {n}", p.display()));
            let want = crate::ffmpeg::picture_at(ff, p, t, 320, 240, false).unwrap();
            let d = diff(&got, &want);
            assert!(d < 6.0, "{}: frame {k}: differs by {d} from ffmpeg's", p.display());
            for near in [k.wrapping_sub(1), k + 1] {
                if near < n {
                    let other = crate::ffmpeg::picture_at(ff, p, i.frames[near], 320, 240, false).unwrap();
                    let e = diff(&got, &other);
                    assert!(d < e, "{}: frame {k} looks more like frame {near}: {d} vs {e}", p.display());
                }
            }
        }
    }

    #[test]
    fn stills_are_exact_with_and_without_a_hidden_lead_in() {
        let Some(ff) = crate::ffmpeg::find() else { return };
        let dir = std::env::temp_dir().join(format!("mp4s-stills-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = fixture(&ff, &dir);
        check_frames(&ff, &src);
        let cut = dir.join("cut.mp4");
        let mut r = std::io::BufReader::new(std::fs::File::open(&src).unwrap());
        let mut w = std::io::BufWriter::new(std::fs::File::create(&cut).unwrap());
        crate::mp4::trim(&mut r, &mut w, Duration::from_secs_f64(0.5), Duration::from_secs_f64(3.5)).unwrap();
        drop(w);
        check_frames(&ff, &cut);
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
        let peaks = load_peaks(&Source::new(p.clone(), &i), Arc::new(move || drop(tx.lock().unwrap().send(()))));
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
