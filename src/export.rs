//! Writing the pieces: each is a lossless copy ([`crate::mp4::trim`]) to a
//! `.part` file, renamed once it is complete.

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
        let part = out.with_extension(format!("{}.part", out.extension().and_then(|e| e.to_str()).unwrap_or("mp4")));
        let r = (|| -> Result<()> {
            let mut f = BufReader::new(File::open(src).with_context(|| format!("open {}", src.display()))?);
            let mut w = BufWriter::new(File::create(&part).with_context(|| format!("create {}", part.display()))?);
            crate::mp4::trim(&mut f, &mut w, a, b)?;
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
            drop(file);
            std::fs::rename(&part, out).with_context(|| format!("rename to {}", out.display()))?;
            Ok(())
        })();
        if let Err(e) = r {
            let _ = std::fs::remove_file(&part);
            return Err(e.context(format!("piece {}", i + 1)));
        }
        written.push(out.clone());
        each(i + 1);
    }
    Ok(written)
}

/// [`write_pieces`] on a thread; `wake` after each piece.
pub fn start(src: PathBuf, pieces: Vec<(Duration, Duration)>, outs: Vec<PathBuf>, wake: crate::player::Wake) -> Arc<Mutex<Progress>> {
    let p = Arc::new(Mutex::new(Progress { done: 0, total: pieces.len(), result: None }));
    let p2 = p.clone();
    std::thread::spawn(move || {
        let r = write_pieces(&src, &pieces, &outs, |n| {
            p2.lock().unwrap_or_else(|e| e.into_inner()).done = n;
            wake();
        });
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
