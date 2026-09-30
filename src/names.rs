use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::collect::Proc;

/// Names from .desktop files: by desktop id and by the basename of the Exec binary.
#[derive(Default)]
pub struct DesktopIndex {
    by_id: HashMap<String, String>,
    /// exe basename -> (name, rank); lower rank wins, see `exec_rank`
    by_exe: HashMap<String, (String, u8)>,
}

impl DesktopIndex {
    pub fn load() -> Self {
        let mut idx = DesktopIndex::default();
        let home = std::env::var("HOME").unwrap_or_default();
        let data_home = std::env::var("XDG_DATA_HOME").unwrap_or_else(|_| format!("{home}/.local/share"));
        let data_dirs = std::env::var("XDG_DATA_DIRS")
            .ok()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
        // user entries first, so they win over system ones
        let mut dirs = vec![PathBuf::from(&data_home).join("applications")];
        dirs.extend(data_dirs.split(':').map(|d| Path::new(d).join("applications")));
        dirs.push(PathBuf::from(format!("{data_home}/flatpak/exports/share/applications")));
        dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
        for dir in dirs {
            idx.scan(&dir, &dir);
        }
        idx
    }

    fn scan(&mut self, root: &Path, dir: &Path) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for ent in rd.flatten() {
            let path = ent.path();
            if path.is_dir() {
                self.scan(root, &path);
                continue;
            }
            if path.extension().is_none_or(|e| e != "desktop") {
                continue;
            }
            // desktop ids of files in subdirectories join the path parts with '-'
            let rel = path.strip_prefix(root).unwrap_or(&path).to_string_lossy();
            let id = rel.trim_end_matches(".desktop").replace('/', "-");
            if self.by_id.contains_key(&id) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let (name, exec, hidden) = parse_desktop(&text);
            let Some(name) = name else { continue };
            if let Some(exec) = exec
                && let Some(bin) = exec_binary(&exec)
            {
                let rank = exec_rank(&exec, hidden);
                let e = self.by_exe.entry(bin).or_insert_with(|| (name.clone(), rank));
                if rank < e.1 {
                    *e = (name.clone(), rank);
                }
            }
            self.by_id.insert(id, name);
        }
    }

    pub fn by_id(&self, id: &str) -> Option<&str> {
        self.by_id.get(id).map(String::as_str)
    }

    pub fn by_exe(&self, exe_basename: &str) -> Option<&str> {
        self.by_exe.get(exe_basename).map(|(n, _)| n.as_str())
    }
}

/// How well a desktop entry names its binary: 0 = plain launcher, 1 = launches the binary with
/// options ("xfce4-panel --add=launcher"), 2 = hidden from menus. Helper entries must not name the program.
fn exec_rank(exec: &str, hidden: bool) -> u8 {
    if hidden {
        return 2;
    }
    let mut words = exec.split_whitespace();
    let first = words.next().unwrap_or("");
    let rest: Vec<&str> = if first == "env" || first.ends_with("/env") {
        words.skip_while(|w| w.contains('=')).skip(1).collect()
    } else {
        words.collect()
    };
    if rest.iter().all(|w| w.starts_with('%')) { 0 } else { 1 }
}

fn parse_desktop(text: &str) -> (Option<String>, Option<String>, bool) {
    let mut in_entry = false;
    let (mut name, mut exec, mut hidden) = (None, None, false);
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_entry = line == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some(v) = line.strip_prefix("Name=") {
            name.get_or_insert_with(|| v.to_string());
        } else if let Some(v) = line.strip_prefix("Exec=") {
            exec.get_or_insert_with(|| v.to_string());
        } else if line == "NoDisplay=true" || line == "Hidden=true" {
            hidden = true;
        }
    }
    (name, exec, hidden)
}

fn exec_binary(exec: &str) -> Option<String> {
    let mut words = exec.split_whitespace().filter(|w| !w.contains('='));
    let mut first = words.next()?;
    if first == "env" || first.ends_with("/env") {
        first = words.next()?;
    }
    Some(basename(first.trim_matches('"')).to_string())
}

