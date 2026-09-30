use std::collections::{BTreeMap, HashMap, HashSet};

use crate::collect::{CgStat, Proc, Snapshot};
use crate::docker::DockerNames;
use crate::gpu::GpuSample;
use crate::i18n::{count, tr};
use crate::names::{self, DesktopIndex, UnitDescriptions};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    App,
    Terminal,
    Browser,
    Docker,
    UserService,
    SystemService,
    Kernel,
    /// something started from a terminal
    Job,
    Group,
    Proc,
    Rest,
}

/// What stopping a row acts on.
#[derive(Clone, Debug)]
pub enum Target {
    /// every process in the cgroup, plus processes attributed to it from elsewhere
    Cgroup {
        path: String,
        extra: Vec<(u32, u64)>,
    },
    /// (pid, start time) so a reused pid is never hit
    Procs(Vec<(u32, u64)>),
    Docker {
        id: String,
        name: String,
    },
}

#[derive(Clone, Debug)]
pub struct Node {
    pub key: String,
    pub name: String,
    pub detail: String,
    /// part of the key instead of `detail` when the detail changes over time ("3 instances")
    pub ident: Option<String>,
    pub kind: Kind,
    pub cpu: f64,
    pub mem: u64,
    pub swap: u64,
    pub cache: Option<u64>,
    pub vram: u64,
    pub procs: usize,
    /// how many identical siblings this row stands for
    pub count: usize,
    pub pids: Vec<(u32, u64)>,
    pub targets: Vec<Target>,
    /// memory pressure (PSI some avg10, %) of the cgroup
    pub psi: Option<f64>,
    /// storage bytes per second
    pub io_read: f64,
    pub io_write: f64,
    /// GPU SM utilization %, summed over processes
    pub gpu: f64,
    /// memory change over the trend window, and whether the window is fully covered
    pub delta: Option<(i64, bool)>,
    pub children: Vec<Node>,
}

impl Node {
    pub(crate) fn new(name: impl Into<String>, detail: impl Into<String>, kind: Kind) -> Self {
        Node {
            key: String::new(),
            name: name.into(),
            detail: detail.into(),
            kind,
            cpu: 0.0,
            mem: 0,
            swap: 0,
            cache: None,
            vram: 0,
            procs: 0,
            count: 1,
            ident: None,
            pids: Vec::new(),
            targets: Vec::new(),
            psi: None,
            io_read: 0.0,
            io_write: 0.0,
            gpu: 0.0,
            delta: None,
            children: Vec::new(),
        }
    }

    fn add_values(&mut self, o: &Node) {
        self.cpu += o.cpu;
        self.mem += o.mem;
        self.swap += o.swap;
        self.vram += o.vram;
        self.procs += o.procs;
        if let Some(c) = o.cache {
            self.cache = Some(self.cache.unwrap_or(0) + c);
        }
        self.io_read += o.io_read;
        self.io_write += o.io_write;
        self.gpu += o.gpu;
        // pressure is a share of time, not additive: the worst member speaks for the group
        if let Some(p) = o.psi {
            self.psi = Some(self.psi.map_or(p, |q| q.max(p)));
        }
        self.pids.extend(&o.pids);
        self.targets.extend(o.targets.iter().cloned());
    }

    fn sub_values(&mut self, o: &Node) {
        self.cpu = (self.cpu - o.cpu).max(0.0);
        self.mem = self.mem.saturating_sub(o.mem);
        self.swap = self.swap.saturating_sub(o.swap);
        self.vram = self.vram.saturating_sub(o.vram);
        self.procs = self.procs.saturating_sub(o.procs);
        self.io_read = (self.io_read - o.io_read).max(0.0);
        self.io_write = (self.io_write - o.io_write).max(0.0);
        self.gpu = (self.gpu - o.gpu).max(0.0);
        let gone: HashSet<(u32, u64)> = o.pids.iter().copied().collect();
        self.pids.retain(|p| !gone.contains(p));
    }

    pub(crate) fn set_keys(&mut self, parent: &str) {
        self.key = format!(
            "{parent}/{}\t{}",
            self.name,
            self.ident.as_deref().unwrap_or(&self.detail)
        );
        let k = self.key.clone();
        for c in &mut self.children {
            c.set_keys(&k);
        }
    }
}

pub struct Summary {
    pub swap_in: f64,
    pub swap_out: f64,
    pub mem_psi: Option<f64>,
    pub cpu_pct: f64,
    pub ncpu: usize,
    pub mem_used: u64,
    pub mem_total: u64,
    pub swap_used: u64,
    pub swap_total: u64,
    pub gpu: Option<GpuSample>,
}

pub struct Model {
    pub roots: Vec<Node>,
    pub summary: Summary,
}

pub struct Ctx<'a> {
    pub cur: &'a Snapshot,
    pub prev: Option<&'a Snapshot>,
    pub desktop: &'a DesktopIndex,
    pub unit_desc: &'a mut UnitDescriptions,
    pub docker: &'a mut DockerNames,
    pub gpu: Option<GpuSample>,
    pub clk_tck: u64,
    /// false: programs started in terminals become top-level rows of their own
    pub split_terminals: bool,
}

