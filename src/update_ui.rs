//! What the update prompt shows and does: a state machine (events in, actions out,
//! unit-tested), drawn and driven by `app.rs`. As Rustshot's `update_ui.rs`
//! (Apache-2.0), without its window.
//!
//! A prompt the user didn't ask for (the daily check) has no default button: Enter
//! does nothing there, so typing when it appears never starts an update.

use crate::update::Release;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Btn {
    Update,
    Skip,
    Cancel,
    Ok,
    Retry,
    Close,
    Restart,
    Later,
}

impl Btn {
    pub fn label(self, page_only: bool) -> &'static str {
        match self {
            Btn::Update if page_only => tr!("update.btn.page"),
            Btn::Update => tr!("update.btn.update"),
            Btn::Skip => tr!("update.btn.skip"),
            Btn::Cancel => tr!("update.btn.cancel"),
            Btn::Ok => tr!("update.btn.ok"),
            Btn::Retry => tr!("update.btn.retry"),
            Btn::Close => tr!("update.btn.close"),
            Btn::Restart => tr!("update.btn.restart"),
            Btn::Later => tr!("update.btn.later"),
        }
    }
}

/// What the prompt shows.
#[derive(Clone, Debug, PartialEq)]
pub enum Screen {
    Checking,
    Available(Release),
    UpToDate,
    /// A store updates this install (its name).
    Managed(&'static str),
    CheckFailed(String),
    /// Downloading (bytes so far, total when known).
    Downloading {
        rel: Release,
        got: u64,
        total: Option<u64>,
    },
    /// Downloaded; being checked and put in place.
    Verifying(Release),
    /// Cancel pressed; the download stops and cleans up.
    Cancelling(Release),
    /// In place: it runs from the next start, or now, if the user says.
    Ready(Release),
    Restarting(Release),
    /// Download, verification, swap or restart failed; Retry starts again.
    Failed {
        rel: Release,
        msg: String,
    },
}

/// Events: the user's and the workers'.
#[derive(Debug)]
pub enum In {
    Click(Btn),
    /// Esc.
    Dismiss,
    /// Enter: the default button, if any.
    Enter,
    Checked(Result<Option<Release>, String>),
    Progress(u64, Option<u64>),
    /// Downloaded, checked and swapped in, or why not (the running exe is as it
    /// was then). The path is the running exe's.
    Fetched(Result<std::path::PathBuf, String>),
    /// The new version is up (time to go), or why not (the swap has been undone).
    Restarted(Result<(), String>),
}

/// What the driver must do.
#[derive(Clone, Debug, PartialEq)]
pub enum Do {
    Close,
    /// Check on a worker (then `Checked`).
    Check,
    /// Download, verify and swap on a worker (then `Progress`, `Fetched`).
    Download(Release),
    /// Stop the running download.
    Cancel,
    /// Remember not to offer this version.
    Skip(String),
    /// Open the release page instead.
    OpenPage,
    /// Start the new version on a worker (then `Restarted`).
    Restart,
    /// It's in place: remember, and stop offering it.
    Installed(Release, std::path::PathBuf),
    /// The swap was undone: it isn't in place any more.
    Uninstalled,
    /// All done: quit.
    Quit,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Machine {
    pub screen: Screen,
    /// Not asked for: Enter does nothing until the user has clicked.
    unsolicited: bool,
    /// This install takes the release page, not a swap.
    page_only: bool,
}

impl Machine {
    /// "Checking…": the user asked.
    pub fn checking(page_only: bool) -> (Machine, Vec<Do>) {
        (Machine { screen: Screen::Checking, unsolicited: false, page_only }, vec![Do::Check])
    }

    /// The daily check found this.
    pub fn offer(rel: Release, page_only: bool) -> Machine {
        Machine { screen: Screen::Available(rel), unsolicited: true, page_only }
    }

    /// The user asked to check, and a store updates this install.
    pub fn managed(store: &'static str) -> Machine {
        Machine { screen: Screen::Managed(store), unsolicited: false, page_only: false }
    }

    /// The user asked to check, and this version is in place already.
    pub fn ready(rel: Release) -> Machine {
        Machine { screen: Screen::Ready(rel), unsolicited: false, page_only: false }
    }

    pub fn page_only(&self) -> bool {
        self.page_only
    }

    /// The buttons, left to right, the first the primary one.
    pub fn buttons(&self) -> &'static [Btn] {
        use Btn::*;
        match self.screen {
            Screen::Available(_) => &[Update, Skip, Cancel],
            Screen::UpToDate | Screen::Managed(_) => &[Ok],
            Screen::CheckFailed(_) | Screen::Failed { .. } => &[Retry, Close],
            Screen::Checking | Screen::Downloading { .. } => &[Cancel],
            Screen::Ready(_) => &[Restart, Later],
            Screen::Verifying(_) | Screen::Cancelling(_) | Screen::Restarting(_) => &[],
        }
    }

