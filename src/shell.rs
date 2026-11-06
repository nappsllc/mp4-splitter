//! What the app needs of the window system, and what it gets from it. Windows
//! implements this by hand on Win32 (`shell_win.rs`); macOS and Linux on winit
//! and softbuffer (`shell_winit.rs`).

use crate::theme::Theme;
use std::path::PathBuf;
use std::sync::Arc;

/// Wakes the app's thread from another one.
pub type Wake = Arc<dyn Fn() + Send + Sync>;

/// What a file dialog gives back (`None`: cancelled).
pub type Picked = Box<dyn FnOnce(Option<PathBuf>) + Send>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    Space,
    Left,
    Right,
    Up,
    Down,
    PageUp,
    PageDown,
    Home,
    End,
    Delete,
    Backspace,
    Escape,
    Enter,
    F1,
    /// A letter (lower case), digit or punctuation key.
    Char(char),
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mods {
    pub shift: bool,
    /// Control, or Command on macOS.
    pub ctrl: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Middle,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorKind {
    Default,
    Pointer,
    EwResize,
    Grabbing,
}

/// The window, as the app sees it.
pub trait Host {
    /// The client area in physical pixels.
    fn size(&self) -> (u32, u32);
    /// Physical pixels a logical one.
    fn scale(&self) -> f32;
    /// Repaint these rows (`y0..y1`), or everything.
    fn redraw(&self, rows: Option<(u32, u32)>);
    fn set_title(&self, title: &str);
    fn set_cursor(&self, c: CursorKind);
    /// Give the title bar the theme's colours, where the OS can.
    fn set_colors(&self, t: &Theme);
    fn wake(&self) -> Wake;
    /// Ask for a video to open, starting in `dir`; `done` may run on another thread.
    fn pick_file(&self, dir: Option<PathBuf>, done: Picked);
    /// Ask for a folder, starting in `dir`; `done` may run on another thread.
    fn pick_folder(&self, dir: Option<PathBuf>, done: Picked);
}

/// When the app wants the next [`crate::app::App::tick`], and whether it is done.
#[derive(Clone, Copy, Debug, Default)]
pub struct Tick {
    pub wait: Option<std::time::Instant>,
    pub exit: bool,
}
