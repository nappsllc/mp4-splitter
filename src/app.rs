//! The window: a preview with playback, transport controls, the split
//! presets and a zoomable timeline of thumbnails and the waveform, with
//! the trim handles, the cuts and the playhead.

use crate::audio::Output;
use crate::export::{self, Progress};
use crate::paint::{Align, C, Canvas, Icon, Rect, Text, pixmap};
use crate::player::{Playback, Source, Stills, Thumbs, Wake};
use crate::split::{self, Marker, Plan};
use crate::theme::{self, Theme};
use crate::{ffmpeg, mp4};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

#[derive(Debug, Clone, Copy)]
pub enum UserEvent {
    Wake,
}

/// What a click on a control does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Act {
    Open,
    Export,
    Play,
    FrameBack,
    FrameFwd,
    ToStart,
    ToEnd,
    MarkIn,
    MarkOut,
    Cut,
    Snap,
    Mute,
    Preset(usize),
    NMinus,
    NPlus,
    NApply,
    ClearCuts,
    ShowFolder,
    CloseToast,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    Scrub,
    Marker(Marker),
    /// Panning the timeline: where the drag began (x) and the view then.
    Pan(f32, Duration, Duration),
}

/// An open file.
struct Doc {
    path: PathBuf,
    info: mp4::Info,
    plan: Plan,
    src: Source,
    stills: Stills,
    thumbs: Thumbs,
    peaks: Arc<Mutex<Vec<f32>>>,
    /// The picture shown and its time.
    frame: Option<(Duration, Pixmap)>,
    last_still: u64,
    /// The timeline's visible range.
    view: (Duration, Duration),
}

impl Doc {
    fn snap_key(&self, t: Duration) -> Duration {
        split::nearest(&self.info.keys, t).unwrap_or(t)
    }
    fn snap_frame(&self, t: Duration) -> Duration {
        split::nearest(&self.info.frames, t).unwrap_or(t)
    }
}

struct Toast {
    text: String,
    error: bool,
    at: Instant,
    reveal: Option<PathBuf>,
}

/// Where everything is (physical pixels).
#[derive(Clone, Copy, Default)]
struct Layout {
    s: f32,
    w: f32,
    h: f32,
    header: Rect,
    preview: Rect,
    controls: Rect,
    tools: Rect,
    card: Rect,
    /// The time axis: x range of the timeline (all its rows).
    track: Rect,
    ruler: Rect,
    thumbs: Rect,
    wave: Rect,
    labels: Rect,
    hint: Rect,
}

fn layout(w: f32, h: f32, s: f32) -> Layout {
    let pad = 16.0 * s;
    let header = Rect::new(0.0, 0.0, w, 56.0 * s);
    let hint = Rect::new(pad, h - 30.0 * s, w - 2.0 * pad, 30.0 * s);
    let card_h = (22.0 + 60.0 + 36.0 + 26.0 + 12.0) * s;
    let card = Rect::new(pad, hint.y - card_h, w - 2.0 * pad, card_h);
    let tools = Rect::new(pad, card.y - 48.0 * s, w - 2.0 * pad, 48.0 * s);
    let controls = Rect::new(pad, tools.y - 56.0 * s, w - 2.0 * pad, 56.0 * s);
    let preview = Rect::new(pad, header.bottom(), w - 2.0 * pad, (controls.y - header.bottom() - 4.0 * s).max(40.0 * s));
    let track = Rect::new(card.x + 18.0 * s, card.y + 6.0 * s, card.w - 36.0 * s, card.h - 12.0 * s);
    let ruler = Rect::new(track.x, track.y, track.w, 22.0 * s);
    let thumbs = Rect::new(track.x, ruler.bottom(), track.w, 60.0 * s);
    let wave = Rect::new(track.x, thumbs.bottom(), track.w, 36.0 * s);
    let labels = Rect::new(track.x, wave.bottom(), track.w, 26.0 * s);
    Layout { s, w, h, header, preview, controls, tools, card, track, ruler, thumbs, wave, labels, hint }
}

pub struct App {
    proxy: EventLoopProxy<UserEvent>,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    pm: Option<Pixmap>,
    text: Option<Text>,
    theme: Theme,
    l: Layout,
    ffmpeg: Option<PathBuf>,
    audio: Option<Output>,
    doc: Option<Doc>,
    playhead: Duration,
    playing: Option<Playback>,
    last_paint: Instant,
    muted: bool,
    snap: bool,
    n: usize,
    mouse: (f32, f32),
    mods: ModifiersState,
    hits: Vec<(Rect, Act, &'static str)>,
    hover: Option<Act>,
    hover_since: Instant,
    pressed: Option<Act>,
    drag: Option<Drag>,
    /// Play again when the scrub ends.
    resume: bool,
    last_click: (Instant, f32),
    toast: Option<Toast>,
    exporting: Option<Arc<Mutex<Progress>>>,
    drop_hover: bool,
    pending_open: Option<PathBuf>,
    /// Quit after this long (tests and screenshots).
    autoclose: Option<Instant>,
}

impl App {
    pub fn new(proxy: EventLoopProxy<UserEvent>, open: Option<PathBuf>) -> App {
        let autoclose = std::env::var("MP4_SPLITTER_AUTOCLOSE_MS").ok().and_then(|v| v.parse().ok()).map(|ms| Instant::now() + Duration::from_millis(ms));
        App {
            proxy,
            window: None,
            surface: None,
            pm: None,
            text: Some(Text::new()),
            theme: theme::DARK,
            l: Layout::default(),
            ffmpeg: ffmpeg::find(),
            audio: None,
            doc: None,
            playhead: Duration::ZERO,
            playing: None,
            last_paint: Instant::now(),
            muted: false,
            snap: false,
            n: 6,
            mouse: (-1.0, -1.0),
            mods: ModifiersState::default(),
            hits: Vec::new(),
            hover: None,
            hover_since: Instant::now(),
            pressed: None,
            drag: None,
            resume: false,
            last_click: (Instant::now() - Duration::from_secs(9), 0.0),
            toast: None,
            exporting: None,
            drop_hover: false,
            pending_open: open,
            autoclose,
        }
    }

    fn wake(&self) -> Wake {
        let p = self.proxy.clone();
        Arc::new(move || {
            let _ = p.send_event(UserEvent::Wake);
        })
    }

    fn redraw(&self) {
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool, reveal: Option<PathBuf>) {
        self.toast = Some(Toast { text: text.into(), error, at: Instant::now(), reveal });
        self.redraw();
    }

    // --- files ---------------------------------------------------------------

    fn open_dialog(&mut self) {
        let mut d = rfd::FileDialog::new().add_filter("Video", &["mp4", "m4v", "mov", "MP4", "M4V", "MOV"]).set_title("Open a video");
        if let Some(dir) = self.doc.as_ref().and_then(|d| d.path.parent()) {
            d = d.set_directory(dir);
        }
        if let Some(p) = d.pick_file() {
            self.open(&p);
        }
    }

    fn open(&mut self, path: &Path) {
        self.stop();
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let path = abs.as_path();
        let info = match std::fs::File::open(path).map_err(anyhow::Error::from).and_then(|f| mp4::info(&mut std::io::BufReader::new(f))) {
            Ok(i) => i,
            Err(e) => {
                self.say(format!("Can't open {}: {e:#}", file_name(path)), true, None);
                return;
            }
        };
        if info.duration.is_zero() {
            self.say(format!("{} has nothing to play", file_name(path)), true, None);
            return;
        }
        let src = Source { ffmpeg: self.ffmpeg.clone(), path: path.to_path_buf(), frames: Arc::new(info.frames.clone()), has_audio: info.has_audio };
        let s = self.l.s.max(1.0);
        let th = (60.0 * s).round() as u32;
        let (vw, vh) = info.size.unwrap_or((16, 9));
        let tw = ((th as f32 * vw as f32 / vh.max(1) as f32).round() as u32).clamp(th / 2, th * 3).max(2) & !1;
        let wake = self.wake();
        let doc = Doc {
            path: path.to_path_buf(),
            plan: Plan::new(info.duration),
            stills: Stills::new(src.clone(), wake.clone()),
            thumbs: Thumbs::new(src.clone(), (tw, th), wake.clone()),
            peaks: crate::player::load_peaks(&src, wake),
            view: (Duration::ZERO, info.duration),
            frame: None,
            last_still: 0,
            src,
            info,
        };
        self.doc = Some(doc);
        self.playhead = Duration::ZERO;
        self.toast = None;
        if let Some(w) = &self.window {
            w.set_title(&format!("{} — MP4 Splitter", file_name(path)));
        }
        if self.ffmpeg.is_none() {
            self.say("ffmpeg was not found: no pictures or sound, but splitting works. Install ffmpeg to see the video.", true, None);
        }
        self.request_still();
        self.redraw();
    }

    fn export(&mut self) {
        if self.exporting.is_some() {
            return;
        }
        let Some(d) = &self.doc else { return self.open_dialog() };
        let dir = if self.mods.shift_key() {
            match rfd::FileDialog::new().set_title("Export the pieces to").set_directory(d.path.parent().unwrap_or(Path::new("."))).pick_folder() {
                Some(dir) => dir,
                None => return,
            }
        } else {
            d.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
        };
        let pieces = d.plan.pieces();
        let outs = split::output_paths(&d.path, &dir, pieces.len());
        self.exporting = Some(export::start(d.path.clone(), pieces, outs, self.wake()));
        self.redraw();
    }