/// Where a cgroup path stops being structure (slices) and becomes one program (a unit).
fn unit_path(cg: &str) -> String {
    if cg.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = cg.split('/').collect();
    for (i, c) in parts.iter().enumerate() {
        let is_user_manager = c.starts_with("user@") && c.ends_with(".service");
        let is_app_slice = c.starts_with("app-") && c.ends_with(".slice") && i > 0 && parts[i - 1] == "app.slice";
        if (c.ends_with(".service") || c.ends_with(".scope")) && !is_user_manager || is_app_slice {
            return parts[..=i].join("/");
        }
    }
    cg.to_string()
}

struct Unit {
    path: String,
    name: String,
    pids: Vec<u32>,
    stat: Option<CgStat>,
    prev_usage: Option<u64>,
    terminal: bool,
    /// processes attributed to this unit although they sit in another cgroup (or the reverse)
    moved_mem: i64,
    moved_swap: i64,
    moved_cpu: f64,
    moved_in: Vec<u32>,
}

/// A top-level candidate before rows with the same name are merged.
struct Item {
    group: String,
    kind: Kind,
    node: Node,
    /// what tells this instance apart from others in the same group
    instance: String,
    profile_key: Option<String>,
}

const TERMINAL_IDS: &[&str] = &[
    "org.kde.yakuake",
    "org.kde.konsole",
    "xfce4-terminal",
    "kitty",
    "Alacritty",
    "foot",
];

struct Acc<'a> {
    cur: &'a Snapshot,
    desktop: &'a DesktopIndex,
    proc_cpu: HashMap<u32, f64>,
    /// storage (read, write) bytes per second
    proc_io: HashMap<u32, (f64, f64)>,
    gpu_util: HashMap<u32, u32>,
    vram: HashMap<u32, u64>,
    children_of: HashMap<u32, Vec<u32>>,
}

impl Acc<'_> {
    fn add(&self, node: &mut Node, pid: u32) {
        let p = &self.cur.procs[&pid];
        node.mem += p.anon + p.shmem;
        node.swap += p.swap;
        node.cpu += self.proc_cpu[&pid];
        node.vram += self.vram.get(&pid).copied().unwrap_or(0);
        node.procs += 1;
        let (r, w) = self.proc_io.get(&pid).copied().unwrap_or_default();
        node.io_read += r;
        node.io_write += w;
        node.gpu += self.gpu_util.get(&pid).copied().unwrap_or(0) as f64;
        node.pids.push((pid, p.start_time));
    }

    fn start(&self, pid: u32) -> (u32, u64) {
        (pid, self.cur.procs[&pid].start_time)
    }

    fn kids(&self, pid: u32, set: &HashSet<u32>) -> Vec<u32> {
        self.children_of
            .get(&pid)
            .map(|v| v.iter().copied().filter(|c| set.contains(c)).collect())
            .unwrap_or_default()
    }
}

