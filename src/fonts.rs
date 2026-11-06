//! From Rustshot (https://github.com/nappsllc/rustshot, src/fonts.rs), Apache-2.0.
//!
//! UI font: Inter Medium (Latin + UI symbols, see tools/subset-font.sh),
//! baked by build.rs into static tables (metrics, cmap, advances, kerning,
//! outlines) and rasterised with [`Coverage`]: no font parser at run time.
//! Layout and coverage match what ab_glyph produced from the TTF (no
//! hinting; `px` is the ascent-to-descent height, as `ab_glyph::PxScale`).

use crate::objects::Pt;
use crate::raster::Coverage;

/// One baked glyph: advance and bounding box in font units (bbox is
/// `[x_min, y_max, x_max, y_min]`, all zero for an empty glyph), and its
/// outline as `n_ops` segment codes from op index `op`, consuming points
/// from point index `pt`.
pub struct G {
    pub adv: u16,
    pub bbox: [i16; 4],
    pub op: u16,
    pub n_ops: u16,
    pub pt: u16,
}

#[allow(dead_code, clippy::all)]
mod baked {
    use super::G;
    include!(concat!(env!("OUT_DIR"), "/font_baked.rs"));
}

/// The outlines, inflated from [`baked::OUTLINES_Z`] on first use: `ops`
/// packs 2-bit segment codes four per byte (0 move, 1 line, 2 quad); `pts`
/// holds the x, y pairs they consume (1, 1 and 2 points), in half font
/// units.
struct Outlines {
    ops: Vec<u8>,
    pts: Vec<i16>,
}

fn outlines() -> &'static Outlines {
    static O: std::sync::OnceLock<Outlines> = std::sync::OnceLock::new();
    O.get_or_init(|| {
        let (no, np) = (baked::N_OPS_BYTES, baked::N_PTS);
        let (b, _) = crate::deflate::inflate(baked::OUTLINES_Z, no + 2 * np).expect("baked font outlines inflate");
        assert_eq!(b.len(), no + 2 * np, "baked font outlines");
        let (lo, hi) = b[no..].split_at(np);
        let mut last = [0i16; 2];
        let pts = (0..np)
            .map(|i| {
                let z = lo[i] as u16 | (hi[i] as u16) << 8;
                let d = (z >> 1) as i16 ^ -((z & 1) as i16);
                last[i % 2] = last[i % 2].wrapping_add(d);
                last[i % 2]
            })
            .collect();
        Outlines { ops: b[..no].to_vec(), pts }
    })
}

/// The baked overlay chrome font (Inter Medium).
pub struct UiFont {
    _private: (),
}

pub static UI: UiFont = UiFont { _private: () };

/// The overlay chrome font.
pub fn ui_font() -> &'static UiFont {
    &UI
}

impl UiFont {
    /// Pixels per font unit at `px` (ab_glyph's `h_scale_factor`).
    fn k(&self, px: f32) -> f32 {
        px / (baked::ASCENT as f32 - baked::DESCENT as f32)
    }

    /// Glyph slot for `ch` (0, .notdef, when unmapped).
    fn slot(&self, ch: char) -> usize {
        baked::CMAP.binary_search_by_key(&ch, |&(c, _)| c).map_or(0, |i| baked::CMAP[i].1 as usize)
    }

    /// Whether `ch` has its own glyph.
    pub fn has_glyph(&self, ch: char) -> bool {
        self.slot(ch) != 0
    }

    fn kern(&self, a: usize, b: usize) -> f32 {
        baked::KERN.binary_search_by_key(&(a as u16, b as u16), |&(l, r, _)| (l, r)).map_or(0.0, |i| baked::KERN[i].2 as f32)
    }

    pub fn ascent(&self, px: f32) -> f32 {
        self.k(px) * baked::ASCENT as f32
    }

    /// Negative: below the baseline.
    pub fn descent(&self, px: f32) -> f32 {
        self.k(px) * baked::DESCENT as f32
    }

    #[cfg(test)]
    pub fn line_gap(&self, px: f32) -> f32 {
        self.k(px) * baked::LINE_GAP as f32
    }

    #[cfg(test)]
    pub fn units_per_em(&self) -> u16 {
        baked::UNITS_PER_EM
    }

    /// The em size at `px` (UI text on the OS stack asks for this em).
    pub fn em(&self, px: f32) -> f32 {
        self.k(px) * baked::UNITS_PER_EM as f32
    }

    /// Ascent minus descent: `px` up to float rounding.
    pub fn height(&self, px: f32) -> f32 {
        self.ascent(px) - self.descent(px)
    }

