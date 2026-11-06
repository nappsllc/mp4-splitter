//! Design tokens, dark and light (Rustshot's palette: periwinkle accent,
//! soft surfaces), picked from the OS setting.

use crate::config::ThemeChoice;
use crate::paint::{C, hex};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    /// The window behind everything.
    pub bg: C,
    /// Cards and bars.
    pub surface: C,
    /// Recessed areas (the timeline's track, fields).
    pub well: C,
    pub border: C,
    pub separator: C,
    pub text: C,
    pub text_muted: C,
    pub text_faint: C,
    pub icon: C,
    pub hover: C,
    pub pressed: C,
    pub accent: C,
    pub accent_hover: C,
    pub accent_soft: C,
    pub on_accent: C,
    /// Behind the video.
    pub stage: C,
    /// Over the parts that are cut away.
    pub dim: C,
    pub playhead: C,
    pub cut: C,
    pub success: C,
    pub error: C,
    pub tooltip_bg: C,
    pub tooltip_text: C,
}

pub const DARK: Theme = Theme {
    dark: true,
    bg: hex(0x111215, 255),
    surface: hex(0x1B1C20, 255),
    well: hex(0x0B0C0E, 255),
    border: hex(0xFFFFFF, 23),
    separator: hex(0xFFFFFF, 26),
    text: hex(0xECEEF3, 255),
    text_muted: hex(0x9296A3, 255),
    text_faint: hex(0x5F6370, 255),
    icon: hex(0xA3A7B3, 255),
    hover: hex(0xFFFFFF, 15),
    pressed: hex(0xFFFFFF, 28),
    accent: hex(0x8B93FF, 255),
    accent_hover: hex(0xA2A9FF, 255),
    accent_soft: hex(0x8B93FF, 46),
    on_accent: hex(0x0E0F1A, 255),
    stage: hex(0x08090B, 255),
    dim: hex(0x07080B, 170),
    playhead: hex(0xFFFFFF, 255),
    cut: hex(0xFFD166, 255),
    success: hex(0x4ADE9A, 255),
    error: hex(0xFF7A7A, 255),
    tooltip_bg: hex(0x0E0F12, 247),
    tooltip_text: hex(0xF2F3F6, 255),
};

pub const LIGHT: Theme = Theme {
    dark: false,
    bg: hex(0xEEEFF3, 255),
    surface: hex(0xFAFAFC, 255),
    well: hex(0xE2E4EA, 255),
    border: hex(0x000000, 20),
    separator: hex(0x000000, 26),
    text: hex(0x15171E, 255),
    text_muted: hex(0x737889, 255),
    text_faint: hex(0xA0A4B0, 255),
    icon: hex(0x5F6473, 255),
    hover: hex(0x000000, 15),
    pressed: hex(0x000000, 26),
    accent: hex(0x5B63F5, 255),
    accent_hover: hex(0x4A52E8, 255),
    accent_soft: hex(0x5B63F5, 36),
    on_accent: hex(0xFFFFFF, 255),
    stage: hex(0x16171B, 255),
    dim: hex(0xEEEFF3, 175),
    playhead: hex(0x15171E, 255),
    cut: hex(0xE08A00, 255),
    success: hex(0x1E9E63, 255),
    error: hex(0xD64545, 255),
    tooltip_bg: hex(0x16171C, 247),
    tooltip_text: hex(0xF2F3F6, 255),
};

/// The accents one can choose (as Rustshot's): the shade for the dark theme,
/// and for the light. The first is the theme's own.
pub const ACCENTS: [(C, C); 3] = [
    (DARK.accent, LIGHT.accent),
    (hex(0x5CC8C2, 255), hex(0x16897F, 255)), // teal
    (hex(0xF2A65A, 255), hex(0xB8661A, 255)), // amber
];

impl Theme {
    /// This theme with accent number `i`.
    pub fn with_accent(mut self, i: usize) -> Theme {
        let (dark, light) = ACCENTS[if i < ACCENTS.len() { i } else { 0 }];
        let c = if self.dark { dark } else { light };
        if c == self.accent {
            return self;
        }
        let lift = |v: u8| if self.dark { v.saturating_add(23) } else { v.saturating_sub(17) };
        self.accent = c;
        self.accent_hover = C { r: lift(c.r), g: lift(c.g), b: lift(c.b), a: 255 };
        self.accent_soft = c.with_alpha(if self.dark { 46 } else { 36 });
        self
    }
}

/// `MP4_SPLITTER_THEME=light|dark` first, then the choice, then the OS (dark
/// when unknown).
pub fn pick(os_dark: Option<bool>, choice: ThemeChoice, accent: usize) -> Theme {
    let base = match std::env::var("MP4_SPLITTER_THEME").ok().as_deref() {
        Some("light") => LIGHT,
        Some("dark") => DARK,
        _ => match choice {
            ThemeChoice::Dark => DARK,
            ThemeChoice::Light => LIGHT,
            ThemeChoice::System => {
                if os_dark == Some(false) {
                    LIGHT
                } else {
                    DARK
                }
            }
        },
    };
    base.with_accent(accent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_choice_beats_the_os() {
        assert!(pick(Some(true), ThemeChoice::Light, 0) == LIGHT);
        assert!(pick(Some(false), ThemeChoice::Dark, 0) == DARK);
        assert!(pick(Some(false), ThemeChoice::System, 0) == LIGHT);
        assert!(pick(Some(true), ThemeChoice::System, 0) == DARK);
        assert!(pick(None, ThemeChoice::System, 0) == DARK);
    }

    #[test]
    fn accents_follow_the_theme_and_a_bad_number_is_the_default() {
        assert_eq!(pick(None, ThemeChoice::Dark, 1).accent, ACCENTS[1].0);
        assert_eq!(pick(None, ThemeChoice::Light, 1).accent, ACCENTS[1].1);
        assert!(pick(None, ThemeChoice::Dark, 99) == DARK);
        assert_eq!(pick(None, ThemeChoice::Dark, 2).accent_soft.a, 46);
    }
}
