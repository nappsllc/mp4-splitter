//! Put a downloaded, verified update in place: the running exe is renamed aside,
//! the new one moved in, and nothing is restarted until the user says so. A new exe
//! that won't run is never swapped in; one that won't start after the swap is swapped
//! back. As Rustshot's `update_install.rs` (Apache-2.0), for this app's one kind of
//! self-updating install, the plain Windows exe.
#![cfg_attr(not(windows), allow(dead_code))]

use crate::update;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Set for a restarted app: the PID of the old one, whose exe can't be deleted
/// until it has gone.
pub const WAIT_PID_VAR: &str = "MP4_SPLITTER_WAIT_PID";
/// Longest a restarted app waits for the old one.
const WAIT_SECS: u64 = 10;
/// How long a new exe gets to answer `--version`.
const PROBE_SECS: u64 = 20;
/// A restarted app still running after this has started.
const SETTLE: Duration = Duration::from_millis(1000);
/// A leftover download older than this is no other window's.
const STALE: Duration = Duration::from_secs(3600);

/// The user-private download folder: `%LOCALAPPDATA%\mp4-splitter\update`.
pub fn update_dir() -> PathBuf {
    update_dir_from(|k| std::env::var_os(k))
}

fn update_dir_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    match env("LOCALAPPDATA").filter(|v| !v.is_empty()).map(PathBuf::from).filter(|p| p.is_absolute()) {
        Some(b) => b.join("mp4-splitter").join("update"),
        None => std::env::temp_dir().join("mp4-splitter-update"),
    }
}

/// `<path><suffix>` (e.g. `mp4-splitter.exe.old`).
fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// A file name we put in the update folder: a release's exe or its sums.
fn is_our_file(name: &str) -> bool {
    if name == update::SUMS_NAME {
        return true;
    }
    name.strip_prefix("mp4-splitter-").and_then(|r| r.strip_suffix(update::ASSET_SUFFIX)).is_some_and(|v| update::parse_version(v).is_some())
}

/// What an earlier update left: `<exe>.old`, `<exe>.new`, and our old downloads
/// (another window may be downloading: not the ones younger than `stale`). Anything
/// still in use stays for next time. Other files are never touched.
fn cleanup(dir: &Path, exe: Option<&Path>, stale: Duration) {
    if let Some(e) = exe {
        let _ = std::fs::remove_file(sibling(e, ".old"));
        let _ = std::fs::remove_file(sibling(e, ".new"));
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(meta) = std::fs::symlink_metadata(e.path()) else { continue };
        let old = meta.modified().ok().and_then(|m| m.elapsed().ok()).is_some_and(|age| age >= stale);
        if meta.is_file() && old && e.file_name().to_str().is_some_and(is_our_file) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

/// At start: tidy up on a thread of its own, once the old app (if this is a
/// restart) has gone.
pub fn cleanup_previous() {
    if cfg!(test) || !cfg!(windows) || update::is_msix() {
        return;
    }
    let wait = std::env::var(WAIT_PID_VAR).ok().and_then(|s| parse_wait_pid(&s, std::process::id()));
    std::thread::spawn(move || {
        if let Some(pid) = wait {
            wait_pid(pid);
        }
        cleanup(&update_dir(), std::env::current_exe().ok().as_deref(), STALE);
    });
}

/// The PID in the variable, if usable: a plain positive number that isn't our own.
fn parse_wait_pid(s: &str, own: u32) -> Option<u32> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let pid: u32 = s.parse().ok()?;
    (pid != 0 && pid != own).then_some(pid)
}

#[cfg(windows)]
fn wait_pid(pid: u32) {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject};
    // SAFETY: the handle is checked and closed here.
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) {
            WaitForSingleObject(h, (WAIT_SECS * 1000) as u32);
            let _ = CloseHandle(h);
        }
    }
}

#[cfg(not(windows))]
fn wait_pid(_pid: u32) {
    let _ = WAIT_SECS;
}

