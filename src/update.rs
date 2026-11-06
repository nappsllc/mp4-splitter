//! Update check against GitHub releases, and what an in-app update needs: the
//! release's assets and notes, how this copy was installed, which asset updates
//! it, `SHA256SUMS.txt` and a verified download. As Rustshot's `update.rs`
//! (Apache-2.0), cut down to what this app ships: the Windows exe updates itself;
//! macOS, Linux and an exe in a folder it can't write get the release page; the
//! Microsoft Store build leaves it all to the Store.
#![cfg_attr(not(windows), allow(dead_code))]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Release {
    pub version: String,
    pub url: String,
    /// Release notes (markdown); see [`notes_lines`].
    pub notes: String,
    /// Downloadable files; only ones whose URL passes [`is_safe_download_url`].
    pub assets: Vec<Asset>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
}

const RELEASES_HOST: &str = "api.github.com";
const RELEASES_PATH: &str = "/repos/nappsllc/mp4-splitter/releases/latest";
pub const RELEASES_PREFIX: &str = "https://github.com/nappsllc/mp4-splitter/";
/// Release assets are only ever fetched from under this prefix.
pub const DOWNLOAD_PREFIX: &str = "https://github.com/nappsllc/mp4-splitter/releases/download/";
/// Hosts GitHub redirects asset downloads to.
const REDIRECT_HOSTS: &[&str] = &["objects.githubusercontent.com", "release-assets.githubusercontent.com"];
/// Name of the checksum asset the release workflow publishes.
pub const SUMS_NAME: &str = "SHA256SUMS.txt";

/// A release download slower than [`STALL_BYTES`] over [`STALL_SECS`] is
/// given up (a dead connection never hangs it).
pub const STALL_BYTES: u64 = 1024;
pub const STALL_SECS: u64 = 30;

/// A release download's whole-time limit: 60 s plus 4 KB/s of `max`
/// bytes (an unknown size: the most), at most 600 s.
pub fn download_time_limit(max: Option<u64>) -> std::time::Duration {
    const MOST: u64 = 600;
    let s = max.map_or(MOST, |m| (60 + m / 4096).min(MOST));
    std::time::Duration::from_secs(s)
}

/// The User-Agent of every request. It says which app and version asks, nothing else.
pub const USER_AGENT: &str = concat!("mp4-splitter/", env!("CARGO_PKG_VERSION"));
/// Error text of a download the progress callback cancelled.
pub const CANCELLED: &str = "cancelled";

/// No control characters or whitespace (argv injection, terminal escapes),
/// bounded length.
fn is_clean_url(u: &str) -> bool {
    !u.chars().any(|c| c.is_control() || c.is_whitespace()) && u.len() <= 512
}

/// Only release pages of this repo may be printed or opened: no other scheme,
/// host, control characters or whitespace (argv injection, terminal escapes).
fn is_safe_release_url(u: &str) -> bool {
    has_prefix(u, RELEASES_PREFIX) && is_clean_url(u)
}

/// `u` starts with `prefix` (one of this repo's), with the owner and repo
/// in any ASCII case: GitHub's are case-insensitive, and its API spells
/// them as the repo is named (as it did for Rustshot after a rename).
fn has_prefix(u: &str, prefix: &str) -> bool {
    let (site, repo) = ("https://github.com/".len(), "nappsllc/mp4-splitter/".len());
    let (u, p) = (u.as_bytes(), prefix.as_bytes());
    u.len() >= p.len()
        && u[..site] == p[..site]
        && u[site..site + repo].eq_ignore_ascii_case(&p[site..site + repo])
        && u[site + repo..p.len()] == p[site + repo..]
}

/// A release asset URL of this repo (the prefix in any ASCII case; no
/// traversal, backslashes or percent-escapes after it).
pub fn is_safe_download_url(u: &str) -> bool {
    has_prefix(u, DOWNLOAD_PREFIX)
        && is_clean_url(u)
        && !u.contains('\\')
        && !u[DOWNLOAD_PREFIX.len()..].contains(['%', '@'])
        && !u[DOWNLOAD_PREFIX.len()..].split(['/', '?', '#']).any(|seg| seg == ".." || seg == ".")
}

/// Hosts an asset download may be redirected to.
pub fn is_allowed_redirect_host(host: &str) -> bool {
    REDIRECT_HOSTS.iter().any(|h| host.eq_ignore_ascii_case(h))
}

/// Split an absolute `https://host/path` URL. None for any other scheme, a
/// port, userinfo, or an odd host. Signed asset-host redirect URLs run to
/// about 1 KB, hence the larger bound than `is_clean_url`.
pub fn split_https_url(u: &str) -> Option<(&str, &str)> {
    if u.chars().any(|c| c.is_control() || c.is_whitespace()) || u.len() > 4096 {
        return None;
    }
    let rest = u.strip_prefix("https://")?;
    let (host, path) = match rest.find(['/', '?', '#']) {
        Some(i) if rest.as_bytes()[i] == b'/' => (&rest[..i], &rest[i..]),
        Some(_) => return None,
        None => (rest, "/"),
    };
    let host_ok = !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    host_ok.then_some((host, path))
}

