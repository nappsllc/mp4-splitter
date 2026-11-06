//! The UI's languages (the file format, plural rules and matching of system
//! languages are Rustshot's, Apache-2.0): `lang/<tag>.txt`, plain `key = value`
//! lines, packed into the app at build time. English is the source; a key a
//! language lacks falls back to it. Rustshot downloads its much larger files;
//! these are small enough to carry.

use crate::plural::Rule;
use std::collections::HashMap;
use std::fmt::Display;
use std::sync::atomic::{AtomicPtr, Ordering};

/// A language in the app.
#[derive(Clone, Copy, Debug)]
pub struct Lang {
    pub tag: &'static str,
    /// The language in itself.
    pub name: &'static str,
    pub rtl: bool,
    /// Every character is in the baked font: it needs no OS text stack.
    pub baked: bool,
    at: usize,
    len: usize,
}

include!(concat!(env!("OUT_DIR"), "/lang_table.rs"));
static PACKED: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/lang.z"));

/// The languages that can be shown here, English among them.
pub fn languages() -> impl Iterator<Item = &'static Lang> {
    LANGS.iter().filter(|l| usable(l))
}

/// The OS text stack is ported to Windows only so far: elsewhere, scripts
/// outside the baked font can't be drawn yet.
fn usable(l: &Lang) -> bool {
    cfg!(windows) || l.baked
}

/// One language's strings, for as long as the app runs (a language is only
/// loaded when it is chosen).
struct Table {
    tag: &'static str,
    rtl: bool,
    rule: Rule,
    strings: HashMap<&'static str, &'static str>,
}

/// `key = value` lines; `key[form]` for plurals. Metadata lines start with `@`.
fn parse(text: &'static str) -> (HashMap<&'static str, &'static str>, Rule) {
    let (mut strings, mut rule) = (HashMap::new(), Rule::OneOther);
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(" = ") else { continue };
        match k {
            "@plural" => rule = Rule::from_name(v),
            "@name" | "@dir" => {}
            _ => {
                strings.insert(k, v);
            }
        }
    }
    (strings, rule)
}

fn all_text() -> &'static str {
    static TEXT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    TEXT.get_or_init(|| {
        let (raw, _) = crate::deflate::inflate(PACKED, RAW_LEN).unwrap_or_default();
        String::from_utf8(raw).unwrap_or_default()
    })
}

fn text_of(l: &Lang) -> &'static str {
    all_text().get(l.at..l.at + l.len).unwrap_or("")
}

fn table(l: &'static Lang) -> &'static Table {
    let (strings, rule) = parse(text_of(l));
    Box::leak(Box::new(Table { tag: l.tag, rtl: l.rtl, rule, strings }))
}

fn english() -> &'static Table {
    static EN: std::sync::OnceLock<&'static Table> = std::sync::OnceLock::new();
    EN.get_or_init(|| {
        LANGS
            .iter()
            .find(|l| l.tag == "en")
            .map_or_else(|| Box::leak(Box::new(Table { tag: "en", rtl: false, rule: Rule::OneOther, strings: HashMap::new() })) as &'static Table, table)
    })
}

static CURRENT: AtomicPtr<Table> = AtomicPtr::new(std::ptr::null_mut());

fn current() -> &'static Table {
    let p = CURRENT.load(Ordering::Acquire);
    // SAFETY: only ever a leaked `&'static Table` (or null).
    if p.is_null() { english() } else { unsafe { &*p } }
}

/// Show the UI in this language: a tag, or empty to follow the system.
pub fn set_language(setting: &str) {
    let tags = if setting.is_empty() { system_languages() } else { vec![setting.to_string()] };
    let codes: Vec<&str> = languages().map(|l| l.tag).collect();
    let tag = best_in(&tags, &codes).unwrap_or("en");
    let tag = codes.iter().copied().find(|c| *c == tag).unwrap_or("en");
    if let Some(l) = LANGS.iter().find(|l| l.tag == tag) {
        let t: &'static Table = table(l);
        CURRENT.store(std::ptr::from_ref(t).cast_mut(), Ordering::Release);
    }
}

