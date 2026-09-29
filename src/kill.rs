use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc::Sender;
use std::thread;

use crate::collect::my_uid;
use crate::i18n::{count, tr};
use crate::model::{Kind, Node, Target};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sig {
    Term,
    Kill,
    /// index into OTHER
    Other(usize),
}

/// Signals offered in the "other" menu: (number, name, English, Hungarian).
pub const OTHER: [(i32, &str, &str, &str); 8] = [
    (
        libc::SIGHUP,
        "SIGHUP",
        "hang up; many services reload their config",
        "bontás; sok szolgáltatás újraolvassa a konfigját",
    ),
    (
        libc::SIGINT,
        "SIGINT",
        "interrupt, like Ctrl+C",
        "megszakítás, mint a Ctrl+C",
    ),
    (
        libc::SIGQUIT,
        "SIGQUIT",
        "quit and write a core dump",
        "kilépés core dumppal",
    ),
    (
        libc::SIGSTOP,
        "SIGSTOP",
        "pause; cannot be caught or ignored",
        "felfüggesztés; nem kapható el, nem hagyható figyelmen kívül",
    ),
    (
        libc::SIGCONT,
        "SIGCONT",
        "resume a paused program",
        "felfüggesztett program folytatása",
    ),
    (
        libc::SIGTSTP,
        "SIGTSTP",
        "pause request, like Ctrl+Z",
        "felfüggesztés kérése, mint a Ctrl+Z",
    ),
    (
        libc::SIGUSR1,
        "SIGUSR1",
        "meaning defined by the program",
        "jelentését a program határozza meg",
    ),
    (
        libc::SIGUSR2,
        "SIGUSR2",
        "meaning defined by the program",
        "jelentését a program határozza meg",
    ),
];

impl Sig {
    pub fn name(self) -> &'static str {
        match self {
            Sig::Term => "SIGTERM",
            Sig::Kill => "SIGKILL",
            Sig::Other(i) => OTHER[i].1,
        }
    }

    fn signo(self) -> i32 {
        match self {
            Sig::Term => libc::SIGTERM,
            Sig::Kill => libc::SIGKILL,
            Sig::Other(i) => OTHER[i].0,
        }
    }

    /// Signals that end or interrupt a program also need SIGCONT to reach a stopped one.
    fn wakes_stopped(self) -> bool {
        matches!(
            self.signo(),
            libc::SIGTERM | libc::SIGINT | libc::SIGQUIT | libc::SIGHUP
        )
    }

    pub fn explain(self) -> &'static str {
        match self {
            Sig::Term => tr("a request, the program can still save", "kérés, a program még menthet"),
            Sig::Kill => tr("immediate, nothing is saved", "azonnali, mentés nélkül"),
            Sig::Other(i) => tr(OTHER[i].2, OTHER[i].3),
        }
    }
}

/// What a stop request would do, worked out from live /proc and cgroup state.
pub struct Plan {
    pub title: String,
    /// what kind of row it is and its detail, so a filtered selection is unmistakable
    pub what: String,
    /// how many separately started programs (targets) are hit
    pub programs: usize,
    pub pids: Vec<(u32, u64)>,
    /// cgroup paths, relative to /sys/fs/cgroup
    pub cgroups: Vec<String>,
    /// (id, name)
    pub containers: Vec<(String, String)>,
    pub forbidden: Option<String>,
    pub includes_self: bool,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.pids.is_empty() && self.containers.is_empty()
    }
}

fn start_time(pid: u32) -> Option<u64> {
    let s = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &s[s.rfind(')')? + 2..];
    rest.split(' ').nth(19)?.parse().ok()
}

fn owner(pid: u32) -> Option<u32> {
    fs::metadata(format!("/proc/{pid}")).ok().map(|m| m.uid())
}

fn cgroup_pids(dir: &Path, out: &mut Vec<u32>) {
    if let Ok(s) = fs::read_to_string(dir.join("cgroup.procs")) {
        out.extend(s.lines().filter_map(|l| l.trim().parse::<u32>().ok()));
    }
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            if e.file_type().is_ok_and(|t| t.is_dir()) {
                cgroup_pids(&e.path(), out);
            }
        }
    }
}

/// apptop itself and every process it runs inside of (shell, terminal, zellij)
fn own_ancestry() -> HashSet<u32> {
    let mut set = HashSet::new();
    let mut pid = std::process::id();
    while pid > 1 && set.insert(pid) {
        let Ok(s) = fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        let Some(close) = s.rfind(')') else { break };
        pid = s[close + 2..]
            .split(' ')
            .nth(1)
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
    }
    set
}