pub fn build(mut ctx: Ctx) -> Model {
    let cur = ctx.cur;
    let elapsed = ctx
        .prev
        .map(|p| cur.taken.duration_since(p.taken).as_secs_f64())
        .unwrap_or(0.0);
    let proc_cpu: HashMap<u32, f64> = cur
        .procs
        .values()
        .map(|p| {
            let prev = ctx
                .prev
                .and_then(|s| s.procs.get(&p.pid))
                .filter(|q| q.start_time == p.start_time);
            let pct = match prev {
                Some(q) if elapsed > 0.0 => {
                    p.ticks.saturating_sub(q.ticks) as f64 / ctx.clk_tck as f64 / elapsed * 100.0
                }
                _ => 0.0,
            };
            (p.pid, pct)
        })
        .collect();
    let proc_io: HashMap<u32, (f64, f64)> = cur
        .procs
        .values()
        .filter_map(|p| {
            let (r, w) = p.io?;
            let q = ctx
                .prev
                .and_then(|s| s.procs.get(&p.pid))
                .filter(|q| q.start_time == p.start_time)?;
            let (qr, qw) = q.io?;
            (elapsed > 0.0).then(|| {
                (
                    p.pid,
                    (
                        r.saturating_sub(qr) as f64 / elapsed,
                        w.saturating_sub(qw) as f64 / elapsed,
                    ),
                )
            })
        })
        .collect();
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for p in cur.procs.values() {
        children_of.entry(p.ppid).or_default().push(p.pid);
    }
    let acc = Acc {
        cur,
        desktop: ctx.desktop,
        proc_cpu,
        proc_io,
        gpu_util: ctx.gpu.as_ref().map(|g| g.util_per_pid.clone()).unwrap_or_default(),
        vram: ctx.gpu.as_ref().map(|g| g.per_pid.clone()).unwrap_or_default(),
        children_of,
    };

    let mut units = collect_units(&ctx, &acc);

    let container_ids: Vec<&str> = units.keys().filter_map(|p| docker_id(p)).collect();
    ctx.docker.ensure(&container_ids);

    let mut items: Vec<Item> = Vec::new();
    // profile key -> (name, from a desktop entry); desktop names win over names made from the main process
    let mut profile_names: HashMap<String, (String, bool)> = HashMap::new();
    for u in units.values_mut() {
        if u.pids.is_empty() {
            continue;
        }
        u.pids.sort_unstable();
        unit_items(u, &mut ctx, &acc, elapsed, &mut items, &mut profile_names);
    }

    let mut groups: BTreeMap<String, Vec<Item>> = BTreeMap::new();
    for mut it in items {
        if let Some((n, _)) = it.profile_key.as_ref().and_then(|k| profile_names.get(k)) {
            it.group = n.clone();
        }
        groups.entry(it.group.clone()).or_default().push(it);
    }
    let mut roots: Vec<Node> = groups
        .into_iter()
        .map(|(name, items)| group_node(name, items))
        .collect();
    for r in &mut roots {
        add_process_level(r, &acc);
        r.set_keys("");
    }

    let (busy, total) = cur.cpu_jiffies;
    let cpu_pct = match ctx.prev {
        Some(p) if total > p.cpu_jiffies.1 => {
            busy.saturating_sub(p.cpu_jiffies.0) as f64 / (total - p.cpu_jiffies.1) as f64 * 100.0
        }
        _ => 0.0,
    };
    let m = &cur.meminfo;
    let page = crate::collect::page_size() as f64;
    let (swap_in, swap_out) = match ctx.prev {
        Some(p) if elapsed > 0.0 => (
            cur.pswp.0.saturating_sub(p.pswp.0) as f64 * page / elapsed,
            cur.pswp.1.saturating_sub(p.pswp.1) as f64 * page / elapsed,
        ),
        _ => (0.0, 0.0),
    };
    Model {
        roots,
        summary: Summary {
            swap_in,
            swap_out,
            mem_psi: cur.mem_psi,
            cpu_pct,
            ncpu: cur.ncpu,
            mem_used: m.total.saturating_sub(m.available),
            mem_total: m.total,
            swap_used: m.swap_total.saturating_sub(m.swap_free),
            swap_total: m.swap_total,
            gpu: ctx.gpu,
        },
    }
}

fn collect_units(ctx: &Ctx, acc: &Acc) -> BTreeMap<String, Unit> {
    let cur = ctx.cur;
    let mut units: BTreeMap<String, Unit> = BTreeMap::new();
    let mut unit_of: HashMap<u32, String> = HashMap::new();
    for p in cur.procs.values() {
        let up = unit_path(&p.cgroup);
        unit_of.insert(p.pid, up.clone());
        units
            .entry(up.clone())
            .or_insert_with(|| Unit {
                name: up.rsplit('/').next().unwrap_or("").to_string(),
                stat: if up.is_empty() {
                    None
                } else {
                    cur.cgroups.get(&up).cloned()
                },
                prev_usage: ctx.prev.and_then(|s| s.cgroups.get(&up)).map(|c| c.usage_usec),
                path: up,
                pids: Vec::new(),
                terminal: false,
                moved_mem: 0,
                moved_swap: 0,
                moved_cpu: 0.0,
                moved_in: Vec::new(),
            })
            .pids
            .push(p.pid);
    }
    for u in units.values_mut() {
        let id = names::app_unit_desktop_id(&u.name);
        u.terminal = u.name.starts_with("vte-spawn-")
            || id.as_deref().is_some_and(|i| TERMINAL_IDS.contains(&i))
            || u.pids.iter().any(|pid| names::is_terminal(&cur.procs[pid]));
    }

    // A program started from a terminal may move its main process into its own scope
    // (Chromium does) while its children stay in the terminal's cgroup. Those children
    // belong to the program, not to the terminal.
    let terminal_units: Vec<String> = units.values().filter(|u| u.terminal).map(|u| u.path.clone()).collect();
    for tu in terminal_units {
        let pids = units[&tu].pids.clone();
        let in_unit: HashSet<u32> = pids.iter().copied().collect();
        let mut moved: HashSet<u32> = HashSet::new();
        for &pid in &pids {
            let p = &cur.procs[&pid];
            if in_unit.contains(&p.ppid) {
                continue;
            }
            let Some(parent) = cur.procs.get(&p.ppid) else { continue };
            let Some(pu) = unit_of.get(&parent.pid).cloned() else {
                continue;
            };
            if pu == tu || units[&pu].terminal || parent.pid == 1 || parent.comm == "systemd" {
                continue;
            }
            let mut stack = vec![pid];
            while let Some(x) = stack.pop() {
                if !in_unit.contains(&x) || !moved.insert(x) {
                    continue;
                }
                let q = &cur.procs[&x];
                let (mem, swap, cpu) = ((q.anon + q.shmem) as i64, q.swap as i64, acc.proc_cpu[&x]);
                let from = units.get_mut(&tu).unwrap();
                from.moved_mem -= mem;
                from.moved_swap -= swap;
                from.moved_cpu -= cpu;
                let to = units.get_mut(&pu).unwrap();
                to.pids.push(x);
                to.moved_in.push(x);
                to.moved_mem += mem;
                to.moved_swap += swap;
                to.moved_cpu += cpu;
                if let Some(cs) = acc.children_of.get(&x) {
                    stack.extend(cs);
                }
            }
        }
        units.get_mut(&tu).unwrap().pids.retain(|p| !moved.contains(p));
    }
    units
}

