//! The window: a preview with playback, transport controls, the split
//! presets and a zoomable timeline of thumbnails and the waveform, with
//! the trim handles, the cuts and the playhead.

use crate::audio::Output;
use crate::config::{Config, ThemeChoice};
use crate::export::{self, Progress};
use crate::mp4;
use crate::paint::{Align, C, Canvas, Icon, Picture, Rect, Text};
use crate::player;
use crate::player::{Playback, Source, Stills, Thumbs};
use crate::raster::Order;
use crate::shell::{Button, CursorKind, Host, Key, Mods, Tick, Wake};
use crate::split::{self, Marker, Plan};
use crate::theme::{self, Theme};
use crate::uifb::Fb;
use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// A frame ready to show: BGRA, `w` * `h`.
pub struct Frame<'a> {
    pub w: u32,
    pub h: u32,
    pub bgra: &'a [u8],
}

/// The bands of the window, top to bottom: what a paint can redo alone.
const HEADER: u8 = 1;
const PREVIEW: u8 = 2;
const CONTROLS: u8 = 4;
const TIMELINE: u8 = 8;
const ALL: u8 = HEADER | PREVIEW | CONTROLS | TIMELINE;
/// Hits of what floats over the bands (the toast): redone with any paint.
const OVER: u8 = 16;

/// What the About page says the app is.
const ABOUT_LINE: &str = "Cut videos into pieces without re-encoding. Tiny, draws its own pixels, runs on Windows, macOS and Linux.";

/// The corners of the preview and the picture in it, in logical pixels.
const PREVIEW_RADIUS: f32 = 12.0;

/// What a file dialog came back with, waiting for the app's thread.
enum Dialog {
    Open(Option<PathBuf>),
    ExportTo(Option<PathBuf>),
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
    /// Switch piece `i` off or on.
    Piece(usize),
    /// Export one file (or separate files).
    Join(bool),
    NMinus,
    NPlus,
    NApply,
    ClearCuts,
    ShowFolder,
    CloseToast,
    About,
    AboutLink,
    Settings,
    Theme(ThemeChoice),
    Accent(usize),
}

/// A page over the window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Page {
    About,
    Settings,
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
    frame: Option<(Duration, Picture)>,
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
    host: Box<dyn Host>,
    /// A file dialog's answer, set from wherever it ran.
    dialog: Arc<Mutex<Option<Dialog>>>,
    /// Asked to quit (Ctrl+Q).
    quit: bool,
    /// The page showing over the window.
    page: Option<Page>,
    /// What is kept between runs.
    config: Config,
    /// The OS's theme, if known.
    os_dark: Option<bool>,
    /// The frame (BGRA), kept between paints.
    canvas: Vec<u8>,
    /// The bands to draw again.
    dirty: Cell<u8>,
    /// The band being drawn: what its controls' hits belong to.
    part: u8,
    text: Option<Text>,
    theme: Theme,
    l: Layout,
    /// Pictures and sound can be decoded here.
    decoder: bool,
    audio: Option<Output>,
    doc: Option<Doc>,
    playhead: Duration,
    playing: Option<Playback>,
    last_paint: Instant,
    muted: bool,
    snap: bool,
    /// Export the kept pieces joined into one file.
    join: bool,
    n: usize,
    mouse: (f32, f32),
    mods: Mods,
    hits: Vec<(Rect, Act, &'static str, u8)>,
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
    /// Quit after this long (tests and screenshots).
    autoclose: Option<Instant>,
}

impl App {
    /// The app in `host`'s window, opening `open`; `dark`: the OS's theme, if known.
    pub fn new(host: Box<dyn Host>, dark: Option<bool>, open: Option<PathBuf>) -> App {
        let autoclose = std::env::var("MP4_SPLITTER_AUTOCLOSE_MS").ok().and_then(|v| v.parse().ok()).map(|ms| Instant::now() + Duration::from_millis(ms));
        let config = Config::load();
        let theme = theme::pick(dark, config.theme, config.accent);
        host.set_colors(&theme);
        let (w, h) = host.size();
        let l = layout(w as f32, h as f32, host.scale());
        let mut app = App {
            host,
            dialog: Arc::default(),
            quit: false,
            page: None,
            config,
            os_dark: dark,
            canvas: Vec::new(),
            dirty: Cell::new(ALL),
            part: HEADER,
            text: Some(Text::new()),
            theme,
            l,
            decoder: player::available(),
            audio: None,
            doc: None,
            playhead: Duration::ZERO,
            playing: None,
            last_paint: Instant::now(),
            muted: false,
            snap: false,
            join: false,
            n: 6,
            mouse: (-1.0, -1.0),
            mods: Mods::default(),
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
            autoclose,
        };
        if let Some(p) = open {
            app.open(&p);
        }
        app.redraw();
        app
    }

    fn wake(&self) -> Wake {
        self.host.wake()
    }

    /// Draw everything again.
    fn redraw(&self) {
        self.dirty.set(ALL);
        self.host.redraw(None);
    }

    /// The rows of a band.
    fn rows(&self, part: u8) -> (u32, u32) {
        let l = &self.l;
        let (a, b) = match part {
            HEADER => (0.0, l.header.bottom()),
            PREVIEW => (l.header.bottom(), l.controls.y),
            CONTROLS => (l.controls.y, l.tools.y),
            _ => (l.tools.y, l.h),
        };
        (a.round().max(0.0) as u32, (b.round().max(0.0) as u32).min(l.h as u32))
    }