/// Whether a download may go to (or be redirected to) `url`: an asset URL of
/// this repo, or https on one of GitHub's asset hosts.
pub fn is_allowed_download_hop(url: &str) -> bool {
    is_safe_download_url(url) || split_https_url(url).is_some_and(|(host, _)| is_allowed_redirect_host(host))
}

/// Asset names become file names: plain ASCII, no separators or traversal.
fn is_safe_asset_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 128 && !n.starts_with('.') && n.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

/// Parse `x.y.z` with an optional leading `v`; anything else is not comparable.
pub fn parse_version(s: &str) -> Option<(u32, u32, u32)> {
    let s = s.strip_prefix('v').unwrap_or(s);
    let mut it = s.split('.');
    let mut next = || {
        let part = it.next()?;
        if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        part.parse::<u32>().ok()
    };
    let v = (next()?, next()?, next()?);
    if it.next().is_some() {
        return None;
    }
    Some(v)
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

// --- Minimal JSON walker ----------------------------------------------------
// Just enough to read top-level fields of an object and walk an array of
// objects, without a JSON crate. Values are `&str` slices starting at the
// value; strings are decoded on demand.

fn ws(s: &str) -> &str {
    s.trim_start_matches([' ', '\t', '\n', '\r'])
}

/// Decode the JSON string at the start of `s`; returns it and the rest.
fn read_string(s: &str) -> Option<(String, &str)> {
    let body = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = body.char_indices();
    fn hex4(chars: &mut std::str::CharIndices) -> Option<u32> {
        let mut v = 0;
        for _ in 0..4 {
            v = v * 16 + chars.next()?.1.to_digit(16)?;
        }
        Some(v)
    }
    while let Some((i, c)) = chars.next() {
        match c {
            '"' => return Some((out, &body[i + 1..])),
            '\\' => match chars.next()?.1 {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                c @ ('"' | '\\' | '/') => out.push(c),
                'u' => {
                    let hi = hex4(&mut chars)?;
                    let cp = if (0xD800..0xDC00).contains(&hi) {
                        let lo = (chars.next()?.1 == '\\' && chars.next()?.1 == 'u').then(|| hex4(&mut chars))??;
                        if !(0xDC00..0xE000).contains(&lo) {
                            return None;
                        }
                        0x10000 + ((hi - 0xD800) << 10) + (lo - 0xDC00)
                    } else {
                        hi
                    };
                    out.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                }
                _ => return None,
            },
            c if (c as u32) < 0x20 => return None,
            c => out.push(c),
        }
    }
    None
}

/// Skip one JSON value at the start of `s`; returns the rest.
fn skip_value(s: &str) -> Option<&str> {
    let s = ws(s);
    let b = s.as_bytes();
    match *b.first()? {
        b'"' => {
            let mut i = 1;
            while i < b.len() {
                match b[i] {
                    b'\\' => i += 2,
                    b'"' => return Some(&s[i + 1..]),
                    _ => i += 1,
                }
            }
            None
        }
        b'{' | b'[' => {
            let mut depth = 0usize;
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    b'"' => {
                        let rest = skip_value(&s[i..])?;
                        i = s.len() - rest.len();
                        continue;
                    }
                    b'{' | b'[' => depth += 1,
                    b'}' | b']' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(&s[i + 1..]);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        _ => {
            let end = s.find([',', '}', ']', ' ', '\t', '\n', '\r']).unwrap_or(s.len());
            (end > 0).then(|| &s[end..])
        }
    }
}

/// Top-level members of the object at the start of `s`: (key, value slice).
fn members(s: &str) -> Option<Vec<(String, &str)>> {
    let mut s = ws(ws(s).strip_prefix('{')?);
    let mut out = Vec::new();
    if s.starts_with('}') {
        return Some(out);
    }
    loop {
        let (key, rest) = read_string(s)?;
        let value = ws(ws(rest).strip_prefix(':')?);
        let rest = ws(skip_value(value)?);
        out.push((key, value));
        if let Some(r) = rest.strip_prefix(',') {
            s = ws(r);
        } else {
            rest.strip_prefix('}')?;
            return Some(out);
        }
    }
}

/// Elements of the array at the start of `s` (each slice starts at a value).
fn items(s: &str) -> Option<Vec<&str>> {
    let mut s = ws(ws(s).strip_prefix('[')?);
    let mut out = Vec::new();
    if s.starts_with(']') {
        return Some(out);
    }
    loop {
        let rest = ws(skip_value(s)?);
        out.push(s);
        if let Some(r) = rest.strip_prefix(',') {
            s = ws(r);
        } else {
            rest.strip_prefix(']')?;
            return Some(out);
        }
    }
}

fn field<'a>(m: &[(String, &'a str)], key: &str) -> Option<&'a str> {
    m.iter().find(|(k, _)| k == key).map(|(_, v)| *v)
}

fn str_field(m: &[(String, &str)], key: &str) -> Option<String> {
    read_string(field(m, key)?).map(|(s, _)| s)
}

