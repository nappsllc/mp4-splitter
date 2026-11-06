//! Writing the pieces: each is a lossless copy ([`crate::mp4::trim`]), or
//! they are joined into one ([`crate::mp4::join`]), to a `.part` file,
//! renamed once it is complete.

use anyhow::{Context, Result};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct Progress {
    pub done: usize,
    pub total: usize,
    /// Set once finished: the files written, or what went wrong.
    pub result: Option<Result<Vec<PathBuf>, String>>,
}

/// Write `pieces` of `src` to `outs` (one path each).
pub fn write_pieces(src: &Path, pieces: &[(Duration, Duration)], outs: &[PathBuf], mut each: impl FnMut(usize)) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (i, (&(a, b), out)) in pieces.iter().zip(outs).enumerate() {
        write_one(src, out, |f, w| crate::mp4::trim(f, w, a, b).map(|_| ())).map_err(|e| e.context(format!("piece {}", i + 1)))?;
        written.push(out.clone());
        each(i + 1);
    }
    Ok(written)
}

/// Join `pieces` of `src` into the one file `out`.
pub fn write_joined(src: &Path, pieces: &[(Duration, Duration)], out: &Path) -> Result<PathBuf> {
    write_one(src, out, |f, w| crate::mp4::join(f, w, pieces).map(|_| ()))?;
    Ok(out.to_path_buf())
}

/// Have `write` copy from `src` to a `.part` file, renamed to `out` once it
/// is complete.
fn write_one(src: &Path, out: &Path, write: impl FnOnce(&mut BufReader<File>, &mut BufWriter<File>) -> Result<()>) -> Result<()> {
    let part = out.with_extension(format!("{}.part", out.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
    let r = (|| -> Result<()> {
        let mut f = BufReader::new(File::open(src).with_context(|| format!("open {}", src.display()))?);
        let mut w = BufWriter::new(File::create(&part).with_context(|| format!("create {}", part.display()))?);
        write(&mut f, &mut w)?;
        let file = w.into_inner().map_err(|e| e.into_error())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&part, out).with_context(|| format!("rename to {}", out.display()))?;
        Ok(())
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    r
}

/// [`write_pieces`] on a thread, or [`write_joined`] when `join` (`outs` then
/// holds the one file); `wake` after each file.
pub fn start(src: PathBuf, pieces: Vec<(Duration, Duration)>, outs: Vec<PathBuf>, join: bool, wake: crate::player::Wake) -> Arc<Mutex<Progress>> {
    let p = Arc::new(Mutex::new(Progress { done: 0, total: outs.len(), result: None }));
    let p2 = p.clone();
    std::thread::spawn(move || {
        let r = if join {
            let one = outs.first().map(PathBuf::as_path).unwrap_or(Path::new(""));
            write_joined(&src, &pieces, one).map(|f| vec![f])
        } else {
            write_pieces(&src, &pieces, &outs, |n| {
                p2.lock().unwrap_or_else(|e| e.into_inner()).done = n;
                wake();
            })
        };
        p2.lock().unwrap_or_else(|e| e.into_inner()).result = Some(r.map_err(|e| format!("{e:#}")));
        wake();
    });
    p
}

/// Show `dir` in the file manager.
pub fn reveal(path: &Path) {
    #[cfg(windows)]
    let r = std::process::Command::new("explorer").arg(format!("/select,{}", path.display())).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg("-R").arg(path).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(path.parent().unwrap_or(path)).spawn();
    if let Err(e) = r {
        eprintln!("show folder: {e}");
    }
}
