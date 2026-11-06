//! Strings the baked font has no glyphs for (Arabic, Hebrew, Hindi, Thai, CJK;
//! file names in any script) through GDI and Uniscribe, which bring shaping,
//! bidi and font linking (adapted from Rustshot's `src/uitext.rs` and
//! `src/text_win.rs`, Apache-2.0). Sized to the baked font's em with its
//! baseline on the baked font's, so they line up with their neighbours.
//! Widths and rasters are cached: the UI redraws the same labels every frame.

use crate::fonts::UiFont;
use std::cell::RefCell;
use std::collections::HashMap;
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    ANTIALIASED_QUALITY, BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateCompatibleDC, CreateDIBSection, CreateFontW, DEFAULT_CHARSET,
    DEFAULT_PITCH, DIB_RGB_COLORS, DRAW_TEXT_FORMAT, DT_CALCRECT, DT_NOCLIP, DT_NOPREFIX, DT_RTLREADING, DT_SINGLELINE, DeleteDC, DeleteObject, DrawTextW,
    FW_NORMAL, GdiFlush, GetTextMetricsW, HDC, OUT_TT_PRECIS, SelectObject, SetBkMode, SetTextColor, TEXTMETRICW, TRANSPARENT,
};
use windows::core::w;

/// Segoe UI's line (ascent + descent) per em.
const EM_RATIO: f32 = 2724.0 / 2048.0;
/// Rasters kept before starting over.
const MAX_ENTRIES: usize = 2048;

/// Does `s` need the OS (a character the baked font lacks)?
pub fn needed(font: &UiFont, s: &str) -> bool {
    !s.is_ascii() && s.chars().any(|c| !c.is_control() && !font.has_glyph(c))
}

struct Entry {
    width: f32,
    /// Coverage `w` wide, top-left at (x0, y0) from the string's top-left.
    x0: i32,
    y0: i32,
    w: usize,
    a: Vec<u8>,
}

thread_local! {
    static CACHE: RefCell<HashMap<(String, u32, bool), Entry>> = RefCell::new(HashMap::new());
}

/// Run `f` with a memory DC holding the UI face at `px`.
fn with_dc<R>(px: f32, f: impl FnOnce(HDC) -> R) -> Option<R> {
    // SAFETY: the DC and font are made, selected and freed here.
    unsafe {
        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            return None;
        }
        let font = CreateFontW(
            px.round().max(1.0) as i32,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_TT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            ANTIALIASED_QUALITY,
            DEFAULT_PITCH.0 as u32,
            w!("Segoe UI"),
        );
        if font.is_invalid() {
            let _ = DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, font.into());
        let r = f(dc);
        SelectObject(dc, old);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        Some(r)
    }
}

fn text(dc: HDC, s: &str, r: &mut RECT, flags: DRAW_TEXT_FORMAT, rtl: bool) {
    // Right to left: DT_RTLREADING, inside an embedding, which Uniscribe
    // honours whatever the font's charset.
    let mut wide: Vec<u16> =
        if rtl { std::iter::once(0x202B).chain(s.encode_utf16()).chain(std::iter::once(0x202C)).collect() } else { s.encode_utf16().collect() };
    let dir = if rtl { DT_RTLREADING } else { DRAW_TEXT_FORMAT(0) };
    // SAFETY: `wide` and `r` outlive the call.
    unsafe {
        DrawTextW(dc, &mut wide, r, flags | dir | DT_SINGLELINE | DT_NOPREFIX | DT_NOCLIP);
    }
}