fn bool_field(m: &[(String, &str)], key: &str) -> Option<bool> {
    let v = field(m, key)?;
    if v.starts_with("true") {
        Some(true)
    } else if v.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn u64_field(m: &[(String, &str)], key: &str) -> Option<u64> {
    let v = field(m, key)?;
    let end = v.find(|c: char| !c.is_ascii_digit()).unwrap_or(v.len());
    v[..end].parse().ok()
}

fn parse_asset(json: &str) -> Option<Asset> {
    let m = members(json)?;
    let name = str_field(&m, "name").filter(|n| is_safe_asset_name(n))?;
    let url = str_field(&m, "browser_download_url").filter(|u| is_safe_download_url(u))?;
    let size = u64_field(&m, "size").unwrap_or(0);
    Some(Asset { name, url, size })
}

/// Parse a `releases/latest` response. None for drafts, prereleases, missing
/// fields or a tag that is not `x.y.z`. Notes and assets are optional; assets
/// with an unexpected name or URL are dropped.
pub fn parse_latest(json: &str) -> Option<Release> {
    let m = members(json)?;
    if bool_field(&m, "draft") == Some(true) || bool_field(&m, "prerelease") == Some(true) {
        return None;
    }
    let tag = str_field(&m, "tag_name")?;
    parse_version(&tag)?;
    let url = str_field(&m, "html_url").filter(|u| is_safe_release_url(u))?;
    let notes = str_field(&m, "body").unwrap_or_default();
    let assets = field(&m, "assets").and_then(items).map(|v| v.into_iter().filter_map(parse_asset).collect()).unwrap_or_default();
    Some(Release { version: tag.strip_prefix('v').unwrap_or(&tag).to_string(), url, notes, assets })
}

// --- Release notes ----------------------------------------------------------

/// Inline markdown to text: links/images to their text, emphasis and code
/// markers removed, `<https://...>` autolinks unwrapped.
fn strip_inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let alnum = |j: Option<&char>| j.is_some_and(|c| c.is_alphanumeric());
    while i < c.len() {
        let ch = c[i];
        // [text](url) and ![alt](url)
        let bang = ch == '!' && c.get(i + 1) == Some(&'[');
        if ch == '[' || bang {
            let open = i + bang as usize;
            if let Some(close) = (open + 1..c.len()).find(|&j| c[j] == ']')
                && c.get(close + 1) == Some(&'(')
                && let Some(end) = (close + 2..c.len()).find(|&j| c[j] == ')')
            {
                let text: String = c[open + 1..close].iter().collect();
                out.push_str(&strip_inline(&text));
                i = end + 1;
                continue;
            }
        }
        if ch == '<'
            && let Some(end) = (i + 1..c.len()).find(|&j| c[j] == '>')
        {
            let inner: String = c[i + 1..end].iter().collect();
            if inner.starts_with("http://") || inner.starts_with("https://") {
                out.push_str(&inner);
                i = end + 1;
                continue;
            }
        }
        match ch {
            '`' => {}
            '*' | '_' | '~' if c.get(i + 1) == Some(&ch) => i += 1,
            '*' | '_' if !(alnum(i.checked_sub(1).and_then(|j| c.get(j))) && alnum(c.get(i + 1))) => {}
            _ => out.push(ch),
        }
        i += 1;
    }
    out
}

/// The first `max_lines` lines of release notes as plain text, each with whether
/// it was a markdown heading: headings, bullets, quotes, links, emphasis, code
/// fences and HTML comments stripped; runs of blank lines collapsed.
pub fn notes_lines(body: &str, max_lines: usize) -> Vec<(String, bool)> {
    let mut out: Vec<(String, bool)> = Vec::new();
    let mut in_comment = false;
    for raw in body.lines() {
        // Drop <!-- ... --> (possibly spanning lines).
        let mut line = String::new();
        let mut rest = raw;
        loop {
            if in_comment {
                match rest.find("-->") {
                    Some(i) => {
                        rest = &rest[i + 3..];
                        in_comment = false;
                    }
                    None => break,
                }
            } else {
                match rest.find("<!--") {
                    Some(i) => {
                        line.push_str(&rest[..i]);
                        rest = &rest[i + 4..];
                        in_comment = true;
                    }
                    None => {
                        line.push_str(rest);
                        break;
                    }
                }
            }
        }
        let mut t = line.trim();
        if t.starts_with("```") || t.starts_with("~~~") {
            continue;
        }
        if t.len() >= 3 && t.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) {
            t = ""; // horizontal rule
        }
        while let Some(r) = t.strip_prefix('>') {
            t = r.trim_start();
        }
        let mut prefix = "";
        let mut heading = false;
        let hashes = t.len() - t.trim_start_matches('#').len();
        if (1..=6).contains(&hashes) && t[hashes..].starts_with([' ', '\t']) {
            t = t[hashes..].trim_start();
            heading = true;
        } else if let Some(r) = t.strip_prefix(['-', '*', '+']).filter(|r| r.starts_with(' ')) {
            prefix = "- ";
            t = r.trim_start();
        }
        let text = strip_inline(t);
        let text = text.trim();
        if text.is_empty() {
            // One blank between paragraphs; none right under a heading.
            if out.last().is_some_and(|l| !l.0.is_empty() && !l.1) {
                out.push((String::new(), false));
            }
        } else {
            out.push((format!("{prefix}{text}"), heading));
        }
    }
    while out.last().is_some_and(|l| l.0.is_empty()) {
        out.pop();
    }
    out.truncate(max_lines);
    // Never end on a blank or on a heading whose section was cut off.
    while out.last().is_some_and(|l| l.0.is_empty() || l.1) {
        out.pop();
    }
    out
}