    fn check_export(&mut self) {
        let Some(p) = &self.exporting else { return };
        let r = p.lock().unwrap_or_else(|e| e.into_inner()).result.clone();
        match r {
            None => {}
            Some(Ok(files)) => {
                self.exporting = None;
                let n = files.len();
                let first = files.first().cloned();
                let dir = first.as_ref().and_then(|f| f.parent()).map(|d| d.display().to_string()).unwrap_or_default();
                let what = if n == 1 { format!("Saved {}", first.as_deref().map(file_name).unwrap_or_default()) } else { format!("Saved {n} pieces") };
                self.say(format!("{what} to {dir}"), false, first);
            }
            Some(Err(e)) => {
                self.exporting = None;
                self.say(format!("Export failed: {e}"), true, None);
            }
        }
    }

    // --- playing -------------------------------------------------------------

    fn video_size(&self) -> (u32, u32) {
        let Some(d) = &self.doc else { return (0, 0) };
        let (vw, vh) = d.info.size.unwrap_or((16, 9));
        let r = fit(vw as f32, vh as f32, self.l.preview);
        // Never larger than the video; even sizes keep ffmpeg's scaler happy.
        let k = (r.w / vw as f32).min(1.0);
        let w = ((vw as f32 * k).round() as u32).max(2) & !1;
        let h = ((vh as f32 * k).round() as u32).max(2) & !1;
        (w, h)
    }

    fn request_still(&mut self) {
        let size = self.video_size();
        let t = self.playhead;
        if let Some(d) = &mut self.doc {
            if size.0 > 0 && d.src.ffmpeg.is_some() && !d.info.frames.is_empty() {
                d.last_still = d.stills.request(t, size);
            }
        }
    }

    fn play(&mut self) {
        if self.playing.is_some() {
            return self.pause();
        }
        let size = self.video_size();
        let Some(d) = &self.doc else { return };
        if self.playhead + Duration::from_millis(50) >= d.info.duration {
            self.playhead = Duration::ZERO;
        }
        if self.audio.is_none() && d.info.has_audio {
            self.audio = Output::open();
            if let Some(a) = &self.audio {
                a.set_muted(self.muted);
            }
        }
        self.playing = Some(Playback::start(&d.src, self.playhead, size, self.audio.as_ref(), self.wake()));
        self.redraw();
    }

    fn pause(&mut self) {
        if let Some(pb) = self.playing.take() {
            self.playhead = pb.clock(self.audio.as_ref()).min(self.duration());
            // Land on a frame, so stepping goes from there.
            if let Some(d) = &self.doc {
                if let Some(&f) = d.info.frames.get(split::frame_index(&d.info.frames, self.playhead)) {
                    self.playhead = f;
                }
            }
            pb.stop(self.audio.as_ref());
            self.request_still();
        }
        self.redraw();
    }

    fn stop(&mut self) {
        if let Some(pb) = self.playing.take() {
            pb.stop(self.audio.as_ref());
        }
    }

    fn duration(&self) -> Duration {
        self.doc.as_ref().map_or(Duration::ZERO, |d| d.info.duration)
    }

    /// Move the playhead to `t` (paused: the exact picture is asked for).
    fn seek(&mut self, t: Duration) {
        let t = t.min(self.duration());
        let was = self.playing.is_some();
        self.stop();
        self.playhead = t;
        self.follow();
        if was && self.drag.is_none() {
            self.play();
        } else {
            self.request_still();
        }
        self.redraw();
    }

    fn step_frame(&mut self, dir: i32) {
        let Some(d) = &self.doc else { return };
        let t = if self.playing.is_some() { self.playing.as_ref().map_or(self.playhead, |p| p.clock(self.audio.as_ref())) } else { self.playhead };
        let list = &d.info.frames;
        let to = if list.is_empty() {
            let f = Duration::from_secs_f64(1.0 / 30.0);
            if dir > 0 { t + f } else { t.saturating_sub(f) }
        } else {
            let cur = list[split::frame_index(list, t)];
            split::step(list, cur, dir).unwrap_or(cur)
        };
        self.stop();
        self.seek(to);
    }

    fn jump(&mut self, by: f64) {
        let t = self.playhead.as_secs_f64() + by;
        let t = Duration::from_secs_f64(t.max(0.0));
        let t = self.doc.as_ref().map_or(t, |d| d.snap_frame(t));
        self.seek(t);
    }

    /// Keep the playhead in view.
    fn follow(&mut self) {
        let t = self.playhead;
        let Some(d) = &mut self.doc else { return };
        let (a, b) = d.view;
        let span = b - a;
        if t < a || t > b {
            let a2 = t.saturating_sub(span / 10).min(d.info.duration.saturating_sub(span));
            d.view = (a2, a2 + span);
        }
    }

    // --- the timeline --------------------------------------------------------

    fn x_of(&self, t: Duration) -> f32 {
        let Some(d) = &self.doc else { return self.l.track.x };
        let (a, b) = d.view;
        let span = (b - a).as_secs_f64().max(1e-6);
        self.l.track.x + ((t.as_secs_f64() - a.as_secs_f64()) / span) as f32 * self.l.track.w
    }

    fn t_of(&self, x: f32) -> Duration {
        let Some(d) = &self.doc else { return Duration::ZERO };
        let (a, b) = d.view;
        let f = ((x - self.l.track.x) / self.l.track.w.max(1.0)).clamp(0.0, 1.0) as f64;
        Duration::from_secs_f64(a.as_secs_f64() + f * (b - a).as_secs_f64()).min(d.info.duration)
    }

    /// Zoom the timeline by `k` (< 1: in) around `at`.
    fn zoom(&mut self, k: f64, at: Duration) {
        let Some(d) = &mut self.doc else { return };
        let dur = d.info.duration.as_secs_f64();
        let (a, b) = (d.view.0.as_secs_f64(), d.view.1.as_secs_f64());
        let min_span = (40.0 / d.info.fps()).max(0.25).min(dur);
        let span = ((b - a) * k).clamp(min_span, dur);
        let f = if b > a { (at.as_secs_f64() - a) / (b - a) } else { 0.5 };
        let a2 = (at.as_secs_f64() - f * span).clamp(0.0, dur - span);
        d.view = (Duration::from_secs_f64(a2), Duration::from_secs_f64(a2 + span));
        d.thumbs.forget_pending();
        self.redraw();
    }

    fn pan(&mut self, by_px: f32) {
        let Some(d) = &mut self.doc else { return };
        let (a, b) = (d.view.0.as_secs_f64(), d.view.1.as_secs_f64());
        let dur = d.info.duration.as_secs_f64();
        let dt = by_px as f64 / self.l.track.w.max(1.0) as f64 * (b - a);
        let a2 = (a + dt).clamp(0.0, (dur - (b - a)).max(0.0));
        d.view = (Duration::from_secs_f64(a2), Duration::from_secs_f64(a2 + (b - a)));
        d.thumbs.forget_pending();
        self.redraw();
    }

    /// The marker under `x` on the timeline (handles first).
    fn marker_at(&self, x: f32, y: f32) -> Option<Marker> {
        let d = self.doc.as_ref()?;
        if y < self.l.ruler.y || y > self.l.wave.bottom() {
            return None;
        }
        let s = self.l.s;
        let near = |t: Duration, r: f32| (self.x_of(t) - x).abs() <= r * s;
        let in_body = y >= self.l.thumbs.y;
        if in_body && x <= self.x_of(d.plan.start) + 2.0 * s && near(d.plan.start, 14.0) {
            return Some(Marker::Start);
        }
        if in_body && x >= self.x_of(d.plan.end) - 2.0 * s && near(d.plan.end, 14.0) {
            return Some(Marker::End);
        }
        let cut = (0..d.plan.cuts.len())
            .filter(|&i| near(d.plan.cuts[i], 7.0))
            .min_by(|&i, &j| (self.x_of(d.plan.cuts[i]) - x).abs().total_cmp(&(self.x_of(d.plan.cuts[j]) - x).abs()));
        if let Some(i) = cut {
            return Some(Marker::Cut(i));
        }
        if in_body && near(d.plan.start, 8.0) {
            return Some(Marker::Start);
        }
        if in_body && near(d.plan.end, 8.0) {
            return Some(Marker::End);
        }
        None
    }

    /// Where a dragged marker goes for the pointer at `x`: a key frame with
    /// Snap on, else a frame; the playhead pulls it within 6 px.
    fn marker_time(&self, x: f32) -> Duration {
        let t = self.t_of(x);
        let Some(d) = &self.doc else { return t };
        if (self.x_of(self.playhead) - x).abs() < 6.0 * self.l.s {
            return self.playhead;
        }
        if self.snap { d.snap_key(t) } else { d.snap_frame(t) }
    }