pub fn basename(p: &str) -> &str {
    p.rsplit('/').next().unwrap_or(p)
}

/// Undo systemd unit-name escaping ("\x2d" -> "-").
pub fn unescape_unit(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\'
            && i + 3 < b.len()
            && b[i + 1] == b'x'
            && let Ok(v) = u8::from_str_radix(&s[i + 2..i + 4], 16)
        {
            out.push(v as char);
            i += 4;
            continue;
        }
        out.push(b[i] as char);
        i += 1;
    }
    out
}

/// Desktop id from a KDE/systemd app unit name:
/// `app-org.kde.dolphin@<hex>.service`, `app-org.kde.yakuake-415808.scope`,
/// `app-dbus-:1.1-org.kde.kdeconnect.slice`.
pub fn app_unit_desktop_id(unit: &str) -> Option<String> {
    let u = unescape_unit(unit);
    let rest = u.strip_prefix("app-")?;
    let rest = rest
        .strip_suffix(".service")
        .or_else(|| rest.strip_suffix(".scope"))
        .or_else(|| rest.strip_suffix(".slice"))
        .unwrap_or(rest);
    let rest = match rest.find('@') {
        Some(i) => &rest[..i],
        None => rest,
    };
    let rest = if let Some(after) = rest.strip_prefix("dbus-:") {
        // "1.1-org.kde.kdeconnect"
        after.split_once('-').map(|x| x.1).unwrap_or(after)
    } else {
        rest
    };
    // trailing "-<pid>" of transient scopes
    let rest = match rest.rsplit_once('-') {
        Some((head, tail)) if !tail.is_empty() && tail.bytes().all(|c| c.is_ascii_digit()) => head,
        _ => rest,
    };
    Some(rest.to_string())
}

pub fn tilde(path: &Path) -> String {
    let s = path.to_string_lossy();
    if let Ok(home) = std::env::var("HOME")
        && let Some(rest) = s.strip_prefix(&home)
    {
        return format!("~{rest}");
    }
    s.into_owned()
}

pub fn exe_basename(p: &Proc) -> String {
    if let Some(exe) = &p.exe {
        let s = exe.to_string_lossy();
        let s = s.trim_end_matches(" (deleted)");
        return basename(s).to_string();
    }
    p.cmdline
        .first()
        .map(|c| basename(c).to_string())
        .unwrap_or_else(|| p.comm.clone())
}

const SHELLS: &[&str] = &["bash", "zsh", "fish", "sh", "dash", "nu", "elvish"];
const TERMINALS: &[&str] = &[
    "yakuake",
    "konsole",
    "xfce4-terminal",
    "kitty",
    "alacritty",
    "wezterm-gui",
    "foot",
    "gnome-terminal-server",
    "xterm",
    "tilix",
    "terminator",
    "ghostty",
    "kgx",
    "ptyxis",
    "ptyxis-agent",
    // kitty's own helpers (__atexit__, __watch_conf__, run-shell)
    "kitten",
];
pub const CHROMIUM_FAMILY: &[&str] = &[
    "vivaldi-bin",
    "chrome",
    "chromium",
    "brave",
    "google-chrome",
    "msedge",
    "opera",
    "thorium",
];

pub fn is_terminal(p: &Proc) -> bool {
    TERMINALS.contains(&exe_basename(p).as_str())
}

/// An interactive shell (no script argument) only hosts what runs inside it.
pub fn is_interactive_shell(p: &Proc) -> bool {
    let exe = exe_basename(p);
    if !SHELLS.contains(&exe.as_str()) && !SHELLS.contains(&p.comm.trim_start_matches('-')) {
        return false;
    }
    p.cmdline.iter().skip(1).all(|a| a.starts_with('-') && a != "-c")
}