    /// Draw these bands again.
    fn redraw_parts(&self, parts: u8) {
        self.dirty.set(self.dirty.get() | parts);
        for part in [HEADER, PREVIEW, CONTROLS, TIMELINE] {
            if parts & part != 0 {
                self.host.redraw(Some(self.rows(part)));
            }
        }
    }

    fn say(&mut self, text: impl Into<String>, error: bool, reveal: Option<PathBuf>) {
        self.toast = Some(Toast { text: text.into(), error, at: Instant::now(), reveal });
        self.redraw();
    }

    // --- files ---------------------------------------------------------------

    fn open_dialog(&mut self) {
        let dir = self.doc.as_ref().and_then(|d| d.path.parent()).map(Path::to_path_buf);
        let (slot, wake) = (self.dialog.clone(), self.wake());
        self.host.pick_file(
            dir,
            Box::new(move |p| {
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Dialog::Open(p));
                wake();
            }),
        );
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
        let src = Source::new(path.to_path_buf(), &info);
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
        self.host.set_title(&format!("{} — MP4 Splitter", file_name(path)));
        if !self.decoder {
            self.say(player::NO_DECODER_TOAST, true, None);
        }
        self.request_still();
        self.redraw();
    }

    fn export(&mut self) {
        if self.exporting.is_some() {
            return;
        }
        let Some(d) = &self.doc else { return self.open_dialog() };
        let next_to = d.path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."));
        if self.mods.shift {
            let (slot, wake) = (self.dialog.clone(), self.wake());
            self.host.pick_folder(
                Some(next_to),
                Box::new(move |p| {
                    *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(Dialog::ExportTo(p));
                    wake();
                }),
            );
            return;
        }
        self.export_to(next_to);
    }

    fn export_to(&mut self, dir: PathBuf) {
        let Some(d) = &self.doc else { return };
        let pieces = d.plan.kept_pieces();
        let join = self.join && d.plan.on.len() > 1;
        let outs = if join { vec![split::joined_path(&d.path, &dir)] } else { split::output_paths(&d.path, &dir, pieces.len()) };
        self.exporting = Some(export::start(d.path.clone(), pieces, outs, join, self.wake()));
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
        let (r, _) = picture_rect(vw as f32, vh as f32, self.l.preview);
        // Exactly the rectangle it is shown in, so it is copied pixel for pixel;
        // never larger than the video (it is scaled up on screen then).
        let (w, h) = if r.w >= vw as f32 { (vw, vh) } else { (r.w as u32, r.h as u32) };
        // ffmpeg's scaler wants even sizes.
        if cfg!(windows) { (w.max(2), h.max(2)) } else { (w.max(2) & !1, h.max(2) & !1) }
    }

    fn request_still(&mut self) {
        let size = self.video_size();
        let t = self.playhead;
        if let Some(d) = &mut self.doc {
            if size.0 > 0 && d.src.available && !d.info.frames.is_empty() {
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
        // Not from a piece that is switched off: the next kept one, else the first.
        if d.plan.dropped_at(self.playhead) {
            let first = d.plan.kept_pieces().first().map(|p| p.0);
            self.playhead = d.plan.next_kept(self.playhead).or(first).unwrap_or(self.playhead);
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
        self.redraw_parts(CONTROLS | TIMELINE);
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
            Act::Piece(i) => self.toggle_piece(i),
            Act::Join(on) => self.join = on,
            Act::NMinus => self.n = (self.n - 1).max(2),
            Act::NPlus => self.n = (self.n + 1).min(99),
            Act::NApply => self.act(Act::Preset(self.n)),
            Act::ClearCuts => {
                if let Some(d) = &mut self.doc {
                    d.plan.clear_cuts();
                }
            }
            Act::ShowFolder => {
                if let Some(p) = self.toast.as_ref().and_then(|t| t.reveal.clone()) {
                    export::reveal(&p);
                }
            }
            Act::CloseToast => self.toast = None,
            Act::About => self.page = if self.page == Some(Page::About) { None } else { Some(Page::About) },
            Act::Settings => self.page = if self.page == Some(Page::Settings) { None } else { Some(Page::Settings) },
            Act::Theme(choice) => {
                self.config.theme = choice;
                self.settings_changed();
            }
            Act::Accent(i) => {
                self.config.accent = i;
                self.settings_changed();
            }
            Act::AboutLink => crate::links::open(crate::links::POWERNAPPS),
        }
        self.redraw();
    }

    /// Save the settings and show them.
    fn settings_changed(&mut self) {
        self.config.save();
        self.theme = theme::pick(self.os_dark, self.config.theme, self.config.accent);
        self.host.set_colors(&self.theme);
    }

    /// Switch a piece off (or back on).
    fn toggle_piece(&mut self, i: usize) {
        let Some(d) = &mut self.doc else { return };
        let keep = !d.plan.on.get(i).copied().unwrap_or(true);
        if !d.plan.set_kept(i, keep) {
            self.say("At least one piece has to stay", true, None);
        }
    }

    /// The cut at the playhead goes; away from any cut, the piece there is
    /// switched off or on.
    fn remove_at_playhead(&mut self) {
        let s = self.l.s;
        let ph = self.x_of(self.playhead);
        let i = self.doc.as_ref().and_then(|d| d.plan.nearest_cut(self.playhead).filter(|&i| (self.x_of(d.plan.cuts[i]) - ph).abs() < 24.0 * s));
        let piece = self.doc.as_ref().and_then(|d| d.plan.piece_at(self.playhead));
        match (i, piece, &mut self.doc) {
            (Some(i), _, Some(d)) => d.plan.remove_cut(i),
            (None, Some(p), Some(_)) => self.toggle_piece(p),
            _ => self.say("Put the playhead on a cut or inside a piece to remove it", false, None),
        }
        self.redraw();
    }

    /// A key went down.
    pub fn on_key(&mut self, key: Key) {
        if key == Key::F1 {
            return self.act(Act::About);
        }
        if key == Key::Char(',') && self.mods.ctrl {
            return self.act(Act::Settings);
        }
        if self.page.is_some() {
            if key == Key::Escape {
                self.page = None;
                self.redraw();
            }
            return;
        }
        let ctrl = self.mods.ctrl;
        let shift = self.mods.shift;
        match key {
            Key::Space => self.play(),
            Key::Left if ctrl => self.jump(-5.0),
            Key::Right if ctrl => self.jump(5.0),
            Key::Left if shift => self.jump(-1.0),
            Key::Right if shift => self.jump(1.0),
            Key::Left => self.step_frame(-1),
            Key::Right => self.step_frame(1),
            Key::Up | Key::Down => {
                let dir = if key == Key::Down { 1 } else { -1 };
                if let Some(t) = self.doc.as_ref().and_then(|d| split::step(&d.plan.edges(), self.playhead, dir)) {
                    self.seek(t);
                }
            }
            Key::PageUp | Key::PageDown => {
                let dir = if key == Key::PageDown { 1 } else { -1 };
                if let Some(t) = self.doc.as_ref().and_then(|d| split::step(&d.info.keys, self.playhead, dir)) {
                    self.seek(t);
                }
            }
            Key::Home => self.seek(Duration::ZERO),
            Key::End => self.seek(self.duration()),
            Key::Delete | Key::Backspace => self.remove_at_playhead(),
            Key::Escape => {
                if self.toast.is_some() {
                    self.toast = None;
                    self.redraw();
                } else if self.playing.is_some() {
                    self.pause();
                }
            }
            Key::Enter if ctrl => self.export(),
            Key::Enter | Key::F1 => {}
            Key::Char(c) => match c {
                'o' if ctrl => self.open_dialog(),
                'e' | 's' if ctrl => self.export(),
                'q' | 'w' if ctrl => self.quit = true,
                'k' => self.play(),
                'j' => self.jump(-1.0),
                'l' => self.jump(1.0),
                ',' => self.step_frame(-1),
                '.' => self.step_frame(1),
                'i' | '[' => self.act(Act::MarkIn),
                'o' | ']' => self.act(Act::MarkOut),
                's' | 'c' => self.act(Act::Cut),
                'x' => self.remove_at_playhead(),
                'm' => self.act(Act::Mute),
                'n' => self.act(Act::Snap),
                '0' => self.act(Act::ClearCuts),
                '=' | '+' => self.zoom(1.0 / 1.5, self.playhead),
                '-' | '_' => self.zoom(1.5, self.playhead),
                '\\' => self.zoom(1e9, self.playhead),
                '2'..='9' => self.act(Act::Preset(c as usize - '0' as usize)),
                _ => {}
            },
        }
    }

    fn mouse_down(&mut self, button: Button) {
        let (x, y) = self.mouse;
        // A page takes every click: a click outside it closes it.
        if let Some(page) = self.page {
            if button == Button::Left {
                if let Some(a) = self.hit(x, y) {
                    self.pressed = Some(a);
                } else if !self.page_rect(page).contains(x, y) {
                    self.page = None;
                }
                self.redraw();
            }
            return;
        }
        if button == Button::Right {
            if let Some(Marker::Cut(i)) = self.marker_at(x, y) {
                if let Some(d) = &mut self.doc {
                    d.plan.remove_cut(i);
                }
                self.redraw();
            }
            return;
        }
        if button == Button::Middle {
            if let Some(d) = &self.doc {
                if self.l.card.contains(x, y) {
                    self.drag = Some(Drag::Pan(x, d.view.0, d.view.1));
                }
            }
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

    fn mouse_up(&mut self, button: Button) {
        let (x, y) = self.mouse;
        if button == Button::Left {
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
        // Under a page, nothing else can be reached.
        self.hits.iter().rev().find(|h| h.0.contains(x, y) && (self.page.is_none() || h.3 == OVER)).map(|h| h.1)
    }

    fn cursor(&self) -> CursorKind {
        let (x, y) = self.mouse;
        if matches!(self.drag, Some(Drag::Marker(_))) || (self.drag.is_none() && self.marker_at(x, y).is_some()) {
            return CursorKind::EwResize;
        }
        if matches!(self.drag, Some(Drag::Pan(..))) {
            return CursorKind::Grabbing;
        }
        if self.hit(x, y).is_some() {
            return CursorKind::Pointer;
        }
        CursorKind::Default
    }

    // --- painting ------------------------------------------------------------

    /// Draw what changed; the frame.
    pub fn paint(&mut self) -> Frame<'_> {
        let (w, h) = self.host.size();
        let (w, h) = (w.max(1), h.max(1));
        let s = self.host.scale();
        self.l = layout(w as f32, h as f32, s);
        let mut canvas = std::mem::take(&mut self.canvas);
        let size = w as usize * h as usize * 4;
        if canvas.len() != size {
            canvas.resize(size, 0);
            self.dirty.set(ALL);
        }
        let mut text = self.text.take().unwrap_or_else(Text::new);
        let dirty = self.dirty.replace(0);
        // A paint with nothing to draw (the window was uncovered) keeps them all.
        if dirty != 0 {
            self.hits.retain(|hit| hit.3 & dirty == 0 && hit.3 != OVER);
        }
        // Everything at once, or band by band (each on its own rows: what
        // floats over them is cut to what is drawn).
        let whole = [(ALL, 0, h)];
        let bands: Vec<(u8, u32, u32)> = if dirty == ALL {
            whole.to_vec()
        } else {
            [HEADER, PREVIEW, CONTROLS, TIMELINE].iter().filter(|p| dirty & **p != 0).map(|&p| (p, self.rows(p).0, self.rows(p).1)).collect()
        };
        for (part, y0, y1) in bands {
            let row = w as usize * 4;
            let Some(pixels) = canvas.get_mut(y0 as usize * row..y1 as usize * row) else { continue };
            let fb = Fb::with_origin(pixels, w as usize, 0, y0 as i32, Order::Bgra);
            let mut c = Canvas { fb, text: &mut text, s };
            self.draw(&mut c, part);
        }
        self.canvas = canvas;
        self.text = Some(text);
        self.last_paint = Instant::now();
        Frame { w, h, bgra: &self.canvas }
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
        self.hits.push((r, act, tip, self.part));
    }

    fn hit_hover(&self, r: Rect) -> bool {
        r.contains(self.mouse.0, self.mouse.1) && self.drag.is_none() && (self.page.is_none() || self.part == OVER)
    }

    fn draw(&mut self, c: &mut Canvas, part: u8) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        c.clear(t.bg);
        if part & HEADER != 0 {
            self.part = HEADER;
            self.draw_header(c);
        }
        if part & PREVIEW != 0 {
            self.part = PREVIEW;
            // The stage: only where a picture leaves bars for it.
            let fills = self.doc.as_ref().is_some_and(|d| {
                let (vw, vh) = d.info.size.unwrap_or((16, 9));
                d.frame.is_some() && picture_rect(vw as f32, vh as f32, l.preview).1
            });
            if !fills {
                c.rrect(snapped(l.preview), PREVIEW_RADIUS * s, t.stage);
            }
            self.draw_preview(c);
        }
        if self.doc.is_some() {
            if part & CONTROLS != 0 {
                self.part = CONTROLS;
                self.draw_controls(c);
            }
            if part & TIMELINE != 0 {
                self.part = TIMELINE;
                self.draw_tools(c);
            }
        }
        if part & TIMELINE != 0 {
            self.part = TIMELINE;
            c.shadow(l.card, 12.0 * s);
            c.rrect(l.card, 12.0 * s, t.surface);
            c.rrect_stroke(l.card, 12.0 * s, 1.0 * s, t.border);
            self.draw_timeline(c);
            let hint = "Space play · ← → frame · Shift 1 s · ↑ ↓ cuts · I / O start / end · S cut · Del remove · 2–9 split · wheel zoom";
            let hint =
                if c.text.measure(hint, 12.0 * s) > l.hint.w { "Space play · ← → frame · I / O start / end · S cut · 2–9 split" } else { hint };
            c.text(l.hint.cx(), l.hint.cy() + 4.0 * s, 12.0 * s, t.text_faint, hint, Align::Center);
        }
        self.draw_toast(c);
        match self.page {
            Some(Page::About) => self.draw_about(c),
            Some(Page::Settings) => self.draw_settings(c),
            None => self.draw_tooltip(c),
        }
        if self.drop_hover {
            c.rrect(l.preview.inset(6.0 * s, 6.0 * s), 12.0 * s, t.accent_soft);
            c.rrect_stroke(l.preview.inset(6.0 * s, 6.0 * s), 12.0 * s, 2.0 * s, t.accent);
        }
    }

    fn draw_header(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let l = self.l;
        let s = l.s;
        let (x0, cy) = (16.0 * s, l.header.cy());
        // The mark; the name is in the window's title bar.
        c.rrect(Rect::new(x0, cy - 15.0 * s, 30.0 * s, 30.0 * s), 8.0 * s, t.accent);
        c.icon(Icon::Scissors, Rect::new(x0 + 6.0 * s, cy - 9.0 * s, 18.0 * s, 18.0 * s), t.on_accent);
        let x = x0 + 42.0 * s;
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
                    let n = d.plan.kept_pieces().len();
                    (if n == 1 || self.join { "Export".to_string() } else { format!("Export {n} pieces") }, false)
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
        // Separate files or one, when there is more than one piece.
        let mut left = er.x;
        if self.doc.as_ref().is_some_and(|d| d.plan.on.len() > 1) {
            let one = c.text.measure("One file", 13.0 * s) + 18.0 * s + 6.0 * s + 24.0 * s;
            let sep = c.text.measure("Separate files", 13.0 * s) + 18.0 * s + 6.0 * s + 24.0 * s;
            let join = self.join;
            let (ro, rs) = (Rect::new(left - 8.0 * s - one, by, one, bh), Rect::new(left - 8.0 * s - one - 2.0 * s - sep, by, sep, bh));
            let on = |yes: bool| if yes { Style::On } else { Style::Secondary };
            self.button(
                c,
                ro,
                Act::Join(true),
                "Join the kept pieces, in order, into one file, without re-encoding",
                Some(Icon::File),
                Some("One file"),
                on(join),
            );
            self.button(c, rs, Act::Join(false), "Write each kept piece to its own file", Some(Icon::Files), Some("Separate files"), on(!join));
            left = rs.x;
        }
        let ow = c.text.measure("Open", 13.0 * s) + 18.0 * s + 6.0 * s + 28.0 * s;
        let or = Rect::new(left - 8.0 * s - ow, by, ow, bh);
        let ir = Rect::new(or.x - 4.0 * s - bh, by, bh, bh);
        self.button(c, ir, Act::About, "About MP4 Splitter (F1)", Some(Icon::Info), None, Style::Ghost);
        let gr = Rect::new(ir.x - 2.0 * s - bh, by, bh, bh);
        self.button(c, gr, Act::Settings, "Settings (Ctrl+,)", Some(Icon::Gear), None, Style::Ghost);
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
            let meta = ellipsize(c.text, &meta.join("  ·  "), 12.0 * s, gr.x - x - 16.0 * s);
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
            if !self.decoder {
                c.text(l.preview.cx(), r.bottom() + 30.0 * s, 12.0 * s, t.error, player::NO_DECODER_HINT, Align::Center);
            }
            return;
        };
        let (vw, vh) = d.info.size.unwrap_or((16, 9));
        let (r, _) = picture_rect(vw as f32, vh as f32, l.preview);
        match &d.frame {
            Some((_, img)) => c.image(img, r, Some((r, PREVIEW_RADIUS * s))),
            None => {
                c.rrect(r, PREVIEW_RADIUS * s, crate::paint::hex(0xFFFFFF, 6));
                let msg = if d.info.frames.is_empty() {
                    "No video track (sound only)"
                } else if !d.src.available {
                    player::NO_DECODER_PREVIEW
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
        let kept_pieces = d.plan.kept_pieces().len();
        let kept = d.plan.kept();
        let has_cuts = !d.plan.cuts.is_empty();
        let mut x = r.x + 4.0 * s;
        c.icon(Icon::Columns, Rect::new(x, cy - 8.0 * s, 16.0 * s, 16.0 * s), t.icon);
        x += 22.0 * s;
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
        let w = c.text.measure(&lbl, 13.0 * s) + 18.0 * s + 6.0 * s + 24.0 * s;
        let style = if pieces == n && self.equal_split(n) { Style::On } else { Style::Secondary };
        self.button(
            c,
            Rect::new(x, cy - bh / 2.0, w, bh),
            Act::NApply,
            "Split the kept part into this many equal pieces",
            Some(Icon::Columns),
            Some(&lbl),
            style,
        );
        x += w + 6.0 * s;
        if has_cuts {
            self.button(c, Rect::new(x, cy - bh / 2.0, bh, bh), Act::ClearCuts, "Remove all cuts (0)", Some(Icon::Trash), None, Style::Ghost);
            x += bh;
        }
        // The summary, on the right.
        let summary = format!("{}  ·  {} piece{}", split::timecode(kept), kept_pieces, if kept_pieces == 1 { "" } else { "s" });
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
        let each = d.plan.span().as_secs_f64() / n as f64;
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
                if let Some(img) = d.thumbs.get(key) {
                    // Cover the cell, centred.
                    let k = (cellr.w / img.w as f32).max(clip.h / img.h as f32);
                    let (iw, ih) = (img.w as f32 * k, img.h as f32 * k);
                    let dst = Rect::new(cellr.cx() - iw / 2.0, cellr.cy() - ih / 2.0, iw, ih);
                    c.image(&img, dst, Some((cellr, 0.0)));
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
            let per = player::PEAKS_PER_SEC as f64;
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
                    let kept = tt >= d.plan.start && tt <= d.plan.end && !d.plan.dropped_at(tt);
                    let col = if kept { t.accent.with_alpha(200) } else { t.text_faint.with_alpha(150) };
                    let hh = (v * amp).max(0.5 * s);
                    c.rect(Rect::new(x, mid - hh, (step - 0.5 * s).max(1.0), hh * 2.0), col);
                }
                x += step;
            }
        } else if d.info.has_audio && d.src.available {
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
        // Pieces switched off: dimmed and struck through.
        for ((a, b), on) in d.plan.pieces().into_iter().zip(&d.plan.on) {
            let (x0, x1) = (x_of(a).max(track.x), x_of(b).min(track.right()));
            if !*on && x1 > x0 {
                c.rect(Rect::new(x0, body.y, x1 - x0, body.h), t.dim);
                c.line(x0, body.cy(), x1, body.cy(), 2.0 * s, t.cut.with_alpha(220));
            }
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
            let on = d.plan.on.get(i).copied().unwrap_or(true);
            let full = format!("{}  ·  {}", i + 1, split::timecode(*b - *a));
            let lbl = if c.text.measure(&full, 11.0 * s) + 20.0 * s < x1 - x0 { full } else { (i + 1).to_string() };
            let tw = c.text.measure(&lbl, 11.0 * s);
            let one = pieces.len() == 1;
            let w = tw + 14.0 * s + if one { 0.0 } else { 15.0 * s };
            let cx = (x0 + x1) / 2.0;
            let pr = Rect::new(cx - w / 2.0, l.labels.y + 5.0 * s, w, 18.0 * s);
            let hot = self.hover == Some(Act::Piece(i));
            c.rrect(pr, 9.0 * s, if on { t.accent_soft } else { t.hover });
            if hot {
                c.rrect_stroke(pr, 9.0 * s, 1.0 * s, if on { t.accent } else { t.text_muted });
            }
            let (ix, tx) = (pr.x + 6.0 * s, pr.x + if one { 7.0 } else { 21.0 } * s);
            if !one {
                c.icon(
                    if on { Icon::Close } else { Icon::Undo },
                    Rect::new(ix, pr.cy() - 5.5 * s, 11.0 * s, 11.0 * s),
                    if on { t.accent } else { t.text_muted },
                );
            }
            c.text(tx, pr.cy() + 4.0 * s, 11.0 * s, if on { t.accent } else { t.text_faint }, &lbl, Align::Left);
            if !on {
                c.line(tx - 1.0 * s, pr.cy(), tx + tw + 1.0 * s, pr.cy(), 1.0 * s, t.text_muted);
            }
            if !one {
                self.hits.push((pr, Act::Piece(i), if on { "Leave this piece out (Delete at the playhead)" } else { "Put this piece back" }, self.part));
            }
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
        let extra = if reveal { c.text.measure("Show folder", px) + 52.0 * s } else { 0.0 } + 36.0 * s;
        let w = tw + 40.0 * s + extra;
        let r = Rect::new(self.l.w / 2.0 - w / 2.0, self.l.preview.bottom() - 60.0 * s, w, 42.0 * s);
        c.shadow(r, 12.0 * s);
        c.rrect(r, 12.0 * s, t.tooltip_bg);
        c.circle(r.x + 18.0 * s, r.cy(), 4.0 * s, if error { t.error } else { t.success });
        c.text(r.x + 32.0 * s, r.cy() + 4.5 * s, px, t.tooltip_text, &text, Align::Left);
        let mut x = r.x + 32.0 * s + tw + 10.0 * s;
        if reveal {
            let bw = c.text.measure("Show folder", px) + 42.0 * s;
            let br = Rect::new(x, r.cy() - 14.0 * s, bw, 28.0 * s);
            let hover = self.hit_hover(br);
            c.rrect(br, 7.0 * s, if hover { t.accent_hover } else { t.accent });
            c.icon(Icon::Folder, Rect::new(br.x + 10.0 * s, br.cy() - 8.0 * s, 16.0 * s, 16.0 * s), t.on_accent);
            c.text(br.x + 32.0 * s, br.cy() + 4.5 * s, px, t.on_accent, "Show folder", Align::Left);
            self.hits.push((br, Act::ShowFolder, "", OVER));
            x += bw + 4.0 * s;
        }
        let cr = Rect::new(x, r.cy() - 13.0 * s, 26.0 * s, 26.0 * s);
        if self.hit_hover(cr) {
            c.rrect(cr, 7.0 * s, crate::paint::hex(0xFFFFFF, 20));
        }
        c.icon(Icon::Close, cr.inset(6.0 * s, 6.0 * s), t.tooltip_text.with_alpha(180));
        self.hits.push((cr, Act::CloseToast, "", OVER));
    }

    /// Where a page is.
    fn page_rect(&self, page: Page) -> Rect {
        let s = self.l.s;
        let (w, h) = match page {
            // Its one-line description, with margins.
            Page::About => (Text::new().measure(ABOUT_LINE, 13.0 * s) + 64.0 * s, 296.0 * s),
            Page::Settings => (420.0 * s, 212.0 * s),
        };
        Rect::new((self.l.w - w) / 2.0, (self.l.h - h) / 2.0, w, h)
    }

    /// The dimmed window and the empty page.
    fn draw_page(&mut self, c: &mut Canvas, page: Page) -> Rect {
        let t = self.theme;
        let s = self.l.s;
        let (bx0, by0, bx1, by1) = c.fb.bounds();
        c.rect(Rect::new(bx0 as f32, by0 as f32, (bx1 - bx0) as f32, (by1 - by0) as f32), crate::paint::hex(0, 120));
        let r = self.page_rect(page);
        c.shadow(r, 16.0 * s);
        c.rrect(r, 16.0 * s, t.surface);
        c.rrect_stroke(r, 16.0 * s, 1.0 * s, t.border);
        self.part = OVER;
        r
    }

    fn draw_settings(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let s = self.l.s;
        let r = self.draw_page(c, Page::Settings);
        c.text(r.x + 24.0 * s, r.y + 40.0 * s, 18.0 * s, t.text, "Settings", Align::Left);
        // Theme.
        let y = r.y + 62.0 * s;
        c.text(r.x + 24.0 * s, y + 21.0 * s, 13.0 * s, t.text_muted, "Theme", Align::Left);
        let mut x = r.x + 120.0 * s;
        for (choice, label) in [(ThemeChoice::System, "System"), (ThemeChoice::Dark, "Dark"), (ThemeChoice::Light, "Light")] {
            let w = c.text.measure(label, 13.0 * s) + 24.0 * s;
            let style = if self.config.theme == choice { Style::On } else { Style::Secondary };
            self.button(c, Rect::new(x, y, w, 32.0 * s), Act::Theme(choice), "", None, Some(label), style);
            x += w + 6.0 * s;
        }
        // Accent.
        let y = y + 50.0 * s;
        c.text(r.x + 24.0 * s, y + 21.0 * s, 13.0 * s, t.text_muted, "Accent", Align::Left);
        let mut x = r.x + 120.0 * s;
        let d = 28.0 * s;
        for (i, (dark, light)) in theme::ACCENTS.iter().enumerate() {
            let sw = Rect::new(x, y + 2.0 * s, d, d);
            let col = if t.dark { *dark } else { *light };
            c.circle(sw.cx(), sw.cy(), d / 2.0 - 3.0 * s, col);
            if self.config.accent == i {
                c.ring(sw.cx(), sw.cy(), d / 2.0 - 0.5 * s, 1.5 * s, col);
            }
            self.hits.push((sw, Act::Accent(i), "", OVER));
            x += d + 8.0 * s;
        }
    }

    fn draw_about(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let s = self.l.s;
        let r = self.draw_page(c, Page::About);
        let cx = r.cx();
        // The mark, the name, the version, what it is.
        let m = 72.0 * s;
        let mr = Rect::new(cx - m / 2.0, r.y + 28.0 * s, m, m);
        c.rrect(mr, 18.0 * s, t.accent);
        c.icon(Icon::Scissors, mr.inset(14.0 * s, 14.0 * s), t.on_accent);
        c.text(cx, mr.bottom() + 36.0 * s, 22.0 * s, t.text, "MP4 Splitter", Align::Center);
        c.text(cx, mr.bottom() + 58.0 * s, 13.0 * s, t.text_muted, concat!("Version ", env!("CARGO_PKG_VERSION")), Align::Center);
        c.text(cx, mr.bottom() + 88.0 * s, 13.0 * s, t.text, ABOUT_LINE, Align::Center);
        let sep = mr.bottom() + 106.0 * s;
        c.rect(Rect::new(r.x + 24.0 * s, sep, r.w - 48.0 * s, 1.0 * s), t.separator);
        // More from PowerNapps.
        let label = "More from PowerNapps";
        let link = "powernapps.net";
        let (lw, kw) = (c.text.measure(label, 13.0 * s), c.text.measure(link, 13.0 * s));
        let x0 = cx - (lw + 8.0 * s + kw) / 2.0;
        let y = sep + 28.0 * s;
        c.text(x0, y, 13.0 * s, t.text_muted, label, Align::Left);
        let lx = x0 + lw + 8.0 * s;
        c.text(lx, y, 13.0 * s, t.accent, link, Align::Left);
        let hit = Rect::new(lx - 4.0 * s, y - 15.0 * s, kw + 8.0 * s, 22.0 * s);
        if hit.contains(self.mouse.0, self.mouse.1) {
            c.rect(Rect::new(lx, y + 3.0 * s, kw, 1.0 * s), t.accent);
        }
        self.hits.push((hit, Act::AboutLink, "", OVER));
        // The footer. The font has no copyright sign: a ring with a small c.
        let foot = "2026 PowerNapps  ·  Made in Los Angeles by Denis Platonov  ·  Apache-2.0 License";
        let px = 11.5 * s;
        let rad = 5.0 * s;
        let fw = c.text.measure(foot, px) + 2.0 * rad + 5.0 * s;
        let (fx, fy) = (cx - fw / 2.0, sep + 58.0 * s);
        c.ring(fx + rad, fy - 4.0 * s, rad, 0.9 * s, t.text_faint);
        c.text(fx + rad, fy - 1.0 * s, 8.5 * s, t.text_faint, "c", Align::Center);
        c.text(fx + 2.0 * rad + 5.0 * s, fy, px, t.text_faint, foot, Align::Left);
    }

    fn draw_tooltip(&mut self, c: &mut Canvas) {
        let t = self.theme;
        let s = self.l.s;
        if self.drag.is_some() || self.pressed.is_some() || self.hover_since.elapsed() < Duration::from_millis(550) {
            return;
        }
        let (mx, my) = self.mouse;
        let Some((r, _, tip, _)) = self.hits.iter().rev().find(|h| h.0.contains(mx, my)).cloned() else { return };
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

/// `r` on whole pixels.
fn snapped(r: Rect) -> Rect {
    let (x0, y0) = (r.x.round(), r.y.round());
    Rect::new(x0, y0, r.right().round() - x0, r.bottom().round() - y0)
}

/// Where a `w` × `h` picture goes in the preview `p`: on whole pixels, sharing
/// the preview's edges wherever it reaches them (bars of a pixel or two are
/// not worth showing). The one rectangle for the picture, its corners and what
/// is behind it; and whether it covers the preview altogether.
fn picture_rect(w: f32, h: f32, p: Rect) -> (Rect, bool) {
    let p = snapped(p);
    let f = snapped(fit(w, h, p));
    let (full_w, full_h) = (p.w - f.w <= 2.0, p.h - f.h <= 2.0);
    let r = Rect::new(if full_w { p.x } else { f.x }, if full_h { p.y } else { f.y }, if full_w { p.w } else { f.w }, if full_h { p.h } else { f.h });
    (r, full_w && full_h)
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

impl App {
    /// Something finished on another thread.
    pub fn on_wake(&mut self) {
        // A still arrived?
        if let Some(d) = &mut self.doc {
            let got = d.stills.done.lock().ok().and_then(|mut g| g.take());
            if let Some((seq, t, p)) = got {
                if seq == d.last_still && self.playing.is_none() {
                    d.frame = Some((t, p));
                    self.redraw_parts(PREVIEW);
                }
            }
        }
        let answer = self.dialog.lock().unwrap_or_else(|e| e.into_inner()).take();
        match answer {
            Some(Dialog::Open(Some(p))) => self.open(&p),
            Some(Dialog::ExportTo(Some(dir))) => self.export_to(dir),
            _ => {}
        }
        if self.exporting.is_some() {
            self.check_export();
            self.redraw_parts(HEADER);
        }
        // Thumbnails and the waveform come in here too; playing, the tick draws them.
        if self.playing.is_none() {
            self.redraw_parts(TIMELINE);
        }
    }

    /// The window's size or scale changed.
    pub fn on_resize(&mut self) {
        let (w, h) = self.host.size();
        self.l = layout(w as f32, h as f32, self.host.scale());
        if self.playing.is_none() {
            self.request_still();
        }
        self.redraw();
    }

    /// The OS's theme changed.
    pub fn on_theme(&mut self, dark: Option<bool>) {
        self.os_dark = dark;
        self.settings_changed();
        self.redraw();
    }

    pub fn on_mods(&mut self, m: Mods) {
        self.mods = m;
    }

    /// The pointer moved, in the window.
    pub fn on_move(&mut self, x: f32, y: f32) {
        let before = self.hit(self.mouse.0, self.mouse.1);
        self.mouse = (x, y);
        let now = self.hit(x, y);
        if now != before {
            self.hover = now;
            self.hover_since = Instant::now();
        }
        if self.drag.is_some() {
            self.drag_to(x);
        }
        self.host.set_cursor(self.cursor());
        // What a pointer changes: a control's look, or the timeline's hover line.
        if self.page.is_some() {
            if now != before {
                self.redraw();
            }
        } else if now != before || self.doc.is_none() {
            self.redraw();
        } else if self.l.card.contains(x, y) || self.drag.is_some() {
            self.redraw_parts(TIMELINE);
        }
    }

    /// The pointer left the window.
    pub fn on_leave(&mut self) {
        self.mouse = (-1.0, -1.0);
        self.redraw();
    }

    pub fn on_button(&mut self, b: Button, down: bool) {
        if down { self.mouse_down(b) } else { self.mouse_up(b) }
    }

    /// The wheel turned: lines, up and right positive.
    pub fn on_wheel(&mut self, dx: f64, dy: f64) {
        if self.page.is_some() {
            return;
        }
        let (mx, my) = self.mouse;
        if self.l.card.contains(mx, my) {
            if self.mods.shift || dx.abs() > dy.abs() {
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

    /// A file is being dragged over the window, or no longer.
    pub fn on_hover_file(&mut self, on: bool) {
        self.drop_hover = on;
        self.redraw();
    }

    /// A file was dropped on the window.
    pub fn on_drop(&mut self, p: &Path) {
        self.drop_hover = false;
        self.open(p);
    }

    /// Between events: playback moves on. When to come back, and whether to quit.
    pub fn tick(&mut self) -> Tick {
        if self.quit {
            return Tick { wait: None, exit: true };
        }
        if self.autoclose.is_some_and(|t| Instant::now() >= t) {
            if let Ok(dir) = std::env::var("MP4_SPLITTER_SCREENSHOT") {
                self.screenshot(Path::new(&dir));
            }
            return Tick { wait: None, exit: true };
        }
        // Playing into a piece that is switched off: on to the next kept one,
        // or stop when none is left.
        if let (Some(pb), Some(d)) = (&self.playing, &self.doc) {
            let now = pb.clock(self.audio.as_ref());
            if d.plan.dropped_at(now) {
                match d.plan.next_kept(now) {
                    Some(t) => self.seek(t),
                    None => self.pause(),
                }
            }
        }
        let mut wait: Option<Instant> = self.autoclose;
        let earliest = |w: &mut Option<Instant>, t: Instant| *w = Some(w.map_or(t, |x| x.min(t)));
        if let Some(pb) = &mut self.playing {
            let now = pb.clock(self.audio.as_ref());
            let dur = self.doc.as_ref().map_or(Duration::ZERO, |d| d.info.duration);
            let mut fresh = false;
            if let Some(f) = pb.due(now) {
                if let Some(d) = &mut self.doc {
                    d.frame = Some((f.t, f.pic));
                }
                fresh = true;
            }
            if fresh {
                self.redraw_parts(PREVIEW);
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
                    self.redraw_parts(CONTROLS | TIMELINE);
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
        if self.toast.as_ref().is_some_and(|t| t.at.elapsed() > Duration::from_secs(if t.error { 9 } else { 12 })) {
            self.redraw();
        }
        if self.hover.is_some() && self.hover_since.elapsed() >= Duration::from_millis(550) && self.hover_since.elapsed() < Duration::from_millis(700) {
            self.redraw();
        }
        Tick { wait, exit: false }
    }

    fn screenshot(&mut self, dir: &Path) {
        self.paint();
        let (w, h) = self.host.size();
        if w > 0 && self.canvas.len() == w as usize * h as usize * 4 {
            let name = std::env::var("MP4_SPLITTER_SCREENSHOT_NAME").unwrap_or_else(|_| "screenshot.png".into());
            if let Err(e) = std::fs::write(dir.join(name), crate::paint::png(w, h, &self.canvas)) {
                eprintln!("screenshot: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_picture_rectangle_stays_on_the_preview_at_any_scale() {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0] {
            for (ww, wh) in [(1180.0, 780.0), (1181.0, 781.0), (760.0, 560.0), (1919.0, 1033.0)] {
                let l = layout(ww * scale, wh * scale, scale);
                let p = snapped(l.preview);
                for (vw, vh) in [(1920.0, 1080.0), (1148.0, 430.0), (1920.0, 720.0), (1080.0, 1920.0), (640.0, 480.0)] {
                    let (r, fills) = picture_rect(vw, vh, l.preview);
                    // Whole pixels, inside the preview.
                    for v in [r.x, r.y, r.w, r.h] {
                        assert_eq!(v, v.round(), "{r:?} at {scale}");
                    }
                    assert!(r.x >= p.x && r.y >= p.y && r.right() <= p.right() && r.bottom() <= p.bottom(), "{r:?} in {p:?} at {scale}");
                    // Touching along at least one axis, exactly.
                    assert!((r.x == p.x && r.w == p.w) || (r.y == p.y && r.h == p.h), "{r:?} in {p:?} at {scale}");
                    assert_eq!(fills, r == p);
                }
            }
        }
    }

    #[test]
    fn a_picture_of_the_previews_shape_covers_it() {
        let p = Rect::new(16.0, 56.0, 1148.0, 430.0);
        assert_eq!(picture_rect(1148.0, 430.0, p), (p, true));
        // A hair off: still no bars of a pixel.
        assert_eq!(picture_rect(1920.0, 720.0, p), (p, true));
        // Really narrower: bars at the sides, and top and bottom shared.
        let (r, fills) = picture_rect(1280.0, 720.0, p);
        assert!(!fills);
        assert_eq!((r.y, r.h), (p.y, p.h));
        assert!(r.w < p.w);
    }
}
