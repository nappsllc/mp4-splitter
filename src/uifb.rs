//! From Rustshot (https://github.com/nappsllc/rustshot, src/uifb.rs), Apache-2.0.
//!
//! Software framebuffer: alpha blending, rectangles, image blits, and
//! baked UI-font text rendering. Draw into an unpremultiplied RGBA buffer via [`Fb`].

use crate::fonts::UiFont;
use crate::objects::{FRect, Pt};
use crate::raster::{Blend, Order, Surf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct C4 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl C4 {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        C4 { r, g, b, a }
    }

    pub const fn rgb(r: u8, g: u8, b: u8) -> Self {
        C4 { r, g, b, a: 255 }
    }

    pub fn with_alpha(self, a: u8) -> Self {
        C4 { a, ..self }
    }

    /// Same color with alpha scaled by `k` (clamped 0..=1): fades, disabled.
    pub fn fade(self, k: f32) -> Self {
        C4 { a: (self.a as f32 * k.clamp(0.0, 1.0)).round() as u8, ..self }
    }
}

/// `x / 255` for `x <= 255 * 255` without a divide (the size-optimised
/// release profile keeps real `div` instructions for `/ 255`).
#[inline(always)]
fn div255(x: u32) -> u32 {
    (x + 1 + (x >> 8)) >> 8
}

/// Borrowed drawing surface over a 4-byte-per-pixel buffer. The buffer
/// covers image coordinates `ox..ox + stride` x `oy..oy + height`; every
/// draw call takes image coordinates and clips to that window.
pub struct Fb<'a> {
    pub d: &'a mut [u8],
    pub stride: usize,
    pub ox: i32,
    pub oy: i32,
    pub order: Order,
    /// A right-to-left frame: the whole frame is flipped horizontally
    /// once drawn ([`Fb::flip`]), so text and pictures are drawn
    /// pre-flipped within their own boxes (they read normally after the
    /// flip, while their places mirror).
    pub mirror: bool,
}

impl<'a> Fb<'a> {
    /// A whole-image RGBA surface (origin 0,0).
    pub fn new(d: &'a mut [u8], stride: usize) -> Self {
        Self::with_origin(d, stride, 0, 0, Order::Rgba)
    }

    /// A surface whose top-left pixel is image pixel (`ox`, `oy`).
    pub fn with_origin(d: &'a mut [u8], stride: usize, ox: i32, oy: i32, order: Order) -> Self {
        Fb { d, stride, ox, oy, order, mirror: false }
    }