    fn drag_to(&mut self, x: f32) {
        match self.drag {
            Some(Drag::Scrub) => {
                let t = self.t_of(x);
                let t = self.doc.as_ref().map_or(t, |d| d.snap_frame(t));
                self.seek(t);
            }
            Some(Drag::Marker(m)) => {
                let t = self.marker_time(x);
                let got = self.doc.as_mut().and_then(|d| d.plan.move_marker(m, t));
                if let Some(t) = got {
                    self.playhead = t;
                    self.request_still();
                }
                self.redraw();
            }
            Some(Drag::Pan(x0, a, b)) => {
                if let Some(d) = &mut self.doc {
                    let span = (b - a).as_secs_f64();
                    let dt = (x0 - x) as f64 / self.l.track.w.max(1.0) as f64 * span;
                    let a2 = (a.as_secs_f64() + dt).clamp(0.0, (d.info.duration.as_secs_f64() - span).max(0.0));
                    d.view = (Duration::from_secs_f64(a2), Duration::from_secs_f64(a2 + span));
                }
                self.redraw();
            }
            None => {}
        }
    }

    // --- actions -------------------------------------------------------------

    fn act(&mut self, a: Act) {
        let snap = self.snap;
        match a {
            Act::Open => self.open_dialog(),
            Act::Export => self.export(),
            Act::Play => self.play(),
            Act::FrameBack => self.step_frame(-1),
            Act::FrameFwd => self.step_frame(1),
            Act::ToStart => {
                let t = self.doc.as_ref().map_or(Duration::ZERO, |d| if self.playhead > d.plan.start { d.plan.start } else { Duration::ZERO });
                self.seek(t);
            }
            Act::ToEnd => {
                let t = self.doc.as_ref().map_or(Duration::ZERO, |d| if self.playhead < d.plan.end { d.plan.end } else { d.info.duration });
                self.seek(t);
            }
            Act::MarkIn | Act::MarkOut | Act::Cut => {
                let t = self.playing.as_ref().map_or(self.playhead, |p| p.clock(self.audio.as_ref()));
                let Some(d) = &mut self.doc else { return };
                let t = if snap { d.snap_key(t) } else { t };
                match a {
                    Act::MarkIn => d.plan.mark_in(t),
                    Act::MarkOut => d.plan.mark_out(t),
                    _ => {
                        if d.plan.add_cut(t).is_none() {
                            self.say("A cut needs at least 0.1 s on each side, inside the kept part", true, None);
                        }
                    }
                }
            }
            Act::Snap => self.snap = !self.snap,
            Act::Mute => {
                self.muted = !self.muted;
                if let Some(o) = &self.audio {
                    o.set_muted(self.muted);
                }
            }
            Act::Preset(n) => {
                if let Some(d) = &mut self.doc {
                    let keys = d.info.keys.clone();
                    d.plan.split_equal(n, |t| if snap { split::nearest(&keys, t).unwrap_or(t) } else { t });
                    let got = d.plan.pieces().len();
                    if got < n {
                        self.say(format!("Only {got} pieces fit between the key frames"), true, None);
                    }
                }
            }
            Act::NMinus => self.n = (self.n - 1).max(2),
            Act::NPlus => self.n = (self.n + 1).min(99),
            Act::NApply => self.act(Act::Preset(self.n)),
            Act::ClearCuts => {
                if let Some(d) = &mut self.doc {
                    d.plan.cuts.clear();
                }
            }
            Act::ShowFolder => {
                if let Some(p) = self.toast.as_ref().and_then(|t| t.reveal.clone()) {
                    export::reveal(&p);
                }
            }
            Act::CloseToast => self.toast = None,
        }
        self.redraw();
    }

    fn remove_cut_near_playhead(&mut self) {
        let s = self.l.s;
        let ph = self.x_of(self.playhead);
        let i = self.doc.as_ref().and_then(|d| d.plan.nearest_cut(self.playhead).filter(|&i| (self.x_of(d.plan.cuts[i]) - ph).abs() < 24.0 * s));
        match (i, &mut self.doc) {
            (Some(i), Some(d)) => d.plan.remove_cut(i),
            _ => self.say("Put the playhead on a cut to remove it (↑ / ↓ go from cut to cut)", false, None),
        }
        self.redraw();
    }