/// Word-wrap `text` to `width`, as `measure` sees it; a word wider than that
/// keeps a line of its own. An empty text is one empty line.
pub fn wrap(text: &str, width: f32, measure: &mut dyn FnMut(&str) -> f32) -> Vec<String> {
    let mut out = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let joined = if line.is_empty() { word.to_string() } else { format!("{line} {word}") };
        if !line.is_empty() && measure(&joined) > width {
            out.push(std::mem::replace(&mut line, word.to_string()));
        } else {
            line = joined;
        }
    }
    out.push(line);
    out
}

// --- Install kind and asset choice ------------------------------------------

/// How this copy was installed, which decides how it updates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstallKind {
    /// The Windows exe in a folder it can write: swaps itself.
    Portable,
    /// A store updates it (its name, as shown).
    Store(&'static str),
    /// Elsewhere (macOS, Linux, a folder it can't write): the release page.
    PageOnly,
}

/// Pure install-kind detection.
pub fn detect_from(windows: bool, msix: bool, dir_writable: bool) -> InstallKind {
    if msix {
        InstallKind::Store("Microsoft Store")
    } else if windows && dir_writable {
        InstallKind::Portable
    } else {
        InstallKind::PageOnly
    }
}

/// How this process was installed.
pub fn detect_install() -> InstallKind {
    let writable = || std::env::current_exe().ok().and_then(|e| e.parent().map(dir_writable)).unwrap_or(false);
    let msix = is_msix();
    detect_from(cfg!(windows), msix, !msix && cfg!(windows) && writable())
}

