//! The window on plain Win32 (adapted from Rustshot's `src/wind_win.rs`,
//! Apache-2.0): the OS draws the title bar, tinted to the theme, so dragging,
//! the caption buttons, resizing and Snap Layouts are the system's own; the
//! client area is one BGRA frame put on screen with GDI, only the rows that
//! changed. File dialogs run on their own threads.

use crate::app::App;
use crate::shell::{Button, CursorKind, Host, Key, Mods, Picked, Wake};
use crate::theme::Theme;
use anyhow::{Result, anyhow};
use std::cell::Cell;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_BORDER_COLOR, DWMWA_CAPTION_COLOR, DWMWA_TEXT_COLOR, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BeginPaint, DIB_RGB_COLORS, EndPaint, GetMonitorInfoW, InvalidateRect, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MonitorFromPoint, PAINTSTRUCT, SetDIBitsToDevice, UpdateWindow,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, GetDpiForWindow, SetThreadDpiAwarenessContext};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent, VK_CONTROL, VK_LWIN, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const CLASS: PCWSTR = w!("mp4_splitter_window");
/// Something finished on another thread.
const WM_WAKE: u32 = WM_APP + 1;
/// The ticking timer while Windows runs its own loop (a move or resize).
const TIMER_SIZING: usize = 1;
const SIZING_MS: u32 = 16;
const WM_MOUSELEAVE: u32 = 0x02A3;
const INFINITE: u32 = u32::MAX;
/// The smallest client area, in logical pixels.
const MIN: (i32, i32) = (760, 560);
const START: (i32, i32) = (1180, 780);

struct Win32Host {
    hwnd: HWND,
    cursor: Cell<CursorKind>,
}

/// A window handle other threads can post to.
#[derive(Clone, Copy)]
struct Post(isize);

// SAFETY: the handle is only given to PostMessageW, which any thread may call.
unsafe impl Send for Post {}
unsafe impl Sync for Post {}

impl Post {
    fn hwnd(self) -> HWND {
        HWND(self.0 as *mut _)
    }
}

fn set_cursor(c: CursorKind) {
    let id = match c {
        CursorKind::Default => IDC_ARROW,
        CursorKind::Pointer => IDC_HAND,
        CursorKind::EwResize => IDC_SIZEWE,
        CursorKind::Grabbing => IDC_SIZEALL,
    };
    // SAFETY: a shared system cursor; nothing to free.
    unsafe {
        if let Ok(h) = LoadCursorW(None, id) {
            SetCursor(Some(h));
        }
    }
}

impl Host for Win32Host {
    fn size(&self) -> (u32, u32) {
        let mut r = RECT::default();
        // SAFETY: the window lives as long as the app.
        let _ = unsafe { GetClientRect(self.hwnd, &mut r) };
        (r.right.max(0) as u32, r.bottom.max(0) as u32)
    }

    fn scale(&self) -> f32 {
        // SAFETY: the window lives as long as the app.
        match unsafe { GetDpiForWindow(self.hwnd) } {
            0 => 1.0,
            dpi => dpi as f32 / 96.0,
        }
    }

    fn redraw(&self, rows: Option<(u32, u32)>) {
        let (w, _) = self.size();
        let r = rows.map(|(y0, y1)| RECT { left: 0, top: y0 as i32, right: w as i32, bottom: y1 as i32 });
        // SAFETY: the window lives as long as the app; the rectangle outlives the call.
        let _ = unsafe { InvalidateRect(Some(self.hwnd), r.as_ref().map(std::ptr::from_ref), false) };
    }

    fn set_title(&self, title: &str) {
        let t: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        // SAFETY: `t` is NUL-terminated and outlives the call.
        let _ = unsafe { SetWindowTextW(self.hwnd, PCWSTR(t.as_ptr())) };
    }

    fn set_cursor(&self, c: CursorKind) {
        if self.cursor.replace(c) != c {
            set_cursor(c);
        }
    }

    fn set_colors(&self, t: &Theme) {
        let rgb = |c: crate::paint::C| c.r as u32 | (c.g as u32) << 8 | (c.b as u32) << 16;
        for (a, v) in [
            (DWMWA_USE_IMMERSIVE_DARK_MODE, u32::from(t.dark)),
            (DWMWA_CAPTION_COLOR, rgb(t.bg)),
            (DWMWA_TEXT_COLOR, rgb(t.text)),
            (DWMWA_BORDER_COLOR, rgb(t.bg)),
        ] {
            // SAFETY: `v` is the 4 bytes given; versions without an attribute refuse it.
            let _ = unsafe { DwmSetWindowAttribute(self.hwnd, a, (&raw const v).cast(), 4) };
        }
    }

