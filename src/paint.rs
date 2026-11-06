//! Drawing, in physical pixels, on Rustshot's hand-rolled renderer
//! ([`crate::uifb`], [`crate::raster`]): shapes, pictures, the icons and text
//! in the Inter font baked at build time ([`crate::fonts`]). The frame is BGRA,
//! as the OS takes it.

use crate::fonts::{UiFont, ui_font};
use crate::objects::{FRect, Pt};
use crate::raster::Blend;
use crate::uifb::{C4, Fb};
use std::borrow::Cow;
use std::sync::Arc;

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
    fn c4(self) -> C4 {
        C4::new(self.r, self.g, self.b, self.a)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Align {
    Left,
    Center,
    Right,
}

/// A picture: opaque BGRA, shared cheaply.
#[derive(Clone)]
pub struct Picture {
    pub w: u32,
    pub h: u32,
    pub bgra: Arc<Vec<u8>>,
}

/// Measures text.
pub struct Text;

impl Text {
    pub fn new() -> Text {
        Text
    }

    /// The width of `s` at `px`.
    pub fn measure(&self, s: &str, px: f32) -> f32 {
        ui_font().width(&shown(ui_font(), s), px)
    }
}

/// `s` as it can be drawn: what the font lacks becomes a look-alike or `?`.
fn shown<'a>(f: &UiFont, s: &'a str) -> Cow<'a, str> {
    if s.chars().all(|c| f.has_glyph(c)) {
        return Cow::Borrowed(s);
    }
    let alt = |c| match c {
        '–' | '—' | '−' => '-',
        '×' => 'x',
        '…' | '·' => '.',
        _ => '?',
    };
    Cow::Owned(s.chars().map(|c| if f.has_glyph(c) { c } else { alt(c) }).collect())
}

fn frect(r: Rect) -> FRect {
    FRect { x: r.x, y: r.y, w: r.w, h: r.h }
}

pub struct Canvas<'a> {
    pub fb: Fb<'a>,
    pub text: &'a mut Text,
    /// Physical pixels a logical one.
    pub s: f32,
    /// What rounded pictures sit on.
    pub stage: C,
}