    fn key(&mut self, ev: &KeyEvent, el: &ActiveEventLoop) {
        if ev.state != ElementState::Pressed {
            return;
        }
        let ctrl = self.mods.control_key() || self.mods.super_key();
        let shift = self.mods.shift_key();
        match &ev.logical_key {
            Key::Named(NamedKey::Space) => self.play(),
            Key::Named(NamedKey::ArrowLeft) if ctrl => self.jump(-5.0),
            Key::Named(NamedKey::ArrowRight) if ctrl => self.jump(5.0),
            Key::Named(NamedKey::ArrowLeft) if shift => self.jump(-1.0),
            Key::Named(NamedKey::ArrowRight) if shift => self.jump(1.0),
            Key::Named(NamedKey::ArrowLeft) => self.step_frame(-1),
            Key::Named(NamedKey::ArrowRight) => self.step_frame(1),
            Key::Named(NamedKey::ArrowUp) | Key::Named(NamedKey::ArrowDown) => {
                let dir = if matches!(ev.logical_key, Key::Named(NamedKey::ArrowDown)) { 1 } else { -1 };
                if let Some(t) = self.doc.as_ref().and_then(|d| split::step(&d.plan.edges(), self.playhead, dir)) {
                    self.seek(t);
                }
            }
            Key::Named(NamedKey::PageUp) | Key::Named(NamedKey::PageDown) => {
                let dir = if matches!(ev.logical_key, Key::Named(NamedKey::PageDown)) { 1 } else { -1 };
                if let Some(t) = self.doc.as_ref().and_then(|d| split::step(&d.info.keys, self.playhead, dir)) {
                    self.seek(t);
                }
            }
            Key::Named(NamedKey::Home) => self.seek(Duration::ZERO),
            Key::Named(NamedKey::End) => self.seek(self.duration()),
            Key::Named(NamedKey::Delete) | Key::Named(NamedKey::Backspace) => self.remove_cut_near_playhead(),
            Key::Named(NamedKey::Escape) => {
                if self.toast.is_some() {
                    self.toast = None;
                    self.redraw();
                } else if self.playing.is_some() {
                    self.pause();
                }
            }
            Key::Named(NamedKey::Enter) if ctrl => self.export(),
            Key::Character(c) => {
                let c = c.to_lowercase();
                match c.as_str() {
                    "o" if ctrl => self.open_dialog(),
                    "e" | "s" if ctrl => self.export(),
                    "q" | "w" if ctrl => el.exit(),
                    "k" => self.play(),
                    "j" => self.jump(-1.0),
                    "l" => self.jump(1.0),
                    "," => self.step_frame(-1),
                    "." => self.step_frame(1),
                    "i" | "[" => self.act(Act::MarkIn),
                    "o" | "]" => self.act(Act::MarkOut),
                    "s" | "c" | "x" => self.act(Act::Cut),
                    "m" => self.act(Act::Mute),
                    "n" => self.act(Act::Snap),
                    "0" => self.act(Act::ClearCuts),
                    "=" | "+" => self.zoom(1.0 / 1.5, self.playhead),
                    "-" | "_" => self.zoom(1.5, self.playhead),
                    "\\" => self.zoom(1e9, self.playhead),
                    d if d.len() == 1 && ('2'..='9').contains(&d.chars().next().unwrap_or(' ')) => {
                        self.act(Act::Preset(d.parse().unwrap_or(2)));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn mouse_down(&mut self, button: MouseButton) {
        let (x, y) = self.mouse;
        if button == MouseButton::Right {
            if let Some(Marker::Cut(i)) = self.marker_at(x, y) {
                if let Some(d) = &mut self.doc {
                    d.plan.remove_cut(i);
                }
                self.redraw();
            }
            return;
        }
        if button == MouseButton::Middle {
            if let Some(d) = &self.doc {
                if self.l.card.contains(x, y) {
                    self.drag = Some(Drag::Pan(x, d.view.0, d.view.1));
                }
            }
            return;
        }
        if button != MouseButton::Left {
            return;
        }
        if let Some(a) = self.hit(x, y) {
            self.pressed = Some(a);
            self.redraw();
            return;
        }
        if self.doc.is_none() {
            if self.l.preview.contains(x, y) {
                self.open_dialog();
            }
            return;
        }
        let double = self.last_click.0.elapsed() < Duration::from_millis(400) && (self.last_click.1 - x).abs() < 4.0 * self.l.s;
        self.last_click = (Instant::now(), x);
        if let Some(m) = self.marker_at(x, y) {
            if let (true, Marker::Cut(i)) = (double, m) {
                if let Some(d) = &mut self.doc {
                    d.plan.remove_cut(i);
                }
                self.redraw();
                return;
            }
            self.resume = self.playing.is_some();
            self.stop();
            self.drag = Some(Drag::Marker(m));
            self.drag_to(x);
            return;
        }
        let tl = Rect::new(self.l.track.x - 12.0 * self.l.s, self.l.ruler.y, self.l.track.w + 24.0 * self.l.s, self.l.labels.bottom() - self.l.ruler.y);
        if tl.contains(x, y) {
            if double && self.l.thumbs.y <= y && y < self.l.wave.bottom() {
                // Double-click: cut here.
                let t = self.marker_time(x);
                if let Some(d) = &mut self.doc {
                    d.plan.add_cut(t);
                }
                self.redraw();
                return;
            }
            self.resume = self.playing.is_some();
            self.stop();
            self.drag = Some(Drag::Scrub);
            self.drag_to(x);
        } else if self.l.preview.contains(x, y) {
            if double {
                return;
            }
            self.play();
        }
    }

    fn mouse_up(&mut self, button: MouseButton) {
        let (x, y) = self.mouse;
        if button == MouseButton::Left {
            if let Some(a) = self.pressed.take() {
                if self.hit(x, y) == Some(a) {
                    self.act(a);
                }
                self.redraw();
            }
        }
        if self.drag.take().is_some() {
            if std::mem::take(&mut self.resume) {
                self.play();
            } else {
                self.request_still();
            }
            self.redraw();
        }
    }

    fn hit(&self, x: f32, y: f32) -> Option<Act> {
        self.hits.iter().rev().find(|h| h.0.contains(x, y)).map(|h| h.1)
    }

    fn cursor(&self) -> CursorIcon {
        let (x, y) = self.mouse;
        if matches!(self.drag, Some(Drag::Marker(_))) || (self.drag.is_none() && self.marker_at(x, y).is_some()) {
            return CursorIcon::EwResize;
        }
        if matches!(self.drag, Some(Drag::Pan(..))) {
            return CursorIcon::Grabbing;
        }
        if self.hit(x, y).is_some() {
            return CursorIcon::Pointer;
        }
        CursorIcon::Default
    }

    // --- painting ------------------------------------------------------------

    fn paint(&mut self) {
        let Some(window) = self.window.clone() else { return };
        let size = window.inner_size();
        let (w, h) = (size.width.max(1), size.height.max(1));
        let s = window.scale_factor() as f32;
        self.l = layout(w as f32, h as f32, s);
        let mut pm = match self.pm.take() {
            Some(p) if p.width() == w && p.height() == h => p,
            _ => match Pixmap::new(w, h) {
                Some(p) => p,
                None => return,
            },
        };
        let mut text = self.text.take().unwrap_or_else(Text::new);
        self.hits.clear();
        {
            let mut c = Canvas { pm: &mut pm, text: &mut text, s };
            self.draw(&mut c);
        }
        if let Some(surface) = &mut self.surface {
            if let (Some(nw), Some(nh)) = (NonZeroU32::new(w), NonZeroU32::new(h)) {
                if surface.resize(nw, nh).is_ok() {
                    if let Ok(mut buf) = surface.buffer_mut() {
                        for (o, p) in buf.iter_mut().zip(pm.data().chunks_exact(4)) {
                            *o = (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32;
                        }
                        let _ = buf.present();
                    }
                }
            }
        }
        self.pm = Some(pm);
        self.text = Some(text);
        self.last_paint = Instant::now();
    }

    #[allow(clippy::too_many_arguments)]
    fn button(&mut self, c: &mut Canvas, r: Rect, act: Act, tip: &'static str, icon: Option<Icon>, label: Option<&str>, style: Style) {
        let t = self.theme;
        let s = c.s;
        let hover = self.hit_hover(r);
        let down = self.pressed == Some(act) && hover;
        let rad = 8.0 * s;
        match style {
            Style::Primary => {
                c.rrect(r, rad, if hover { t.accent_hover } else { t.accent });
                if down {
                    c.rrect(r, rad, crate::paint::hex(0, 40));
                }
            }
            Style::Secondary => {
                c.rrect(
                    r,
                    rad,
                    if down {
                        t.pressed
                    } else if hover {
                        t.hover
                    } else {
                        t.surface
                    },
                );
                c.rrect_stroke(r, rad, 1.0 * s, t.border);
            }
            Style::Ghost | Style::On => {
                if style == Style::On {
                    c.rrect(r, rad, t.accent_soft);
                }
                if down {
                    c.rrect(r, rad, t.pressed);
                } else if hover {
                    c.rrect(r, rad, t.hover);
                }
            }
            Style::Round => {
                let col = if hover { t.accent_hover } else { t.accent };
                c.circle(r.cx(), r.cy(), r.w / 2.0, col);
                if down {
                    c.circle(r.cx(), r.cy(), r.w / 2.0, crate::paint::hex(0, 40));
                }
            }
        }
        let fg = match style {
            Style::Primary | Style::Round => t.on_accent,
            Style::On => t.accent,
            _ if hover => t.text,
            _ => t.icon,
        };
        let isz = if style == Style::Round { 22.0 * s } else { 18.0 * s };
        match (icon, label) {
            (Some(i), None) => c.icon(i, Rect::new(r.cx() - isz / 2.0, r.cy() - isz / 2.0, isz, isz), fg),
            (icon, Some(l)) => {
                let px = 13.0 * s;
                let tw = c.text.measure(l, px);
                let iw = if icon.is_some() { isz + 6.0 * s } else { 0.0 };
                let x0 = r.cx() - (tw + iw) / 2.0;
                if let Some(i) = icon {
                    c.icon(i, Rect::new(x0, r.cy() - isz / 2.0, isz, isz), fg);
                }
                let col = if matches!(style, Style::Primary) {
                    t.on_accent
                } else if style == Style::On {
                    t.accent
                } else {
                    t.text
                };
                c.text(x0 + iw, r.cy() + 4.5 * s, px, col, l, Align::Left);
            }
            (None, None) => {}
        }
        self.hits.push((r, act, tip));
    }

    fn hit_hover(&self, r: Rect) -> bool {
        r.contains(self.mouse.0, self.mouse.1) && self.drag.is_none()
    }

    fn draw(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        c.clear(t.bg);
        self.draw_header(c);
        // The stage.
        c.rrect(l.preview, 12.0 * s, t.stage);
        self.draw_preview(c);
        if self.doc.is_some() {
            self.draw_controls(c);
            self.draw_tools(c);
        }
        c.shadow(l.card, 12.0 * s);
        c.rrect(l.card, 12.0 * s, t.surface);
        c.rrect_stroke(l.card, 12.0 * s, 1.0 * s, t.border);
        self.draw_timeline(c);
        let hint = "Space play · ← → frame · Shift 1 s · ↑ ↓ cuts · I / O start / end · S cut · Del remove · 2–9 split · wheel zoom";
        let hint = if c.text.measure(hint, 12.0 * s) > l.hint.w { "Space play · ← → frame · I / O start / end · S cut · 2–9 split" } else { hint };
        c.text(l.hint.cx(), l.hint.cy() + 4.0 * s, 12.0 * s, t.text_faint, hint, Align::Center);
        self.draw_toast(c);
        self.draw_tooltip(c);
        if self.drop_hover {
            c.rrect(l.preview.inset(6.0 * s, 6.0 * s), 12.0 * s, t.accent_soft);
            c.rrect_stroke(l.preview.inset(6.0 * s, 6.0 * s), 12.0 * s, 2.0 * s, t.accent);
        }
    }

    fn draw_header(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let (x, cy) = (16.0 * s, l.header.cy());
        // The buttons, from the right.
        let bh = 34.0 * s;
        let by = cy - bh / 2.0;
        let (label, busy) = match &self.exporting {
            Some(p) => {
                let p = p.lock().unwrap_or_else(|e| e.into_inner());
                (format!("Exporting {}/{}…", (p.done + 1).min(p.total), p.total), true)
            }
            None => match &self.doc {
                Some(d) => {
                    let n = d.plan.pieces().len();
                    (if n == 1 { "Export".to_string() } else { format!("Export {n} pieces") }, false)
                }
                None => ("Export".to_string(), false),
            },
        };
        let ew = c.text.measure(&label, 13.0 * s) + 18.0 * s + 6.0 * s + 32.0 * s;
        let er = Rect::new(l.w - 16.0 * s - ew, by, ew, bh);
        if self.doc.is_some() && !busy {
            self.button(
                c,
                er,
                Act::Export,
                "Write the pieces next to the video, without re-encoding (Ctrl+E; Shift: choose a folder)",
                Some(Icon::Export),
                Some(&label),
                Style::Primary,
            );
        } else {
            c.rrect(er, 8.0 * s, if busy { t.accent_soft } else { t.hover });
            c.icon(Icon::Export, Rect::new(er.x + 16.0 * s, er.cy() - 9.0 * s, 18.0 * s, 18.0 * s), t.text_faint);
            c.text(er.x + 40.0 * s, er.cy() + 4.5 * s, 13.0 * s, if busy { t.accent } else { t.text_faint }, &label, Align::Left);
        }
        let ow = c.text.measure("Open", 13.0 * s) + 18.0 * s + 6.0 * s + 28.0 * s;
        let or = Rect::new(er.x - 8.0 * s - ow, by, ow, bh);
        self.button(c, or, Act::Open, "Open a video (Ctrl+O), or drop one on the window", Some(Icon::Folder), Some("Open"), Style::Secondary);
        // What the file is; its name and the app's are in the window's title bar.
        if let Some(d) = &self.doc {
            let mut meta = Vec::new();
            if let Some((w, h)) = d.info.size {
                meta.push(format!("{w}×{h}"));
            }
            if !d.info.frames.is_empty() {
                meta.push(format!("{:.2} fps", d.info.fps()).replace(".00 fps", " fps"));
            }
            if let Some(cdc) = &d.info.video_codec {
                meta.push(codec_name(cdc));
            }
            if !d.info.has_audio {
                meta.push("no sound".into());
            }
            meta.push(split::timecode(d.info.duration));
            let meta = ellipsize(c.text, &meta.join("  ·  "), 12.0 * s, or.x - x - 16.0 * s);
            c.text(x, cy + 4.5 * s, 12.0 * s, t.text_muted, &meta, Align::Left);
        }
    }

    fn draw_preview(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let Some(d) = &self.doc else {
            // The empty state: a drop zone.
            let r = Rect::new(l.preview.cx() - 190.0 * s, l.preview.cy() - 80.0 * s, 380.0 * s, 160.0 * s);
            let hover = r.contains(self.mouse.0, self.mouse.1);
            c.rrect(r, 14.0 * s, if hover { crate::paint::hex(0xFFFFFF, 10) } else { crate::paint::hex(0xFFFFFF, 5) });
            dashed_rrect(c, r, 14.0 * s, crate::paint::hex(0xFFFFFF, if hover { 70 } else { 40 }));
            c.icon(Icon::Film, Rect::new(r.cx() - 18.0 * s, r.y + 30.0 * s, 36.0 * s, 36.0 * s), crate::paint::hex(0xA3A7B3, 255));
            c.text(r.cx(), r.y + 98.0 * s, 15.0 * s, crate::paint::hex(0xECEEF3, 255), "Drop an MP4 here, or click to open one", Align::Center);
            c.text(r.cx(), r.y + 122.0 * s, 12.0 * s, crate::paint::hex(0x9296A3, 255), "Lossless: pieces are copied, never re-encoded", Align::Center);
            if self.ffmpeg.is_none() {
                c.text(
                    l.preview.cx(),
                    r.bottom() + 30.0 * s,
                    12.0 * s,
                    t.error,
                    "ffmpeg was not found: install it to see pictures and hear sound",
                    Align::Center,
                );
            }
            return;
        };
        let (vw, vh) = d.info.size.unwrap_or((16, 9));
        let r = fit(vw as f32, vh as f32, l.preview.inset(0.0, 0.0));
        let r = Rect::new(r.x.round(), r.y.round(), r.w.round(), r.h.round());
        match &d.frame {
            Some((_, img)) => c.image(img, r, Some((r, 10.0 * s))),
            None => {
                c.rrect(r, 10.0 * s, crate::paint::hex(0xFFFFFF, 6));
                let msg = if d.info.frames.is_empty() {
                    "No video track (sound only)"
                } else if d.src.ffmpeg.is_none() {
                    "No preview: ffmpeg was not found"
                } else {
                    "Loading…"
                };
                c.text(r.cx(), r.cy() + 5.0 * s, 13.0 * s, crate::paint::hex(0x9296A3, 255), msg, Align::Center);
            }
        }
        // Outside the kept part: say so on the picture.
        let ph = self.playhead;
        if ph < d.plan.start || ph > d.plan.end {
            let tag = "Cut away";
            let tw = c.text.measure(tag, 12.0 * s) + 20.0 * s;
            let tr = Rect::new(r.x + 12.0 * s, r.y + 12.0 * s, tw, 26.0 * s);
            c.rrect(tr, 13.0 * s, crate::paint::hex(0, 170));
            c.text(tr.cx(), tr.cy() + 4.0 * s, 12.0 * s, crate::paint::hex(0xFFFFFF, 230), tag, Align::Center);
        }
    }

    fn draw_controls(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let r = l.controls;
        let cy = r.cy();
        let Some(d) = &self.doc else { return };
        let has_audio = d.info.has_audio;
        let ph = self.playing.as_ref().map_or(self.playhead, |p| p.clock(self.audio.as_ref()).min(d.info.duration));
        // The time.
        let tc = split::timecode(ph);
        let mut x = r.x + 4.0 * s;
        x += c.text(x, cy + 7.0 * s, 20.0 * s, t.text, &tc, Align::Left);
        x += c.text(x, cy + 7.0 * s, 14.0 * s, t.text_muted, &format!("  /  {}", split::timecode(d.info.duration)), Align::Left);
        if !d.info.frames.is_empty() {
            let fi = split::frame_index(&d.info.frames, ph) + 1;
            let key = d.info.keys.binary_search(&d.info.frames[fi - 1]).is_ok();
            let label = format!("   frame {fi}{}", if key { " · key" } else { "" });
            c.text(x, cy + 6.5 * s, 12.0 * s, t.text_faint, &label, Align::Left);
        }
        // Transport, centred.
        let b = 36.0 * s;
        let big = 44.0 * s;
        let gap = 6.0 * s;
        let total = 4.0 * b + big + 4.0 * gap;
        let mut bx = (r.cx() - total / 2.0).max(r.x + 300.0 * s);
        let playing = self.playing.is_some();
        for (act, icon, tip) in [
            (Act::ToStart, Icon::ToStart, "Go to the start of the kept part (again: the very start)"),
            (Act::FrameBack, Icon::FrameBack, "Previous frame (←; Shift: 1 s)"),
        ] {
            self.button(c, Rect::new(bx, cy - b / 2.0, b, b), act, tip, Some(icon), None, Style::Ghost);
            bx += b + gap;
        }
        self.button(
            c,
            Rect::new(bx, cy - big / 2.0, big, big),
            Act::Play,
            "Play / pause (Space)",
            Some(if playing { Icon::Pause } else { Icon::Play }),
            None,
            Style::Round,
        );
        bx += big + gap;
        for (act, icon, tip) in
            [(Act::FrameFwd, Icon::FrameFwd, "Next frame (→; Shift: 1 s)"), (Act::ToEnd, Icon::ToEnd, "Go to the end of the kept part (again: the very end)")]
        {
            self.button(c, Rect::new(bx, cy - b / 2.0, b, b), act, tip, Some(icon), None, Style::Ghost);
            bx += b + gap;
        }
        // Editing, on the right.
        let mut rx = r.right();
        let snap = self.snap;
        let muted = self.muted;
        let items: [(Act, Icon, &'static str, Style); 5] = [
            (Act::Mute, if muted { Icon::Muted } else { Icon::Volume }, "Mute (M)", if muted { Style::On } else { Style::Ghost }),
            (
                Act::Snap,
                Icon::KeyFrame,
                "Snap to key frames (N): pieces then start exactly on a key frame, for players that ignore edit lists",
                if snap { Style::On } else { Style::Ghost },
            ),
            (Act::MarkOut, Icon::MarkOut, "Set the end at the playhead (O)", Style::Ghost),
            (Act::Cut, Icon::Scissors, "Cut at the playhead (S); double-click the timeline to cut there", Style::Ghost),
            (Act::MarkIn, Icon::MarkIn, "Set the start at the playhead (I)", Style::Ghost),
        ];
        for (i, (act, icon, tip, style)) in items.into_iter().enumerate() {
            if act == Act::Mute && !has_audio {
                continue;
            }
            rx -= b;
            self.button(c, Rect::new(rx, cy - b / 2.0, b, b), act, tip, Some(icon), None, style);
            rx -= gap;
            if i == 1 {
                rx -= 4.0 * s;
                c.line(rx, cy - 10.0 * s, rx, cy + 10.0 * s, 1.0 * s, t.separator);
                rx -= 8.0 * s;
            }
        }
    }

    fn draw_tools(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let r = l.tools;
        let cy = r.cy() - 2.0 * s;
        let Some(d) = &self.doc else { return };
        let pieces = d.plan.pieces().len();
        let kept = d.plan.kept();
        let has_cuts = !d.plan.cuts.is_empty();
        let mut x = r.x + 4.0 * s;
        x += c.text(x, cy + 4.5 * s, 13.0 * s, t.text_muted, "Split into", Align::Left) + 10.0 * s;
        let bh = 30.0 * s;
        for n in 2..=5usize {
            let w = 34.0 * s;
            let style = if pieces == n && self.equal_split(n) { Style::On } else { Style::Secondary };
            let label = n.to_string();
            let tip: &'static str = match n {
                2 => "Two equal pieces (2)",
                3 => "Three equal pieces (3)",
                4 => "Four equal pieces (4)",
                _ => "Five equal pieces (5)",
            };
            self.button(c, Rect::new(x, cy - bh / 2.0, w, bh), Act::Preset(n), tip, None, Some(&label), style);
            x += w + 6.0 * s;
        }
        // N pieces: − N + and Split.
        x += 6.0 * s;
        let grp = Rect::new(x, cy - bh / 2.0, 30.0 * s * 2.0 + 34.0 * s, bh);
        c.rrect(grp, 8.0 * s, t.surface);
        c.rrect_stroke(grp, 8.0 * s, 1.0 * s, t.border);
        self.button(c, Rect::new(x, grp.y, 30.0 * s, bh), Act::NMinus, "Fewer pieces", Some(Icon::Minus), None, Style::Ghost);
        let n = self.n;
        c.text(x + 30.0 * s + 17.0 * s, cy + 4.5 * s, 13.0 * s, t.text, &n.to_string(), Align::Center);
        self.button(c, Rect::new(x + 64.0 * s, grp.y, 30.0 * s, bh), Act::NPlus, "More pieces", Some(Icon::Plus), None, Style::Ghost);
        x = grp.right() + 6.0 * s;
        let lbl = format!("Split in {n}");
        let w = c.text.measure(&lbl, 13.0 * s) + 24.0 * s;
        let style = if pieces == n && self.equal_split(n) { Style::On } else { Style::Secondary };
        self.button(c, Rect::new(x, cy - bh / 2.0, w, bh), Act::NApply, "Split the kept part into this many equal pieces", None, Some(&lbl), style);
        x += w + 6.0 * s;
        if has_cuts {
            self.button(c, Rect::new(x, cy - bh / 2.0, bh, bh), Act::ClearCuts, "Remove all cuts (0)", Some(Icon::Trash), None, Style::Ghost);
            x += bh;
        }
        // The summary, on the right.
        let summary = format!("{}  ·  {} piece{}", split::timecode(kept), pieces, if pieces == 1 { "" } else { "s" });
        let sw = c.text.measure(&summary, 13.0 * s);
        if r.right() - sw > x + 20.0 * s {
            let lw = c.text.measure("Kept ", 13.0 * s);
            c.text(r.right() - 4.0 * s - sw - lw, cy + 4.5 * s, 13.0 * s, t.text_muted, "Kept ", Align::Left);
            c.text(r.right() - 4.0 * s, cy + 4.5 * s, 13.0 * s, t.text, &summary, Align::Right);
        }
    }

    fn equal_split(&self, n: usize) -> bool {
        let Some(d) = &self.doc else { return false };
        let p = d.plan.pieces();
        if p.len() != n {
            return false;
        }
        let each = d.plan.kept().as_secs_f64() / n as f64;
        let tol = if self.snap { each * 0.5 } else { 0.002 };
        p.iter().all(|(a, b)| ((*b - *a).as_secs_f64() - each).abs() <= tol)
    }

    fn draw_timeline(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let Some(d) = &self.doc else {
            c.text(l.card.cx(), l.card.cy() + 5.0 * s, 13.0 * s, t.text_faint, "The timeline shows here once a video is open", Align::Center);
            return;
        };
        let (va, vb) = d.view;
        let span = (vb - va).as_secs_f64().max(1e-6);
        let track = l.track;
        let px_per_s = track.w as f64 / span;
        let x_of = |tt: Duration| track.x + ((tt.as_secs_f64() - va.as_secs_f64()) * px_per_s) as f32;
        // Ruler.
        let tick = nice_tick(span, track.w as f64 / s as f64);
        let minor = tick / 5.0;
        let first = (va.as_secs_f64() / minor).floor() as i64;
        let last = (vb.as_secs_f64() / minor).ceil() as i64;
        for k in first..=last {
            let tt = k as f64 * minor;
            if tt < 0.0 {
                continue;
            }
            let x = x_of(Duration::from_secs_f64(tt));
            if x < track.x - 1.0 || x > track.right() + 1.0 {
                continue;
            }
            let major = k % 5 == 0;
            let h = if major { 7.0 } else { 4.0 } * s;
            c.line(x, l.ruler.bottom() - h, x, l.ruler.bottom(), 1.0 * s, if major { t.text_faint } else { t.separator });
            if major && x + 40.0 * s < track.right() + 30.0 * s {
                let lbl = split::ruler_label(Duration::from_secs_f64(tt), Duration::from_secs_f64(tick));
                c.text(x + 4.0 * s, l.ruler.y + 12.0 * s, 10.5 * s, t.text_muted, &lbl, Align::Left);
            }
        }
        // Key frames, when they are far enough apart to see.
        if !d.info.keys.is_empty() {
            let i0 = d.info.keys.partition_point(|k| *k < va);
            let i1 = d.info.keys.partition_point(|k| *k <= vb);
            let n = i1.saturating_sub(i0);
            if n > 0 && (track.w / n as f32) > 5.0 * s {
                for k in &d.info.keys[i0..i1] {
                    let x = x_of(*k);
                    let col = if self.snap { t.accent } else { t.text_faint };
                    c.poly(&[(x - 3.0 * s, l.ruler.bottom() - 1.0 * s), (x + 3.0 * s, l.ruler.bottom() - 1.0 * s), (x, l.ruler.bottom() - 5.0 * s)], col);
                }
            }
        }
        // Thumbnails.
        let body = Rect::new(track.x, l.thumbs.y, track.w, l.wave.bottom() - l.thumbs.y);
        c.rrect(body, 8.0 * s, t.well);
        let tw = d.thumbs.width as f64;
        let cell = span * tw / track.w as f64; // seconds a thumbnail covers
        let q_ms = (cell * 1000.0).max(1.0).log2().ceil().exp2() as u64; // a power of two, so cells are reused while panning
        let q = q_ms as f64 / 1000.0;
        let k0 = (va.as_secs_f64() / q).floor() as u64;
        let k1 = (vb.as_secs_f64() / q).ceil() as u64;
        let dur_ms = d.info.duration.as_millis() as u64;
        let clip = Rect::new(track.x, l.thumbs.y, track.w, l.thumbs.h);
        if !d.info.frames.is_empty() {
            for k in k0..k1.max(k0 + 1) {
                let key = (k * q_ms + q_ms / 2).min(dur_ms.saturating_sub(1));
                let x0 = x_of(Duration::from_millis(k * q_ms)).max(track.x);
                let x1 = x_of(Duration::from_millis((k + 1) * q_ms)).min(track.right());
                if x1 <= x0 {
                    continue;
                }
                let cellr = Rect::new(x0, clip.y, x1 - x0, clip.h);
                if let Some(p) = d.thumbs.get(key) {
                    if let Some(img) = pixmap(p.w, p.h, p.rgba) {
                        // Cover the cell, centred.
                        let k = (cellr.w / img.width() as f32).max(clip.h / img.height() as f32);
                        let (iw, ih) = (img.width() as f32 * k, img.height() as f32 * k);
                        let dst = Rect::new(cellr.cx() - iw / 2.0, cellr.cy() - ih / 2.0, iw, ih);
                        c.image(&img, dst, Some((cellr, 0.0)));
                    }
                }
                c.line(x0, clip.y, x0, clip.bottom(), 1.0 * s, crate::paint::hex(0, 90));
            }
            // Round the strip's top corners back.
            round_corners_top(c, clip, 8.0 * s, t.surface);
        }
        // Waveform.
        let peaks = d.peaks.lock().map(|p| p.clone()).unwrap_or_default();
        let wave = l.wave;
        c.line(track.x, wave.y, track.right(), wave.y, 1.0 * s, crate::paint::hex(0, 120));
        if !peaks.is_empty() {
            let per = crate::ffmpeg::PEAKS_PER_SEC as f64;
            let mid = wave.cy();
            let amp = wave.h * 0.42;
            let step = (1.5 * s).max(1.0);
            let mut x = track.x;
            while x < track.right() {
                let ta = va.as_secs_f64() + ((x - track.x) as f64 / px_per_s);
                let tb = ta + step as f64 / px_per_s;
                let (i0, i1) = ((ta * per) as usize, ((tb * per).ceil() as usize).max((ta * per) as usize + 1));
                let v = peaks.get(i0.min(peaks.len())..i1.min(peaks.len())).map(|p| p.iter().copied().fold(0.0f32, f32::max)).unwrap_or(0.0);
                let v = v.sqrt(); // perceptual
                if v > 0.0 {
                    let tt = Duration::from_secs_f64(ta);
                    let kept = tt >= d.plan.start && tt <= d.plan.end;
                    let col = if kept { t.accent.with_alpha(200) } else { t.text_faint.with_alpha(150) };
                    let hh = (v * amp).max(0.5 * s);
                    c.rect(Rect::new(x, mid - hh, (step - 0.5 * s).max(1.0), hh * 2.0), col);
                }
                x += step;
            }
        } else if d.info.has_audio && d.src.ffmpeg.is_some() {
            c.text(wave.cx(), wave.cy() + 4.0 * s, 11.0 * s, t.text_faint, "Reading the sound…", Align::Center);
        } else if !d.info.has_audio {
            c.text(wave.cx(), wave.cy() + 4.0 * s, 11.0 * s, t.text_faint, "No sound", Align::Center);
        }
        // Cut away: dimmed.
        let xs = x_of(d.plan.start).clamp(track.x, track.right());
        let xe = x_of(d.plan.end).clamp(track.x, track.right());
        if xs > track.x {
            c.rect(Rect::new(track.x, body.y, xs - track.x, body.h), t.dim);
        }
        if xe < track.right() {
            c.rect(Rect::new(xe, body.y, track.right() - xe, body.h), t.dim);
        }
        // The kept part's frame and handles.
        let hover_m = if self.drag.is_none() { self.marker_at(self.mouse.0, self.mouse.1) } else { None };
        let drag_m = match self.drag {
            Some(Drag::Marker(m)) => Some(m),
            _ => None,
        };
        let bw = 2.0 * s;
        if xe > xs {
            c.rect(Rect::new(xs, body.y, xe - xs, bw), t.accent);
            c.rect(Rect::new(xs, body.bottom() - bw, xe - xs, bw), t.accent);
        }
        let hw = 12.0 * s;
        for (m, x, left) in [(Marker::Start, x_of(d.plan.start), true), (Marker::End, x_of(d.plan.end), false)] {
            if x < track.x - hw || x > track.right() + hw {
                continue;
            }
            let hot = hover_m == Some(m) || drag_m == Some(m);
            let r = if left { Rect::new(x - hw, body.y, hw, body.h) } else { Rect::new(x, body.y, hw, body.h) };
            let col = if hot { t.accent_hover } else { t.accent };
            c.rrect(r, 5.0 * s, col);
            // Square the inner side.
            let inner = if left { Rect::new(x - 5.0 * s, body.y, 5.0 * s, body.h) } else { Rect::new(x, body.y, 5.0 * s, body.h) };
            c.rect(inner, col);
            let gx = r.cx();
            for dy in [-5.0, 0.0, 5.0] {
                c.line(gx - 2.5 * s, body.cy() + dy * s, gx + 2.5 * s, body.cy() + dy * s, 1.5 * s, t.on_accent.with_alpha(170));
            }
        }
        // Cuts.
        for (i, ct) in d.plan.cuts.iter().enumerate() {
            let x = x_of(*ct);
            if x < track.x - 8.0 * s || x > track.right() + 8.0 * s {
                continue;
            }
            let hot = hover_m == Some(Marker::Cut(i)) || drag_m == Some(Marker::Cut(i));
            let col = if hot { t.cut } else { t.cut.with_alpha(230) };
            c.rect(Rect::new(x - 1.0 * s, body.y, 2.0 * s, body.h), crate::paint::hex(0, 120));
            c.rect(Rect::new(x - 1.0 * s, body.y, 2.0 * s, body.h), col);
            let hr = Rect::new(x - 7.0 * s, l.ruler.y + 3.0 * s, 14.0 * s, 16.0 * s);
            c.rrect(hr, 4.0 * s, col);
            c.poly(&[(x - 4.0 * s, hr.bottom() - 1.0), (x + 4.0 * s, hr.bottom() - 1.0), (x, hr.bottom() + 4.0 * s)], col);
            if hot {
                c.rrect_stroke(hr.inset(-2.0 * s, -2.0 * s), 6.0 * s, 1.5 * s, t.text);
            }
        }
        // The pieces.
        let pieces = d.plan.pieces();
        for (i, (a, b)) in pieces.iter().enumerate() {
            let (x0, x1) = (x_of(*a).max(track.x), x_of(*b).min(track.right()));
            if x1 - x0 < 18.0 * s {
                continue;
            }
            let full = format!("{}  ·  {}", i + 1, split::timecode(*b - *a));
            let lbl = if c.text.measure(&full, 11.0 * s) + 20.0 * s < x1 - x0 { full } else { (i + 1).to_string() };
            let w = c.text.measure(&lbl, 11.0 * s) + 14.0 * s;
            let cx = (x0 + x1) / 2.0;
            let pr = Rect::new(cx - w / 2.0, l.labels.y + 5.0 * s, w, 18.0 * s);
            c.rrect(pr, 9.0 * s, t.accent_soft);
            c.text(cx, pr.cy() + 4.0 * s, 11.0 * s, t.accent, &lbl, Align::Center);
        }
        // Where the pointer is.
        let (mx, my) = self.mouse;
        let over = Rect::new(track.x, l.ruler.y, track.w, l.labels.y - l.ruler.y).contains(mx, my);
        if over && hover_m.is_none() && self.drag.is_none() {
            c.line(mx, l.ruler.y + 4.0 * s, mx, body.bottom(), 1.0 * s, t.text.with_alpha(90));
        }
        // The playhead.
        let ph = self.playing.as_ref().map_or(self.playhead, |p| p.clock(self.audio.as_ref()).min(d.info.duration));
        let x = x_of(ph);
        if x >= track.x - 1.0 && x <= track.right() + 1.0 {
            c.rect(Rect::new(x - 1.5 * s, l.ruler.y + 6.0 * s, 3.0 * s, body.bottom() - l.ruler.y - 6.0 * s), crate::paint::hex(0, 110));
            c.rect(Rect::new(x - 1.0 * s, l.ruler.y + 6.0 * s, 2.0 * s, body.bottom() - l.ruler.y - 6.0 * s), t.playhead);
            c.circle(x, l.ruler.y + 7.0 * s, 6.0 * s, t.playhead);
        }
        // The time under the pointer (or of what is dragged).
        let tip_t = match self.drag {
            Some(Drag::Marker(m)) => d.plan.at(m),
            Some(Drag::Scrub) => Some(self.playhead),
            _ if over => Some(self.t_of(mx)),
            _ => None,
        };
        if let Some(tt) = tip_t {
            let lbl = split::timecode(tt);
            let w = c.text.measure(&lbl, 11.0 * s) + 14.0 * s;
            let bx = if self.drag.is_some() { x_of(tt) } else { mx };
            let r = Rect::new((bx - w / 2.0).clamp(l.card.x + 4.0 * s, l.card.right() - w - 4.0 * s), l.card.y - 24.0 * s, w, 20.0 * s);
            c.rrect(r, 6.0 * s, t.tooltip_bg);
            c.text(r.cx(), r.cy() + 4.0 * s, 11.0 * s, t.tooltip_text, &lbl, Align::Center);
        }
        // The zoom.
        if vb - va < d.info.duration {
            let full = d.info.duration.as_secs_f64();
            let bar = Rect::new(track.x, l.card.bottom() - 5.0 * s, track.w, 3.0 * s);
            let a = (va.as_secs_f64() / full) as f32;
            let b = (vb.as_secs_f64() / full) as f32;
            c.rrect(Rect::new(bar.x + a * bar.w, bar.y, ((b - a) * bar.w).max(12.0 * s), bar.h), 1.5 * s, t.text_faint);
        }
    }

    fn draw_toast(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let s = self.l.s;
        let Some(toast) = &self.toast else { return };
        let ttl = if toast.error { 9 } else { 12 };
        if toast.at.elapsed() > Duration::from_secs(ttl) {
            self.toast = None;
            return;
        }
        let text = toast.text.clone();
        let error = toast.error;
        let reveal = toast.reveal.is_some();
        let px = 13.0 * s;
        let maxw = self.l.w - 120.0 * s;
        let text = ellipsize(c.text, &text, px, maxw - 160.0 * s);
        let tw = c.text.measure(&text, px);
        let extra = if reveal { c.text.measure("Show folder", px) + 30.0 * s } else { 0.0 } + 36.0 * s;
        let w = tw + 40.0 * s + extra;
        let r = Rect::new(self.l.w / 2.0 - w / 2.0, self.l.preview.bottom() - 60.0 * s, w, 42.0 * s);
        c.shadow(r, 12.0 * s);
        c.rrect(r, 12.0 * s, t.tooltip_bg);
        c.circle(r.x + 18.0 * s, r.cy(), 4.0 * s, if error { t.error } else { t.success });
        c.text(r.x + 32.0 * s, r.cy() + 4.5 * s, px, t.tooltip_text, &text, Align::Left);
        let mut x = r.x + 32.0 * s + tw + 10.0 * s;
        if reveal {
            let bw = c.text.measure("Show folder", px) + 20.0 * s;
            let br = Rect::new(x, r.cy() - 14.0 * s, bw, 28.0 * s);
            let hover = self.hit_hover(br);
            c.rrect(br, 7.0 * s, if hover { t.accent_hover } else { t.accent });
            c.text(br.cx(), br.cy() + 4.5 * s, px, t.on_accent, "Show folder", Align::Center);
            self.hits.push((br, Act::ShowFolder, ""));
            x += bw + 4.0 * s;
        }
        let cr = Rect::new(x, r.cy() - 13.0 * s, 26.0 * s, 26.0 * s);
        if self.hit_hover(cr) {
            c.rrect(cr, 7.0 * s, crate::paint::hex(0xFFFFFF, 20));
        }
        c.icon(Icon::Close, cr.inset(6.0 * s, 6.0 * s), t.tooltip_text.with_alpha(180));
        self.hits.push((cr, Act::CloseToast, ""));
    }

    fn draw_tooltip(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let s = self.l.s;
        if self.drag.is_some() || self.pressed.is_some() || self.hover_since.elapsed() < Duration::from_millis(550) {
            return;
        }
        let (mx, my) = self.mouse;
        let Some((r, _, tip)) = self.hits.iter().rev().find(|h| h.0.contains(mx, my)).cloned() else { return };
        if tip.is_empty() {
            return;
        }
        let px = 12.0 * s;
        let tip = ellipsize(c.text, tip, px, self.l.w - 40.0 * s);
        let w = c.text.measure(&tip, px) + 20.0 * s;
        let h = 28.0 * s;
        let x = (r.cx() - w / 2.0).clamp(8.0 * s, self.l.w - w - 8.0 * s);
        let y = if r.bottom() + h + 8.0 * s < self.l.h { r.bottom() + 6.0 * s } else { r.y - h - 6.0 * s };
        let tr = Rect::new(x, y, w, h);
        c.rrect(tr, 7.0 * s, t.tooltip_bg);
        c.text(tr.cx(), tr.cy() + 4.0 * s, px, t.tooltip_text, &tip, Align::Center);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Style {
    Primary,
    Secondary,
    Ghost,
    On,
    Round,
}

fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

fn codec_name(fourcc: &str) -> String {
    match fourcc {
        "avc1" | "avc3" => "H.264".into(),
        "hvc1" | "hev1" => "HEVC".into(),
        "av01" => "AV1".into(),
        "vp09" => "VP9".into(),
        "mp4v" => "MPEG-4".into(),
        other => other.trim().to_uppercase(),
    }
}

/// A `w` × `h` picture fitted (aspect kept) and centred in `r`.
fn fit(w: f32, h: f32, r: Rect) -> Rect {
    let k = (r.w / w.max(1.0)).min(r.h / h.max(1.0));
    let (fw, fh) = (w * k, h * k);
    Rect::new(r.cx() - fw / 2.0, r.cy() - fh / 2.0, fw, fh)
}

/// A step between labelled ticks that leaves them ~90 logical px apart.
fn nice_tick(span: f64, width_logical: f64) -> f64 {
    let want = span * 90.0 / width_logical.max(1.0);
    const STEPS: [f64; 19] = [0.01, 0.02, 0.05, 0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 900.0, 1800.0, 3600.0];
    STEPS.iter().copied().find(|s| *s >= want).unwrap_or(3600.0)
}

fn ellipsize(text: &Text, s: &str, px: f32, max: f32) -> String {
    if text.measure(s, px) <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut n = chars.len();
    while n > 1 {
        n -= 1;
        let t: String = chars[..n].iter().collect::<String>() + "…";
        if text.measure(&t, px) <= max {
            return t;
        }
    }
    "…".into()
}

fn dashed_rrect(c: &mut Canvas, r: Rect, _rad: f32, col: C) {
    let s = c.s;
    let (dash, gap) = (8.0 * s, 6.0 * s);
    let w = 1.5 * s;
    let inset = 14.0 * s;
    let mut x = r.x + inset;
    while x < r.right() - inset {
        let e = (x + dash).min(r.right() - inset);
        c.line(x, r.y, e, r.y, w, col);
        c.line(x, r.bottom(), e, r.bottom(), w, col);
        x += dash + gap;
    }
    let mut y = r.y + inset;
    while y < r.bottom() - inset {
        let e = (y + dash).min(r.bottom() - inset);
        c.line(r.x, y, r.x, e, w, col);
        c.line(r.right(), y, r.right(), e, w, col);
        y += dash + gap;
    }
}

/// Paint the outside of `r`'s top rounded corners with `bg`.
fn round_corners_top(c: &mut Canvas, r: Rect, rad: f32, bg: C) {
    // Cheap: small quarter masks built from a rounded rect over a square.
    for (x, left) in [(r.x, true), (r.right() - rad, false)] {
        let sq = Rect::new(x, r.y, rad, rad);
        let steps = (rad as i32).max(1);
        for i in 0..steps {
            let yy = i as f32 + 0.5;
            let dy = rad - yy;
            let dx = rad - (rad * rad - dy * dy).max(0.0).sqrt();
            if dx <= 0.0 {
                continue;
            }
            let rr = if left { Rect::new(sq.x, sq.y + i as f32, dx, 1.0) } else { Rect::new(sq.right() - dx, sq.y + i as f32, dx, 1.0) };
            c.rect(rr, bg);
        }
    }
}

impl ApplicationHandler<UserEvent> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("MP4 Splitter")
            .with_inner_size(LogicalSize::new(1180.0, 780.0))
            .with_min_inner_size(LogicalSize::new(760.0, 560.0));
        let window = match el.create_window(attrs) {
            Ok(w) => Rc::new(w),
            Err(e) => {
                eprintln!("no window: {e}");
                el.exit();
                return;
            }
        };
        self.theme = theme::pick(window.theme());
        let ctx = match softbuffer::Context::new(window.clone()) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("no drawing surface: {e}");
                el.exit();
                return;
            }
        };
        self.surface = softbuffer::Surface::new(&ctx, window.clone()).ok();
        let size = window.inner_size();
        self.l = layout(size.width as f32, size.height as f32, window.scale_factor() as f32);
        self.window = Some(window);
        if let Some(p) = self.pending_open.take() {
            self.open(&p);
        }
        self.redraw();
    }

    fn user_event(&mut self, _el: &ActiveEventLoop, _ev: UserEvent) {
        // A still arrived?
        if let Some(d) = &mut self.doc {
            let got = d.stills.done.lock().ok().and_then(|mut g| g.take());
            if let Some((seq, t, p)) = got {
                if seq == d.last_still && self.playing.is_none() {
                    if let Some(img) = pixmap(p.w, p.h, p.rgba) {
                        d.frame = Some((t, img));
                    }
                }
            }
        }
        self.check_export();
        self.redraw();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, ev: WindowEvent) {
        match ev {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.paint(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                if let Some(w) = &self.window {
                    let size = w.inner_size();
                    self.l = layout(size.width as f32, size.height as f32, w.scale_factor() as f32);
                }
                if self.playing.is_none() {
                    self.request_still();
                }
                self.redraw();
            }
            WindowEvent::ThemeChanged(th) => {
                self.theme = theme::pick(Some(th));
                self.redraw();
            }
            WindowEvent::ModifiersChanged(m) => self.mods = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.key(&event, el),
            WindowEvent::CursorMoved { position, .. } => {
                let p = (position.x as f32, position.y as f32);
                let before = self.hit(self.mouse.0, self.mouse.1);
                self.mouse = p;
                let now = self.hit(p.0, p.1);
                if now != before {
                    self.hover = now;
                    self.hover_since = Instant::now();
                }
                if self.drag.is_some() {
                    self.drag_to(p.0);
                }
                if let Some(w) = &self.window {
                    w.set_cursor(self.cursor());
                }
                self.redraw();
            }
            WindowEvent::CursorLeft { .. } => {
                self.mouse = (-1.0, -1.0);
                self.redraw();
            }
            WindowEvent::MouseInput { state, button, .. } => match state {
                ElementState::Pressed => self.mouse_down(button),
                ElementState::Released => self.mouse_up(button),
            },
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::LineDelta(x, y) => (x as f64, y as f64),
                    MouseScrollDelta::PixelDelta(p) => (p.x / 40.0, p.y / 40.0),
                };
                let (mx, my) = self.mouse;
                if self.l.card.contains(mx, my) {
                    if self.mods.shift_key() || dx.abs() > dy.abs() {
                        let by = if dx.abs() > dy.abs() { -dx } else { -dy };
                        self.pan(by as f32 * 60.0 * self.l.s);
                    } else {
                        let at = self.t_of(mx);
                        self.zoom(1.2f64.powf(-dy), at);
                    }
                } else if self.l.preview.contains(mx, my) && self.doc.is_some() && dy != 0.0 {
                    // Over the picture: frame by frame.
                    self.step_frame(if dy < 0.0 { 1 } else { -1 });
                }
            }
            WindowEvent::HoveredFile(_) => {
                self.drop_hover = true;
                self.redraw();
            }
            WindowEvent::HoveredFileCancelled => {
                self.drop_hover = false;
                self.redraw();
            }
            WindowEvent::DroppedFile(p) => {
                self.drop_hover = false;
                self.open(&p);
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        if self.autoclose.is_some_and(|t| Instant::now() >= t) {
            if let Ok(dir) = std::env::var("MP4_SPLITTER_SCREENSHOT") {
                self.screenshot(Path::new(&dir));
            }
            el.exit();
            return;
        }
        let mut wait: Option<Instant> = self.autoclose;
        let earliest = |w: &mut Option<Instant>, t: Instant| *w = Some(w.map_or(t, |x| x.min(t)));
        if let Some(pb) = &mut self.playing {
            let now = pb.clock(self.audio.as_ref());
            let dur = self.doc.as_ref().map_or(Duration::ZERO, |d| d.info.duration);
            let mut fresh = false;
            if let Some(f) = pb.due(now) {
                if let (Some(d), Some(img)) = (&mut self.doc, pixmap(f.pic.w, f.pic.h, f.pic.rgba)) {
                    d.frame = Some((f.t, img));
                }
                fresh = true;
            }
            if fresh {
                self.redraw();
            }
            if now >= dur {
                self.playhead = dur;
                self.pause();
            } else {
                self.playhead = now;
                // Page the timeline along.
                if let Some(d) = &mut self.doc {
                    if now > d.view.1 {
                        let span = d.view.1 - d.view.0;
                        let a = now.min(d.info.duration.saturating_sub(span));
                        d.view = (a, a + span);
                    }
                }
                if self.last_paint.elapsed() > Duration::from_millis(33) {
                    self.redraw();
                }
                earliest(&mut wait, Instant::now() + Duration::from_millis(5));
            }
        }
        if self.hover.is_some() && self.hover_since.elapsed() < Duration::from_millis(600) {
            earliest(&mut wait, self.hover_since + Duration::from_millis(560));
        }
        if let Some(t) = &self.toast {
            earliest(&mut wait, t.at + Duration::from_secs(if t.error { 9 } else { 12 }) + Duration::from_millis(20));
        }
        el.set_control_flow(match wait {
            Some(t) => ControlFlow::WaitUntil(t),
            None => ControlFlow::Wait,
        });
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_secs(if t.error { 9 } else { 12 })) {
            self.redraw();
        }
        if self.hover.is_some() && self.hover_since.elapsed() >= Duration::from_millis(550) && self.hover_since.elapsed() < Duration::from_millis(700) {
            self.redraw();
        }
    }
}

impl App {
    /// Save what the window shows as a PNG in `dir` (docs and CI).
    fn screenshot(&mut self, dir: &Path) {
        self.paint();
        if let Some(pm) = &self.pm {
            let name = std::env::var("MP4_SPLITTER_SCREENSHOT_NAME").unwrap_or_else(|_| "screenshot.png".into());
            if let Err(e) = pm.save_png(dir.join(name)) {
                eprintln!("screenshot: {e}");
            }
        }
    }
}