/// Values of a whole unit: from its cgroup when there is one, else summed over its processes.
fn unit_values(u: &Unit, acc: &Acc, elapsed: f64) -> Node {
    let mut node = Node::new("", "", Kind::App);
    node.procs = u.pids.len();
    node.vram = u.pids.iter().map(|p| acc.vram.get(p).copied().unwrap_or(0)).sum();
    for &p in &u.pids {
        let (r, w) = acc.proc_io.get(&p).copied().unwrap_or_default();
        node.io_read += r;
        node.io_write += w;
        node.gpu += acc.gpu_util.get(&p).copied().unwrap_or(0) as f64;
        node.pids.push(acc.start(p));
    }
    match &u.stat {
        Some(st) => {
            node.psi = st.mem_psi;
            node.mem = (st.resident() as i64 + u.moved_mem).max(0) as u64;
            node.swap = (st.swap as i64 + u.moved_swap).max(0) as u64;
            node.cache = Some(st.cache());
            node.cpu = match u.prev_usage {
                Some(prev) if elapsed > 0.0 => {
                    (st.usage_usec.saturating_sub(prev) as f64 / 1e6 / elapsed * 100.0 + u.moved_cpu).max(0.0)
                }
                _ => 0.0,
            };
        }
        None => {
            node.pids.clear();
            node.io_read = 0.0;
            node.io_write = 0.0;
            node.gpu = 0.0;
            for &p in &u.pids {
                acc.add(&mut node, p);
            }
            node.procs = u.pids.len();
        }
    }
    node
}

fn browser_profile_key(procs: &[&Proc], cur: &Snapshot) -> Option<String> {
    for p in procs {
        if !names::CHROMIUM_FAMILY.contains(&names::exe_basename(p).as_str()) {
            continue;
        }
        let mut q = *p;
        for _ in 0..6 {
            if names::is_chromium_browser_main(q) {
                return Some(format!(
                    "{}\t{}",
                    names::exe_basename(q),
                    names::user_data_dir(q).unwrap_or("")
                ));
            }
            q = cur.procs.get(&q.ppid)?;
        }
    }
    None
}

