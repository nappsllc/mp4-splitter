//! The few settings kept between runs: a small `key = value` file in the
//! user's configuration folder. Anything missing or unknown means the default.

use std::path::PathBuf;

/// Follow the OS, or a fixed theme.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Config {
    pub theme: ThemeChoice,
    /// Which accent ([`crate::theme::ACCENTS`]): 0 is the theme's own.
    pub accent: usize,
    /// A language tag, or empty to follow the OS.
    pub language: String,
}

impl Config {
    pub fn parse(text: &str) -> Config {
        let mut c = Config::default();
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let v = v.trim();
            match k.trim() {
                "theme" => {
                    c.theme = match v {
                        "dark" => ThemeChoice::Dark,
                        "light" => ThemeChoice::Light,
                        _ => ThemeChoice::System,
                    }
                }
                "accent" => c.accent = v.parse().unwrap_or(0),
                "language" => c.language = v.to_string(),
                _ => {}
            }
        }
        c
    }

    pub fn to_text(&self) -> String {
        let theme = match self.theme {
            ThemeChoice::System => "system",
            ThemeChoice::Dark => "dark",
            ThemeChoice::Light => "light",
        };
        format!("theme = {theme}\naccent = {}\nlanguage = {}\n", self.accent, self.language)
    }

    /// What is saved, or the defaults.
    pub fn load() -> Config {
        path().and_then(|p| std::fs::read_to_string(p).ok()).map_or_else(Config::default, |t| Config::parse(&t))
    }

    /// Save it (not being able to is not worth troubling anyone).
    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&p, self.to_text()) {
            eprintln!("settings: {e}");
        }
    }
}

/// `MP4_SPLITTER_CONFIG` first (tests), then the OS's place for it.
fn path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MP4_SPLITTER_CONFIG") {
        return Some(PathBuf::from(p));
    }
    let env = |k: &str| std::env::var_os(k).map(PathBuf::from);
    let dir = if cfg!(windows) {
        env("APPDATA")?
    } else if cfg!(target_os = "macos") {
        env("HOME")?.join("Library/Application Support")
    } else {
        env("XDG_CONFIG_HOME").or_else(|| env("HOME").map(|h| h.join(".config")))?
    };
    Some(dir.join("mp4-splitter").join("settings.txt"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_round_trip() {
        let c = Config { theme: ThemeChoice::Light, accent: 2, language: "pt-BR".into() };
        assert_eq!(Config::parse(&c.to_text()), c);
    }

    #[test]
    fn what_is_missing_or_unknown_is_the_default() {
        assert_eq!(Config::parse(""), Config::default());
        let c = Config::parse("theme = purple\naccent = many\nnonsense\nfuture = 1\nlanguage = de");
        assert_eq!(c, Config { theme: ThemeChoice::System, accent: 0, language: "de".into() });
    }
}