    fn wake(&self) -> Wake {
        let p = Post(self.hwnd.0 as isize);
        Arc::new(move || {
            let p = p;
            // SAFETY: posting to a window that is gone just fails.
            let _ = unsafe { PostMessageW(Some(p.hwnd()), WM_WAKE, WPARAM(0), LPARAM(0)) };
        })
    }

    fn pick_file(&self, dir: Option<PathBuf>, done: Picked) {
        let owner = Post(self.hwnd.0 as isize);
        std::thread::spawn(move || {
            let owner = owner;
            done(crate::dialogs::pick_file(owner.hwnd(), dir.as_deref()));
        });
    }

    fn pick_folder(&self, dir: Option<PathBuf>, done: Picked) {
        let owner = Post(self.hwnd.0 as isize);
        std::thread::spawn(move || {
            let owner = owner;
            done(crate::dialogs::pick_folder(owner.hwnd(), dir.as_deref()));
        });
    }
}

/// What the window procedure reaches.
struct State {
    app: Option<App>,
    /// Inside a call to the app (a message sent meanwhile must not reach it).
    busy: bool,
    tracking: bool,
}

/// Run `f` on the app, unless the app is what sent this message.
unsafe fn with_app<T>(st: *mut State, f: impl FnOnce(&mut App) -> T) -> Option<T> {
    // SAFETY: `st` is `run`'s state, alive while the window is, used on this thread only.
    unsafe {
        if (*st).busy {
            return None;
        }
        (*st).busy = true;
        let r = (*st).app.as_mut().map(f);
        (*st).busy = false;
        r
    }
}

fn mods_now() -> Mods {
    let down = |vk: i32| unsafe { GetKeyState(vk) } as u16 & 0x8000 != 0;
    Mods { shift: down(VK_SHIFT.0 as i32), ctrl: down(VK_CONTROL.0 as i32) || down(VK_LWIN.0 as i32) || down(VK_RWIN.0 as i32) }
}

/// A key by its virtual-key code: shortcuts work on any keyboard layout.
fn key_of(vk: u32) -> Option<Key> {
    Some(match vk {
        0x20 => Key::Space,
        0x25 => Key::Left,
        0x27 => Key::Right,
        0x26 => Key::Up,
        0x28 => Key::Down,
        0x21 => Key::PageUp,
        0x22 => Key::PageDown,
        0x24 => Key::Home,
        0x23 => Key::End,
        0x2E => Key::Delete,
        0x08 => Key::Backspace,
        0x1B => Key::Escape,
        0x0D => Key::Enter,
        0x70 => Key::F1,
        0x30..=0x39 => Key::Char(vk as u8 as char),
        0x41..=0x5A => Key::Char((vk as u8 as char).to_ascii_lowercase()),
        0x60..=0x69 => Key::Char((b'0' + (vk - 0x60) as u8) as char),
        0x6B | 0xBB => Key::Char('='),
        0x6D | 0xBD => Key::Char('-'),
        0xBC => Key::Char(','),
        0xBE => Key::Char('.'),
        0xDB => Key::Char('['),
        0xDD => Key::Char(']'),
        0xDC => Key::Char('\\'),
        _ => return None,
    })
}

fn xy(lp: LPARAM) -> (f32, f32) {
    ((lp.0 & 0xFFFF) as i16 as f32, ((lp.0 >> 16) & 0xFFFF) as i16 as f32)
}