/// Processes that start and hold a desktop session together; the programs below them are what counts.
const SESSION_INFRA: &[&str] = &[
    "lightdm",
    "sddm-helper",
    "gdm-session-worker",
    "xinit",
    "startx",
    "startxfce4",
    "xfce4-session",
    "startplasma-x11",
    "startplasma-wayland",
    "plasma_session",
    "gnome-session-binary",
    "gnome-session-ctl",
    "lxsession",
    "lxqt-session",
    "mate-session",
    "cinnamon-session",
    "budgie-session",
    "dbus-launch",
    "dbus-daemon",
    "dbus-broker-launch",
    "dbus-broker",
    "ssh-agent",
    "uwsm",
];

pub fn is_session_infra(p: &Proc) -> bool {
    let exe = exe_basename(p);
    // `sh -c "program"` wrappers only pass the program on
    let shell_wrapper = SHELLS.contains(&exe.as_str()) && p.cmdline.iter().any(|a| a == "-c");
    SESSION_INFRA.contains(&exe.as_str()) || shell_wrapper || is_interactive_shell(p)
}

/// A child that is part of its parent program rather than a program of its own.
pub fn same_program(parent: &Proc, child: &Proc) -> bool {
    let (pe, ce) = (exe_basename(parent), exe_basename(child));
    pe == ce || (pe == "xfce4-panel" && ce == "wrapper-2.0") || (ce == "wrapper-1.0" && pe == "xfce4-panel")
}

pub fn is_zellij_client(p: &Proc) -> bool {
    exe_basename(p) == "zellij" && !p.cmdline.iter().any(|a| a == "--server")
}

/// Session name of a zellij server, taken from its socket path.
pub fn zellij_server_session(p: &Proc) -> Option<String> {
    if exe_basename(p) != "zellij" {
        return None;
    }
    let i = p.cmdline.iter().position(|a| a == "--server")?;
    p.cmdline.get(i + 1).map(|s| basename(s).to_string())
}

pub fn is_claude(p: &Proc) -> bool {
    p.exe
        .as_ref()
        .is_some_and(|e| e.to_string_lossy().contains("/claude-code/"))
        || (p.comm == "claude" && exe_basename(p) == "claude")
}

pub fn chromium_type(p: &Proc) -> Option<&str> {
    p.cmdline.iter().find_map(|a| a.strip_prefix("--type="))
}

pub fn user_data_dir(p: &Proc) -> Option<&str> {
    p.cmdline.iter().find_map(|a| a.strip_prefix("--user-data-dir="))
}

pub fn is_chromium_browser_main(p: &Proc) -> bool {
    CHROMIUM_FAMILY.contains(&exe_basename(p).as_str()) && chromium_type(p).is_none()
}

/// A short human name for one process: the program, plus the script for interpreters.
pub fn process_name(p: &Proc) -> String {
    let exe = exe_basename(p);
    let exe_path = p
        .exe
        .as_ref()
        .map(|e| e.to_string_lossy().into_owned())
        .unwrap_or_default();

    if exe_path.contains("/.agent-browser/") {
        return "agent-browser Chrome".into();
    }
    if is_claude(p) {
        return "Claude Code".into();
    }
    if let Some(s) = zellij_server_session(p) {
        return format!("zellij: {s}");
    }
    // xfce4-panel plugins: wrapper-2.0 <plugin.so> <id> <socket> <name> <display name> <comment>
    if exe.starts_with("wrapper-") && p.cmdline.get(1).is_some_and(|a| a.contains("/panel/plugins/")) {
        let shown = p.cmdline.get(5).or(p.cmdline.get(4)).cloned().unwrap_or(exe.clone());
        return format!("{}: {shown}", crate::i18n::tr("panel", "panel"));
    }

    let interp = interpreter_kind(&exe);
    if interp.is_some()
        && let Some(argv0) = p.cmdline.first()
        && interpreter_kind(basename(argv0)).is_none()
    {
        // the program set its own title ("npm run dev"), which says more than the interpreter
        let title = p.cmdline.join(" ");
        return if title.chars().count() > 48 {
            title.chars().take(47).chain(['…']).collect()
        } else {
            title
        };
    }
    if let Some(kind) = interp
        && let Some(script) = script_arg(p, kind)
    {
        if kind == Interp::Python && (script == "main.py" || script.contains("ComfyUI")) {
            let in_comfy = p.cwd.as_ref().is_some_and(|c| c.to_string_lossy().contains("ComfyUI"))
                || p.cmdline.iter().any(|a| a.contains("ComfyUI"));
            if in_comfy {
                return "ComfyUI".into();
            }
        }
        let prog = match kind {
            Interp::Python => "python",
            Interp::Node => "node",
            Interp::Java => "java",
            Interp::Shell => exe.as_str(),
            Interp::Other => exe.as_str(),
        };
        return format!("{prog}: {script}");
    }
    exe
}

