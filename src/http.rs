//! HTTPS for the update check. Windows: WinHTTP, with hand-followed redirects for
//! downloads (as Rustshot's `export_win.rs`, Apache-2.0): every hop must be an
//! allowed one. Elsewhere: the system's `curl`, for the check only (there the
//! update itself is a page in the browser).

/// HTTPS GET; a non-2xx status is an `Err`.
#[cfg(not(windows))]
pub fn http_get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<String, String> {
    let mut cmd = std::process::Command::new("curl");
    cmd.args(["-fsS", "--max-time", "15"]);
    for (k, v) in headers {
        cmd.arg("-H").arg(format!("{k}: {v}"));
    }
    cmd.arg(format!("https://{host}{path}"));
    let out = cmd.output().map_err(|e| format!("spawn curl: {e}"))?;
    if !out.status.success() {
        return Err(format!("curl: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(windows)]
pub use win::*;

#[cfg(windows)]
mod win {
    use crate::update::{CANCELLED, STALL_BYTES, STALL_SECS, download_time_limit, is_allowed_download_hop, is_safe_download_url, split_https_url};
    use std::path::Path;
    use windows::Win32::Networking::WinHttp::{
        WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_FLAG_SECURE, WINHTTP_OPTION_REDIRECT_POLICY, WINHTTP_OPTION_REDIRECT_POLICY_NEVER,
        WINHTTP_QUERY_CONTENT_LENGTH, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_FLAG_NUMBER64, WINHTTP_QUERY_LOCATION, WINHTTP_QUERY_STATUS_CODE,
        WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders, WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest,
        WinHttpSetOption, WinHttpSetTimeouts,
    };
    use windows::core::PCWSTR;

    /// An owned WinHTTP handle.
    struct Handle(*mut std::ffi::c_void);
    impl Drop for Handle {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: ours, and closed once.
                let _ = unsafe { WinHttpCloseHandle(self.0) };
            }
        }
    }

    /// A sent request whose response headers have arrived. Fields drop in
    /// declaration order: request, connection, session.
    struct Response {
        req: Handle,
        _conn: Handle,
        _session: Handle,
        status: u32,
    }

    /// Null-terminated UTF-16, for PCWSTR parameters.
    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    /// Send one GET and wait for the response headers. With `follow` false, a 3xx
    /// comes back as it is.
    fn send(host: &str, path: &str, headers: &str, follow: bool) -> Result<Response, String> {
        let err = |step: &str| format!("{step}: {}", std::io::Error::last_os_error());
        let (agent, host, method, object) = (wide(crate::update::USER_AGENT), wide(host), wide("GET"), wide(path));
        // SAFETY: the strings outlive the calls; the handles are checked and owned.
        unsafe {
            let session = Handle(WinHttpOpen(PCWSTR(agent.as_ptr()), WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, PCWSTR::null(), PCWSTR::null(), 0));
            if session.0.is_null() {
                return Err(err("WinHttpOpen"));
            }
            let _ = WinHttpSetTimeouts(session.0, 10_000, 10_000, 30_000, 30_000);
            let conn = Handle(WinHttpConnect(session.0, PCWSTR(host.as_ptr()), 443, 0));
            if conn.0.is_null() {
                return Err(err("WinHttpConnect"));
            }
            let req = Handle(WinHttpOpenRequest(
                conn.0,
                PCWSTR(method.as_ptr()),
                PCWSTR(object.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                WINHTTP_FLAG_SECURE,
            ));
            if req.0.is_null() {
                return Err(err("WinHttpOpenRequest"));
            }
            if !follow {
                let never = WINHTTP_OPTION_REDIRECT_POLICY_NEVER.to_ne_bytes();
                WinHttpSetOption(Some(req.0), WINHTTP_OPTION_REDIRECT_POLICY, Some(&never)).map_err(|e| format!("WinHttpSetOption: {e}"))?;
            }
            // Counted, not NUL-terminated: windows-rs passes the slice's length.
            let headers: Vec<u16> = headers.encode_utf16().collect();
            let headers = if headers.is_empty() { None } else { Some(&headers[..]) };
            WinHttpSendRequest(req.0, headers, None, 0, 0, 0).map_err(|e| format!("WinHttpSendRequest: {e}"))?;
            WinHttpReceiveResponse(req.0, std::ptr::null_mut()).map_err(|e| format!("WinHttpReceiveResponse: {e}"))?;
            let mut status = 0u32;
            let mut size = size_of::<u32>() as u32;
            WinHttpQueryHeaders(
                req.0,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                PCWSTR::null(),
                Some((&raw mut status).cast()),
                &mut size,
                std::ptr::null_mut(),
            )
            .map_err(|e| format!("WinHttpQueryHeaders: {e}"))?;
            Ok(Response { req, _conn: conn, _session: session, status })
        }
    }

    /// The next chunk of the response body; 0 at the end.
    fn read_some(resp: &Response, buf: &mut [u8]) -> Result<usize, String> {
        let mut read = 0u32;
        // SAFETY: `buf` is valid for the length given.
        unsafe { WinHttpReadData(resp.req.0, buf.as_mut_ptr().cast(), buf.len() as u32, &mut read) }.map_err(|e| format!("WinHttpReadData: {e}"))?;
        Ok(read as usize)
    }

    /// `Location` of a redirect.
    fn location(resp: &Response) -> Option<String> {
        let mut buf = vec![0u16; 4096];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: `buf` is valid for the size given, which comes back as the size used.
        unsafe { WinHttpQueryHeaders(resp.req.0, WINHTTP_QUERY_LOCATION, PCWSTR::null(), Some(buf.as_mut_ptr().cast()), &mut size, std::ptr::null_mut()) }
            .ok()?;
        buf.truncate(size as usize / 2);
        String::from_utf16(&buf).ok()
    }

    /// `Content-Length`, when the server sent one.
    fn content_length(resp: &Response) -> Option<u64> {
        let mut len = 0u64;
        let mut size = size_of::<u64>() as u32;
        // SAFETY: `len` is valid for the size given.
        unsafe {
            WinHttpQueryHeaders(
                resp.req.0,
                WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
                PCWSTR::null(),
                Some((&raw mut len).cast()),
                &mut size,
                std::ptr::null_mut(),
            )
        }
        .ok()?;
        Some(len)
    }

    /// HTTPS GET; a non-2xx status is an `Err`.
    pub fn http_get(host: &str, path: &str, headers: &[(&str, &str)]) -> Result<String, String> {
        let headers = headers.iter().map(|(k, v)| format!("{k}: {v}")).collect::<Vec<_>>().join("\r\n");
        let resp = send(host, path, &headers, true)?;
        if !(200..300).contains(&resp.status) {
            return Err(format!("HTTP {}", resp.status));
        }
        let mut out = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = read_some(&resp, &mut buf)?;
            if n == 0 {
                return Ok(String::from_utf8_lossy(&out).into_owned());
            }
            out.extend_from_slice(&buf[..n]);
        }
    }

    const MAX_REDIRECTS: usize = 5;

    /// Download a release asset to `dest`. Starts only at a URL that passes
    /// `is_safe_download_url`; redirects are followed by hand and each hop must
    /// pass `is_allowed_download_hop`. `progress(got, total)` returning false
    /// cancels (error `CANCELLED`). A partial file is deleted on any failure.
    /// `max`: at most this many bytes. A download that stalls or runs past
    /// `download_time_limit` fails.
    pub fn download_to(url: &str, dest: &Path, max: Option<u64>, progress: &dyn Fn(u64, Option<u64>) -> bool) -> Result<(), String> {
        if !is_safe_download_url(url) {
            return Err("refusing to download from an unexpected URL".into());
        }
        let mut current = url.to_string();
        for _ in 0..=MAX_REDIRECTS {
            let (host, path) = split_https_url(&current).ok_or("unexpected download URL")?;
            let resp = send(host, path, "", false)?;
            match resp.status {
                301 | 302 | 303 | 307 | 308 => {
                    let next = location(&resp).ok_or("redirect without a Location")?;
                    if !is_allowed_download_hop(&next) {
                        return Err("refusing a redirect to an unexpected host".into());
                    }
                    current = next;
                }
                200 => {
                    let r = write_body(&resp, dest, max, progress);
                    if r.is_err() {
                        let _ = std::fs::remove_file(dest);
                    }
                    return r;
                }
                s => return Err(format!("HTTP {s}")),
            }
        }
        Err("too many redirects".into())
    }

    fn write_body(resp: &Response, dest: &Path, max: Option<u64>, progress: &dyn Fn(u64, Option<u64>) -> bool) -> Result<(), String> {
        use std::io::Write;
        use std::time::{Duration, Instant};
        let total = content_length(resp);
        if let (Some(t), Some(m)) = (total, max)
            && t > m
        {
            return Err("download is larger than expected".into());
        }
        let start = Instant::now();
        let limit = download_time_limit(max);
        let mut window = (start, 0u64);
        let wr = |e: std::io::Error| format!("write {}: {e}", dest.display());
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(dest).map_err(wr)?;
        let mut buf = vec![0u8; 64 * 1024];
        let mut got = 0u64;
        if !progress(0, total) {
            return Err(CANCELLED.into());
        }
        loop {
            let n = read_some(resp, &mut buf)?;
            if n == 0 {
                break;
            }
            got += n as u64;
            if max.is_some_and(|m| got > m) {
                return Err("download is larger than expected".into());
            }
            file.write_all(&buf[..n]).map_err(wr)?;
            if total.is_some_and(|t| got > t) {
                return Err("download is larger than announced".into());
            }
            let now = Instant::now();
            if now.duration_since(start) > limit {
                return Err("download timed out".into());
            }
            if now.duration_since(window.0) >= Duration::from_secs(STALL_SECS) {
                if got - window.1 < STALL_BYTES {
                    return Err("download stalled (timed out)".into());
                }
                window = (now, got);
            }
            if !progress(got, total) {
                return Err(CANCELLED.into());
            }
        }
        if total.is_some_and(|t| got != t) {
            return Err("download ended early".into());
        }
        file.sync_all().map_err(wr)
    }
}