    /// Advance on `ev`; what the driver must do, in order.
    pub fn on(&mut self, ev: In) -> Vec<Do> {
        use Screen as S;
        if matches!(ev, In::Click(_)) {
            self.unsolicited = false;
        }
        let btn = match &ev {
            In::Click(b) => Some(*b),
            In::Enter if !self.unsolicited => self.buttons().first().copied(),
            In::Enter => return Vec::new(),
            // Esc stands for the button that backs out, where there is one.
            In::Dismiss => self.buttons().iter().rev().find(|b| matches!(b, Btn::Cancel | Btn::Close | Btn::Ok | Btn::Later)).copied(),
            _ => None,
        };
        let page_only = self.page_only;
        let screen = std::mem::replace(&mut self.screen, S::Checking);
        let (next, acts) = match (screen, ev, btn) {
            (S::Checking, In::Checked(r), _) => match r {
                Ok(Some(rel)) => (S::Available(rel), vec![]),
                Ok(None) => (S::UpToDate, vec![]),
                Err(e) => (S::CheckFailed(e), vec![]),
            },
            (S::Checking, _, Some(Btn::Cancel)) => (S::Checking, vec![Do::Close]),
            (S::CheckFailed(_), _, Some(Btn::Retry)) => (S::Checking, vec![Do::Check]),
            (S::Available(rel), _, Some(Btn::Update)) if page_only => (S::Available(rel), vec![Do::OpenPage, Do::Close]),
            (S::Available(rel), _, Some(Btn::Update)) | (S::Failed { rel, .. }, _, Some(Btn::Retry)) => {
                (S::Downloading { rel: rel.clone(), got: 0, total: None }, vec![Do::Download(rel)])
            }
            (S::Available(rel), _, Some(Btn::Skip)) => {
                let v = rel.version.clone();
                (S::Available(rel), vec![Do::Skip(v), Do::Close])
            }
            (S::Downloading { rel, .. } | S::Verifying(rel), In::Progress(got, total), _) => {
                if total.is_some_and(|t| t > 0 && got >= t) {
                    (S::Verifying(rel), vec![])
                } else {
                    (S::Downloading { rel, got, total }, vec![])
                }
            }
            (S::Downloading { rel, .. }, _, Some(Btn::Cancel)) => (S::Cancelling(rel), vec![Do::Cancel]),
            (S::Downloading { rel, .. } | S::Verifying(rel) | S::Cancelling(rel), In::Fetched(Ok(exe)), _) => {
                (S::Ready(rel.clone()), vec![Do::Installed(rel, exe)])
            }
            (S::Cancelling(rel), In::Fetched(Err(_)), _) => (S::Cancelling(rel), vec![Do::Close]),
            (S::Downloading { rel, .. } | S::Verifying(rel), In::Fetched(Err(msg)), _) => (S::Failed { rel, msg }, vec![]),
            (S::Ready(rel), _, Some(Btn::Restart)) => (S::Restarting(rel), vec![Do::Restart]),
            (S::Restarting(rel), In::Restarted(r), _) => match r {
                Ok(()) => (S::Restarting(rel), vec![Do::Quit]),
                Err(msg) => (S::Failed { rel, msg }, vec![Do::Uninstalled]),
            },
            (s, _, Some(Btn::Cancel | Btn::Ok | Btn::Close | Btn::Later)) => (s, vec![Do::Close]),
            (s, _, _) => (s, vec![]),
        };
        self.screen = next;
        acts
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn rel() -> Release {
        Release { version: "0.2.0".into(), ..Default::default() }
    }

    fn exe() -> PathBuf {
        PathBuf::from("app.exe")
    }

    #[test]
    fn a_check_ends_in_what_it_found() {
        for (r, want) in
            [(Ok(Some(rel())), Screen::Available(rel())), (Ok(None), Screen::UpToDate), (Err("offline".to_string()), Screen::CheckFailed("offline".into()))]
        {
            let (mut m, acts) = Machine::checking(false);
            assert_eq!(acts, [Do::Check]);
            assert!(m.on(In::Checked(r)).is_empty());
            assert_eq!(m.screen, want);
        }
    }

    #[test]
    fn a_failed_check_can_be_tried_again() {
        let (mut m, _) = Machine::checking(false);
        m.on(In::Checked(Err("x".into())));
        assert_eq!(m.on(In::Click(Btn::Retry)), [Do::Check]);
        assert_eq!(m.screen, Screen::Checking);
    }

    #[test]
    fn the_whole_way_needs_the_users_ok_to_restart() {
        let mut m = Machine::offer(rel(), false);
        assert_eq!(m.on(In::Click(Btn::Update)), [Do::Download(rel())]);
        m.on(In::Progress(10, Some(100)));
        assert_eq!(m.screen, Screen::Downloading { rel: rel(), got: 10, total: Some(100) });
        m.on(In::Progress(100, Some(100)));
        assert_eq!(m.screen, Screen::Verifying(rel()));
        assert_eq!(m.on(In::Fetched(Ok(exe()))), [Do::Installed(rel(), exe())]);
        assert_eq!(m.screen, Screen::Ready(rel()), "it waits here");
        assert_eq!(m.on(In::Click(Btn::Restart)), [Do::Restart]);
        assert_eq!(m.on(In::Restarted(Ok(()))), [Do::Quit]);
    }

    #[test]
    fn later_leaves_it_for_the_next_start() {
        let mut m = Machine::ready(rel());
        assert_eq!(m.on(In::Click(Btn::Later)), [Do::Close]);
        let mut m = Machine::ready(rel());
        assert_eq!(m.on(In::Dismiss), [Do::Close], "Esc is Later");
    }

    #[test]
    fn a_failed_restart_says_it_is_undone_and_can_be_retried() {
        let mut m = Machine::ready(rel());
        m.on(In::Click(Btn::Restart));
        assert_eq!(m.on(In::Restarted(Err("no".into()))), [Do::Uninstalled]);
        assert_eq!(m.screen, Screen::Failed { rel: rel(), msg: "no".into() });
        assert_eq!(m.on(In::Click(Btn::Retry)), [Do::Download(rel())]);
    }

    #[test]
    fn a_failed_download_can_be_tried_again() {
        let mut m = Machine::offer(rel(), false);
        m.on(In::Click(Btn::Update));
        assert!(m.on(In::Fetched(Err("SHA-256".into()))).is_empty());
        assert_eq!(m.screen, Screen::Failed { rel: rel(), msg: "SHA-256".into() });
        assert_eq!(m.on(In::Click(Btn::Close)), [Do::Close]);
    }

    #[test]
    fn cancel_stops_the_download_then_closes() {
        let mut m = Machine::offer(rel(), false);
        m.on(In::Click(Btn::Update));
        assert_eq!(m.on(In::Dismiss), [Do::Cancel], "Esc cancels too");
        assert_eq!(m.screen, Screen::Cancelling(rel()));
        assert_eq!(m.on(In::Fetched(Err("cancelled".into()))), [Do::Close]);
    }

    #[test]
    fn a_cancel_too_late_finds_it_in_place() {
        let mut m = Machine::offer(rel(), false);
        m.on(In::Click(Btn::Update));
        m.on(In::Click(Btn::Cancel));
        assert_eq!(m.on(In::Fetched(Ok(exe()))), [Do::Installed(rel(), exe())]);
        assert_eq!(m.screen, Screen::Ready(rel()));
    }

    #[test]
    fn nothing_interrupts_the_part_that_cannot_stop() {
        for mut m in [
            Machine { screen: Screen::Verifying(rel()), unsolicited: false, page_only: false },
            Machine { screen: Screen::Restarting(rel()), unsolicited: false, page_only: false },
            Machine { screen: Screen::Cancelling(rel()), unsolicited: false, page_only: false },
        ] {
            let before = m.screen.clone();
            assert!(m.on(In::Dismiss).is_empty());
            assert!(m.on(In::Enter).is_empty());
            assert_eq!(m.screen, before);
        }
    }

    #[test]
    fn enter_does_nothing_in_a_prompt_nobody_asked_for() {
        let mut m = Machine::offer(rel(), false);
        assert!(m.on(In::Enter).is_empty());
        assert_eq!(m.screen, Screen::Available(rel()));
        // Asked for: Enter is the first button.
        let (mut m, _) = Machine::checking(false);
        m.on(In::Checked(Ok(Some(rel()))));
        assert_eq!(m.on(In::Enter), [Do::Download(rel())]);
    }

    #[test]
    fn skip_remembers_the_version_and_closes() {
        let mut m = Machine::offer(rel(), false);
        assert_eq!(m.on(In::Click(Btn::Skip)), [Do::Skip("0.2.0".into()), Do::Close]);
    }

    #[test]
    fn where_it_cannot_swap_update_opens_the_page() {
        let mut m = Machine::offer(rel(), true);
        assert_eq!(m.on(In::Click(Btn::Update)), [Do::OpenPage, Do::Close]);
    }

    #[test]
    fn a_store_install_only_says_so() {
        let mut m = Machine::managed("Microsoft Store");
        assert_eq!(m.buttons(), [Btn::Ok]);
        assert_eq!(m.on(In::Click(Btn::Ok)), [Do::Close]);
    }

    #[test]
    fn a_late_answer_from_a_worker_changes_nothing() {
        let mut m = Machine::ready(rel());
        assert!(m.on(In::Checked(Ok(None))).is_empty());
        assert!(m.on(In::Progress(1, None)).is_empty());
        assert_eq!(m.screen, Screen::Ready(rel()));
    }
}
