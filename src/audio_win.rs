//! Sound out through WASAPI (shared mode, 48 kHz stereo floats, the engine
//! converts to the device; as Rustshot's `src/rec/win/wasapi.rs` opens its
//! streams, Apache-2.0). One thread plays whatever is queued and counts what
//! played, which is the clock pictures follow while playing.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, IAudioClient, IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVE_FORMAT_PCM, WAVEFORMATEX, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, CoCreateInstance};

const RATE: u32 = 48_000;
const CHANNELS: u16 = 2;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const AUTOCONVERTPCM: u32 = 0x8000_0000;
const SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;
/// The engine's buffer: 80 ms, in 100 ns.
const BUFFER: i64 = 800_000;
const POLL: Duration = Duration::from_millis(8);

/// What the output thread and the feeding thread share.
pub struct Shared {
    pub queue: Mutex<VecDeque<f32>>,
    /// Signalled when the queue has room again.
    pub room: Condvar,
    /// Frames (one sample per channel) played since the last [`Output::reset`].
    pub played: AtomicU64,
    /// Frames of the queue handed to the engine since the last reset.
    written: AtomicU64,
    /// Which playback the queued sound belongs to.
    pub generation: AtomicU64,
    pub muted: AtomicBool,
    stop: AtomicBool,
}

pub struct Output {
    pub rate: u32,
    pub channels: u16,
    pub shared: Arc<Shared>,
}

impl Drop for Output {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
    }
}

/// The default device's client and render service, started.
fn open_device() -> windows::core::Result<(IAudioClient, IAudioRenderClient, u32)> {
    // SAFETY: COM calls on a thread in the MTA; `fmt` outlives Initialize.
    unsafe {
        let e: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = e.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let block = CHANNELS * 4;
        let fmt = WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
            nChannels: CHANNELS,
            nSamplesPerSec: RATE,
            nAvgBytesPerSec: RATE * block as u32,
            nBlockAlign: block,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let _ = WAVE_FORMAT_PCM;
        client.Initialize(AUDCLNT_SHAREMODE_SHARED, AUTOCONVERTPCM | SRC_DEFAULT_QUALITY, BUFFER, 0, &fmt, None)?;
        let render: IAudioRenderClient = client.GetService()?;
        let frames = client.GetBufferSize()?;
        client.Start()?;
        Ok((client, render, frames))
    }
}

fn play(sh: &Shared) {
    crate::mf::com_init();
    while !sh.stop.load(Ordering::SeqCst) {
        let Ok((client, render, size)) = open_device() else {
            // No device (yet): try again, and let the pictures run on the wall clock's pace.
            sh.played.store(sh.written.load(Ordering::SeqCst), Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(500));
            continue;
        };
        // SAFETY: the buffer the engine gives is `room` frames of 8 bytes, ours until released.
        unsafe {
            loop {
                if sh.stop.load(Ordering::SeqCst) {
                    let _ = client.Stop();
                    return;
                }
                // An error here: the device went away, open the new default.
                let Ok(padding) = client.GetCurrentPadding() else { break };
                let room = size.saturating_sub(padding);
                if room > 0 {
                    let Ok(data) = render.GetBuffer(room) else { break };
                    let out = std::slice::from_raw_parts_mut(data.cast::<f32>(), room as usize * CHANNELS as usize);
                    let muted = sh.muted.load(Ordering::Relaxed);
                    let mut real = 0usize;
                    if let Ok(mut q) = sh.queue.lock() {
                        real = q.len().min(out.len()) / CHANNELS as usize * CHANNELS as usize;
                        for (o, v) in out.iter_mut().zip(q.drain(..real)) {
                            *o = if muted { 0.0 } else { v };
                        }
                    }
                    out[real..].fill(0.0);
                    let _ = render.ReleaseBuffer(room, 0);
                    if real > 0 {
                        sh.written.fetch_add(real as u64 / CHANNELS as u64, Ordering::SeqCst);
                        sh.room.notify_all();
                    }
                }
                // What was handed over, less what the engine still holds.
                let held = client.GetCurrentPadding().unwrap_or(0) as u64;
                sh.played.store(sh.written.load(Ordering::SeqCst).saturating_sub(held), Ordering::SeqCst);
                std::thread::sleep(POLL);
            }
        }
    }
}

impl Output {
    /// The output: it follows the default device, and waits for one if there is none.
    pub fn open() -> Option<Output> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(VecDeque::new()),
            room: Condvar::new(),
            played: AtomicU64::new(0),
            written: AtomicU64::new(0),
            generation: AtomicU64::new(0),
            muted: AtomicBool::new(false),
            stop: AtomicBool::new(false),
        });
        let sh = shared.clone();
        std::thread::Builder::new().name("sound".into()).spawn(move || play(&sh)).ok()?;
        Some(Output { rate: RATE, channels: CHANNELS, shared })
    }

    /// Drop what is queued and start counting again; the new generation.
    pub fn reset(&self) -> u64 {
        let g = self.shared.generation.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut q) = self.shared.queue.lock() {
            q.clear();
        }
        self.shared.written.store(0, Ordering::SeqCst);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_starts_the_count_again() {
        let Some(o) = Output::open() else { return };
        let g = o.reset();
        assert_eq!(o.reset(), g + 1);
        assert_eq!(o.played(), Duration::ZERO);
        o.set_muted(true);
        assert!(o.shared.muted.load(Ordering::SeqCst));
    }
}
