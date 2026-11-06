//! Drawing: anti-aliased shapes (tiny-skia), text (Inter, rasterised with
//! ab_glyph and cached), pictures and the icons, all in physical pixels.

use ab_glyph::{Font, FontRef, GlyphId, PxScale, ScaleFont, point};
use std::collections::HashMap;
use tiny_skia::{FillRule, FilterQuality, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Rect as SkRect, Stroke, Transform};

pub static FONT: &[u8] = include_bytes!("../assets/fonts/Inter-Medium.subset.ttf");

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Rect {
        Rect { x, y, w, h }
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    pub fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
    pub fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.x && x < self.right() && y >= self.y && y < self.bottom()
    }
    pub fn inset(&self, dx: f32, dy: f32) -> Rect {
        Rect::new(self.x + dx, self.y + dy, (self.w - 2.0 * dx).max(0.0), (self.h - 2.0 * dy).max(0.0))
    }
    fn sk(&self) -> Option<SkRect> {
        SkRect::from_xywh(self.x, self.y, self.w.max(0.01), self.h.max(0.01))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct C {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

/// `0xRRGGBB` at `a` (0..255).
pub const fn hex(v: u32, a: u8) -> C {
    C { r: (v >> 16) as u8, g: (v >> 8) as u8, b: v as u8, a }
}

impl C {
    pub fn with_alpha(self, a: u8) -> C {
        C { a, ..self }
    }
    fn paint(self) -> Paint<'static> {
        let mut p = Paint::default();
        p.set_color_rgba8(self.r, self.g, self.b, self.a);
        p.anti_alias = true;
        p
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

struct Glyph {
    w: usize,
    h: usize,
    left: i32,
    top: i32,
    cov: Vec<u8>,
}

pub struct Text {
    font: FontRef<'static>,
    cache: HashMap<(GlyphId, u32), Option<Glyph>>,
}

impl Text {
    pub fn new() -> Text {
        Text { font: FontRef::try_from_slice(FONT).expect("the built-in font"), cache: HashMap::new() }
    }

    fn id(&self, c: char) -> GlyphId {
        let id = self.font.glyph_id(c);
        if id.0 == 0 {
            // Not in the subset: a look-alike that is.
            let alt = match c {
                '–' | '—' | '−' => '-',
                '×' => 'x',
                '…' => '.',
                '·' => '.',
                _ => '?',
            };
            return self.font.glyph_id(alt);
        }
        id
    }

    /// The width of `s` at `px`.
    pub fn measure(&self, s: &str, px: f32) -> f32 {
        let f = self.font.as_scaled(PxScale::from(px));
        let mut w = 0.0;
        let mut prev: Option<GlyphId> = None;
        for c in s.chars() {
            let id = self.id(c);
            if let Some(p) = prev {
                w += f.kern(p, id);
            }
            w += f.h_advance(id);
            prev = Some(id);
        }
        w
    }

    fn glyph(&mut self, id: GlyphId, px: f32) -> Option<&Glyph> {
        let key = (id, (px * 4.0).round() as u32);
        let font = &self.font;
        self.cache
            .entry(key)
            .or_insert_with(|| {
                let g = id.with_scale_and_position(PxScale::from(px), point(0.0, 0.0));
                let o = font.outline_glyph(g)?;
                let b = o.px_bounds();
                let (w, h) = (b.width() as usize, b.height() as usize);
                let mut cov = vec![0u8; w * h];
                o.draw(|x, y, c| {
                    if let Some(v) = cov.get_mut(y as usize * w + x as usize) {
                        *v = (c.clamp(0.0, 1.0) * 255.0) as u8;
                    }
                });
                Some(Glyph { w, h, left: b.min.x as i32, top: b.min.y as i32, cov })
            })
            .as_ref()
    }
}

/// A picture to draw (premultiplied; ours are opaque).
pub fn pixmap(w: u32, h: u32, rgba: Vec<u8>) -> Option<Pixmap> {
    Pixmap::from_vec(rgba, tiny_skia::IntSize::from_wh(w, h)?)
}

pub struct Canvas<'a> {
    pub pm: &'a mut Pixmap,
    pub text: &'a mut Text,
    /// Physical pixels a logical one.
    pub s: f32,
}

impl Canvas<'_> {
    pub fn clear(&mut self, c: C) {
        self.pm.fill(tiny_skia::Color::from_rgba8(c.r, c.g, c.b, 255));
    }

    /// A rectangle snapped to whole pixels (crisp, and tiny-skia's
    /// anti-aliased fill trips over slivers under a pixel wide).
    pub fn rect(&mut self, r: Rect, c: C) {
        let (x0, y0) = (r.x.round(), r.y.round());
        let (x1, y1) = (r.right().round().max(x0 + 1.0), r.bottom().round().max(y0 + 1.0));
        if let Some(sr) = SkRect::from_ltrb(x0, y0, x1, y1) {
            let mut p = c.paint();
            p.anti_alias = false;
            self.pm.fill_rect(sr, &p, Transform::identity(), None);
        }
    }

    fn rrect_path(r: Rect, rad: f32) -> Option<tiny_skia::Path> {
        let rad = rad.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
        let k = 0.552_284_8 * rad;
        let (x0, y0, x1, y1) = (r.x, r.y, r.right(), r.bottom());
        let mut pb = PathBuilder::new();
        pb.move_to(x0 + rad, y0);
        pb.line_to(x1 - rad, y0);
        pb.cubic_to(x1 - rad + k, y0, x1, y0 + rad - k, x1, y0 + rad);
        pb.line_to(x1, y1 - rad);
        pb.cubic_to(x1, y1 - rad + k, x1 - rad + k, y1, x1 - rad, y1);
        pb.line_to(x0 + rad, y1);
        pb.cubic_to(x0 + rad - k, y1, x0, y1 - rad + k, x0, y1 - rad);
        pb.line_to(x0, y0 + rad);
        pb.cubic_to(x0, y0 + rad - k, x0 + rad - k, y0, x0 + rad, y0);
        pb.close();
        pb.finish()
    }

    pub fn rrect(&mut self, r: Rect, rad: f32, c: C) {
        if let Some(p) = Self::rrect_path(r, rad) {
            self.pm.fill_path(&p, &c.paint(), FillRule::Winding, Transform::identity(), None);
        }
    }

    pub fn rrect_stroke(&mut self, r: Rect, rad: f32, width: f32, c: C) {
        let r = r.inset(width / 2.0, width / 2.0);
        if let Some(p) = Self::rrect_path(r, rad) {
            let st = Stroke { width, ..Stroke::default() };
            self.pm.stroke_path(&p, &c.paint(), &st, Transform::identity(), None);
        }
    }

    /// A soft drop shadow under a rounded rectangle.
    pub fn shadow(&mut self, r: Rect, rad: f32) {
        let s = self.s;
        for (dy, spread, a) in [(10.0, 8.0, 5u8), (6.0, 4.0, 9), (3.0, 2.0, 14), (1.0, 0.0, 24)] {
            let rr = Rect::new(r.x - spread * s, r.y + (dy - spread) * s, r.w + 2.0 * spread * s, r.h + 2.0 * spread * s);
            self.rrect(rr, rad + spread * s, hex(0, a));
        }
    }

    pub fn circle(&mut self, x: f32, y: f32, rad: f32, c: C) {
        if let Some(p) = PathBuilder::from_circle(x, y, rad) {
            self.pm.fill_path(&p, &c.paint(), FillRule::Winding, Transform::identity(), None);
        }
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: C) {
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        if let Some(p) = pb.finish() {
            let st = Stroke { width, line_cap: LineCap::Round, ..Stroke::default() };
            self.pm.stroke_path(&p, &c.paint(), &st, Transform::identity(), None);
        }
    }

    pub fn poly(&mut self, pts: &[(f32, f32)], c: C) {
        let mut pb = PathBuilder::new();
        for (i, &(x, y)) in pts.iter().enumerate() {
            if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
        }
        pb.close();
        if let Some(p) = pb.finish() {
            self.pm.fill_path(&p, &c.paint(), FillRule::Winding, Transform::identity(), None);
        }
    }

    /// `img` scaled into `dst`, shown only inside `clip` (rounded by its
    /// radius; `dst` itself when `None`).
    pub fn image(&mut self, img: &Pixmap, dst: Rect, clip: Option<(Rect, f32)>) {
        let (sx, sy) = (dst.w / img.width() as f32, dst.h / img.height() as f32);
        let q = if (sx - 1.0).abs() < 0.001 && (sy - 1.0).abs() < 0.001 { FilterQuality::Nearest } else { FilterQuality::Bilinear };
        let shader = tiny_skia::Pattern::new(img.as_ref(), tiny_skia::SpreadMode::Pad, q, 1.0, Transform::from_row(sx, 0.0, 0.0, sy, dst.x, dst.y));
        let paint = Paint { shader, anti_alias: true, ..Paint::default() };
        let (r, rad) = clip.unwrap_or((dst, 0.0));
        if rad > 0.0 {
            if let Some(p) = Self::rrect_path(r, rad) {
                self.pm.fill_path(&p, &paint, FillRule::Winding, Transform::identity(), None);
            }
        } else if let Some(sr) = r.sk() {
            self.pm.fill_rect(sr, &paint, Transform::identity(), None);
        }
    }

    /// Text with its baseline at `y`; its width.
    pub fn text(&mut self, x: f32, y: f32, px: f32, c: C, s: &str, align: Align) -> f32 {
        let w = self.text.measure(s, px);
        let mut pen = match align {
            Align::Left => x,
            Align::Center => x - w / 2.0,
            Align::Right => x - w,
        };
        let (pw, ph) = (self.pm.width() as i32, self.pm.height() as i32);
        let mut prev: Option<GlyphId> = None;
        let chars: Vec<char> = s.chars().collect();
        for ch in chars {
            let id = self.text.id(ch);
            let (adv, kern) = {
                let f = self.text.font.as_scaled(PxScale::from(px));
                (f.h_advance(id), prev.map_or(0.0, |p| f.kern(p, id)))
            };
            pen += kern;
            let (ox, oy) = (pen.round() as i32, y.round() as i32);
            if let Some(g) = self.text.glyph(id, px) {
                let data = self.pm.data_mut();
                for gy in 0..g.h {
                    let py = oy + g.top + gy as i32;
                    if py < 0 || py >= ph {
                        continue;
                    }
                    for gx in 0..g.w {
                        let px_ = ox + g.left + gx as i32;
                        if px_ < 0 || px_ >= pw {
                            continue;
                        }
                        let cov = g.cov[gy * g.w + gx] as u32 * c.a as u32 / 255;
                        if cov == 0 {
                            continue;
                        }
                        let i = ((py * pw + px_) * 4) as usize;
                        let inv = 255 - cov;
                        // Premultiplied "over".
                        data[i] = ((c.r as u32 * cov + data[i] as u32 * inv) / 255) as u8;
                        data[i + 1] = ((c.g as u32 * cov + data[i + 1] as u32 * inv) / 255) as u8;
                        data[i + 2] = ((c.b as u32 * cov + data[i + 2] as u32 * inv) / 255) as u8;
                        data[i + 3] = (cov + data[i + 3] as u32 * inv / 255) as u8;
                    }
                }
            }
            pen += adv;
            prev = Some(id);
        }
        w
    }

    /// An icon in the `r` box (24-unit Lucide-style strokes).
    pub fn icon(&mut self, icon: Icon, r: Rect, c: C) {
        let k = r.w.min(r.h) / 24.0;
        let (ox, oy) = (r.cx() - 12.0 * k, r.cy() - 12.0 * k);
        let p = |x: f32, y: f32| (ox + x * k, oy + y * k);
        let st = Stroke { width: 2.0 * k, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() };
        let mut strokes: Vec<tiny_skia::Path> = Vec::new();
        let mut fills: Vec<tiny_skia::Path> = Vec::new();
        let poly = |pts: &[(f32, f32)], close: bool| {
            let mut pb = PathBuilder::new();
            for (i, &(x, y)) in pts.iter().enumerate() {
                let (x, y) = p(x, y);
                if i == 0 { pb.move_to(x, y) } else { pb.line_to(x, y) }
            }
            if close {
                pb.close();
            }
            pb.finish()
        };
        let circ = |x: f32, y: f32, rad: f32| {
            let (x, y) = p(x, y);
            PathBuilder::from_circle(x, y, rad * k)
        };
        match icon {
            Icon::Play => fills.extend(poly(&[(7.0, 4.5), (19.5, 12.0), (7.0, 19.5)], true)),
            Icon::Pause => {
                fills.extend(poly(&[(6.5, 4.5), (10.0, 4.5), (10.0, 19.5), (6.5, 19.5)], true));
                fills.extend(poly(&[(14.0, 4.5), (17.5, 4.5), (17.5, 19.5), (14.0, 19.5)], true));
            }
            Icon::FrameBack => {
                fills.extend(poly(&[(17.0, 6.0), (8.0, 12.0), (17.0, 18.0)], true));
                strokes.extend(poly(&[(6.0, 6.0), (6.0, 18.0)], false));
            }
            Icon::FrameFwd => {
                fills.extend(poly(&[(7.0, 6.0), (16.0, 12.0), (7.0, 18.0)], true));
                strokes.extend(poly(&[(18.0, 6.0), (18.0, 18.0)], false));
            }
            Icon::ToStart => {
                fills.extend(poly(&[(19.0, 5.0), (9.0, 12.0), (19.0, 19.0)], true));
                strokes.extend(poly(&[(5.0, 5.0), (5.0, 19.0)], false));
            }
            Icon::ToEnd => {
                fills.extend(poly(&[(5.0, 5.0), (15.0, 12.0), (5.0, 19.0)], true));
                strokes.extend(poly(&[(19.0, 5.0), (19.0, 19.0)], false));
            }
            Icon::Scissors => {
                strokes.extend(circ(6.0, 6.0, 3.0));
                strokes.extend(circ(6.0, 18.0, 3.0));
                strokes.extend(poly(&[(20.0, 4.0), (8.12, 15.88)], false));
                strokes.extend(poly(&[(14.47, 14.48), (20.0, 20.0)], false));
                strokes.extend(poly(&[(8.12, 8.12), (12.0, 12.0)], false));
            }
            Icon::MarkIn => strokes.extend(poly(&[(14.0, 4.0), (8.0, 4.0), (8.0, 20.0), (14.0, 20.0)], false)),
            Icon::MarkOut => strokes.extend(poly(&[(10.0, 4.0), (16.0, 4.0), (16.0, 20.0), (10.0, 20.0)], false)),
            Icon::Folder => strokes.extend(poly(&[(3.0, 6.0), (3.0, 19.0), (21.0, 19.0), (21.0, 8.0), (12.0, 8.0), (10.0, 5.0), (4.0, 5.0)], true)),
            Icon::Export => {
                strokes.extend(poly(&[(21.0, 15.0), (21.0, 19.0), (19.0, 21.0), (5.0, 21.0), (3.0, 19.0), (3.0, 15.0)], false));
                strokes.extend(poly(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)], false));
                strokes.extend(poly(&[(12.0, 15.0), (12.0, 3.0)], false));
            }
            Icon::Volume => {
                fills.extend(poly(&[(4.0, 9.0), (8.0, 9.0), (13.0, 4.5), (13.0, 19.5), (8.0, 15.0), (4.0, 15.0)], true));
                strokes.extend(poly(&[(16.5, 9.0), (17.5, 12.0), (16.5, 15.0)], false));
                strokes.extend(poly(&[(19.5, 6.0), (21.0, 12.0), (19.5, 18.0)], false));
            }
            Icon::Muted => {
                fills.extend(poly(&[(4.0, 9.0), (8.0, 9.0), (13.0, 4.5), (13.0, 19.5), (8.0, 15.0), (4.0, 15.0)], true));
                strokes.extend(poly(&[(16.0, 9.0), (22.0, 15.0)], false));
                strokes.extend(poly(&[(22.0, 9.0), (16.0, 15.0)], false));
            }
            Icon::KeyFrame => {
                // A key frame: a diamond with a dot.
                strokes.extend(poly(&[(12.0, 3.5), (20.5, 12.0), (12.0, 20.5), (3.5, 12.0)], true));
                fills.extend(circ(12.0, 12.0, 2.2));
            }
            Icon::Trash => {
                strokes.extend(poly(&[(3.0, 6.0), (21.0, 6.0)], false));
                strokes.extend(poly(&[(19.0, 6.0), (18.0, 20.0), (6.0, 20.0), (5.0, 6.0)], false));
                strokes.extend(poly(&[(9.0, 6.0), (9.0, 3.5), (15.0, 3.5), (15.0, 6.0)], false));
            }
            Icon::Film => {
                strokes.extend(poly(&[(3.0, 4.0), (21.0, 4.0), (21.0, 20.0), (3.0, 20.0)], true));
                strokes.extend(poly(&[(7.0, 4.0), (7.0, 20.0)], false));
                strokes.extend(poly(&[(17.0, 4.0), (17.0, 20.0)], false));
                strokes.extend(poly(&[(3.0, 12.0), (21.0, 12.0)], false));
            }
            Icon::Columns => {
                strokes.extend(poly(&[(3.0, 4.0), (21.0, 4.0), (21.0, 20.0), (3.0, 20.0)], true));
                strokes.extend(poly(&[(9.0, 4.0), (9.0, 20.0)], false));
                strokes.extend(poly(&[(15.0, 4.0), (15.0, 20.0)], false));
            }
            Icon::File => {
                strokes.extend(poly(&[(14.0, 3.0), (5.0, 3.0), (5.0, 21.0), (19.0, 21.0), (19.0, 8.0)], true));
                strokes.extend(poly(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)], false));
            }
            Icon::Files => {
                strokes.extend(poly(&[(15.0, 7.0), (9.0, 7.0), (9.0, 21.0), (20.0, 21.0), (20.0, 12.0)], true));
                strokes.extend(poly(&[(15.0, 7.0), (15.0, 12.0), (20.0, 12.0)], false));
                strokes.extend(poly(&[(5.0, 17.0), (4.0, 17.0), (4.0, 3.0), (14.0, 3.0)], false));
            }
            Icon::Undo => {
                strokes.extend(poly(&[(9.0, 14.0), (4.0, 9.0), (9.0, 4.0)], false));
                strokes.extend(poly(&[(4.0, 9.0), (14.5, 9.0), (18.0, 10.5), (20.0, 14.0), (18.0, 17.5), (14.5, 20.0), (11.0, 20.0)], false));
            }
            Icon::Minus => strokes.extend(poly(&[(6.0, 12.0), (18.0, 12.0)], false)),
            Icon::Plus => {
                strokes.extend(poly(&[(6.0, 12.0), (18.0, 12.0)], false));
                strokes.extend(poly(&[(12.0, 6.0), (12.0, 18.0)], false));
            }
            Icon::Close => {
                strokes.extend(poly(&[(6.0, 6.0), (18.0, 18.0)], false));
                strokes.extend(poly(&[(18.0, 6.0), (6.0, 18.0)], false));
            }
        }
        let paint = c.paint();
        for f in fills {
            self.pm.fill_path(&f, &paint, FillRule::Winding, Transform::identity(), None);
            // Soften the triangle tips like a rounded join.
            self.pm.stroke_path(&f, &paint, &Stroke { width: 1.5 * k, line_join: LineJoin::Round, ..Stroke::default() }, Transform::identity(), None);
        }
        for s in strokes {
            self.pm.stroke_path(&s, &paint, &st, Transform::identity(), None);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Columns,
    File,
    Files,
    Undo,
    Play,
    Pause,
    FrameBack,
    FrameFwd,
    ToStart,
    ToEnd,
    Scissors,
    MarkIn,
    MarkOut,
    Folder,
    Export,
    Volume,
    Muted,
    KeyFrame,
    Trash,
    Film,
    Minus,
    Plus,
    Close,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_font_has_what_the_ui_writes() {
        let t = Text::new();
        let ui = "MP4 Splitter Open Export Split into pieces Snap to key frames 0123456789:.,/()+-[]% Drop an MP4 file here or press Ctrl+O Saved piece pieces ffmpeg was not found frame Selected";
        for c in ui.chars().filter(|c| !c.is_whitespace()) {
            assert_ne!(t.font.glyph_id(c).0, 0, "no glyph for {c:?}");
        }
    }

    #[test]
    fn text_draws_pixels() {
        let mut pm = Pixmap::new(200, 40).unwrap();
        let mut t = Text::new();
        let mut c = Canvas { pm: &mut pm, text: &mut t, s: 1.0 };
        let w = c.text(4.0, 28.0, 20.0, hex(0xFFFFFF, 255), "Split", Align::Left);
        assert!(w > 30.0);
        assert!(pm.data().chunks(4).any(|p| p[3] > 200));
    }
}