impl Canvas<'_> {
    pub fn clear(&mut self, c: C) {
        let (x0, y0, x1, y1) = self.fb.bounds();
        self.fb.fill_rect(x0, y0, x1 - x0, y1 - y0, c.with_alpha(255).c4());
    }

    /// A rectangle snapped to whole pixels (crisp).
    pub fn rect(&mut self, r: Rect, c: C) {
        let (x0, y0) = (r.x.round(), r.y.round());
        let (x1, y1) = (r.right().round().max(x0 + 1.0), r.bottom().round().max(y0 + 1.0));
        self.fb.fill_rect(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32, c.c4());
    }

    pub fn rrect(&mut self, r: Rect, rad: f32, c: C) {
        let (x0, y0) = (r.x.round(), r.y.round());
        let (x1, y1) = (r.right().round(), r.bottom().round());
        self.fb.fill_rounded(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32, rad, c.c4());
    }

    pub fn rrect_stroke(&mut self, r: Rect, rad: f32, width: f32, c: C) {
        self.fb.stroke_rounded(frect(r.inset(width / 2.0, width / 2.0)), rad, width, c.c4());
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
        self.fb.fill_circle(x, y, rad, c.c4());
    }

    pub fn line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: C) {
        self.fb.surf().stroke_polyline(&[Pt::new(x0, y0), Pt::new(x1, y1)], width, c.c4(), Blend::Normal);
    }

    /// A convex polygon.
    pub fn poly(&mut self, pts: &[(f32, f32)], c: C) {
        let pts: Vec<Pt> = pts.iter().map(|&(x, y)| Pt::new(x, y)).collect();
        self.fb.surf().fill_convex(&pts, c.c4(), Blend::Normal);
    }

    /// `img` scaled into `dst`, shown only inside `clip` (rounded by its
    /// radius; `dst` itself when `None`).
    pub fn image(&mut self, img: &Picture, dst: Rect, clip: Option<(Rect, f32)>) {
        let (r, rad) = clip.unwrap_or((dst, 0.0));
        let (bx0, by0, bx1, by1) = self.fb.bounds();
        let (cx0, cy0) = ((r.x.round() as i32).max(bx0), (r.y.round() as i32).max(by0));
        let (cx1, cy1) = ((r.right().round() as i32).min(bx1), (r.bottom().round() as i32).min(by1));
        let (sw, sh) = (img.w as usize, img.h as usize);
        if cx1 <= cx0 || cy1 <= cy0 || sw == 0 || sh == 0 || dst.w <= 0.0 || dst.h <= 0.0 || img.bgra.len() < sw * sh * 4 {
            return;
        }
        // Source position per target pixel, 16.16.
        let (kx, ky) = ((sw as f32 / dst.w * 65536.0) as i64, (sh as f32 / dst.h * 65536.0) as i64);
        let same = sw as i64 == dst.w.round() as i64 && sh as i64 == dst.h.round() as i64;
        let (ox, oy) = (dst.x.round() as i32, dst.y.round() as i32);
        let (fx0, fy0) = (self.fb.ox, self.fb.oy);
        let stride = self.fb.stride;
        let src = img.bgra.as_slice();
        let row_w = (cx1 - cx0) as usize;
        for y in cy0..cy1 {
            let at = ((y - fy0) as usize * stride + (cx0 - fx0) as usize) * 4;
            let Some(out) = self.fb.d.get_mut(at..at + row_w * 4) else { continue };
            if same {
                let sy = (y - oy).clamp(0, sh as i32 - 1) as usize;
                let sx = (cx0 - ox).clamp(0, sw as i32) as usize;
                let n = row_w.min(sw - sx);
                let from = (sy * sw + sx) * 4;
                out[..n * 4].copy_from_slice(&src[from..from + n * 4]);
                continue;
            }
            // Bilinear, around each pixel's centre.
            let fy = (((y - oy) as i64 * 2 + 1) * ky / 2 - 32768).clamp(0, ((sh as i64) - 1) << 16);
            let (y0, wy) = ((fy >> 16) as usize, (fy & 0xFFFF) as u32 >> 8);
            let y1 = (y0 + 1).min(sh - 1);
            for (i, o) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                let fx = (((cx0 - ox + i as i32) as i64 * 2 + 1) * kx / 2 - 32768).clamp(0, ((sw as i64) - 1) << 16);
                let (x0, wx) = ((fx >> 16) as usize, (fx & 0xFFFF) as u32 >> 8);
                let x1 = (x0 + 1).min(sw - 1);
                let px = |x: usize, y: usize, c: usize| src[(y * sw + x) * 4 + c] as u32;
                for (c, v) in o.iter_mut().enumerate().take(3) {
                    let top = px(x0, y0, c) * (256 - wx) + px(x1, y0, c) * wx;
                    let bottom = px(x0, y1, c) * (256 - wx) + px(x1, y1, c) * wx;
                    *v = ((top * (256 - wy) + bottom * wy) >> 16) as u8;
                }
                o[3] = 255;
            }
        }
        if rad > 0.0 {
            self.round_corners(r, rad);
        }
    }

    /// Put back, in `r`'s corners, what a rounded picture does not cover:
    /// the stage behind it (the corner pixels are only ever drawn over it).
    fn round_corners(&mut self, r: Rect, rad: f32) {
        let stage = self.stage;
        let (x0, y0) = (r.x.round() as i32, r.y.round() as i32);
        let (x1, y1) = (r.right().round() as i32, r.bottom().round() as i32);
        let n = rad.ceil() as i32;
        for j in 0..n {
            for i in 0..n {
                let (dx, dy) = (rad - (i as f32 + 0.5), rad - (j as f32 + 0.5));
                let cov = ((dx * dx + dy * dy).sqrt() - rad + 0.5).clamp(0.0, 1.0);
                if cov <= 0.0 {
                    continue;
                }
                let col = stage.c4().fade(cov);
                for (x, y) in [(x0 + i, y0 + j), (x1 - 1 - i, y0 + j), (x0 + i, y1 - 1 - j), (x1 - 1 - i, y1 - 1 - j)] {
                    self.fb.blend_px(x, y, col);
                }
            }
        }
    }

    /// Text with its baseline at `y`; its width.
    pub fn text(&mut self, x: f32, y: f32, px: f32, c: C, s: &str, align: Align) -> f32 {
        let f = ui_font();
        let s = shown(f, s);
        let w = f.width(&s, px);
        let left = match align {
            Align::Left => x,
            Align::Center => x - w / 2.0,
            Align::Right => x - w,
        };
        self.fb.draw_text(f, px, &s, left.round(), y.round() - f.ascent(px), c.c4());
        w
    }

    /// An icon in the `r` box (24-unit Lucide-style strokes).
    pub fn icon(&mut self, icon: Icon, r: Rect, c: C) {
        let k = r.w.min(r.h) / 24.0;
        let (ox, oy) = (r.cx() - 12.0 * k, r.cy() - 12.0 * k);
        let p = |x: f32, y: f32| Pt::new(ox + x * k, oy + y * k);
        let col = c.c4();
        let width = 2.0 * k;
        let mut strokes: Vec<Vec<Pt>> = Vec::new();
        let mut fills: Vec<Vec<Pt>> = Vec::new();
        let mut rings: Vec<(f32, f32, f32)> = Vec::new();
        let mut dots: Vec<(f32, f32, f32)> = Vec::new();
        let poly = |pts: &[(f32, f32)], close: bool| {
            let mut v: Vec<Pt> = pts.iter().map(|&(x, y)| p(x, y)).collect();
            if close {
                v.push(v[0]);
            }
            v
        };
        match icon {
            Icon::Play => fills.push(poly(&[(7.0, 4.5), (19.5, 12.0), (7.0, 19.5)], false)),
            Icon::Pause => {
                fills.push(poly(&[(6.5, 4.5), (10.0, 4.5), (10.0, 19.5), (6.5, 19.5)], false));
                fills.push(poly(&[(14.0, 4.5), (17.5, 4.5), (17.5, 19.5), (14.0, 19.5)], false));
            }
            Icon::FrameBack => {
                fills.push(poly(&[(17.0, 6.0), (8.0, 12.0), (17.0, 18.0)], false));
                strokes.push(poly(&[(6.0, 6.0), (6.0, 18.0)], false));
            }
            Icon::FrameFwd => {
                fills.push(poly(&[(7.0, 6.0), (16.0, 12.0), (7.0, 18.0)], false));
                strokes.push(poly(&[(18.0, 6.0), (18.0, 18.0)], false));
            }
            Icon::ToStart => {
                fills.push(poly(&[(19.0, 5.0), (9.0, 12.0), (19.0, 19.0)], false));
                strokes.push(poly(&[(5.0, 5.0), (5.0, 19.0)], false));
            }
            Icon::ToEnd => {
                fills.push(poly(&[(5.0, 5.0), (15.0, 12.0), (5.0, 19.0)], false));
                strokes.push(poly(&[(19.0, 5.0), (19.0, 19.0)], false));
            }
            Icon::Scissors => {
                rings.push((6.0, 6.0, 3.0));
                rings.push((6.0, 18.0, 3.0));
                strokes.push(poly(&[(20.0, 4.0), (8.12, 15.88)], false));
                strokes.push(poly(&[(14.47, 14.48), (20.0, 20.0)], false));
                strokes.push(poly(&[(8.12, 8.12), (12.0, 12.0)], false));
            }
            Icon::MarkIn => strokes.push(poly(&[(14.0, 4.0), (8.0, 4.0), (8.0, 20.0), (14.0, 20.0)], false)),
            Icon::MarkOut => strokes.push(poly(&[(10.0, 4.0), (16.0, 4.0), (16.0, 20.0), (10.0, 20.0)], false)),
            Icon::Folder => strokes.push(poly(&[(3.0, 6.0), (3.0, 19.0), (21.0, 19.0), (21.0, 8.0), (12.0, 8.0), (10.0, 5.0), (4.0, 5.0)], true)),
            Icon::Export => {
                strokes.push(poly(&[(21.0, 15.0), (21.0, 19.0), (19.0, 21.0), (5.0, 21.0), (3.0, 19.0), (3.0, 15.0)], false));
                strokes.push(poly(&[(7.0, 10.0), (12.0, 15.0), (17.0, 10.0)], false));
                strokes.push(poly(&[(12.0, 15.0), (12.0, 3.0)], false));
            }
            Icon::Volume => {
                fills.push(poly(&[(4.0, 9.0), (8.0, 9.0), (8.0, 15.0), (4.0, 15.0)], false));
                fills.push(poly(&[(8.0, 9.0), (13.0, 4.5), (13.0, 19.5), (8.0, 15.0)], false));
                strokes.push(poly(&[(16.5, 9.0), (17.5, 12.0), (16.5, 15.0)], false));
                strokes.push(poly(&[(19.5, 6.0), (21.0, 12.0), (19.5, 18.0)], false));
            }
            Icon::Muted => {
                fills.push(poly(&[(4.0, 9.0), (8.0, 9.0), (8.0, 15.0), (4.0, 15.0)], false));
                fills.push(poly(&[(8.0, 9.0), (13.0, 4.5), (13.0, 19.5), (8.0, 15.0)], false));
                strokes.push(poly(&[(16.0, 9.0), (22.0, 15.0)], false));
                strokes.push(poly(&[(22.0, 9.0), (16.0, 15.0)], false));
            }
            Icon::KeyFrame => {
                // A key frame: a diamond with a dot.
                strokes.push(poly(&[(12.0, 3.5), (20.5, 12.0), (12.0, 20.5), (3.5, 12.0)], true));
                dots.push((12.0, 12.0, 2.2));
            }
            Icon::Trash => {
                strokes.push(poly(&[(3.0, 6.0), (21.0, 6.0)], false));
                strokes.push(poly(&[(19.0, 6.0), (18.0, 20.0), (6.0, 20.0), (5.0, 6.0)], false));
                strokes.push(poly(&[(9.0, 6.0), (9.0, 3.5), (15.0, 3.5), (15.0, 6.0)], false));
            }
            Icon::Film => {
                strokes.push(poly(&[(3.0, 4.0), (21.0, 4.0), (21.0, 20.0), (3.0, 20.0)], true));
                strokes.push(poly(&[(7.0, 4.0), (7.0, 20.0)], false));
                strokes.push(poly(&[(17.0, 4.0), (17.0, 20.0)], false));
                strokes.push(poly(&[(3.0, 12.0), (21.0, 12.0)], false));
            }
            Icon::Columns => {
                strokes.push(poly(&[(3.0, 4.0), (21.0, 4.0), (21.0, 20.0), (3.0, 20.0)], true));
                strokes.push(poly(&[(9.0, 4.0), (9.0, 20.0)], false));
                strokes.push(poly(&[(15.0, 4.0), (15.0, 20.0)], false));
            }
            Icon::File => {
                strokes.push(poly(&[(14.0, 3.0), (5.0, 3.0), (5.0, 21.0), (19.0, 21.0), (19.0, 8.0)], true));
                strokes.push(poly(&[(14.0, 3.0), (14.0, 8.0), (19.0, 8.0)], false));
            }
            Icon::Files => {
                strokes.push(poly(&[(15.0, 7.0), (9.0, 7.0), (9.0, 21.0), (20.0, 21.0), (20.0, 12.0)], true));
                strokes.push(poly(&[(15.0, 7.0), (15.0, 12.0), (20.0, 12.0)], false));
                strokes.push(poly(&[(5.0, 17.0), (4.0, 17.0), (4.0, 3.0), (14.0, 3.0)], false));
            }
            Icon::Undo => {
                strokes.push(poly(&[(9.0, 14.0), (4.0, 9.0), (9.0, 4.0)], false));
                strokes.push(poly(&[(4.0, 9.0), (14.5, 9.0), (18.0, 10.5), (20.0, 14.0), (18.0, 17.5), (14.5, 20.0), (11.0, 20.0)], false));
            }
            Icon::Minus => strokes.push(poly(&[(6.0, 12.0), (18.0, 12.0)], false)),
            Icon::Plus => {
                strokes.push(poly(&[(6.0, 12.0), (18.0, 12.0)], false));
                strokes.push(poly(&[(12.0, 6.0), (12.0, 18.0)], false));
            }
            Icon::Close => {
                strokes.push(poly(&[(6.0, 6.0), (18.0, 18.0)], false));
                strokes.push(poly(&[(18.0, 6.0), (6.0, 18.0)], false));
            }
        }
        let mut surf = self.fb.surf();
        for f in &fills {
            surf.fill_convex(f, col, Blend::Normal);
            // Soften the tips like a rounded join.
            let mut ring = f.clone();
            ring.push(f[0]);
            surf.stroke_polyline(&ring, 1.5 * k, col, Blend::Normal);
        }
        for st in &strokes {
            surf.stroke_polyline(st, width, col, Blend::Normal);
        }
        for &(x, y, rad) in &rings {
            let c = p(x, y);
            surf.stroke_ellipse(FRect { x: c.x - rad * k, y: c.y - rad * k, w: 2.0 * rad * k, h: 2.0 * rad * k }, width, col, Blend::Normal);
        }
        for &(x, y, rad) in &dots {
            let c = p(x, y);
            self.fb.fill_circle(c.x, c.y, rad * k, col);
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

/// The frame as a PNG file (stored, not compressed: screenshots for the
/// smoke test, not for keeping).
pub fn png(w: u32, h: u32, bgra: &[u8]) -> Vec<u8> {
    fn crc(parts: &[&[u8]]) -> u32 {
        let mut c = !0u32;
        for b in parts.iter().flat_map(|p| p.iter()) {
            c ^= *b as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
        }
        !c
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], body: &[u8]) {
        out.extend((body.len() as u32).to_be_bytes());
        out.extend(kind);
        out.extend(body);
        out.extend(crc(&[kind, body]).to_be_bytes());
    }
    let mut raw = Vec::with_capacity((w as usize * 4 + 1) * h as usize);
    for row in bgra.chunks_exact(w as usize * 4) {
        raw.push(0);
        for p in row.chunks_exact(4) {
            raw.extend([p[2], p[1], p[0], 255]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let mut parts = raw.chunks(0xFFFF).peekable();
    while let Some(part) = parts.next() {
        z.push(u8::from(parts.peek().is_none()));
        z.extend((part.len() as u16).to_le_bytes());
        z.extend((!(part.len() as u16)).to_le_bytes());
        z.extend(part);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for v in &raw {
        a = (a + *v as u32) % 65521;
        b = (b + a) % 65521;
    }
    z.extend(((b << 16) | a).to_be_bytes());
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend(w.to_be_bytes());
    ihdr.extend(h.to_be_bytes());
    ihdr.extend([8, 6, 0, 0, 0]);
    chunk(&mut out, b"IHDR", &ihdr);
    chunk(&mut out, b"IDAT", &z);
    chunk(&mut out, b"IEND", &[]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas<'a>(buf: &'a mut Vec<u8>, text: &'a mut Text, w: usize) -> Canvas<'a> {
        Canvas { fb: Fb::with_origin(buf, w, 0, 0, crate::raster::Order::Bgra), text, s: 1.0, stage: hex(0, 255) }
    }

    #[test]
    fn the_font_has_what_the_ui_writes() {
        for c in "ABCabc019 ·…×—–←→↑↓/[]+-:()".chars() {
            assert!(ui_font().has_glyph(c), "{c:?}");
        }
    }

    #[test]
    fn text_draws_pixels() {
        let (mut buf, mut text) = (vec![0u8; 200 * 40 * 4], Text::new());
        let w = canvas(&mut buf, &mut text, 200).text(4.0, 24.0, 16.0, hex(0xFFFFFF, 255), "Hello", Align::Left);
        assert!(w > 20.0);
        assert!(buf.chunks_exact(4).filter(|p| p[0] > 128).count() > 30);
    }

    #[test]
    fn what_the_font_lacks_is_replaced() {
        assert_eq!(shown(ui_font(), "abc"), "abc");
        assert_eq!(shown(ui_font(), "a保c"), "a?c");
    }

    #[test]
    fn colours_land_in_bgra_order() {
        let (mut buf, mut text) = (vec![0u8; 4 * 4 * 4], Text::new());
        canvas(&mut buf, &mut text, 4).rect(Rect::new(0.0, 0.0, 4.0, 4.0), hex(0x102030, 255));
        assert_eq!(&buf[..4], &[0x30, 0x20, 0x10, 255]);
    }

    #[test]
    fn a_picture_at_its_size_is_copied_exactly() {
        let bgra: Vec<u8> = (0..4u8).flat_map(|i| [i, i + 10, i + 20, 255]).collect();
        let pic = Picture { w: 2, h: 2, bgra: Arc::new(bgra.clone()) };
        let (mut buf, mut text) = (vec![0u8; 2 * 2 * 4], Text::new());
        canvas(&mut buf, &mut text, 2).image(&pic, Rect::new(0.0, 0.0, 2.0, 2.0), None);
        assert_eq!(buf, bgra);
    }

    #[test]
    fn a_scaled_picture_keeps_its_colour_and_stays_inside() {
        let pic = Picture { w: 2, h: 2, bgra: Arc::new([9u8, 8, 7, 255].repeat(4)) };
        let (mut buf, mut text) = (vec![0u8; 8 * 8 * 4], Text::new());
        canvas(&mut buf, &mut text, 8).image(&pic, Rect::new(2.0, 2.0, 4.0, 4.0), None);
        assert_eq!(&buf[(3 * 8 + 3) * 4..(3 * 8 + 3) * 4 + 4], &[9, 8, 7, 255]);
        assert_eq!(&buf[..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn the_png_is_what_a_decoder_expects() {
        let p = png(1, 1, &[1, 2, 3, 255]);
        assert_eq!(&p[..8], b"\x89PNG\r\n\x1a\n");
        // The filter byte and the pixel, RGBA, in the stored block.
        assert!(p.windows(5).any(|w| w == [0, 3, 2, 1, 255]));
    }
}