fn unit_items(
    u: &Unit,
    ctx: &mut Ctx,
    acc: &Acc,
    elapsed: f64,
    items: &mut Vec<Item>,
    profile_names: &mut HashMap<String, (String, bool)>,
) {
    let cur = acc.cur;
    let procs: Vec<&Proc> = u.pids.iter().map(|pid| &cur.procs[pid]).collect();
    let set: HashSet<u32> = u.pids.iter().copied().collect();
    let main = main_process(&procs, &set, acc);
    let desktop_name = names::app_unit_desktop_id(&u.name)
        .and_then(|id| ctx.desktop.by_id(&id))
        .map(String::from);
    let by_exe = main
        .and_then(|m| ctx.desktop.by_exe(&names::exe_basename(m)))
        .map(String::from);
    let is_app = u.path.contains("/app.slice/");
    let is_user = u.path.starts_with("user.slice/");

    let mut node = unit_values(u, acc, elapsed);
    let mut instance = main.map(instance_label).unwrap_or_default();
    let mut profile_key = None;
    let mut jobs: Vec<Node> = Vec::new();

    let (group, kind) = if u.path.is_empty() {
        ("Kernel".to_string(), Kind::Kernel)
    } else if let Some(id) = docker_id(&u.path) {
        let c = ctx.docker.get(id);
        let cname = c
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("docker {}", &id[..12]));
        instance = cname.clone();
        node.detail = cname.clone();
        match c.and_then(|c| c.project.clone()) {
            Some(p) => (format!("Docker: {p}"), Kind::Docker),
            None => (cname, Kind::Docker),
        }
    } else if u.terminal {
        let name = desktop_name
            .or(by_exe)
            .unwrap_or_else(|| tr("terminal", "terminál").into());
        (name, Kind::Terminal)
    } else if let Some(key) = browser_profile_key(&procs, cur) {
        if let Some(n) = &desktop_name {
            let e = profile_names.entry(key.clone()).or_insert_with(|| (n.clone(), true));
            if !e.1 {
                *e = (n.clone(), true);
            }
        }
        let bm = procs.iter().find(|p| names::is_chromium_browser_main(p));
        let fallback = match bm {
            Some(bm) => {
                let n = ctx
                    .desktop
                    .by_exe(&names::exe_basename(bm))
                    .map(String::from)
                    .unwrap_or_else(|| names::process_name(bm));
                match names::user_data_dir(bm) {
                    Some(d) if n != "agent-browser Chrome" => format!("{n} ({})", names::basename(d)),
                    _ => n,
                }
            }
            None => desktop_name.clone().unwrap_or_else(|| tr("browser", "böngésző").into()),
        };
        if bm.is_some() {
            // renderers left in another cgroup take the name of the unit that holds the main process
            profile_names
                .entry(key.clone())
                .or_insert_with(|| (fallback.clone(), false));
        }
        profile_key = Some(key);
        (fallback, Kind::Browser)
    } else if is_app {
        let name = desktop_name
            .or(by_exe)
            .or_else(|| main.map(names::process_name))
            .unwrap_or_else(|| service_name(&u.name));
        (name, Kind::App)
    } else if let Some((name, detail)) = names::login_session(&u.name) {
        instance = detail.clone();
        node.detail = detail;
        (name, Kind::SystemService)
    } else if u.name == "init.scope" {
        (
            if is_user {
                "systemd (user)".into()
            } else {
                "systemd".into()
            },
            Kind::SystemService,
        )
    } else {
        node.detail = ctx
            .unit_desc
            .get(&u.name, is_user && u.path.contains("/user@"))
            .unwrap_or_default();
        (
            service_name(&u.name),
            if is_user {
                Kind::UserService
            } else {
                Kind::SystemService
            },
        )
    };
    node.kind = kind;
    node.targets = match kind {
        Kind::Kernel => Vec::new(),
        Kind::Docker => match docker_id(&u.path) {
            Some(id) => vec![Target::Docker {
                id: id.to_string(),
                name: node.detail.clone(),
            }],
            None => Vec::new(),
        },
        _ => vec![Target::Cgroup {
            path: u.path.clone(),
            extra: u.moved_in.iter().map(|&p| acc.start(p)).collect(),
        }],
    };

    let mut rows = match kind {
        Kind::Terminal => {
            let (rows, js) = terminal_breakdown(&procs, &set, acc, ctx.split_terminals);
            for j in &js {
                node.sub_values(j);
            }
            jobs = js;
            rows
        }
        Kind::Kernel => label_rows(&procs, acc, |p| {
            p.comm.split(['/', ':', '-']).next().unwrap_or(&p.comm).to_string()
        }),
        Kind::Browser => label_rows(&procs, acc, |p| {
            if names::CHROMIUM_FAMILY.contains(&names::exe_basename(p).as_str()) {
                chromium_label(p)
            } else {
                generic_label(p)
            }
        }),
        _ => label_rows(&procs, acc, generic_label),
    };
    if rows.len() > 1 || rows.iter().any(|r| !r.children.is_empty()) || kind == Kind::Terminal {
        if node.cache.is_some() {
            push_rest(&node, &mut rows, u.stat.as_ref());
        }
        node.children = rows;
    }

    for j in jobs {
        items.push(Item {
            group: j.name.clone(),
            kind: Kind::Job,
            instance: j.detail.clone(),
            node: j,
            profile_key: None,
        });
    }
    items.push(Item {
        group,
        kind,
        node,
        instance,
        profile_key,
    });
}

/// cgroup totals include kernel memory, shared memory and exited children;
/// one row holds the part no listed process accounts for.
fn push_rest(total: &Node, rows: &mut Vec<Node>, stat: Option<&CgStat>) {
    let mem: u64 = rows.iter().map(|r| r.mem).sum();
    let cpu: f64 = rows.iter().map(|r| r.cpu).sum();
    let swap: u64 = rows.iter().map(|r| r.swap).sum();
    let mut rest = Node::new(
        tr("other", "egyéb"),
        tr(
            "kernel, shared memory, exited processes",
            "kernel, megosztott memória, kilépett folyamatok",
        ),
        Kind::Rest,
    );
    rest.mem = total.mem.saturating_sub(mem);
    rest.cpu = (total.cpu - cpu).max(0.0);
    rest.swap = total.swap.saturating_sub(swap);
    if let Some(st) = stat {
        rest.children = rest_parts(&rest, st);
    }
    if rest.mem > 1 << 20 || rest.cpu >= 0.1 || rest.swap > 1 << 20 {
        rows.push(rest);
    }
}

