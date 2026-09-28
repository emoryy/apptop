use std::fs;
use std::path::PathBuf;

use crate::ui::Col;

/// Settings kept between runs in ~/.config/apptop/config (key=value lines).
#[derive(Clone, Debug)]
pub struct Config {
    pub sort: Col,
    pub desc: bool,
    pub split: bool,
    pub info: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            sort: Col::Mem,
            desc: true,
            split: false,
            info: false,
        }
    }
}

fn path() -> Option<PathBuf> {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var("HOME").ok().map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("apptop").join("config"))
}

impl Config {
    pub fn load() -> Self {
        let mut c = Config::default();
        let Some(text) = path().and_then(|p| fs::read_to_string(p).ok()) else {
            return c;
        };
        for line in text.lines() {
            let Some((k, v)) = line.split_once('=') else { continue };
            let (k, v) = (k.trim(), v.trim());
            match k {
                "sort" => c.sort = Col::from_name(v).unwrap_or(c.sort),
                "desc" => c.desc = v == "true",
                "split" => c.split = v == "true",
                "info" => c.info = v == "true",
                _ => {}
            }
        }
        c
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
        if let Some(dir) = p.parent() {
            let _ = fs::create_dir_all(dir);
        }
        let text = format!(
            "sort={}\ndesc={}\nsplit={}\ninfo={}\n",
            self.sort.name(),
            self.desc,
            self.split,
            self.info
        );
        let _ = fs::write(p, text);
    }
}
