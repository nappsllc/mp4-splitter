//! Pictures and sound through ffmpeg (a child process): a still at any
//! time, a stream of pictures and of sound while playing, and the sound's
//! loudness for the waveform. Cutting never needs it (see [`crate::mp4`]).

use anyhow::{Context, Result, bail};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

/// Where ffmpeg is: `MP4_SPLITTER_FFMPEG`, beside our executable, on
/// `PATH`, or (macOS) where Homebrew and MacPorts put it.
pub fn find() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MP4_SPLITTER_FFMPEG").map(PathBuf::from) {
        return p.is_file().then_some(p);
    }
    let name = if cfg!(windows) { "ffmpeg.exe" } else { "ffmpeg" };
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(Path::to_path_buf)) {
        dirs.push(d);
    }
    if let Some(path) = std::env::var_os("PATH") {
        dirs.extend(std::env::split_paths(&path));
    }
    if cfg!(target_os = "macos") {
        dirs.extend(["/opt/homebrew/bin", "/usr/local/bin", "/opt/local/bin"].map(PathBuf::from));
    }
    dirs.into_iter().map(|d| d.join(name)).find(|p| p.is_file())
}

fn command(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    c.args(["-hide_banner", "-loglevel", "error", "-nostdin"]).stdin(Stdio::null()).stderr(Stdio::null()).stdout(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

fn secs(t: Duration) -> String {
    format!("{:.6}", t.as_secs_f64())
}

pub use crate::paint::Picture;

/// The picture shown at `t`, `w` × `h`. `fast`: the key frame before it
/// (thumbnails), else exactly the frame at `t`.
pub fn picture_at(exe: &Path, src: &Path, t: Duration, w: u32, h: u32, fast: bool) -> Result<Picture> {
    let mut c = command(exe);
    if fast {
        c.args(["-skip_frame", "nokey", "-noaccurate_seek"]);
    }
    c.args(["-ss", &secs(t)]).arg("-i").arg(src);
    let flags = if fast { "fast_bilinear" } else { "bicubic" };
    c.args(["-an", "-sn", "-dn", "-frames:v", "1", "-vf", &format!("scale={w}:{h}:flags={flags}"), "-f", "rawvideo", "-pix_fmt", "bgra", "-"]);
    let out = c.output().context("run ffmpeg")?;
    let want = (w * h * 4) as usize;
    if out.stdout.len() < want {
        // Near the end a key-frame-only decode can give nothing: decode
        // every frame instead.
        if fast {
            return picture_at(exe, src, t, w, h, false);
        }
        bail!("ffmpeg gave no picture at {:.3}s", t.as_secs_f64());
    }
    let mut bgra = out.stdout;
    bgra.truncate(want);
    Ok(Picture { w, h, bgra: std::sync::Arc::new(bgra) })
}

/// A child that is killed when dropped.
pub struct Proc(pub Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The pictures from `t` on, `w` × `h`, in presentation order, as they
/// are decoded (one RGBA buffer each).
pub fn video_from(exe: &Path, src: &Path, t: Duration, w: u32, h: u32) -> Result<(Proc, impl Read + Send + use<>)> {
    let mut c = command(exe);
    c.args(["-ss", &secs(t)]).arg("-i").arg(src);
    c.args(["-an", "-sn", "-dn", "-fps_mode", "passthrough", "-vf", &format!("scale={w}:{h}:flags=bilinear"), "-f", "rawvideo", "-pix_fmt", "bgra", "-"]);
    let mut child = c.spawn().context("run ffmpeg")?;
    let out = child.stdout.take().context("no ffmpeg output")?;
    Ok((Proc(child), out))
}

/// The sound from `t` on, as interleaved 32-bit floats at `rate` Hz in
/// `channels` channels.
pub fn audio_from(exe: &Path, src: &Path, t: Duration, rate: u32, channels: u16) -> Result<(Proc, impl Read + Send + use<>)> {
    let mut c = command(exe);
    c.args(["-ss", &secs(t)]).arg("-i").arg(src);
    c.args(["-vn", "-sn", "-dn", "-ac", &channels.to_string(), "-ar", &rate.to_string(), "-f", "f32le", "-"]);
    let mut child = c.spawn().context("run ffmpeg")?;
    let out = child.stdout.take().context("no ffmpeg output")?;
    Ok((Proc(child), out))
}

/// Buckets a second of [`peaks`].
pub const PEAKS_PER_SEC: u32 = 100;

/// The sound's loudest sample (0..1) in each 1/[`PEAKS_PER_SEC`] s, mono;
/// `each` is told how far it got.
pub fn peaks(exe: &Path, src: &Path, mut each: impl FnMut(&[f32])) -> Result<Vec<f32>> {
    const RATE: u32 = 8000;
    let per = (RATE / PEAKS_PER_SEC) as usize;
    let mut c = command(exe);
    c.arg("-i").arg(src).args(["-vn", "-sn", "-dn", "-ac", "1", "-ar", &RATE.to_string(), "-f", "s16le", "-"]);
    let mut child = Proc(c.spawn().context("run ffmpeg")?);
    let mut out = child.0.stdout.take().context("no ffmpeg output")?;
    let mut peaks = Vec::new();
    let mut buf = vec![0u8; per * 2 * PEAKS_PER_SEC as usize];
    let mut have = 0usize;
    loop {
        let n = out.read(&mut buf[have..])?;
        have += n;
        let whole = have / (per * 2) * (per * 2);
        for chunk in buf[..whole].chunks_exact(per * 2) {
            let m = chunk.chunks_exact(2).map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs()).max().unwrap_or(0);
            peaks.push(m as f32 / 32768.0);
        }
        buf.copy_within(whole..have, 0);
        have -= whole;
        if n == 0 {
            break;
        }
        each(&peaks);
    }
    Ok(peaks)
}