#[derive(Clone, Copy, PartialEq)]
enum Interp {
    Python,
    Node,
    Java,
    Shell,
    Other,
}

fn interpreter_kind(exe: &str) -> Option<Interp> {
    if exe.starts_with("python") || exe == "uv" {
        Some(Interp::Python)
    } else if exe == "node" || exe == "bun" || exe == "deno" {
        Some(Interp::Node)
    } else if exe == "java" {
        Some(Interp::Java)
    } else if SHELLS.contains(&exe) {
        Some(Interp::Shell)
    } else if exe.starts_with("perl") || exe.starts_with("ruby") {
        Some(Interp::Other)
    } else {
        None
    }
}

const GENERIC_SCRIPTS: &[&str] = &[
    "main.py",
    "__main__.py",
    "app.py",
    "server.py",
    "run.py",
    "index.js",
    "main.js",
    "server.js",
    "app.js",
    "cli.js",
    "index.mjs",
    "cli.mjs",
];

fn script_arg(p: &Proc, kind: Interp) -> Option<String> {
    let args = p.cmdline.get(1..)?;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match kind {
            Interp::Python if a == "-m" => return args.get(i + 1).cloned(),
            Interp::Python if a == "-c" => return None,
            Interp::Java if a == "-jar" => return args.get(i + 1).map(|j| basename(j).to_string()),
            Interp::Java if a == "-cp" || a == "-classpath" || a == "--class-path" || a == "-p" => {
                i += 2;
                continue;
            }
            Interp::Java if !a.starts_with('-') => return Some(a.rsplit('.').next().unwrap_or(a).to_string()),
            Interp::Shell if a == "-c" => return None,
            _ => {}
        }
        if kind == Interp::Python && p.cmdline.first().is_some_and(|c| basename(c) == "uv") {
            // "uv run ..." / "uv tool uvx ...": name the tool after the subcommand arguments
            return args
                .iter()
                .rev()
                .find(|a| !a.starts_with('-'))
                .map(|a| basename(a).to_string());
        }
        if !a.starts_with('-') {
            let base = basename(a);
            if GENERIC_SCRIPTS.contains(&base) {
                // a bare "main.py" tells nothing; add the directory it lives in
                let dir = Path::new(a)
                    .parent()
                    .filter(|d| !d.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .or_else(|| p.cwd.clone());
                if let Some(d) = dir.as_ref().and_then(|d| d.file_name()) {
                    return Some(format!("{base} ({})", d.to_string_lossy()));
                }
            }
            return Some(base.to_string());
        }
        i += 1;
    }
    None
}

/// `Description=` of systemd units, looked up in the unit search paths and cached.
#[derive(Default)]
pub struct UnitDescriptions {
    cache: HashMap<String, Option<String>>,
}

impl UnitDescriptions {
    /// `unit` is the escaped name as it appears in the cgroup path.
    pub fn get(&mut self, unit: &str, user: bool) -> Option<String> {
        let key = format!("{user}\t{unit}");
        if let Some(v) = self.cache.get(&key) {
            return v.clone();
        }
        let v = lookup_description(unit, user).map(|d| expand_specifiers(&d, unit));
        self.cache.insert(key, v.clone());
        v
    }
}

