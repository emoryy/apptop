use std::sync::OnceLock;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    En,
    Hu,
}

static LANG: OnceLock<Lang> = OnceLock::new();

pub fn set(l: Lang) {
    let _ = LANG.set(l);
}

pub fn lang() -> Lang {
    *LANG.get().unwrap_or(&Lang::En)
}

pub fn parse(s: &str) -> Option<Lang> {
    match s.to_ascii_lowercase().as_str() {
        "en" => Some(Lang::En),
        "hu" => Some(Lang::Hu),
        _ => None,
    }
}

/// Language from the locale, checked in the order glibc uses for messages.
pub fn from_env() -> Lang {
    for var in ["LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var)
            && !v.is_empty()
        {
            return if v.starts_with("hu") { Lang::Hu } else { Lang::En };
        }
    }
    Lang::En
}

pub fn tr(en: &'static str, hu: &'static str) -> &'static str {
    match lang() {
        Lang::En => en,
        Lang::Hu => hu,
    }
}

/// "3 processes" / "1 process" / "3 folyamat"
pub fn count(n: usize, one: &str, many: &str, hu: &str) -> String {
    let word = match lang() {
        Lang::Hu => hu,
        Lang::En if n == 1 => one,
        Lang::En => many,
    };
    format!("{n} {word}")
}