fn raster(font: &UiFont, s: &str, px: f32, rtl: bool) -> Entry {
    let opx = font.em(px) * EM_RATIO;
    let empty = |width: f32| Entry { width, x0: 0, y0: 0, w: 1, a: Vec::new() };
    with_dc(opx, |dc| {
        // SAFETY: the bitmap is made, read and freed here, selected only meanwhile.
        unsafe {
            let mut ext = RECT::default();
            text(dc, s, &mut ext, DT_CALCRECT, rtl);
            let width = (ext.right - ext.left).max(0) as f32;
            let mut tm = TEXTMETRICW::default();
            let _ = GetTextMetricsW(dc, &mut tm);
            let top = (font.ascent(px) - tm.tmAscent as f32).round() as i32;
            let pad = (opx / 3.0).ceil() as i32 + 3;
            let (cw, ch) = (ext.right + 2 * pad, ext.bottom + 2 * pad);
            if cw <= 0 || ch <= 0 {
                return empty(width);
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: cw,
                    biHeight: -ch,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits = std::ptr::null_mut();
            let Ok(bmp) = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                return empty(width);
            };
            if bits.is_null() {
                let _ = DeleteObject(bmp.into());
                return empty(width);
            }
            let old = SelectObject(dc, bmp.into());
            SetBkMode(dc, TRANSPARENT);
            SetTextColor(dc, COLORREF(0x00FF_FFFF));
            let mut r = RECT { left: pad, top: pad, right: pad + ext.right, bottom: pad + ext.bottom };
            text(dc, s, &mut r, DRAW_TEXT_FORMAT(0), rtl);
            let _ = GdiFlush();
            let data = std::slice::from_raw_parts(bits as *const u8, (cw * ch * 4) as usize);
            let a: Vec<u8> = data.chunks_exact(4).map(|p| p[0].max(p[1]).max(p[2])).collect();
            SelectObject(dc, old);
            let _ = DeleteObject(bmp.into());
            Entry { width, x0: -pad, y0: top - pad, w: cw as usize, a }
        }
    })
    .unwrap_or_else(|| empty(0.0))
}

fn with_entry<R>(font: &UiFont, s: &str, px: f32, f: impl FnOnce(&Entry) -> R) -> R {
    let rtl = crate::i18n::is_rtl();
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() > MAX_ENTRIES {
            c.clear();
        }
        let e = c.entry((s.to_string(), px.to_bits(), rtl)).or_insert_with(|| raster(font, s, px, rtl));
        f(e)
    })
}

/// The width of `s` at the baked font's `px`.
pub fn width(font: &UiFont, s: &str, px: f32) -> f32 {
    with_entry(font, s, px, |e| e.width)
}

/// Draw `s` with its top-left at (x, y) as the baked font would place it:
/// `f(x, y, coverage)` per pixel. Its width.
pub fn draw(font: &UiFont, s: &str, px: f32, x: f32, y: f32, f: &mut dyn FnMut(i32, i32, f32)) -> f32 {
    let (ox, oy) = (x.round() as i32, y.round() as i32);
    with_entry(font, s, px, |e| {
        for (i, &v) in e.a.iter().enumerate() {
            if v > 0 {
                f(ox + e.x0 + (i % e.w) as i32, oy + e.y0 + (i / e.w) as i32, v as f32 / 255.0);
            }
        }
        e.width
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fonts::UI;

    #[test]
    fn only_strings_with_missing_glyphs_go_to_the_os() {
        assert!(!needed(&UI, "Save as"));
        assert!(!needed(&UI, "Größe"));
        assert!(!needed(&UI, "Сохранить"));
        assert!(needed(&UI, "الحفظ"));
        assert!(needed(&UI, "保存"));
        assert!(needed(&UI, "บันทึก"));
    }

    #[test]
    fn scripts_outside_the_font_measure_and_draw() {
        for s in ["保存する", "한국어", "ไทย", "हिन्दी", "العربية", "עברית"] {
            let w = width(&UI, s, 16.0);
            assert!(w > 4.0, "{s}: {w}");
            let mut lit = 0;
            draw(&UI, s, 16.0, 10.0, 10.0, &mut |_, _, c| {
                if c > 0.3 {
                    lit += 1;
                }
            });
            assert!(lit > 10, "{s}: {lit} pixels");
        }
    }
}