    /// Advance of `ch` alone at `px`.
    #[cfg(test)]
    pub fn advance(&self, ch: char, px: f32) -> f32 {
        self.k(px) * baked::GLYPHS[self.slot(ch)].adv as f32
    }

    /// Pen positions from `start`: `f(slot, pen x)` per character; returns
    /// the final pen. (Accumulated from `start` in order, the same float
    /// sums the old ab_glyph loops made.)
    fn layout(&self, s: &str, px: f32, start: f32, mut f: impl FnMut(usize, f32)) -> f32 {
        let k = self.k(px);
        let mut pen = start;
        let mut prev: Option<usize> = None;
        // Bidi format characters (isolates round number pairs): no
        // glyph, no width, no effect on the kerning.
        for ch in s.chars() {
            let g = self.slot(ch);
            if let Some(p) = prev {
                pen += k * self.kern(p, g);
            }
            f(g, pen);
            pen += k * baked::GLYPHS[g].adv as f32;
            prev = Some(g);
        }
        pen
    }

    /// Advance width of `s` at `px` (on the OS text stack when the baked
    /// glyphs do not cover it, on Windows).
    pub fn width(&self, s: &str, px: f32) -> f32 {
        #[cfg(windows)]
        if crate::ostext::needed(self, s) {
            return crate::ostext::width(self, s, px);
        }
        self.baked_width(s, px)
    }

    /// [`UiFont::width`] with the baked glyphs only (.notdef for others).
    pub fn baked_width(&self, s: &str, px: f32) -> f32 {
        self.layout(s, px, 0.0, |_, _| {})
    }

    /// Rasterise `s` with the top-left corner at (x, y): calls `f(x, y,
    /// coverage)` for every pixel of each glyph's box (coverage 0..=1, or a
    /// little above 1 where contours overlap). Returns the advance width.
    pub fn draw(&self, s: &str, px: f32, x: f32, y: f32, mut f: impl FnMut(i32, i32, f32)) -> f32 {
        #[cfg(windows)]
        if crate::ostext::needed(self, s) {
            return crate::ostext::draw(self, s, px, x, y, &mut f);
        }
        self.baked_draw(s, px, x, y, &mut f)
    }

    /// [`UiFont::draw`] with the baked glyphs only (.notdef for others).
    pub fn baked_draw(&self, s: &str, px: f32, x: f32, y: f32, mut f: impl FnMut(i32, i32, f32)) -> f32 {
        let k = self.k(px);
        let baseline = y + self.ascent(px);
        let end = self.layout(s, px, x, |g, pen| {
            draw_glyph(&baked::GLYPHS[g], k, Pt::new(pen, baseline), &mut f);
        });
        end - x
    }
}

/// One glyph at `pos` (pen on the baseline), `k` pixels per font unit:
/// ab_glyph's `px_bounds` and `OutlinedGlyph::draw`, on baked data.
fn draw_glyph(g: &G, k: f32, pos: Pt, f: &mut impl FnMut(i32, i32, f32)) {
    if g.n_ops == 0 {
        return;
    }
    let [x_min, y_max, x_max, y_min] = g.bbox.map(|v| v as f32);
    let (xt, xf) = (pos.x.trunc(), pos.x.fract());
    let (yt, yf) = (pos.y.trunc(), pos.y.fract());
    let min = Pt::new((x_min * k + xf).floor() + xt, (y_max * -k + yf).floor() + yt);
    let max = Pt::new((x_max * k + xf).ceil() + xt, (y_min * -k + yf).ceil() + yt);
    let (w, h) = ((max.x - min.x) as usize, (max.y - min.y) as usize);
    let off = Pt::new(pos.x - min.x, pos.y - min.y);
    let mut cov = Coverage::new(w, h);
    let o = outlines();
    let mut pi = g.pt as usize;
    let mut next = || {
        let p = Pt::new(o.pts[2 * pi] as f32 * 0.5 * k + off.x, o.pts[2 * pi + 1] as f32 * 0.5 * -k + off.y);
        pi += 1;
        p
    };
    let mut cur = Pt::new(0.0, 0.0);
    for i in g.op as usize..(g.op + g.n_ops) as usize {
        match (o.ops[i / 4] >> (2 * (i % 4))) & 3 {
            0 => cur = next(),
            1 => {
                let p = next();
                cov.line(cur, p);
                cur = p;
            }
            _ => {
                let (c, p) = (next(), next());
                cov.quad(cur, c, p);
                cur = p;
            }
        }
    }
    let (bx, by) = (min.x as i32, min.y as i32);
    cov.for_each(|gx, gy, c| f(bx + gx as i32, by + gy as i32, c));
}