/// What the "other" row is made of, from the cgroup's memory.stat. The kernel parts are exact;
/// whatever they do not explain lands in "unattributed".
fn rest_parts(rest: &Node, st: &CgStat) -> Vec<Node> {
    let mut parts = Vec::new();
    let mut part = |name: &'static str, detail: &'static str, mem: u64, swap: u64| {
        let mut n = Node::new(name, detail, Kind::Rest);
        n.mem = mem;
        n.swap = swap;
        if mem > 1 << 20 || swap > 1 << 20 {
            parts.push(n);
        }
    };
    let kernel = st.kernel.saturating_sub(st.slab_reclaimable);
    let known = st.zswap + st.pagetables + st.kernel_stack + st.slab_unreclaimable;
    part(
        tr("zswap", "zswap"),
        tr("compressed swap kept in RAM", "tömörített swap a RAM-ban"),
        st.zswap,
        0,
    );
    part(tr("page tables", "laptáblák"), "", st.pagetables, 0);
    part(
        tr("slab", "slab"),
        tr(
            "kernel objects that cannot be reclaimed",
            "nem visszavehető kernel objektumok",
        ),
        st.slab_unreclaimable,
        0,
    );
    part(tr("kernel stacks", "kernel stackek"), "", st.kernel_stack, 0);
    part(
        tr("other kernel", "egyéb kernel"),
        tr("socket buffers, percpu, vmalloc", "socket pufferek, percpu, vmalloc"),
        kernel.saturating_sub(known),
        0,
    );
    let swapcache = st.swapcached.min(rest.swap);
    part(
        tr("swap cache", "swap cache"),
        tr(
            "read back into RAM, slot kept; also counted in Memory",
            "visszaolvasva a RAM-ba, a swap-hely megmaradt; a Memóriában is számít",
        ),
        0,
        swapcache,
    );
    part(
        tr("unattributed", "besorolatlan"),
        tr(
            "left by exited or moved processes, unmapped shared memory",
            "kilépett vagy átköltözött folyamatoktól, nem leképezett megosztott memória",
        ),
        rest.mem.saturating_sub(kernel),
        rest.swap - swapcache,
    );
    parts
}

fn group_node(name: String, mut items: Vec<Item>) -> Node {
    let kind = items
        .iter()
        .map(|i| i.kind)
        .find(|k| *k == Kind::Terminal)
        .unwrap_or(items[0].kind);
    if items.len() == 1 {
        let it = items.pop().unwrap();
        let mut n = it.node;
        if n.kind == Kind::Docker && n.detail == name {
            n.detail.clear();
        }
        n.name = name;
        return n;
    }
    let mut node = Node::new(name, "", kind);
    for it in &items {
        node.add_values(&it.node);
    }
    match kind {
        Kind::Browser => {
            // several cgroups of one browser profile: one combined breakdown
            node.children = merge_same(items.into_iter().flat_map(|i| i.node.children).collect());
        }
        Kind::Job => {
            node.count = items.len();
            let rows = items
                .into_iter()
                .map(|i| {
                    let mut n = i.node;
                    n.name = if i.instance.is_empty() { n.name } else { i.instance };
                    n.detail.clear();
                    n
                })
                .collect();
            node.children = merge_same(rows);
        }
        _ => {
            node.detail = if kind == Kind::Docker {
                count(items.len(), "container", "containers", "konténer")
            } else {
                count(items.len(), "instance", "instances", "példány")
            };
            node.ident = Some(String::new());
            node.children = items
                .into_iter()
                .map(|i| {
                    let mut n = i.node;
                    n.name = if i.instance.is_empty() { n.name } else { i.instance };
                    n.detail.clear();
                    n
                })
                .collect();
        }
    }
    node
}

fn service_name(unit: &str) -> String {
    let u = names::unescape_unit(unit);
    let u = u
        .strip_suffix(".service")
        .or_else(|| u.strip_suffix(".scope"))
        .unwrap_or(&u);
    let u = u.strip_prefix("app-").unwrap_or(u);
    let u = u.strip_prefix("plasma-").unwrap_or(u);
    // drop random instance ids like "@3b3ffbc8999742bd9e7534c07f7de661"
    match u.split_once('@') {
        Some((head, inst)) if inst.len() >= 16 && inst.bytes().all(|c| c.is_ascii_hexdigit()) => head.to_string(),
        _ => u.to_string(),
    }
}

fn docker_id(path: &str) -> Option<&str> {
    let last = path.rsplit('/').next()?;
    let id = last.strip_prefix("docker-")?.strip_suffix(".scope")?;
    (id.len() == 64).then_some(id)
}

/// The root process (parent outside the unit) with the most memory beneath it.
fn main_process<'a>(procs: &[&'a Proc], set: &HashSet<u32>, acc: &Acc) -> Option<&'a Proc> {
    procs
        .iter()
        .filter(|p| !set.contains(&p.ppid) && !p.kernel_thread)
        .max_by_key(|p| (subtree_mem(p.pid, set, acc), std::cmp::Reverse(p.pid)))
        .copied()
}

fn subtree(pid: u32, set: &HashSet<u32>, acc: &Acc) -> Vec<u32> {
    let mut out = Vec::new();
    let mut stack = vec![pid];
    while let Some(x) = stack.pop() {
        if set.contains(&x) {
            out.push(x);
            stack.extend(acc.kids(x, set));
        }
    }
    out
}

fn subtree_mem(pid: u32, set: &HashSet<u32>, acc: &Acc) -> u64 {
    subtree(pid, set, acc)
        .iter()
        .map(|x| acc.cur.procs[x].anon + acc.cur.procs[x].shmem)
        .sum()
}