/// systemd's `%i`, `%I`, `%f` and friends in a template's Description; unknown ones are dropped.
fn expand_specifiers(desc: &str, unit: &str) -> String {
    let instance = unit
        .split_once('@')
        .map(|(_, rest)| rest.rsplit_once('.').map(|(i, _)| i).unwrap_or(rest))
        .unwrap_or("");
    let unescaped = unescape_unit(instance);
    let mut out = String::new();
    let mut chars = desc.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('i') => out.push_str(instance),
            Some('I') => out.push_str(&unescaped),
            Some('f') => {
                out.push('/');
                out.push_str(&unescape_unit(&instance.replace('-', "/")));
            }
            Some('%') => out.push('%'),
            _ => {}
        }
    }
    out
}

/// A logind session scope (`session-12.scope`) described from /run/systemd/sessions:
/// graphical, SSH or console, plus tty / remote host.
pub fn login_session(unit: &str) -> Option<(String, String, bool)> {
    let id = unit.strip_prefix("session-")?.strip_suffix(".scope")?;
    let text = fs::read_to_string(format!("/run/systemd/sessions/{id}")).ok()?;
    let get = |k: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(k).and_then(|v| v.strip_prefix('=')))
            .unwrap_or("")
            .to_string()
    };
    let (kind, service) = (get("TYPE"), get("SERVICE"));
    let name = if service == "sshd" {
        crate::i18n::tr("SSH session", "SSH munkamenet")
    } else if kind == "x11" || kind == "wayland" {
        crate::i18n::tr("desktop session", "grafikus munkamenet")
    } else if kind == "tty" {
        crate::i18n::tr("console session", "konzolos munkamenet")
    } else {
        crate::i18n::tr("login session", "bejelentkezési munkamenet")
    };
    let mut detail = vec![format!("#{id}")];
    let graphical = kind == "x11" || kind == "wayland";
    for part in [service, kind, get("TTY"), get("REMOTE_HOST")] {
        if !part.is_empty() && part != "unspecified" && !detail.contains(&part) {
            detail.push(part);
        }
    }
    Some((name.to_string(), detail.join(" · "), graphical))
}