/// The language showing.
pub fn current_tag() -> &'static str {
    current().tag
}

/// Is the UI laid out right to left?
pub fn is_rtl() -> bool {
    current().rtl
}

/// The string for `key`.
pub fn get(key: &'static str) -> &'static str {
    current().strings.get(key).or_else(|| english().strings.get(key)).copied().unwrap_or(key)
}

/// The plural string for `key` and the count `n`.
pub fn get_n(key: &'static str, n: u64) -> &'static str {
    let t = current();
    let form = t.rule.category(n).name();
    let en = english();
    [(t, form), (t, "other"), (en, en.rule.category(n).name()), (en, "other")]
        .iter()
        .find_map(|(t, f)| {
            t.strings.iter().find(|(k, _)| k.strip_prefix(key).and_then(|r| r.strip_prefix('[')).and_then(|r| r.strip_suffix(']')) == Some(*f)).map(|(_, v)| *v)
        })
        .unwrap_or(key)
}

/// `{name}` placeholders filled in.
pub fn fill(s: &str, args: &[(&str, &dyn Display)]) -> String {
    let mut out = s.to_string();
    for (k, v) in args {
        out = out.replace(&format!("{{{k}}}"), &v.to_string());
    }
    out
}

/// A string by key, with `name = value` placeholders if any.
#[macro_export]
macro_rules! tr {
    ($k:literal) => {
        $crate::i18n::get($k)
    };
    ($k:literal, $($name:ident = $v:expr),+ $(,)?) => {
        $crate::i18n::fill($crate::i18n::get($k), &[$((stringify!($name), &$v as &dyn ::std::fmt::Display)),+])
    };
}

/// A plural string by key and count (`{n}` is the count), with more placeholders if any.
#[macro_export]
macro_rules! trn {
    ($k:literal, $n:expr $(, $name:ident = $v:expr)* $(,)?) => {{
        let n = $n as u64;
        $crate::i18n::fill($crate::i18n::get_n($k, n), &[("n", &n as &dyn ::std::fmt::Display) $(, (stringify!($name), &$v as &dyn ::std::fmt::Display))*])
    }};
}

/// A plural string for a tooltip, which lives as long as the app: each is
/// made once per language.
pub fn tip_n(key: &'static str, n: u64) -> &'static str {
    type Made = HashMap<(&'static str, &'static str, u64), &'static str>;
    static MADE: std::sync::Mutex<Option<Made>> = std::sync::Mutex::new(None);
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    made.get_or_insert_with(HashMap::new).entry((current_tag(), key, n)).or_insert_with(|| Box::leak(fill(get_n(key, n), &[("n", &n)]).into_boxed_str()))
}

/// The OS's preferred UI languages, best first.
fn system_languages() -> Vec<String> {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::{GetUserPreferredUILanguages, MUI_LANGUAGE_NAME};
        use windows::core::PWSTR;
        let (mut count, mut len) = (0u32, 0u32);
        // SAFETY: the sizes outlive the calls; the buffer is the length asked for.
        unsafe {
            if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, None, &mut len).is_ok() {
                let mut buf = vec![0u16; len as usize];
                if GetUserPreferredUILanguages(MUI_LANGUAGE_NAME, &mut count, Some(PWSTR(buf.as_mut_ptr())), &mut len).is_ok() {
                    return buf.split(|c| *c == 0).filter(|s| !s.is_empty()).map(String::from_utf16_lossy).collect();
                }
            }
        }
        Vec::new()
    }
    #[cfg(not(windows))]
    {
        let mut out = Vec::new();
        for k in ["LANGUAGE", "LC_ALL", "LC_MESSAGES", "LANG"] {
            if let Ok(v) = std::env::var(k) {
                out.extend(v.split(':').filter(|s| !s.is_empty()).map(str::to_string));
            }
        }
        out
    }
}

/// The first of `tags` (an OS's or a setting's, best first) that one of `codes`
/// can serve.
pub fn best_in<'a>(tags: &[String], codes: &[&'a str]) -> Option<&'a str> {
    let has = |c: &str| codes.iter().find(|x| x.eq_ignore_ascii_case(c)).copied();
    for t in tags {
        // `pt_BR.UTF-8`, `sr_RS@latin`: the encoding and modifier go, `_` is `-`.
        let latin = t.contains("@latin");
        let t = t.split(['.', '@']).next().unwrap_or("").replace('_', "-");
        if t.is_empty() || t.eq_ignore_ascii_case("C") || t.eq_ignore_ascii_case("POSIX") {
            continue;
        }
        if let Some(c) = has(&t) {
            return Some(c);
        }
        let low = t.to_ascii_lowercase();
        let lang = low.split('-').next().unwrap_or("");
        let special = match lang {
            "zh" if ["hant", "tw", "hk", "mo"].iter().any(|x| low.split('-').any(|p| p == *x)) => Some("zh-Hant"),
            "zh" => Some("zh-Hans"),
            "pt" if low.ends_with("-pt") => Some("pt-PT"),
            "pt" => Some("pt-BR"),
            "no" | "nn" | "nb" => Some("nb"),
            "sr" | "sh" => Some("sr-Latn"),
            _ => None,
        };
        let _ = latin;
        if let Some(c) = special.and_then(has) {
            return Some(c);
        }
        if let Some(c) = codes.iter().find(|c| c.to_ascii_lowercase().split('-').next() == Some(lang)) {
            return Some(c);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn placeholders(s: &str) -> BTreeSet<&str> {
        s.split('{').skip(1).filter_map(|p| p.split_once('}').map(|x| x.0)).collect()
    }

    /// Plural keys by their base name.
    fn base(k: &str) -> &str {
        k.split('[').next().unwrap_or(k)
    }

    fn strings_of(l: &'static Lang) -> HashMap<&'static str, &'static str> {
        parse(text_of(l)).0
    }

    #[test]
    fn there_are_35_languages_with_rustshots_tags() {
        assert_eq!(LANGS.len(), 35);
        for t in ["en", "ar", "he", "zh-Hans", "zh-Hant", "pt-BR", "pt-PT", "sr-Latn", "nb", "ru", "uk", "ja", "ko", "th", "hi", "vi"] {
            assert!(LANGS.iter().any(|l| l.tag == t), "{t}");
        }
        for l in &LANGS {
            assert!(!l.name.is_empty() && l.name != l.tag, "{} has no name of its own", l.tag);
        }
        assert!(LANGS.iter().filter(|l| l.rtl).map(|l| l.tag).eq(["ar", "he"]));
    }

    #[test]
    fn every_language_has_every_key_with_its_placeholders() {
        let en = strings_of(LANGS.iter().find(|l| l.tag == "en").unwrap());
        let en_bases: BTreeSet<&str> = en.keys().map(|k| base(k)).collect();
        for l in &LANGS {
            let s = strings_of(l);
            let (_, rule) = parse(text_of(l));
            let bases: BTreeSet<&str> = s.keys().map(|k| base(k)).collect();
            assert_eq!(bases, en_bases, "{}: different keys", l.tag);
            for (k, v) in &s {
                let want = en.iter().find(|(e, _)| base(e) == base(k)).map(|(e, _)| placeholders(en[*e])).unwrap();
                // A plural form may leave out {n} (Arabic's "one" and "two").
                let got = placeholders(v);
                assert!(got.is_subset(&want), "{}: {k} has {got:?}, English {want:?}", l.tag);
                if !k.contains('[') {
                    assert_eq!(got, want, "{}: {k}", l.tag);
                }
                assert!(!v.trim().is_empty(), "{}: {k} is empty", l.tag);
            }
            // Every form its plural rule can pick, for every plural key.
            let plural_bases: BTreeSet<&str> = en.keys().filter(|k| k.contains('[')).map(|k| base(k)).collect();
            for b in plural_bases {
                for n in 0..200 {
                    let form = rule.category(n).name();
                    let has = s.contains_key(format!("{b}[{form}]").as_str()) || s.contains_key(format!("{b}[other]").as_str());
                    assert!(has, "{}: {b} lacks the form for {n}", l.tag);
                }
                assert!(s.contains_key(format!("{b}[other]").as_str()), "{}: {b}[other]", l.tag);
            }
        }
    }

    /// Latin, Greek, Cyrillic and Vietnamese are drawn from the baked font;
    /// only the scripts it lacks go through the OS.
    #[test]
    fn every_glyph_is_in_the_baked_font_or_the_language_says_otherwise() {
        let os_text = ["ar", "he", "hi", "ja", "ko", "th", "zh-Hans", "zh-Hant"];
        for l in &LANGS {
            assert_eq!(l.baked, !os_text.contains(&l.tag), "{}", l.tag);
            let lacking: Vec<char> = text_of(l)
                .lines()
                .filter(|x| !x.starts_with('#'))
                .flat_map(str::chars)
                .filter(|c| !c.is_control() && !crate::fonts::ui_font().has_glyph(*c))
                .collect();
            assert_eq!(l.baked, lacking.is_empty(), "{}: {lacking:?}", l.tag);
        }
    }

    #[test]
    fn every_key_in_the_code_exists_and_every_key_is_used() {
        let en = strings_of(LANGS.iter().find(|l| l.tag == "en").unwrap());
        let bases: BTreeSet<&str> = en.keys().map(|k| base(k)).collect();
        let mut used = BTreeSet::new();
        for f in std::fs::read_dir("src").unwrap().flatten() {
            let text = std::fs::read_to_string(f.path()).unwrap();
            if f.path().file_name().is_some_and(|n| n == "i18n.rs") {
                continue;
            }
            for pat in ["tr!(\"", "trn!(\"", "tip_n(\""] {
                for part in text.split(pat).skip(1) {
                    used.insert(part.split('"').next().unwrap().to_string());
                }
            }
        }
        for k in &used {
            assert!(bases.contains(k.as_str()), "{k} is not in lang/en.txt");
        }
        for k in &bases {
            assert!(used.contains(*k), "{k} is not used");
        }
    }

    /// The app's whole name, in every language.
    #[test]
    fn every_language_calls_the_app_by_its_name() {
        for l in &LANGS {
            assert!(strings_of(l)["tip.about"].contains(crate::APP_NAME), "{}", l.tag);
        }
        assert_eq!(crate::APP_NAME, "PowerNapps MP4 Splitter");
    }

    #[test]
    fn system_languages_find_their_files() {
        let codes: Vec<&str> = LANGS.iter().map(|l| l.tag).collect();
        let best = |t: &str| best_in(&[t.to_string()], &codes);
        for (t, want) in [
            ("de-DE", "de"),
            ("de_AT.UTF-8", "de"),
            ("pt-BR", "pt-BR"),
            ("pt-PT", "pt-PT"),
            ("pt_AO", "pt-BR"),
            ("zh-CN", "zh-Hans"),
            ("zh-TW", "zh-Hant"),
            ("zh-Hant-TW", "zh-Hant"),
            ("zh-Hans-CN", "zh-Hans"),
            ("nb-NO", "nb"),
            ("nn", "nb"),
            ("sr-Latn-RS", "sr-Latn"),
            ("sr_RS.UTF-8@latin", "sr-Latn"),
            ("he-IL", "he"),
            ("ar-EG", "ar"),
            ("es-419", "es"),
            ("en-GB", "en"),
        ] {
            assert_eq!(best(t), Some(want), "{t}");
        }
        assert_eq!(best("C"), None);
        assert_eq!(best("POSIX"), None);
        assert_eq!(best("xx-YY"), None);
        assert_eq!(best_in(&["xx".to_string(), "fr-CA".to_string()], &codes), Some("fr"), "the next preferred");
    }

    #[test]
    fn plurals_and_placeholders_fill_in() {
        assert_eq!(fill("a {x} b {y}", &[("x", &1), ("y", &"z")]), "a 1 b z");
        let en = LANGS.iter().find(|l| l.tag == "en").unwrap();
        let (s, rule) = parse(text_of(en));
        assert_eq!(s[format!("tools.pieces[{}]", rule.category(1).name()).as_str()], "{n} piece");
        assert_eq!(s[format!("tools.pieces[{}]", rule.category(2).name()).as_str()], "{n} pieces");
    }
}
