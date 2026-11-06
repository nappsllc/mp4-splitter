//! The background work behind the window: playback (pictures and sound),
//! the still shown while paused, the timeline's thumbnails and the
//! waveform. Each runs on its own threads and wakes the window when it
//! has something new.

use crate::audio::{self, Output};
use crate::ffmpeg::{self, Picture, Proc};
use crate::shell::Wake;
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub use crate::ffmpeg::PEAKS_PER_SEC;

/// What to say when pictures and sound can't be decoded here.
pub fn no_decoder() -> (&'static str, &'static str, &'static str) {
    (tr!("msg.no_ffmpeg"), tr!("msg.no_ffmpeg_hint"), tr!("msg.no_ffmpeg_preview"))
}

/// Can pictures and sound be decoded here?
pub fn available() -> bool {
    ffmpeg::find().is_some()
}

/// The file being worked on, as the workers need it.
#[derive(Clone)]
pub struct Source {
    pub available: bool,
    ffmpeg: Option<PathBuf>,
    pub path: PathBuf,
    /// Presentation times of the frames (sorted), for the stream's timing.
    pub frames: Arc<Vec<Duration>>,
    pub has_audio: bool,
}

impl Source {
    pub fn new(path: PathBuf, info: &crate::mp4::Info) -> Source {
        let ffmpeg = ffmpeg::find();
        Source { available: ffmpeg.is_some(), ffmpeg, path, frames: Arc::new(info.frames.clone()), has_audio: info.has_audio }
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
    _video: Option<Proc>,
    _audio: Option<Proc>,
    rx: Option<Receiver<Frame>>,
    next: Option<Frame>,
    generation: u64,
    /// No more pictures will come.
    pub video_done: bool,
}

impl Playback {
    pub fn start(src: &Source, t0: Duration, size: (u32, u32), out: Option<&Output>, wake: Wake) -> Playback {
        let generation = out.map(|o| o.reset()).unwrap_or(0);
        let mut pb =
            Playback { t0, started: Instant::now(), audio_clock: false, _video: None, _audio: None, rx: None, next: None, generation, video_done: true };
        let Some(exe) = src.ffmpeg.clone() else { return pb };
        let (w, h) = size;
        if w > 0 && h > 0 && !src.frames.is_empty() {
            if let Ok((proc, mut pipe)) = ffmpeg::video_from(&exe, &src.path, t0, w, h) {
                let (tx, rx): (SyncSender<Frame>, _) = sync_channel(3);
                let frames = src.frames.clone();
                let wake2 = wake.clone();
                std::thread::spawn(move || {
                    let list = &frames;
                    let mut i = crate::split::frame_index(list, t0);
                    loop {
                        let mut buf = vec![0u8; (w * h * 4) as usize];
                        if pipe.read_exact(&mut buf).is_err() {
                            break;
                        }
                        let t = list.get(i).copied().unwrap_or_else(|| list.last().copied().unwrap_or_default());
                        i += 1;
                        if tx.send(Frame { t, pic: Picture { w, h, bgra: Arc::new(buf) } }).is_err() {
                            break;
                        }
                        wake2();
                    }
                    wake2();
                });
                pb._video = Some(proc);
                pb.rx = Some(rx);
                pb.video_done = false;
            }
        }
        if let (true, Some(out)) = (src.has_audio, out) {
            if let Ok((proc, mut pipe)) = ffmpeg::audio_from(&exe, &src.path, t0, out.rate, out.channels) {
                let sh = out.shared.clone();
                let cap = audio::max_queued(out.rate, out.channels);
                let generation = pb.generation;
                std::thread::spawn(move || {
                    let mut buf = vec![0u8; 4096 * 4];
                    let mut carry = Vec::<u8>::new();
                    loop {
                        let n = match pipe.read(&mut buf) {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        carry.extend_from_slice(&buf[..n]);
                        let whole = carry.len() / 4 * 4;
                        let samples: Vec<f32> = carry[..whole].chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
                        carry.drain(..whole);
                        let Ok(q) = sh.queue.lock() else { break };
                        let mut q = sh
                            .room
                            .wait_while(q, |q| q.len() + samples.len() > cap && sh.generation.load(Ordering::SeqCst) == generation)
                            .unwrap_or_else(|e| e.into_inner());
                        if sh.generation.load(Ordering::SeqCst) != generation {
                            break;
                        }
                        q.extend(samples);
                    }
                });
                pb._audio = Some(proc);
                pb.audio_clock = true;
            }
        }
        pb
    }

    /// Where playback is now.
    pub fn clock(&self, out: Option<&Output>) -> Duration {
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

// --- the still while paused ---------------------------------------------------

struct StillJob {
    t: Duration,
    size: (u32, u32),
    seq: u64,
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
            let Some(exe) = src.ffmpeg.clone() else { return };
            loop {
                let Some(job) = j2.upgrade() else { return };
                let next = {
                    let g = job.0.lock().unwrap_or_else(|e| e.into_inner());
                    let mut g = job.1.wait_timeout_while(g, Duration::from_millis(500), |j| j.is_none()).unwrap_or_else(|e| e.into_inner()).0;
                    g.take()
                };
                drop(job);
                let Some(StillJob { t, size, seq }) = next else { continue };
                if let Ok(p) = ffmpeg::picture_at(&exe, &src.path, t, size.0, size.1, false) {
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
        let workers = std::thread::available_parallelism().map_or(2, |n| n.get().clamp(2, 4));
        for _ in 0..workers {
            let (c, q, src, wake) = (Arc::downgrade(&cache), Arc::downgrade(&queue), src.clone(), wake.clone());
            std::thread::spawn(move || {
                let Some(exe) = src.ffmpeg.clone() else { return };
                loop {
                    let Some(queue) = q.upgrade() else { return };
                    let key = {
                        let g = queue.0.lock().unwrap_or_else(|e| e.into_inner());
                        let mut g = queue.1.wait_timeout_while(g, Duration::from_millis(500), |q| q.0.is_empty()).unwrap_or_else(|e| e.into_inner()).0;
                        g.0.pop()
                    };
                    drop(queue);
                    let Some(key) = key else { continue };
                    let pic = ffmpeg::picture_at(&exe, &src.path, Duration::from_millis(key), size.0, size.1, true).ok();
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
        }
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

/// The waveform, filled in as ffmpeg reads the file.
pub fn load_peaks(src: &Source, wake: Wake) -> Arc<Mutex<Vec<f32>>> {
    let out = Arc::new(Mutex::new(Vec::new()));
    if let (Some(exe), true) = (src.ffmpeg.clone(), src.has_audio) {
        let (o, path) = (Arc::downgrade(&out), src.path.clone());
        std::thread::spawn(move || {
            let mut last = Instant::now();
            let r = ffmpeg::peaks(&exe, &path, |p| {
                if last.elapsed() > Duration::from_millis(150) {
                    last = Instant::now();
                    if let Some(o) = o.upgrade() {
                        *o.lock().unwrap_or_else(|e| e.into_inner()) = p.to_vec();
                        wake();
                    }
                }
            });
            if let (Ok(p), Some(o)) = (r, o.upgrade()) {
                *o.lock().unwrap_or_else(|e| e.into_inner()) = p;
                wake();
            }
        });
    }
    out
}