/// Swap `new` in for `current`: `current` to `current.old`, `new` to `current`. If
/// the second rename fails the first is undone. Windows allows renaming a running exe.
fn swap_with(current: &Path, new: &Path, rename: &dyn Fn(&Path, &Path) -> std::io::Result<()>) -> Result<(), String> {
    let old = sibling(current, ".old");
    match std::fs::remove_file(&old) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("remove {}: {e}", old.display())),
        _ => {}
    }
    rename(current, &old).map_err(|e| format!("rename {}: {e}", current.display()))?;
    if let Err(e) = rename(new, current) {
        return Err(match rename(&old, current) {
            Ok(()) => format!("install {}: {e}", current.display()),
            Err(e2) => format!("install {}: {e}; restoring it from {} also failed: {e2}", current.display(), old.display()),
        });
    }
    Ok(())
}

/// Undo a swap: `current.old` back to `current`; what was swapped in goes.
fn swap_back(current: &Path) -> Result<(), String> {
    let old = sibling(current, ".old");
    let bad = sibling(current, ".new");
    let _ = std::fs::remove_file(&bad);
    std::fs::rename(current, &bad).map_err(|e| format!("rename {}: {e}", current.display()))?;
    if let Err(e) = std::fs::rename(&old, current) {
        return Err(format!("restoring {} from {} failed: {e}", current.display(), old.display()));
    }
    let _ = std::fs::remove_file(&bad);
    Ok(())
}

/// Copy into a file that must not exist yet.
fn copy_exclusive(src: &Path, dst: &Path) -> Result<(), String> {
    let mut out = std::fs::OpenOptions::new().write(true).create_new(true).open(dst).map_err(|e| format!("create {}: {e}", dst.display()))?;
    let mut inp = std::fs::File::open(src).map_err(|e| format!("open {}: {e}", src.display()))?;
    std::io::copy(&mut inp, &mut out).map_err(|e| format!("write {}: {e}", dst.display()))?;
    out.sync_all().map_err(|e| format!("write {}: {e}", dst.display()))
}