/// Is the OS in dark mode? (`None`: it does not say.)
fn os_dark() -> Option<bool> {
    let (mut v, mut size) = (0u32, 4u32);
    // SAFETY: `v` and `size` outlive the call and are what it writes.
    let r = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            w!("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"),
            w!("AppsUseLightTheme"),
            RRF_RT_REG_DWORD,
            None,
            Some((&raw mut v).cast()),
            Some(&raw mut size),
        )
    };
    r.is_ok().then_some(v == 0)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    // SAFETY: the pointer in GWLP_USERDATA is `run`'s state, cleared before it goes.
    unsafe {
        let st = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut State;
        if st.is_null() {
            return DefWindowProcW(hwnd, msg, wp, lp);
        }
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                with_app(st, |a| {
                    let f = a.paint();
                    let mut bmi = BITMAPINFO::default();
                    bmi.bmiHeader = BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: f.w as i32,
                        biHeight: -(f.h as i32),
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    };
                    // The dirty rectangle only (DIB rows are counted from the bottom).
                    let r = ps.rcPaint;
                    let (x, y, w, h) = (r.left, r.top, r.right - r.left, r.bottom - r.top);
                    SetDIBitsToDevice(hdc, x, y, w as u32, h as u32, x, f.h as i32 - y - h, 0, f.h, f.bgra.as_ptr().cast(), &bmi, DIB_RGB_COLORS);
                });
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_SIZE => {
                if wp.0 != SIZE_MINIMIZED as usize {
                    with_app(st, App::on_resize);
                }
                LRESULT(0)
            }
            WM_GETMINMAXINFO => {
                let dpi = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
                let mut rc = RECT { left: 0, top: 0, right: (MIN.0 as f32 * dpi) as i32, bottom: (MIN.1 as f32 * dpi) as i32 };
                let style = WINDOW_STYLE(GetWindowLongPtrW(hwnd, GWL_STYLE) as u32);
                let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), GetDpiForWindow(hwnd).max(96));
                (*(lp.0 as *mut MINMAXINFO)).ptMinTrackSize = POINT { x: rc.right - rc.left, y: rc.bottom - rc.top };
                LRESULT(0)
            }
            WM_DPICHANGED => {
                // Moved to a monitor with another scale: the size Windows suggests.
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                LRESULT(0)
            }
            WM_SETTINGCHANGE => {
                if lp.0 != 0 && PCWSTR(lp.0 as *const u16).to_string().is_ok_and(|s| s == "ImmersiveColorSet") {
                    with_app(st, |a| a.on_theme(os_dark()));
                }
                DefWindowProcW(hwnd, msg, wp, lp)
            }
            WM_MOUSEMOVE => {
                if !(*st).tracking {
                    (*st).tracking = true;
                    let mut t = TRACKMOUSEEVENT { cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32, dwFlags: TME_LEAVE, hwndTrack: hwnd, dwHoverTime: 0 };
                    let _ = TrackMouseEvent(&mut t);
                }
                let (x, y) = xy(lp);
                with_app(st, |a| a.on_move(x, y));
                LRESULT(0)
            }
            WM_MOUSELEAVE => {
                (*st).tracking = false;
                with_app(st, App::on_leave);
                LRESULT(0)
            }
            WM_LBUTTONDOWN | WM_LBUTTONUP | WM_MBUTTONDOWN | WM_MBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP => {
                let down = matches!(msg, WM_LBUTTONDOWN | WM_MBUTTONDOWN | WM_RBUTTONDOWN);
                let b = match msg {
                    WM_LBUTTONDOWN | WM_LBUTTONUP => Button::Left,
                    WM_MBUTTONDOWN | WM_MBUTTONUP => Button::Middle,
                    _ => Button::Right,
                };
                if down {
                    SetCapture(hwnd);
                } else {
                    let _ = ReleaseCapture();
                }
                let (x, y) = xy(lp);
                let mods = mods_now();
                with_app(st, |a| {
                    a.on_mods(mods);
                    a.on_move(x, y);
                    a.on_button(b, down);
                });
                LRESULT(0)
            }
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
                let v = ((wp.0 >> 16) & 0xFFFF) as i16 as f64 / WHEEL_DELTA as f64;
                let mods = mods_now();
                let (dx, dy) = if msg == WM_MOUSEWHEEL { (0.0, v) } else { (-v, 0.0) };
                with_app(st, |a| {
                    a.on_mods(mods);
                    a.on_wheel(dx, dy);
                });
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if let Some(k) = key_of(wp.0 as u32) {
                    let mods = mods_now();
                    with_app(st, |a| {
                        a.on_mods(mods);
                        a.on_key(k);
                    });
                }
                LRESULT(0)
            }
            WM_KEYUP => {
                let mods = mods_now();
                with_app(st, |a| a.on_mods(mods));
                LRESULT(0)
            }
            WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
                // The app sets it as the pointer moves; keep it from the class's.
                LRESULT(1)
            }
            WM_DROPFILES => {
                let drop = HDROP(wp.0 as *mut _);
                let mut buf = [0u16; 4096];
                let n = DragQueryFileW(drop, 0, Some(&mut buf)) as usize;
                DragFinish(drop);
                if n > 0 {
                    let p = PathBuf::from(String::from_utf16_lossy(&buf[..n]));
                    with_app(st, |a| a.on_drop(&p));
                }
                LRESULT(0)
            }
            WM_WAKE => {
                with_app(st, App::on_wake);
                LRESULT(0)
            }
            WM_ENTERSIZEMOVE => {
                SetTimer(Some(hwnd), TIMER_SIZING, SIZING_MS, None);
                LRESULT(0)
            }
            WM_EXITSIZEMOVE => {
                let _ = KillTimer(Some(hwnd), TIMER_SIZING);
                LRESULT(0)
            }
            WM_TIMER => {
                with_app(st, App::tick);
                LRESULT(0)
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Open the window and run until it closes.
pub fn run(open: Option<PathBuf>) -> Result<()> {
    // SAFETY: plain Win32 on this thread; `st` outlives the window (cleared from
    // the window before it goes), and handles are used as Windows documents.
    unsafe {
        SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let module = GetModuleHandleW(None)?;
        let inst = HINSTANCE(module.0);
        // The icon embedded by build.rs (resource 1): title bar, Alt+Tab, taskbar.
        let icon = |m: SYSTEM_METRICS_INDEX, n: SYSTEM_METRICS_INDEX| {
            LoadImageW(Some(inst), PCWSTR(std::ptr::without_provenance(1)), IMAGE_ICON, GetSystemMetrics(m), GetSystemMetrics(n), LR_SHARED)
                .map(|i| HICON(i.0))
                .unwrap_or_default()
        };
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: inst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: icon(SM_CXICON, SM_CYICON),
            hIconSm: icon(SM_CXSMICON, SM_CYSMICON),
            lpszClassName: CLASS,
            ..Default::default()
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(anyhow!("RegisterClassExW: {}", windows::core::Error::from_thread()));
        }
        // Centred on the monitor under the pointer, in its scale.
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO { cbSize: std::mem::size_of::<MONITORINFO>() as u32, ..Default::default() };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        let style = WS_OVERLAPPEDWINDOW;
        let mut dx = 96;
        let mut dy = 96;
        let _ = windows::Win32::UI::HiDpi::GetDpiForMonitor(mon, windows::Win32::UI::HiDpi::MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let k = dx.max(96) as f32 / 96.0;
        let mut rc = RECT { left: 0, top: 0, right: (START.0 as f32 * k) as i32, bottom: (START.1 as f32 * k) as i32 };
        let _ = AdjustWindowRectExForDpi(&mut rc, style, false, WINDOW_EX_STYLE(0), dx.max(96));
        let (ow, oh) = ((rc.right - rc.left).min(wa.right - wa.left), (rc.bottom - rc.top).min(wa.bottom - wa.top));
        let (x, y) = (wa.left + (wa.right - wa.left - ow) / 2, wa.top + (wa.bottom - wa.top - oh) / 2);
        let hwnd = CreateWindowExW(WINDOW_EX_STYLE(0), CLASS, w!("MP4 Splitter"), style, x, y, ow, oh, None, None, Some(inst), None)?;
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_WINDOW_CORNER_PREFERENCE, (&raw const round).cast(), 4);
        DragAcceptFiles(hwnd, true);
        let host = Win32Host { hwnd, cursor: Cell::new(CursorKind::Default) };
        // Themed before it is first shown: no white title bar flash.
        let app = App::new(Box::new(host), os_dark(), open);
        let mut st = State { app: Some(app), busy: false, tracking: false };
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, (&raw mut st) as isize);
        let _ = ShowWindow(hwnd, SW_SHOWNORMAL);
        let _ = SetForegroundWindow(hwnd);
        let _ = UpdateWindow(hwnd);
        'run: loop {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    break 'run;
                }
                DispatchMessageW(&msg);
            }
            let t = with_app(&raw mut st, App::tick).unwrap_or_default();
            if t.exit {
                let _ = DestroyWindow(hwnd);
                continue;
            }
            let timeout = t.wait.map_or(INFINITE, |at| at.saturating_duration_since(Instant::now()).as_millis().min(u32::MAX as u128 - 1) as u32);
            MsgWaitForMultipleObjectsEx(None, timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
        drop(st);
        Ok(())
    }
}