fn instance_label(p: &Proc) -> String {
    let args: Vec<&str> = p.cmdline.iter().skip(1).map(String::as_str).collect();
    let s = if args.is_empty() {
        p.cwd.as_deref().map(names::tilde).unwrap_or_default()
    } else {
        let s = args.join(" ");
        match std::env::var("HOME") {
            Ok(home) => s.replace(&home, "~"),
            Err(_) => s,
        }
    };
    if s.is_empty() {
        format!("PID {}", p.pid)
    } else {
        format!("{s}  (PID {})", p.pid)
    }
}

fn label_rows(procs: &[&Proc], acc: &Acc, label: impl Fn(&Proc) -> String) -> Vec<Node> {
    let rows = procs
        .iter()
        .map(|p| {
            let mut n = Node::new(label(p), "", Kind::Proc);
            acc.add(&mut n, p.pid);
            n.targets = vec![Target::Procs(n.pids.clone())];
            n
        })
        .collect();
    merge_same(rows)
}

/// Rows for a terminal, and the jobs to lift out to the top level when not splitting.
fn terminal_breakdown(procs: &[&Proc], set: &HashSet<u32>, acc: &Acc, split: bool) -> (Vec<Node>, Vec<Node>) {
    let mut rows = Vec::new();
    let mut jobs = Vec::new();
    let mut overhead = Node::new(tr("terminal, shells", "terminál, shellek"), "", Kind::Rest);
    for p in procs.iter().filter(|p| !set.contains(&p.ppid)) {
        terminal_walk(p.pid, set, acc, split, &mut rows, &mut jobs, &mut overhead);
    }
    let mut rows = merge_same(rows);
    if overhead.procs > 0 {
        rows.push(overhead);
    }
    (rows, jobs)
}

fn terminal_walk(
    pid: u32,
    set: &HashSet<u32>,
    acc: &Acc,
    split: bool,
    rows: &mut Vec<Node>,
    jobs: &mut Vec<Node>,
    overhead: &mut Node,
) {
    let p = &acc.cur.procs[&pid];
    let kids = acc.kids(pid, set);
    if names::is_terminal(p) || names::is_interactive_shell(p) || names::is_zellij_client(p) {
        acc.add(overhead, pid);
        for c in kids {
            terminal_walk(c, set, acc, split, rows, jobs, overhead);
        }
        return;
    }
    if let Some(session) = names::zellij_server_session(p) {
        if !split {
            acc.add(overhead, pid);
            for c in kids {
                terminal_walk(c, set, acc, split, rows, jobs, overhead);
            }
            return;
        }
        let mut group = Node::new(format!("zellij: {session}"), "", Kind::Group);
        let mut inner_rows = Vec::new();
        let mut inner = Node::new(tr("zellij, shells", "zellij, shellek"), "", Kind::Rest);
        acc.add(&mut inner, pid);
        for c in kids {
            terminal_walk(c, set, acc, split, &mut inner_rows, jobs, &mut inner);
        }
        let mut inner_rows = merge_same(inner_rows);
        inner_rows.push(inner);
        for r in &inner_rows {
            group.add_values(r);
        }
        // stopping a session means its server, shells and everything in them
        group.targets = vec![Target::Procs(group.pids.clone())];
        group.children = inner_rows;
        rows.push(group);
        return;
    }
    let job = job_node(pid, set, acc);
    if split { rows.push(job) } else { jobs.push(job) }
}

/// Everything below a process started in a terminal, named after its most telling member.
fn job_node(pid: u32, set: &HashSet<u32>, acc: &Acc) -> Node {
    let procs = &acc.cur.procs;
    let mut members = subtree(pid, set, acc);
    // breadth-first so the outermost match wins
    members.sort_by_key(|x| depth_below(*x, pid, procs));
    let notable = members
        .iter()
        .map(|x| &procs[x])
        .find(|p| names::is_claude(p) || names::process_name(p) == "ComfyUI")
        .unwrap_or(&procs[&pid]);
    let detail = notable.cwd.as_deref().map(names::tilde).unwrap_or_default();
    let mut name = names::process_name(notable);
    if name == names::exe_basename(notable)
        && let Some(d) = acc.desktop.by_exe(&name)
    {
        // a GUI app started from a shell joins the row of the same app started from the menu
        name = d.to_string();
    }
    let mut n = Node::new(name, detail, Kind::Job);
    for x in members {
        acc.add(&mut n, x);
    }
    n.targets = vec![Target::Procs(n.pids.clone())];
    n
}

fn depth_below(pid: u32, root: u32, procs: &HashMap<u32, Proc>) -> usize {
    let mut d = 0;
    let mut x = pid;
    while x != root && d < 64 {
        let Some(p) = procs.get(&x) else { break };
        x = p.ppid;
        d += 1;
    }
    d
}