/// Copy `src` to a fresh `<current>.new`, check its SHA-256, see that it `runs`, and
/// swap it in. On any error `current` is as it was and `.new` is gone.
fn replace_exe(current: &Path, src: &Path, sha256: [u8; 32], runs: &dyn Fn(&Path) -> Result<(), String>) -> Result<(), String> {
    let new = sibling(current, ".new");
    let _ = std::fs::remove_file(&new);
    let r = (|| {
        copy_exclusive(src, &new)?;
        update::verify_file(&new, sha256)?;
        runs(&new)?;
        swap_with(current, &new, &|a, b| std::fs::rename(a, b))
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(&new);
    }
    r
}

/// Does this exe run, and is it the version it should be? It is asked its
/// `--version`, which opens no window.
fn answers_version(exe: &Path, version: &str) -> Result<(), String> {
    let bad = || format!("the downloaded program did not start properly as version {version}; nothing was changed");
    let mut child =
        Command::new(exe).arg("--version").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().map_err(|e| format!("{}: {e}", bad()))?;
    let end = Instant::now() + Duration::from_secs(PROBE_SECS);
    loop {
        if child.try_wait().map_err(|e| e.to_string())?.is_some() {
            break;
        }
        if Instant::now() >= end {
            let _ = child.kill();
            let _ = child.wait();
            return Err(bad());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let said = String::from_utf8_lossy(&out.stdout);
    if out.status.success() && said.trim() == format!("mp4-splitter {version}") { Ok(()) } else { Err(bad()) }
}

/// Download and verify `rel`'s exe, then swap it in for the running one.
/// `progress` returning false cancels. The running exe's path comes back.
#[cfg(windows)]
pub fn install(rel: &update::Release, progress: &dyn Fn(u64, Option<u64>) -> bool) -> Result<PathBuf, String> {
    let kind = update::detect_install();
    let asset = update::pick_asset(kind, rel).ok_or_else(|| format!("this release has no {}", update::windows_asset_name(&rel.version)))?;
    let exe = std::env::current_exe().map_err(|e| format!("can't find this program: {e}"))?;
    let (file, sha) = update::fetch_verified(rel, asset, &update_dir(), progress, &crate::http::download_to)?;
    let r = replace_exe(&exe, &file, sha, &|new| answers_version(new, &rel.version));
    let _ = std::fs::remove_file(&file);
    r.map(|()| exe)
}

/// Elsewhere an update is a page in the browser.
#[cfg(not(windows))]
pub fn install(_: &update::Release, _: &dyn Fn(u64, Option<u64>) -> bool) -> Result<PathBuf, String> {
    Err("this system's updates come from the release page".into())
}

/// Start the swapped-in exe, opening `open` again. If it doesn't start, or stops
/// straight away, the swap is undone and the old one is what's there again.
pub fn restart(exe: &Path, open: Option<&Path>) -> Result<(), String> {
    restart_with(exe, &|| {
        let mut c = Command::new(exe);
        c.env(WAIT_PID_VAR, std::process::id().to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        if let Some(p) = open {
            c.arg(p);
        }
        let mut child = c.spawn().map_err(|e| format!("cannot start {}: {e}", exe.display()))?;
        std::thread::sleep(SETTLE);
        match child.try_wait() {
            Ok(None) => Ok(()),
            Ok(Some(st)) => Err(format!("the new version stopped straight away ({st})")),
            Err(e) => Err(e.to_string()),
        }
    })
}

fn restart_with(exe: &Path, start: &dyn Fn() -> Result<(), String>) -> Result<(), String> {
    start().map_err(|e| match swap_back(exe) {
        Ok(()) => format!("{e}; the previous version was put back"),
        Err(e2) => format!("{e}; {e2}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sha256::digest;

    /// A fresh temp dir, removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("mp4-splitter-inst-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn read(p: &Path) -> Vec<u8> {
        std::fs::read(p).unwrap()
    }

    /// `current`, holding "old", and a download holding `new`.
    fn setup(t: &Tmp, new: &[u8]) -> (PathBuf, PathBuf) {
        let (cur, src) = (t.0.join("app.exe"), t.0.join("download.bin"));
        std::fs::write(&cur, b"old").unwrap();
        std::fs::write(&src, new).unwrap();
        (cur, src)
    }

    #[test]
    fn the_swap_keeps_the_old_exe_aside() {
        let t = Tmp::new("swap");
        let (cur, src) = setup(&t, b"new build");
        std::fs::write(sibling(&cur, ".old"), b"stale").unwrap();
        replace_exe(&cur, &src, digest(b"new build"), &|_| Ok(())).unwrap();
        assert_eq!(read(&cur), b"new build");
        assert_eq!(read(&sibling(&cur, ".old")), b"old");
        assert!(!sibling(&cur, ".new").exists());
    }

    #[test]
    fn a_tampered_file_changes_nothing() {
        let t = Tmp::new("tamper");
        let (cur, src) = setup(&t, b"tampered");
        let err = replace_exe(&cur, &src, digest(b"new build"), &|_| panic!("must not run it")).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert_eq!(read(&cur), b"old");
        assert!(!sibling(&cur, ".new").exists() && !sibling(&cur, ".old").exists());
    }

    #[test]
    fn a_new_exe_that_does_not_run_changes_nothing() {
        let t = Tmp::new("norun");
        let (cur, src) = setup(&t, b"new build");
        let err = replace_exe(&cur, &src, digest(b"new build"), &|_| Err("won't run".into())).unwrap_err();
        assert_eq!(err, "won't run");
        assert_eq!(read(&cur), b"old");
        assert!(!sibling(&cur, ".new").exists() && !sibling(&cur, ".old").exists());
    }

    #[test]
    fn a_stale_new_file_is_not_trusted() {
        let t = Tmp::new("stale");
        let (cur, src) = setup(&t, b"good");
        std::fs::write(sibling(&cur, ".new"), b"planted").unwrap();
        replace_exe(&cur, &src, digest(b"good"), &|new| {
            assert_eq!(read(new), b"good");
            Ok(())
        })
        .unwrap();
        assert_eq!(read(&cur), b"good");
    }

    #[test]
    fn a_failed_second_rename_rolls_back() {
        let t = Tmp::new("back");
        let cur = t.0.join("app.exe");
        std::fs::write(&cur, b"old").unwrap();
        let err = swap_with(&cur, &t.0.join("missing"), &|a, b| std::fs::rename(a, b)).unwrap_err();
        assert!(err.starts_with("install "), "{err}");
        assert_eq!(read(&cur), b"old");
        assert!(!sibling(&cur, ".old").exists());
    }

    #[test]
    fn a_failed_rollback_says_where_the_old_exe_is() {
        let t = Tmp::new("back2");
        let (cur, new) = (t.0.join("app.exe"), t.0.join("n"));
        std::fs::write(&cur, b"old").unwrap();
        std::fs::write(&new, b"new").unwrap();
        let calls = std::cell::Cell::new(0);
        let rename = |a: &Path, b: &Path| {
            calls.set(calls.get() + 1);
            if calls.get() == 1 { std::fs::rename(a, b) } else { Err(std::io::Error::other("injected")) }
        };
        let err = swap_with(&cur, &new, &rename).unwrap_err();
        assert!(err.contains("also failed: injected") && err.contains("app.exe.old"), "{err}");
        assert_eq!(read(&sibling(&cur, ".old")), b"old");
    }

    #[test]
    fn a_new_version_that_fails_to_start_is_swapped_back() {
        let t = Tmp::new("restart");
        let (cur, src) = setup(&t, b"new build");
        replace_exe(&cur, &src, digest(b"new build"), &|_| Ok(())).unwrap();
        restart_with(&cur, &|| Ok(())).unwrap();
        assert_eq!(read(&cur), b"new build", "started: it stays");
        let err = restart_with(&cur, &|| Err("stopped straight away".into())).unwrap_err();
        assert!(err.contains("put back"), "{err}");
        assert_eq!(read(&cur), b"old");
        assert!(!sibling(&cur, ".old").exists() && !sibling(&cur, ".new").exists());
    }

    /// The real probe: what isn't a program doesn't run, and the test program
    /// itself runs without answering as `mp4-splitter`.
    #[test]
    fn the_probe_wants_the_right_answer() {
        let t = Tmp::new("probe");
        let junk = t.0.join("junk.exe.new");
        std::fs::write(&junk, b"not a program").unwrap();
        assert!(answers_version(&junk, "0.2.0").is_err());
        assert!(answers_version(&t.0.join("absent.exe"), "0.2.0").is_err());
    }

    #[test]
    fn cleanup_removes_only_ours() {
        let t = Tmp::new("clean");
        let dir = t.0.join("update");
        std::fs::create_dir_all(dir.join("mp4-splitter-1.0.0-windows-x86_64.exe")).unwrap();
        for f in [update::SUMS_NAME, "mp4-splitter-9.9.9-windows-x86_64.exe", "notes.txt", "mp4-splitter-x-windows-x86_64.exe"] {
            std::fs::write(dir.join(f), b"x").unwrap();
        }
        let exe = t.0.join("app.exe");
        for f in ["app.exe", "app.exe.old", "app.exe.new"] {
            std::fs::write(t.0.join(f), b"x").unwrap();
        }
        // Young downloads may be another window's.
        cleanup(&dir, Some(&exe), STALE);
        assert!(dir.join(update::SUMS_NAME).exists());
        assert!(exe.exists() && !sibling(&exe, ".old").exists() && !sibling(&exe, ".new").exists());
        cleanup(&dir, None, Duration::ZERO);
        let mut left: Vec<_> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().into_string().unwrap()).collect();
        left.sort();
        assert_eq!(left, ["mp4-splitter-1.0.0-windows-x86_64.exe", "mp4-splitter-x-windows-x86_64.exe", "notes.txt"]);
        cleanup(&t.0.join("absent"), None, STALE);
    }

    #[test]
    fn the_update_folder_is_the_users_own() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| OsString::from(v));
        #[cfg(windows)]
        assert_eq!(update_dir_from(env(&[("LOCALAPPDATA", r"C:\Users\u\AppData\Local")])), PathBuf::from(r"C:\Users\u\AppData\Local\mp4-splitter\update"));
        assert_eq!(update_dir_from(env(&[("LOCALAPPDATA", "relative")])), std::env::temp_dir().join("mp4-splitter-update"));
    }

    #[test]
    fn wait_pid_values() {
        assert_eq!(parse_wait_pid("1234", 99), Some(1234));
        for bad in ["", "0", "99", "-5", " 12", "abc", "99999999999"] {
            assert_eq!(parse_wait_pid(bad, 99), None, "{bad:?}");
        }
    }
}