    /// Flip the pixels of `x0..x1` (image coordinates) horizontally in
    /// every row of `y0..y1`, clipped to the window.
    pub fn flip_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        let (bx0, by0, bx1, by1) = self.bounds();
        let (x0, x1, y0, y1) = (x0.max(bx0), x1.min(bx1), y0.max(by0), y1.min(by1));
        if x1 - x0 < 2 || y1 <= y0 {
            return;
        }
        for y in y0..y1 {
            let i = ((y - self.oy) as usize * self.stride + (x0 - self.ox) as usize) * 4;
            if let Some(row) = self.d.get_mut(i..i + (x1 - x0) as usize * 4) {
                row.as_chunks_mut::<4>().0.reverse();
            }
        }
    }

    /// Flip the whole buffer horizontally (a right-to-left frame, drawn
    /// left to right, made to read right to left).
    pub fn flip(&mut self) {
        let (bx0, by0, bx1, by1) = self.bounds();
        self.flip_rect(bx0, by0, bx1, by1);
    }

    /// Buffer height in pixels.
    pub fn height(&self) -> i32 {
        self.d.len() as i32 / 4 / self.stride as i32
    }

    /// The window in image coordinates: (x0, y0, x1, y1), end-exclusive.
    pub fn bounds(&self) -> (i32, i32, i32, i32) {
        (self.ox, self.oy, self.ox + self.stride as i32, self.oy + self.height())
    }

    /// The same pixels as an AA raster surface (strokes, polygons).
    pub fn surf(&mut self) -> Surf<'_> {
        let h = self.height().max(0) as u32;
        Surf::with_origin(&mut *self.d, self.stride as u32, h, self.ox, self.oy, self.order)
    }

    /// Blend `c` over the pixel at image coordinates (x, y); outside the
    /// window is a no-op.
    pub fn blend_px(&mut self, x: i32, y: i32, c: C4) {
        let (x, y) = (x - self.ox, y - self.oy);
        if x < 0 || y < 0 || x >= self.stride as i32 {
            return;
        }
        let i = (y as usize * self.stride + x as usize) * 4;
        let Some(px) = self.d.get_mut(i..i + 4) else {
            return;
        };
        let bgra = self.order == Order::Bgra;
        let a = c.a as u32;
        if a == 255 {
            px.copy_from_slice(&if bgra { [c.b, c.g, c.r, 255] } else { [c.r, c.g, c.b, 255] });
            return;
        }
        if a == 0 {
            return;
        }
        if bgra {
            px.swap(0, 2); // blend in RGBA, swap back
        }
        let ia = 255 - a;
        px[0] = div255(c.r as u32 * a + px[0] as u32 * ia) as u8;
        px[1] = div255(c.g as u32 * a + px[1] as u32 * ia) as u8;
        px[2] = div255(c.b as u32 * a + px[2] as u32 * ia) as u8;
        px[3] = (a + div255(px[3] as u32 * ia)).min(255) as u8;
        if bgra {
            px.swap(0, 2);
        }
    }

    /// [`Fb::blend_px`] of `c` over pixels `x0..x1` of row `y`, which
    /// must lie inside the window: the same arithmetic, without the
    /// per-pixel clipping and channel swaps.
    fn blend_span(&mut self, y: i32, x0: i32, x1: i32, c: C4) {
        if x1 <= x0 || c.a == 0 {
            return;
        }
        let i = ((y - self.oy) as usize * self.stride + (x0 - self.ox) as usize) * 4;
        let Some(row) = self.d.get_mut(i..i + (x1 - x0) as usize * 4) else {
            return;
        };
        // Byte offsets of red and blue in this buffer's order.
        let (ri, bi) = if self.order == Order::Bgra { (2, 0) } else { (0, 2) };
        let a = c.a as u32;
        if a == 255 {
            let mut p = [0u8; 4];
            (p[ri], p[1], p[bi], p[3]) = (c.r, c.g, c.b, 255);
            for px in row.as_chunks_mut::<4>().0 {
                *px = p;
            }
            return;
        }
        let ia = 255 - a;
        let (cr, cg, cb) = (c.r as u32 * a, c.g as u32 * a, c.b as u32 * a);
        for px in row.as_chunks_mut::<4>().0 {
            px[ri] = div255(cr + px[ri] as u32 * ia) as u8;
            px[1] = div255(cg + px[1] as u32 * ia) as u8;
            px[bi] = div255(cb + px[bi] as u32 * ia) as u8;
            px[3] = (a + div255(px[3] as u32 * ia)).min(255) as u8;
        }
    }

    pub fn fill_rect(&mut self, x0: i32, y0: i32, w: i32, h: i32, c: C4) {
        if w <= 0 || h <= 0 {
            return;
        }
        let (bx0, by0, bx1, by1) = self.bounds();
        let (xs, xe) = (x0.max(bx0), (x0 + w).min(bx1));
        for y in y0.max(by0)..(y0 + h).min(by1) {
            self.blend_span(y, xs, xe, c);
        }
    }

    /// Rectangle outline with sub-pixel coverage around the border.
    pub fn stroke_rect(&mut self, x0: f32, y0: f32, w: f32, h: f32, thickness: f32, c: C4) {
        if w <= 0.0 || h <= 0.0 || thickness <= 0.0 {
            return;
        }
        let (x1, y1) = (x0 + w, y0 + h);
        let half = thickness / 2.0;
        let (bx0, by0, bx1, by1) = self.bounds();
        let ys = ((y0 - half - 1.0).floor() as i32).max(by0);
        let ye = ((y1 + half + 1.0).ceil() as i32).min(by1);
        let xs = ((x0 - half - 1.0).floor() as i32).max(bx0);
        let xe = ((x1 + half + 1.0).ceil() as i32).min(bx1);
        // Coverage is zero farther than `half + 1` from the outline, so only
        // the four border strips are visited, never the hollow interior:
        // rows near the top/bottom edge span the full width; other rows
        // visit just the columns near the left/right edges.
        let band = |e: f32| ((e - half - 1.0).floor() as i32, (e + half + 1.0).ceil() as i32);
        let (top, bottom, left, right) = (band(y0), band(y1), band(x0), band(x1));
        let in_band = |v: i32, b: (i32, i32)| v >= b.0 && v < b.1;
        let qx_at = |x: i32| (x as f32 + 0.5 - x0 - w / 2.0).abs() - w / 2.0;
        // The color at (x, y), from the signed distance to the rectangle
        // (negative inside); alpha 0 = untouched.
        let at = |x: i32, y: i32| {
            let cy = y as f32 + 0.5;
            let qx = qx_at(x);
            let qy = (cy - y0 - h / 2.0).abs() - h / 2.0;
            let dx = qx.max(0.0);
            let dy = qy.max(0.0);
            let sdf = (dx * dx + dy * dy).sqrt() + qx.max(qy).min(0.0);
            let cov = (half + 0.5 - sdf.abs()).clamp(0.0, 1.0);
            let mut col = c;
            col.a = if cov > 0.0 { ((c.a as f32 * cov) as u32).min(255) as u8 } else { 0 };
            col
        };
        for y in ys..ye {
            let cols = if in_band(y, top) || in_band(y, bottom) || left.1 >= right.0 {
                // Along a horizontal edge, where `qx` is at most `qy` (at
                // most 0 outside the rectangle), `dx` is 0 and the
                // distance is `qy`'s alone: one color for the run (an
                // interval, `qx` being V-shaped in x), blended as a span.
                let qy = (y as f32 + 0.5 - y0 - h / 2.0).abs() - h / 2.0;
                let flat = |x: i32| qx_at(x) <= qy.min(0.0);
                let mut ms = xs;
                while ms < xe && !flat(ms) {
                    ms += 1;
                }
                let mut me = xe;
                while me > ms && !flat(me - 1) {
                    me -= 1;
                }
                if ms < me {
                    let col = at(ms, y);
                    self.blend_span(y, ms, me, col);
                }
                [(xs, ms), (me, xe)]
            } else {
                [(xs, left.1.min(xe)), (right.0.max(xs), xe)]
            };
            for x in cols.into_iter().flat_map(|(a, b)| a..b) {
                let col = at(x, y);
                if col.a > 0 {
                    self.blend_px(x, y, col);
                }
            }
        }
    }

    /// Filled rounded rectangle (corner radius in px).
    pub fn fill_rounded(&mut self, x0: i32, y0: i32, w: i32, h: i32, r: f32, c: C4) {
        if w <= 0 || h <= 0 {
            return;
        }
        let r = r.min(w as f32 / 2.0).min(h as f32 / 2.0);
        let (bx0, by0, bx1, by1) = self.bounds();
        let (xs, xe) = (x0.max(bx0), (x0 + w).min(bx1));
        for y in y0.max(by0)..(y0 + h).min(by1) {
            // Rows clear of the corners: one alpha across (the per-pixel
            // path below gives every pixel of such a row `c.a`).
            let cy = y as f32 + 0.5;
            if r <= 0.0 || (cy >= y0 as f32 + r && cy <= (y0 + h) as f32 - r) {
                self.blend_span(y, xs, xe, c);
                continue;
            }
            // A corner row: pixels between the corners (`!outx` below,
            // monotonic in x) are one span as well.
            let (inx0, inx1) = (x0 as f32 + r, (x0 + w) as f32 - r);
            let mut ms = xs;
            while ms < xe && (ms as f32 + 0.5) < inx0 {
                ms += 1;
            }
            let mut me = ms;
            while me < xe && (me as f32 + 0.5) <= inx1 {
                me += 1;
            }
            self.blend_span(y, ms, me, c);
            for x in (xs..ms).chain(me..xe) {
                let mut a = c.a;
                if r > 0.0 {
                    let cx = x as f32 + 0.5;
                    let cy = y as f32 + 0.5;
                    let inx0 = x0 as f32 + r;
                    let inx1 = (x0 + w) as f32 - r;
                    let iny0 = y0 as f32 + r;
                    let iny1 = (y0 + h) as f32 - r;
                    let outx = cx < inx0 || cx > inx1;
                    let outy = cy < iny0 || cy > iny1;
                    if outx && outy {
                        let qx = if cx < inx0 { inx0 - cx } else { cx - inx1 };
                        let qy = if cy < iny0 { iny0 - cy } else { cy - iny1 };
                        let dist = (qx * qx + qy * qy).sqrt();
                        let cov = (r + 0.5 - dist).clamp(0.0, 1.0);
                        if cov <= 0.0 {
                            continue;
                        }
                        a = (a as f32 * cov) as u8;
                    }
                }
                let mut col = c;
                col.a = a;
                self.blend_px(x, y, col);
            }
        }
    }

    /// Draw `s` with the top-left corner at (x, y). Returns the advance width.
    pub fn draw_text(&mut self, font: &UiFont, px: f32, s: &str, x: f32, y: f32, c: C4) -> f32 {
        let (bx0, by0, bx1, by1) = self.bounds();
        // Mirrored frame: each glyph pixel goes to its mirror image within
        // the text's own box (the frame's flip puts it back).
        let flip = if self.mirror { (x.floor() as i32) + ((x + font.width(s, px)).ceil() as i32) - 1 } else { 0 };
        font.draw(s, px, x, y, |sx, sy, cov| {
            let sx = if self.mirror { flip - sx } else { sx };
            if cov <= 0.0 || sx < bx0 || sy < by0 || sx >= bx1 || sy >= by1 {
                return;
            }
            let mut col = c;
            col.a = ((c.a as f32 * cov) as u32).min(255) as u8;
            self.blend_px(sx, sy, col);
        })
    }

    /// Anti-aliased filled circle.
    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: C4) {
        let (bx0, by0, bx1, by1) = self.bounds();
        let x0 = ((cx - r - 1.0).floor() as i32).max(bx0);
        let x1 = ((cx + r + 1.0).ceil() as i32).min(bx1);
        let y0 = ((cy - r - 1.0).floor() as i32).max(by0);
        let y1 = ((cy + r + 1.0).ceil() as i32).min(by1);
        for y in y0..y1 {
            for x in x0..x1 {
                let d = (x as f32 + 0.5 - cx).hypot(y as f32 + 0.5 - cy);
                let cov = (r + 0.5 - d).clamp(0.0, 1.0);
                if cov > 0.0 {
                    self.blend_px(x, y, c.fade(cov));
                }
            }
        }
    }

    /// Circle outline `width` px wide, centred on radius `r`.
    pub fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: C4) {
        let rect = FRect { x: cx - r, y: cy - r, w: 2.0 * r, h: 2.0 * r };
        self.surf().stroke_ellipse(rect, width, c, Blend::Normal);
    }

    /// Rounded-rect outline `width` px wide, centred on the edge of `r`.
    pub fn stroke_rounded(&mut self, r: FRect, radius: f32, width: f32, c: C4) {
        self.surf().stroke_round_rect(r, radius, width, c, Blend::Normal);
    }

    /// Dashed rectangle outline: `dash` on, `gap` off, clockwise from top-left.
    pub fn stroke_dashed_rect(&mut self, r: FRect, dash: f32, gap: f32, width: f32, c: C4) {
        if dash <= 0.0 {
            return;
        }
        let corners = [Pt::new(r.x, r.y), Pt::new(r.x1(), r.y), Pt::new(r.x1(), r.y1()), Pt::new(r.x, r.y1()), Pt::new(r.x, r.y)];
        let period = dash + gap.max(0.0);
        let lerp = |a: Pt, b: Pt, t: f32| Pt::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t);
        let mut s = self.surf();
        let mut phase = 0.0f32;
        for w in corners.windows(2) {
            let (a, b) = (w[0], w[1]);
            let len = (b.x - a.x).hypot(b.y - a.y);
            let mut t = 0.0;
            while t < len {
                let into = phase % period;
                let on = into < dash;
                let seg = if on { dash - into } else { period - into }.min(len - t);
                let seg = seg.max(1e-3);
                if on {
                    s.stroke_polyline(&[lerp(a, b, t / len), lerp(a, b, (t + seg) / len)], width, c, Blend::Normal);
                }
                t += seg;
                phase += seg;
            }
        }
    }
}

pub fn text_width(font: &UiFont, px: f32, s: &str) -> f32 {
    font.width(s, px)
}

pub fn text_height(font: &UiFont, px: f32) -> f32 {
    font.height(px)
}