pub fn plan(n: &Node) -> Plan {
    let mut p = Plan {
        title: n.name.clone(),
        what: n.detail.clone(),
        programs: n.targets.len(),
        pids: Vec::new(),
        cgroups: Vec::new(),
        containers: Vec::new(),
        forbidden: None,
        includes_self: false,
    };
    if n.targets.is_empty() {
        p.forbidden = Some(match n.kind {
            Kind::Kernel => tr(
                "kernel threads cannot be stopped",
                "kernel szálakat nem lehet leállítani",
            )
            .into(),
            _ => tr(
                "this row is a summary, not a program",
                "ez a sor összesítés, nem egy program",
            )
            .into(),
        });
        return p;
    }
    let uid = my_uid();
    let user_prefix = format!("user.slice/user-{uid}.slice/");
    let mut seen = HashSet::new();
    let mut add = |pid: u32, start: Option<u64>, p: &mut Plan| {
        let Some(live) = start_time(pid) else { return };
        if start.is_some_and(|s| s != live) || !seen.insert(pid) {
            return;
        }
        if uid != 0 && owner(pid).is_some_and(|o| o != uid) && p.forbidden.is_none() {
            p.forbidden = Some(match crate::i18n::lang() {
                crate::i18n::Lang::En => format!("process {pid} belongs to another user (needs root)"),
                crate::i18n::Lang::Hu => format!("a {pid} folyamat más felhasználóé (root kell hozzá)"),
            });
        }
        p.pids.push((pid, live));
    };
    for t in &n.targets {
        match t {
            Target::Cgroup { path, extra } => {
                if uid != 0 && !path.starts_with(&user_prefix) {
                    p.forbidden = Some(format!(
                        "{} ({path}), {}",
                        tr("system service", "rendszerszolgáltatás"),
                        tr("needs root", "root kell hozzá")
                    ));
                }
                let mut pids = Vec::new();
                cgroup_pids(&Path::new("/sys/fs/cgroup").join(path), &mut pids);
                for pid in pids {
                    add(pid, None, &mut p);
                }
                for &(pid, st) in extra {
                    add(pid, Some(st), &mut p);
                }
                p.cgroups.push(path.clone());
            }
            Target::Procs(v) => {
                for &(pid, st) in v {
                    add(pid, Some(st), &mut p);
                }
            }
            Target::Docker { id, name } => p.containers.push((id.clone(), name.clone())),
        }
    }
    let own = own_ancestry();
    p.includes_self = p.pids.iter().any(|(pid, _)| own.contains(pid));
    p
}

/// Carries the request out; returns a status line. Docker runs in the background and reports
/// through `tx` when it finishes.
pub fn execute(plan: &Plan, sig: Sig, tx: Sender<(String, bool)>) -> (String, bool) {
    let mut parts = Vec::new();
    let mut failed = false;
    if !plan.containers.is_empty() {
        let verb = if sig == Sig::Term { "stop" } else { "kill" };
        let mut args = vec![verb.to_string()];
        if let Sig::Other(_) = sig {
            args.push(format!("--signal={}", sig.name()));
        }
        let ids: Vec<String> = plan.containers.iter().map(|(id, _)| id.clone()).collect();
        let n = ids.len();
        thread::spawn(move || {
            let msg = match Command::new("docker").args(&args).args(&ids).output() {
                Ok(o) if o.status.success() => (
                    format!(
                        "docker {verb}: {} {}",
                        count(n, "container", "containers", "konténer"),
                        tr("done", "kész")
                    ),
                    false,
                ),
                Ok(o) => (
                    format!("docker {verb}: {}", String::from_utf8_lossy(&o.stderr).trim()),
                    true,
                ),
                Err(e) => (format!("docker {verb}: {e}"), true),
            };
            let _ = tx.send(msg);
        });
        parts.push(format!(
            "docker {verb} ({})…",
            count(n, "container", "containers", "konténer")
        ));
    }
    if !plan.pids.is_empty() {
        let (mut ok, mut gone, mut denied) = (0, 0, 0);
        let mut handled: HashSet<u32> = HashSet::new();
        if sig == Sig::Kill {
            for cg in &plan.cgroups {
                let dir = Path::new("/sys/fs/cgroup").join(cg);
                let mut pids = Vec::new();
                cgroup_pids(&dir, &mut pids);
                if fs::write(dir.join("cgroup.kill"), "1").is_ok() {
                    ok += pids.len();
                    handled.extend(pids);
                }
            }
        }
        let signo = sig.signo();
        for &(pid, st) in &plan.pids {
            if handled.contains(&pid) {
                continue;
            }
            if start_time(pid) != Some(st) {
                gone += 1;
                continue;
            }
            // SAFETY: plain kill(2) on a pid whose start time was just verified.
            let r = unsafe { libc::kill(pid as i32, signo) };
            if r == 0 && sig.wakes_stopped() {
                // a stopped process only acts on the signal once it runs again (systemd does the same)
                // SAFETY: as above.
                unsafe { libc::kill(pid as i32, libc::SIGCONT) };
            }
            if r == 0 {
                ok += 1;
            } else if std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM) {
                denied += 1;
            } else {
                gone += 1;
            }
        }
        let mut s = format!("{}: {}", sig.name(), count(ok, "process", "processes", "folyamat"));
        if gone > 0 {
            s.push_str(&format!(", {gone} {}", tr("no longer running", "már nem futott")));
        }
        if denied > 0 {
            s.push_str(&format!(
                ", {denied} {}",
                tr("skipped, permission denied", "jogosultság hiányában kimaradt")
            ));
            failed = true;
        }
        parts.push(s);
    }
    (parts.join("; "), failed)
}