fn chromium_label(p: &Proc) -> String {
    match names::chromium_type(p) {
        None => tr("browser main process", "böngésző főfolyamat").into(),
        Some("renderer") if p.cmdline.iter().any(|a| a == "--extension-process") => {
            tr("extensions", "bővítmények").into()
        }
        Some("renderer") => tr("tabs (renderer)", "lapok (renderer)").into(),
        Some("gpu-process") => tr("GPU process", "GPU folyamat").into(),
        Some("utility") => {
            let sub = p
                .cmdline
                .iter()
                .find_map(|a| a.strip_prefix("--utility-sub-type="))
                .unwrap_or("");
            // "network.mojom.NetworkService" -> "network"
            match sub.split('.').next() {
                Some(s) if !s.is_empty() => format!("{}: {s}", tr("helper", "segéd")),
                _ => tr("helper process", "segédfolyamat").into(),
            }
        }
        Some("zygote") => "zygote".into(),
        Some("crashpad-handler") => "crashpad".into(),
        Some(t) => t.to_string(),
    }
}

fn generic_label(p: &Proc) -> String {
    let name = names::process_name(p);
    let exe = names::exe_basename(p);
    // Firefox and friends name their helper processes through comm
    if name == exe && !p.comm.is_empty() && !exe.starts_with(p.comm.as_str()) {
        return p.comm.clone();
    }
    name
}

/// Collapse siblings that share a name and detail into one "×N" row.
fn merge_same(rows: Vec<Node>) -> Vec<Node> {
    let mut order: Vec<(String, String)> = Vec::new();
    let mut by: HashMap<(String, String), Vec<Node>> = HashMap::new();
    for r in rows {
        let k = (r.name.clone(), r.detail.clone());
        if !by.contains_key(&k) {
            order.push(k.clone());
        }
        by.entry(k).or_default().push(r);
    }
    let mut out = Vec::new();
    for k in order {
        let mut v = by.remove(&k).unwrap();
        if v.len() == 1 {
            out.push(v.pop().unwrap());
            continue;
        }
        let mut n = Node::new(k.0, k.1, v[0].kind);
        n.count = 0;
        for c in &v {
            n.add_values(c);
            n.count += c.count;
        }
        // identical leaf rows add nothing when listed one by one
        if v.iter().any(|c| !c.children.is_empty()) {
            n.children = merge_same(v.into_iter().flat_map(|c| c.children).collect());
        }
        out.push(n);
    }
    out
}

/// Rows that stand for several processes open up to the single processes.
fn add_process_level(n: &mut Node, acc: &Acc) {
    for c in &mut n.children {
        add_process_level(c, acc);
    }
    if !n.children.is_empty() || n.pids.len() < 2 || n.kind == Kind::Rest {
        return;
    }
    let same_label = n.count > 1 && n.kind == Kind::Proc;
    n.children = n
        .pids
        .iter()
        .filter(|(pid, _)| acc.cur.procs.contains_key(pid))
        .map(|&(pid, _)| {
            let p = &acc.cur.procs[&pid];
            let label = if same_label { n.name.clone() } else { generic_label(p) };
            let mut c = Node::new(label, format!("PID {pid} · {}", short_args(p)), Kind::Proc);
            c.ident = Some(pid.to_string());
            acc.add(&mut c, pid);
            c.targets = vec![Target::Procs(c.pids.clone())];
            c
        })
        .collect();
}

fn short_args(p: &Proc) -> String {
    let args = p
        .cmdline
        .iter()
        .skip(1)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(" ");
    let args = match std::env::var("HOME") {
        Ok(home) => args.replace(&home, "~"),
        Err(_) => args,
    };
    if args.chars().count() > 120 {
        args.chars().take(119).chain(['…']).collect()
    } else {
        args
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_is_first_service_or_scope() {
        let base = "user.slice/user-1000.slice/user@1000.service/app.slice";
        assert_eq!(
            unit_path(&format!("{base}/app-org.kde.yakuake-1.scope/tab(2).scope")),
            format!("{base}/app-org.kde.yakuake-1.scope")
        );
        assert_eq!(
            unit_path(&format!("{base}/app-dbus-:1.1-x.slice/dbus-x.service")),
            format!("{base}/app-dbus-:1.1-x.slice")
        );
        assert_eq!(unit_path("system.slice/docker.service"), "system.slice/docker.service");
        assert_eq!(unit_path(""), "");
    }

    #[test]
    fn service_names_drop_noise() {
        assert_eq!(service_name("plasma-kwin_wayland.service"), "kwin_wayland");
        assert_eq!(service_name("app-foo@3b3ffbc8999742bd9e7534c07f7de661.service"), "foo");
        assert_eq!(service_name("getty@tty1.service"), "getty@tty1");
    }

    #[test]
    fn docker_ids_need_64_hex() {
        let id = "a".repeat(64);
        assert_eq!(docker_id(&format!("system.slice/docker-{id}.scope")), Some(id.as_str()));
        assert_eq!(docker_id("system.slice/docker.service"), None);
    }

    #[test]
    fn merge_same_sums_and_counts() {
        let mut a = Node::new("x", "", Kind::Proc);
        a.mem = 10;
        let mut b = Node::new("x", "", Kind::Proc);
        b.mem = 5;
        let merged = merge_same(vec![a, b, Node::new("y", "", Kind::Proc)]);
        assert_eq!(merged.len(), 2);
        assert_eq!((merged[0].mem, merged[0].count), (15, 2));
    }
}
