//! The system's file dialogs (IFileOpenDialog; from Rustshot's
//! `src/ui/folder_dialog.rs`, Apache-2.0). Blocking: each is called on a thread
//! of its own, modal to the window.

use std::path::{Path, PathBuf};
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, COINIT_DISABLE_OLE1DDE, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize,
};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog, IFileOpenDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows::core::{HSTRING, PWSTR, w};

/// Balances a successful CoInitializeEx.
struct Com(bool);

impl Com {
    fn sta() -> Com {
        // SAFETY: paired with CoUninitialize on this thread in `drop`.
        Com(unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED | COINIT_DISABLE_OLE1DDE) }.is_ok())
    }
}

impl Drop for Com {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: this thread's CoInitializeEx succeeded.
            unsafe { CoUninitialize() };
        }
    }
}

fn show(owner: HWND, dir: Option<&Path>, folders: bool) -> Option<PathBuf> {
    let _com = Com::sta();
    // SAFETY: COM calls on this STA thread; the strings outlive the calls that read them.
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        if folders {
            dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST).ok()?;
            dlg.SetTitle(w!("Export the pieces to")).ok()?;
        } else {
            dlg.SetOptions(opts | FOS_FILEMUSTEXIST | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST).ok()?;
            dlg.SetTitle(w!("Open a video")).ok()?;
            let types = [COMDLG_FILTERSPEC { pszName: w!("Video"), pszSpec: w!("*.mp4;*.m4v;*.mov") }];
            dlg.SetFileTypes(&types).ok()?;
        }
        if let Some(d) = dir.filter(|d| d.is_dir()) {
            let p = HSTRING::from(d.as_os_str());
            if let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&p, None) {
                let _ = dlg.SetFolder(&item);
            }
        }
        dlg.Show(Some(owner)).ok()?; // an error when cancelled
        let item = dlg.GetResult().ok()?;
        let name: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let path = name.to_string().ok();
        CoTaskMemFree(Some(name.0 as *const _));
        path.map(PathBuf::from)
    }
}

/// Ask for a video, starting in `dir`.
pub fn pick_file(owner: HWND, dir: Option<&Path>) -> Option<PathBuf> {
    show(owner, dir, false)
}

/// Ask for a folder, starting in `dir`.
pub fn pick_folder(owner: HWND, dir: Option<&Path>) -> Option<PathBuf> {
    show(owner, dir, true)
}