fn lookup_description(unit: &str, user: bool) -> Option<String> {
    let dirs: Vec<String> = if user {
        let home = std::env::var("HOME").unwrap_or_default();
        let run = std::env::var("XDG_RUNTIME_DIR").unwrap_or_default();
        vec![
            format!("{home}/.config/systemd/user"),
            "/etc/systemd/user".into(),
            format!("{run}/systemd/transient"),
            format!("{run}/systemd/user"),
            "/usr/lib/systemd/user".into(),
        ]
    } else {
        vec![
            "/etc/systemd/system".into(),
            "/run/systemd/transient".into(),
            "/run/systemd/system".into(),
            "/usr/lib/systemd/system".into(),
        ]
    };
    let template = unit.find('@').and_then(|at| {
        let dot = unit.rfind('.')?;
        Some(format!("{}{}", &unit[..=at], &unit[dot..]))
    });
    for name in std::iter::once(unit.to_string()).chain(template) {
        for d in &dirs {
            let Ok(text) = fs::read_to_string(Path::new(d).join(&name)) else {
                continue;
            };
            if let Some(desc) = text.lines().find_map(|l| l.trim().strip_prefix("Description=")) {
                return Some(desc.trim().to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(exe: &str, cmdline: &[&str], cwd: Option<&str>) -> Proc {
        Proc {
            pid: 100,
            ppid: 1,
            comm: basename(exe).chars().take(15).collect(),
            exe: Some(PathBuf::from(exe)),
            cmdline: cmdline.iter().map(|s| s.to_string()).collect(),
            cwd: cwd.map(PathBuf::from),
            cgroup: String::new(),
            ticks: 0,
            start_time: 0,
            anon: 0,
            shmem: 0,
            swap: 0,
            io: None,
            kernel_thread: false,
        }
    }

    #[test]
    fn unescapes_unit_names() {
        assert_eq!(
            unescape_unit(r"app-vivaldi\x2dsnapshot@1.service"),
            "app-vivaldi-snapshot@1.service"
        );
        assert_eq!(unescape_unit("plain.service"), "plain.service");
    }

    #[test]
    fn desktop_ids_from_unit_names() {
        assert_eq!(
            app_unit_desktop_id("app-org.kde.dolphin@a8ce0f16.service").as_deref(),
            Some("org.kde.dolphin")
        );
        assert_eq!(
            app_unit_desktop_id("app-org.kde.yakuake-415808.scope").as_deref(),
            Some("org.kde.yakuake")
        );
        assert_eq!(
            app_unit_desktop_id(r"app-dbus\x2d:1.1\x2dorg.kde.kdeconnect.slice").as_deref(),
            Some("org.kde.kdeconnect")
        );
        assert_eq!(
            app_unit_desktop_id(r"app-vivaldi\x2dsnapshot\x2dwork@31ac.service").as_deref(),
            Some("vivaldi-snapshot-work")
        );
        assert_eq!(app_unit_desktop_id("docker-abc.scope"), None);
    }

    #[test]
    fn names_interpreters_by_script() {
        assert_eq!(
            process_name(&proc("/usr/bin/python3.12", &["python3", "-m", "http.server"], None)),
            "python: http.server"
        );
        assert_eq!(
            process_name(&proc(
                "/usr/bin/java",
                &["java", "-Xmx1G", "-jar", "/srv/app.war"],
                None
            )),
            "java: app.war"
        );
        assert_eq!(
            process_name(&proc("/usr/bin/node", &["node", "/srv/index.js"], None)),
            "node: index.js (srv)"
        );
    }

    #[test]
    fn prefers_a_self_set_title() {
        assert_eq!(
            process_name(&proc("/usr/bin/node", &["npm", "run", "dev"], None)),
            "npm run dev"
        );
    }

    #[test]
    fn recognizes_comfyui() {
        let p = proc(
            "/usr/bin/python3",
            &["python3", "main.py", "--listen"],
            Some("/opt/x/ComfyUI"),
        );
        assert_eq!(process_name(&p), "ComfyUI");
    }

    #[test]
    fn shells_with_scripts_are_not_interactive() {
        assert!(is_interactive_shell(&proc("/usr/bin/bash", &["/bin/bash"], None)));
        assert!(is_interactive_shell(&proc(
            "/usr/bin/bash",
            &["-bash", "--login"],
            None
        )));
        assert!(!is_interactive_shell(&proc(
            "/usr/bin/bash",
            &["bash", "./build.sh"],
            None
        )));
        assert!(!is_interactive_shell(&proc(
            "/usr/bin/bash",
            &["bash", "-c", "make"],
            None
        )));
    }

    #[test]
    fn helper_desktop_entries_rank_below_plain_ones() {
        assert_eq!(exec_rank("thunar %F", false), 0);
        assert_eq!(exec_rank("xfce4-panel --add=launcher %F", false), 1);
        assert_eq!(exec_rank("env FOO=1 app %U", false), 0);
        assert_eq!(exec_rank("app", true), 2);
    }

    #[test]
    fn expands_template_specifiers() {
        assert_eq!(
            expand_specifiers(
                "GnuPG network certificate management daemon for %f",
                "dirmngr@etc-pacman.d-gnupg.socket"
            ),
            "GnuPG network certificate management daemon for /etc/pacman.d/gnupg"
        );
        assert_eq!(expand_specifiers("Getty on %I", "getty@tty1.service"), "Getty on tty1");
        assert_eq!(expand_specifiers("100%% plain", "x.service"), "100% plain");
    }

    #[test]
    fn zellij_session_from_socket_path() {
        let p = proc(
            "/usr/bin/zellij",
            &["zellij", "--server", "/run/user/1000/zellij/0.1/work"],
            None,
        );
        assert_eq!(zellij_server_session(&p).as_deref(), Some("work"));
        assert!(!is_zellij_client(&p));
    }
}