/// Create and remove a probe file: the only portable writability test.
fn dir_writable(dir: &Path) -> bool {
    let probe = dir.join(format!(".mp4-splitter-write-test-{}", std::process::id()));
    match std::fs::OpenOptions::new().write(true).create_new(true).open(&probe) {
        Ok(_) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// This process runs from an MSIX package (the Store build).
#[cfg(windows)]
pub fn is_msix() -> bool {
    use windows::Win32::Foundation::ERROR_INSUFFICIENT_BUFFER;
    use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
    let mut len = 0u32;
    // SAFETY: a zero-length probe. Packaged, it reports a too-small buffer;
    // unpackaged, APPMODEL_ERROR_NO_PACKAGE (15700).
    unsafe { GetCurrentPackageFullName(&mut len, None) == ERROR_INSUFFICIENT_BUFFER }
}

#[cfg(not(windows))]
pub fn is_msix() -> bool {
    false
}

/// The name of the file that updates the Windows exe to `version`.
pub fn windows_asset_name(version: &str) -> String {
    format!("mp4-splitter-{version}{ASSET_SUFFIX}")
}

/// What follows the version in the Windows exe's name.
pub const ASSET_SUFFIX: &str = "-windows-x86_64.exe";

/// The release asset that updates an install of `kind`, matched by exact name.
pub fn pick_asset(kind: InstallKind, rel: &Release) -> Option<&Asset> {
    if kind != InstallKind::Portable {
        return None;
    }
    let want = windows_asset_name(&rel.version);
    rel.assets.iter().find(|a| a.name == want)
}

// --- Integrity --------------------------------------------------------------

/// Parse `sha256sum` output: `<hex>  <name>` or `<hex> *<name>` per line.
/// Malformed lines are skipped. A name listed twice with different digests is
/// poisoned: it comes back in the second list, not the first, so a lookup finds
/// nothing. Identical duplicates are fine.
pub fn parse_sums(text: &str) -> (HashMap<String, [u8; 32]>, Vec<String>) {
    let mut out: HashMap<String, [u8; 32]> = HashMap::new();
    let mut poisoned: Vec<String> = Vec::new();
    for line in text.lines() {
        let Some((hex, rest)) = line.trim().split_once(' ') else { continue };
        let Some(digest) = crate::sha256::parse_hex(hex) else { continue };
        let name = rest.trim_start_matches(' ');
        let name = name.strip_prefix('*').unwrap_or(name);
        let name = name.strip_prefix("./").unwrap_or(name);
        if !name.is_empty() {
            match out.get(name) {
                Some(d) if *d != digest => poisoned.push(name.to_string()),
                Some(_) => {}
                None => {
                    out.insert(name.to_string(), digest);
                }
            }
        }
    }
    for n in &poisoned {
        out.remove(n);
    }
    (out, poisoned)
}

/// Largest `SHA256SUMS.txt` we accept (a few hundred bytes in practice).
const SUMS_MAX: u64 = 64 * 1024;
/// Largest exe we accept: this app is under a megabyte.
pub const EXE_MAX: u64 = 64 * 1024 * 1024;

/// Fetches a URL to a file, with a size limit and a progress callback (see
/// `http::download_to`, which tests replace).
pub type Download<'a> = &'a dyn Fn(&str, &Path, Option<u64>, &dyn Fn(u64, Option<u64>) -> bool) -> Result<(), String>;

/// Download `SHA256SUMS.txt` and `asset` into `dir` (user-private: stale files of
/// either name go first and the new ones are created exclusively) and check the
/// asset's SHA-256. Returns its path and digest, so the swap can check again right
/// before it happens. Refuses a release without the sums or without an entry for
/// the asset; on any failure, mismatch or cancel (`progress` returning false:
/// error [`CANCELLED`]) nothing is left behind.
pub fn fetch_verified(
    rel: &Release,
    asset: &Asset,
    dir: &Path,
    progress: &dyn Fn(u64, Option<u64>) -> bool,
    download: Download,
) -> Result<(PathBuf, [u8; 32]), String> {
    if !is_safe_asset_name(&asset.name) || asset.name.eq_ignore_ascii_case(SUMS_NAME) || !is_safe_download_url(&asset.url) {
        return Err("refusing an unexpected release asset".into());
    }
    let sums_asset = rel.assets.iter().find(|a| a.name == SUMS_NAME).ok_or("this release has no SHA256SUMS.txt; refusing to install")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let sums_path = dir.join(SUMS_NAME);
    let dest = dir.join(&asset.name);
    let total = Some(asset.size).filter(|&s| s > 0);
    let _ = std::fs::remove_file(&sums_path);
    let _ = std::fs::remove_file(&dest);
    let result = (|| -> Result<[u8; 32], String> {
        download(&sums_asset.url, &sums_path, Some(SUMS_MAX), &|_, _| progress(0, total))?;
        let text = std::fs::read(&sums_path).map_err(|e| e.to_string())?;
        if text.len() as u64 > SUMS_MAX {
            return Err("SHA256SUMS.txt is unexpectedly large".into());
        }
        let (sums, poisoned) = parse_sums(&String::from_utf8_lossy(&text));
        if poisoned.contains(&asset.name) {
            return Err(format!("SHA256SUMS.txt lists {} twice", asset.name));
        }
        let want = *sums.get(&asset.name).ok_or_else(|| format!("{} is not listed in SHA256SUMS.txt; refusing to install", asset.name))?;
        download(&asset.url, &dest, Some(EXE_MAX), &|got, t| progress(got, t.or(total)))?;
        let got = crate::sha256::file_digest(&dest).map_err(|e| format!("read download: {e}"))?;
        if got != want {
            return Err(format!("{} failed its SHA-256 check; refusing to install", asset.name));
        }
        Ok(want)
    })();
    let _ = std::fs::remove_file(&sums_path);
    match result {
        Ok(want) => Ok((dest, want)),
        Err(e) => {
            let _ = std::fs::remove_file(&dest);
            Err(e)
        }
    }
}

/// Hash `path` again and compare with `expected`.
pub fn verify_file(path: &Path, expected: [u8; 32]) -> Result<(), String> {
    let got = crate::sha256::file_digest(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    if got == expected { Ok(()) } else { Err("the downloaded file does not match its SHA-256; refusing to use it".into()) }
}

// --- The daily check --------------------------------------------------------

const DAY_SECS: u64 = 86_400;
const STAMP_FILE: &str = "update-check";
const FIRST_DELAY: std::time::Duration = std::time::Duration::from_secs(5);
const RECHECK: std::time::Duration = std::time::Duration::from_secs(6 * 3600);

/// A check is due when none has succeeded yet or the last one is a day old.
pub fn due(last_unix: Option<u64>, now_unix: u64) -> bool {
    last_unix.is_none_or(|t| now_unix.saturating_sub(t) >= DAY_SECS)
}

/// The user chose "Skip this version" for this release. Only the daily check honours it.
fn is_skipped(rel: &Release, skip_version: &str) -> bool {
    let s = skip_version.trim();
    !s.is_empty() && s.strip_prefix('v').unwrap_or(s) == rel.version
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// One round of the daily check, when `on` and due: the stamp written on success
/// (a failure leaves it, and stays quiet), a release `skip` does not name.
fn once(stamp: &Path, now: u64, on: bool, skip: &str, check: impl FnOnce() -> Result<Option<Release>, String>) -> Option<Release> {
    if !on {
        return None;
    }
    let last = std::fs::read_to_string(stamp).ok().and_then(|t| t.trim().parse().ok());
    if !due(last, now) {
        return None;
    }
    match check() {
        Ok(r) => {
            if let Some(dir) = stamp.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(stamp, now.to_string());
            r.filter(|r| !is_skipped(r, skip))
        }
        Err(e) => {
            eprintln!("update check failed: {e}");
            None
        }
    }
}

/// Start the daily check on its own thread, which never blocks exit. Settings are
/// read again each round, so turning it off stops it. Not for a store's build,
/// and never in tests.
pub fn spawn_checker(send: impl Fn(Release) + Send + 'static) {
    if cfg!(test) || matches!(detect_install(), InstallKind::Store(_)) {
        return;
    }
    std::thread::spawn(move || {
        std::thread::sleep(FIRST_DELAY);
        let Some(stamp) = crate::config::dir().map(|d| d.join(STAMP_FILE)) else { return };
        loop {
            let c = crate::config::Config::load();
            if let Some(r) = once(&stamp, unix_now(), c.check_updates, &c.skip_version, check_now) {
                send(r);
            }
            std::thread::sleep(RECHECK);
        }
    });
}

/// Ask GitHub for the latest release; `Some` only if newer than this build.
pub fn check_now() -> Result<Option<Release>, String> {
    let body = crate::http::http_get(RELEASES_HOST, RELEASES_PATH, &[("User-Agent", USER_AGENT), ("Accept", "application/vnd.github+json")])?;
    Ok(parse_latest(&body).filter(|r| is_newer(&r.version, env!("CARGO_PKG_VERSION"))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_as_numbers() {
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("0.1.1"), Some((0, 1, 1)));
        for bad in ["", "1.2", "1.2.3.4", "1.2.x", "v", "1..3", "1.2.3-rc1", " 1.2.3", "+1.2.3"] {
            assert_eq!(parse_version(bad), None, "{bad:?}");
        }
        assert!(is_newer("v0.1.2", "0.1.1"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9"), "numbers, not text");
        assert!(is_newer("1.0.0", "0.99.99"));
        assert!(!is_newer("0.1.1", "0.1.1"));
        assert!(!is_newer("0.1.0", "0.1.1"));
        assert!(!is_newer("nonsense", "0.1.1"), "what can't be compared is not newer");
        assert!(!is_newer("0.2.0", "nonsense"));
    }

    fn asset(name: &str) -> Asset {
        Asset { name: name.into(), url: format!("{DOWNLOAD_PREFIX}v0.2.0/{name}"), size: 3 }
    }

    /// The real shape of the API's answer: nested objects with the same keys
    /// (the author's `html_url`) must not be taken for the release's.
    const FIXTURE: &str = r#"{
      "url": "https://api.github.com/repos/nappsllc/mp4-splitter/releases/1",
      "html_url": "https://github.com/nappsllc/mp4-splitter/releases/tag/v0.2.0",
      "author": { "login": "someone", "html_url": "https://github.com/someone" },
      "tag_name": "v0.2.0", "draft": false, "prerelease": false,
      "body": "notes with \"quotes\" é and a link https:\/\/example.com",
      "assets": [
        { "name": "mp4-splitter-0.2.0-windows-x86_64.exe", "size": 687616, "uploader": { "name": "x" },
          "browser_download_url": "https://github.com/nappsllc/mp4-splitter/releases/download/v0.2.0/mp4-splitter-0.2.0-windows-x86_64.exe" },
        { "name": "SHA256SUMS.txt", "size": 310,
          "browser_download_url": "https://github.com/nappsllc/mp4-splitter/releases/download/v0.2.0/SHA256SUMS.txt" },
        { "name": "evil.exe", "size": 1, "browser_download_url": "https://example.com/evil.exe" },
        { "name": "..\\evil.exe", "size": 1, "browser_download_url": "https://github.com/nappsllc/mp4-splitter/releases/download/v0.2.0/evil.exe" }
      ]
    }"#;

    #[test]
    fn a_release_is_read_and_only_safe_assets_kept() {
        let r = parse_latest(FIXTURE).unwrap();
        assert_eq!(r.version, "0.2.0");
        assert_eq!(r.url, "https://github.com/nappsllc/mp4-splitter/releases/tag/v0.2.0");
        assert_eq!(r.notes, "notes with \"quotes\" é and a link https://example.com");
        let names: Vec<_> = r.assets.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, ["mp4-splitter-0.2.0-windows-x86_64.exe", "SHA256SUMS.txt"]);
        assert_eq!(r.assets[0].size, 687616);
    }

    #[test]
    fn drafts_prereleases_and_odd_tags_are_not_releases() {
        for (a, b) in [
            ("\"draft\": false", "\"draft\": true"),
            ("\"prerelease\": false", "\"prerelease\": true"),
            ("\"tag_name\": \"v0.2.0\"", "\"tag_name\": \"nightly\""),
        ] {
            assert!(parse_latest(&FIXTURE.replacen(a, b, 1)).is_none(), "{b}");
        }
        let other = FIXTURE.replacen("\"html_url\": \"https://github.com/nappsllc/mp4-splitter/", "\"html_url\": \"https://github.com/someone/else/", 1);
        assert!(parse_latest(&other).is_none(), "a release page elsewhere");
        assert!(parse_latest("not json").is_none());
    }

    #[test]
    fn only_this_repos_release_files_can_be_fetched() {
        let good = "https://github.com/nappsllc/mp4-splitter/releases/download/v1.0.0/x.exe";
        assert!(is_safe_download_url(good));
        assert!(is_safe_download_url("https://github.com/NappsLLC/MP4-Splitter/releases/download/v1.0.0/x.exe"), "GitHub's case");
        for bad in [
            "http://github.com/nappsllc/mp4-splitter/releases/download/v1.0.0/x.exe",
            "https://github.com/nappsllc/rustshot/releases/download/v1.0.0/x.exe",
            "https://github.com/nappsllc/mp4-splitter/releases/download/../../x.exe",
            "https://github.com/nappsllc/mp4-splitter/releases/download/v1.0.0/%2e%2e/x.exe",
            "https://github.com/nappsllc/mp4-splitter/releases/download/v1.0.0/x y.exe",
            "https://github.com.evil.com/nappsllc/mp4-splitter/releases/download/v1.0.0/x.exe",
        ] {
            assert!(!is_safe_download_url(bad), "{bad}");
        }
        assert!(is_allowed_download_hop("https://release-assets.githubusercontent.com/abc?sig=1"));
        assert!(!is_allowed_download_hop("https://example.com/x.exe"));
        assert!(!is_allowed_download_hop("http://release-assets.githubusercontent.com/abc"));
        assert_eq!(split_https_url("https://a.b/c?d"), Some(("a.b", "/c?d")));
        assert_eq!(split_https_url("https://a.b:444/c"), None);
        assert_eq!(split_https_url("https://u@a.b/c"), None);
    }

    #[test]
    fn the_right_asset_is_picked_by_its_exact_name() {
        let rel = Release {
            version: "0.2.0".into(),
            assets: vec![
                asset("mp4-splitter-0.2.0-linux-x86_64.tar.gz"),
                asset("mp4-splitter-0.1.9-windows-x86_64.exe"),
                asset("mp4-splitter-0.2.0-windows-x86_64.exe"),
                asset(SUMS_NAME),
            ],
            ..Default::default()
        };
        assert_eq!(pick_asset(InstallKind::Portable, &rel).unwrap().name, "mp4-splitter-0.2.0-windows-x86_64.exe");
        assert!(pick_asset(InstallKind::PageOnly, &rel).is_none());
        assert!(pick_asset(InstallKind::Store("Microsoft Store"), &rel).is_none());
        let none = Release { assets: vec![asset("mp4-splitter-0.1.9-windows-x86_64.exe")], ..rel };
        assert!(pick_asset(InstallKind::Portable, &none).is_none(), "another version's file is no use");
    }

    #[test]
    fn only_the_plain_exe_updates_itself() {
        assert_eq!(detect_from(true, false, true), InstallKind::Portable);
        assert_eq!(detect_from(true, false, false), InstallKind::PageOnly);
        assert_eq!(detect_from(true, true, true), InstallKind::Store("Microsoft Store"), "the Store updates its build");
        assert_eq!(detect_from(true, true, false), InstallKind::Store("Microsoft Store"));
        assert_eq!(detect_from(false, false, true), InstallKind::PageOnly);
    }

    /// The Store package starts the exe the build makes, and every language has a
    /// Store code (msix.ps1 stops without one, but only when someone packages).
    #[test]
    fn the_store_package_matches_the_app() {
        let manifest = include_str!("../packaging/windows/AppxManifest.xml.in");
        assert!(manifest.contains(r#"Executable="mp4-splitter.exe""#));
        assert!(manifest.contains(&format!("<DisplayName>{}</DisplayName>", crate::APP_NAME)));
        assert!(!manifest.contains("internetClient"), "this build makes no requests");
        let script = include_str!("../packaging/windows/msix.ps1");
        for l in crate::i18n::all_strings() {
            let tag = l.0;
            assert!(script.contains(&format!("'{tag}' = '")), "{tag} has no Store code in msix.ps1");
        }
    }

    #[test]
    fn checksum_lines_are_read_strictly() {
        let d = "a".repeat(64);
        let e = "b".repeat(64);
        let text =
            format!("{d}  one.exe\n{e} *two.exe\n{d}  ./three.exe\nnot a line\nshort  four.exe\n{d}  dup.exe\n{d}  dup.exe\n{d}  bad.exe\n{e}  bad.exe\n");
        let (sums, poisoned) = parse_sums(&text);
        assert_eq!(sums.get("one.exe"), crate::sha256::parse_hex(&d).as_ref());
        assert_eq!(sums.get("two.exe"), crate::sha256::parse_hex(&e).as_ref());
        assert!(sums.contains_key("three.exe") && sums.contains_key("dup.exe"));
        assert!(!sums.contains_key("four.exe"));
        assert!(!sums.contains_key("bad.exe"), "two different digests: neither is trusted");
        assert_eq!(poisoned, ["bad.exe"]);
    }

    /// A fresh temp dir, removed on drop.
    struct Tmp(PathBuf);
    impl Tmp {
        fn new(tag: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("mp4-splitter-upd-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn files(&self) -> usize {
            std::fs::read_dir(&self.0).unwrap().count()
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    const EXE: &str = "mp4-splitter-0.2.0-windows-x86_64.exe";

    fn rel_with_sums() -> Release {
        Release { version: "0.2.0".into(), assets: vec![asset(EXE), asset(SUMS_NAME)], ..Default::default() }
    }

    /// Fetch with a fake download serving these bytes.
    fn fetch(t: &Tmp, rel: &Release, sums: &str, exe: &[u8], progress: &dyn Fn(u64, Option<u64>) -> bool) -> Result<(PathBuf, [u8; 32]), String> {
        let dl = |url: &str, dest: &Path, _: Option<u64>, p: &dyn Fn(u64, Option<u64>) -> bool| {
            if !p(0, None) {
                return Err(CANCELLED.to_string());
            }
            let body: &[u8] = if url.ends_with(SUMS_NAME) { sums.as_bytes() } else { exe };
            std::fs::write(dest, body).map_err(|e| e.to_string())
        };
        fetch_verified(rel, &asset(EXE), &t.0, progress, &dl)
    }

    fn sums_for(bytes: &[u8]) -> String {
        format!("{}  {EXE}\n", crate::sha256::hex(&crate::sha256::digest(bytes)))
    }

    #[test]
    fn a_matching_download_is_kept_and_the_sums_are_not() {
        let t = Tmp::new("ok");
        let (path, sha) = fetch(&t, &rel_with_sums(), &sums_for(b"new build"), b"new build", &|_, _| true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new build");
        assert_eq!(sha, crate::sha256::digest(b"new build"));
        assert_eq!(t.files(), 1);
        verify_file(&path, sha).unwrap();
        assert!(verify_file(&path, [0; 32]).is_err());
    }

    #[test]
    fn what_does_not_match_its_checksum_is_refused_and_removed() {
        let t = Tmp::new("bad");
        let err = fetch(&t, &rel_with_sums(), &sums_for(b"new build"), b"tampered", &|_, _| true).unwrap_err();
        assert!(err.contains("SHA-256"), "{err}");
        assert_eq!(t.files(), 0);
    }

    #[test]
    fn no_checksum_no_install() {
        let t = Tmp::new("nosums");
        let bare = Release { version: "0.2.0".into(), assets: vec![asset(EXE)], ..Default::default() };
        assert!(fetch(&t, &bare, "", b"x", &|_, _| true).unwrap_err().contains("no SHA256SUMS"));
        let other = format!("{}  other.exe\n", "a".repeat(64));
        assert!(fetch(&t, &rel_with_sums(), &other, b"x", &|_, _| true).unwrap_err().contains("not listed"));
        let twice = format!("{}{}  {EXE}\n", sums_for(b"x"), "a".repeat(64));
        assert!(fetch(&t, &rel_with_sums(), &twice, b"x", &|_, _| true).unwrap_err().contains("twice"));
        assert_eq!(t.files(), 0);
    }

    #[test]
    fn a_cancelled_download_leaves_nothing() {
        let t = Tmp::new("cancel");
        assert_eq!(fetch(&t, &rel_with_sums(), &sums_for(b"x"), b"x", &|_, _| false).unwrap_err(), CANCELLED);
        assert_eq!(t.files(), 0);
    }

    #[test]
    fn the_daily_check_runs_when_due_and_says_what_is_new() {
        let t = Tmp::new("once");
        let stamp = t.0.join("sub").join(STAMP_FILE);
        let rel = || Ok(Some(Release { version: "9.9.9".into(), ..Default::default() }));
        let now = 1_000_000;
        assert!(once(&stamp, now, false, "", || panic!("off: no check")).is_none());
        assert!(!stamp.exists());
        assert_eq!(once(&stamp, now, true, "", rel).map(|r| r.version), Some("9.9.9".into()));
        assert_eq!(std::fs::read_to_string(&stamp).unwrap(), now.to_string());
        assert!(once(&stamp, now + 3600, true, "", || panic!("checked an hour ago")).is_none());
        assert!(once(&stamp, now + DAY_SECS, true, "v9.9.9", rel).is_none(), "skipped");
        assert_eq!(std::fs::read_to_string(&stamp).unwrap(), (now + DAY_SECS).to_string());
        assert!(once(&stamp, now + 3 * DAY_SECS, true, "", || Err("offline".into())).is_none());
        assert_eq!(std::fs::read_to_string(&stamp).unwrap(), (now + DAY_SECS).to_string(), "a failure keeps the stamp");
    }

    #[test]
    fn skip_names_one_version() {
        let rel = Release { version: "0.1.2".into(), ..Default::default() };
        assert!(is_skipped(&rel, "0.1.2") && is_skipped(&rel, " v0.1.2 "));
        assert!(!is_skipped(&rel, "") && !is_skipped(&rel, "0.1.1") && !is_skipped(&rel, "0.1.20"));
    }

    #[test]
    fn notes_read_as_text() {
        let lines = notes_lines("## Fixes\n\n- **Bold** and [a link](https://x)\n\n\n<!-- hidden -->\nPlain `code`\n", 10);
        let text: Vec<_> = lines.iter().map(|l| l.0.as_str()).collect();
        assert_eq!(text, ["Fixes", "- Bold and a link", "", "Plain code"]);
        assert!(lines[0].1 && !lines[1].1);
        assert_eq!(notes_lines("a\nb\nc\n", 2).len(), 2);
        assert!(notes_lines("", 5).is_empty());
    }

    #[test]
    fn lines_wrap_on_words() {
        let mut m = |s: &str| s.chars().count() as f32;
        assert_eq!(wrap("one two three", 7.0, &mut m), ["one two", "three"]);
        assert_eq!(wrap("averyveryverylongword x", 7.0, &mut m), ["averyveryverylongword", "x"]);
        assert_eq!(wrap("", 7.0, &mut m), [""]);
    }

    #[test]
    fn download_time_limits() {
        assert_eq!(download_time_limit(None).as_secs(), 600);
        assert_eq!(download_time_limit(Some(4096 * 40)).as_secs(), 100);
        assert_eq!(download_time_limit(Some(u64::MAX)).as_secs(), 600);
    }
}
