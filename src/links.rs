//! The web pages the app opens (as Rustshot's `src/actions.rs`, Apache-2.0).

/// PowerNapps' short link: a redirect on powernapps.net, so where it goes can
/// change without an app update.
pub const POWERNAPPS: &str = "https://powernapps.net/r/mp4-splitter";

/// The only URLs [`open`] opens.
const LINKS: [&str; 1] = [POWERNAPPS];

fn allowed(url: &str) -> bool {
    LINKS.contains(&url)
}

/// Open one of [`LINKS`] in the default browser; any other URL is refused.
pub fn open(url: &str) {
    if !allowed(url) {
        eprintln!("refusing to open an unexpected URL");
        return;
    }
    #[cfg(windows)]
    {
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
        use windows::core::{HSTRING, PCWSTR, w};
        let u = HSTRING::from(url);
        // SAFETY: the strings outlive the call, which takes no ownership.
        let _ = unsafe { ShellExecuteW(None, w!("open"), &u, PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL) };
    }
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(not(any(windows, target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_listed_page_opens() {
        assert_eq!(POWERNAPPS, "https://powernapps.net/r/mp4-splitter");
        assert!(allowed(POWERNAPPS));
        assert!(!allowed("https://example.com"));
        assert!(!allowed("https://powernapps.net/r/mp4-splitter/../other"));
    }
}
