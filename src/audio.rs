//! Sound out (cpal): one stream that plays whatever is queued and counts
//! what it played, which is the clock pictures follow while playing.

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// What the stream and the feeding thread share.
pub struct Shared {
    pub queue: Mutex<VecDeque<f32>>,
    /// Signalled when the queue has room again.
    pub room: Condvar,
    /// Frames (one sample per channel) played since the last [`Output::reset`].
    pub played: AtomicU64,
    /// Which playback the queued sound belongs to.
    pub generation: AtomicU64,
    pub muted: AtomicBool,
}

pub struct Output {
    _stream: cpal::Stream,
    pub rate: u32,
    pub channels: u16,
    pub shared: Arc<Shared>,
}

impl Output {
    /// The default output device, or `None` without one.
    pub fn open() -> Option<Output> {
        let host = cpal::default_host();
        let dev = host.default_output_device()?;
        let cfg = dev.default_output_config().ok()?;
        let (rate, channels, fmt) = (cfg.sample_rate().0, cfg.channels(), cfg.sample_format());
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            room: Condvar::new(),
            played: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            muted: AtomicBool::new(false),
        });
        let config: cpal::StreamConfig = cfg.into();
        let err = |e| eprintln!("sound: {e}");
        let stream = match fmt {
            cpal::SampleFormat::F32 => {
                let sh = shared.clone();
                dev.build_output_stream(&config, move |out: &mut [f32], _| fill(&sh, channels, out, |v| v), err, None)
            }
            cpal::SampleFormat::I16 => {
                let sh = shared.clone();
                dev.build_output_stream(&config, move |out: &mut [i16], _| fill(&sh, channels, out, |v| (v.clamp(-1.0, 1.0) * 32767.0) as i16), err, None)
            }
            cpal::SampleFormat::U16 => {
                let sh = shared.clone();
                dev.build_output_stream(
                    &config,
                    move |out: &mut [u16], _| fill(&sh, channels, out, |v| ((v.clamp(-1.0, 1.0) * 32767.0) as i32 + 32768) as u16),
                    err,
                    None,
                )
            }
            _ => return None,
        }
        .ok()?;
        stream.play().ok()?;
        Some(Output { _stream: stream, rate, channels, shared })
    }

    /// Drop what is queued and start counting again; the new generation.
    pub fn reset(&self) -> u64 {
        let g = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut q) = self.shared.queue.lock() {
            q.clear();
        }
        self.shared.played.store(0, Ordering::SeqCst);
        self.shared.room.notify_all();
        g
    }

    /// How long the sound played since [`Output::reset`].
    pub fn played(&self) -> Duration {
        Duration::from_nanos((self.shared.played.load(Ordering::SeqCst) as u128 * 1_000_000_000 / self.rate.max(1) as u128) as u64)
    }

    pub fn set_muted(&self, m: bool) {
        self.shared.muted.store(m, Ordering::SeqCst);
    }
}

/// The most queued (a second): the feeder waits for room past it.
pub fn max_queued(rate: u32, channels: u16) -> usize {
    rate as usize * channels as usize
}

fn fill<T: Copy>(sh: &Shared, channels: u16, out: &mut [T], conv: impl Fn(f32) -> T) {
    let muted = sh.muted.load(Ordering::Relaxed);
    let mut played = 0u64;
    if let Ok(mut q) = sh.queue.lock() {
        // Whole frames only, so the channels never swap.
        let ch = channels.max(1) as usize;
        let n = (q.len().min(out.len()) / ch) * ch;
        for (o, v) in out.iter_mut().zip(q.drain(..n)) {
            *o = conv(if muted { 0.0 } else { v });
        }
        for o in &mut out[n..] {
            *o = conv(0.0);
        }
        played = (n / ch) as u64;
    } else {
        for o in out.iter_mut() {
            *o = conv(0.0);
        }
    }
    if played > 0 {
        sh.played.fetch_add(played, Ordering::SeqCst);
        sh.room.notify_all();
    }
}
