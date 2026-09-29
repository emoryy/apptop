use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

const CGROUP_ROOT: &str = "/sys/fs/cgroup";

#[derive(Clone, Debug)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub comm: String,
    pub exe: Option<PathBuf>,
    pub cmdline: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// cgroup path relative to the cgroup root, without the leading slash
    pub cgroup: String,
    /// utime + stime in clock ticks
    pub ticks: u64,
    pub start_time: u64,
    /// resident anonymous memory in bytes
    pub anon: u64,
    /// this process's share of resident shared memory (shm, memfd, tmpfs mappings)
    pub shmem: u64,
    pub swap: u64,
    /// cumulative storage (read, write) bytes; None when /proc/pid/io is not readable
    pub io: Option<(u64, u64)>,
    pub kernel_thread: bool,
}

#[derive(Clone, Debug, Default)]
pub struct CgStat {
    pub anon: u64,
    pub file: u64,
    pub kernel: u64,
    pub shmem: u64,
    pub slab_reclaimable: u64,
    pub slab_unreclaimable: u64,
    pub pagetables: u64,
    pub kernel_stack: u64,
    /// compressed swap held in RAM; charged as kernel memory
    pub zswap: u64,
    /// swapped pages read back into RAM that still keep their swap slot
    pub swapcached: u64,
    pub swap: u64,
    pub usage_usec: u64,
    /// memory.pressure "some avg10": % of time some task waited for memory
    pub mem_psi: Option<f64>,
}

impl CgStat {
    /// Memory the kernel cannot simply drop: anonymous pages, tmpfs/shm and unreclaimable kernel memory.
    pub fn resident(&self) -> u64 {
        self.anon + self.shmem + self.kernel.saturating_sub(self.slab_reclaimable)
    }

    /// Page cache that can be dropped under pressure.
    pub fn cache(&self) -> u64 {
        self.file.saturating_sub(self.shmem)
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemInfo {
    pub total: u64,
    pub available: u64,
    pub swap_total: u64,
    pub swap_free: u64,
}

pub struct Snapshot {
    pub taken: Instant,
    pub procs: HashMap<u32, Proc>,
    /// every cgroup directory, keyed by path relative to the root ("" is the root itself)
    pub cgroups: HashMap<String, CgStat>,
    pub meminfo: MemInfo,
    /// (busy, total) jiffies summed over all CPUs
    pub cpu_jiffies: (u64, u64),
    pub ncpu: usize,
    /// cumulative pages swapped in / out (/proc/vmstat)
    pub pswp: (u64, u64),
    pub mem_psi: Option<f64>,
}

/// Fields that never change for a given process instance are read once.
#[derive(Default)]
pub struct ProcCache {
    static_info: HashMap<(u32, u64), (Option<PathBuf>, Vec<String>)>,
    /// (RssShmem when read, Pss_Shmem, sample number when read)
    pss_shmem: HashMap<(u32, u64), (u64, u64, u64)>,
    sample_no: u64,
}

impl ProcCache {
    pub fn retain_live(&mut self, procs: &HashMap<u32, Proc>) {
        let live = |(pid, start): &(u32, u64)| procs.get(pid).is_some_and(|p| p.start_time == *start);
        self.static_info.retain(|k, _| live(k));
        self.pss_shmem.retain(|k, _| live(k));
    }
}

/// smaps_rollup costs up to ~100 ms for a process with gigabytes mapped, so its result
/// is reused until RssShmem moves by more than 5% or it is ~20 samples old.
fn pss_shmem(base: &Path, key: (u32, u64), rss_shmem: u64, cache: &mut ProcCache) -> u64 {
    if let Some(&(rss, pss, at)) = cache.pss_shmem.get(&key)
        && rss.abs_diff(rss_shmem) * 20 <= rss
        && cache.sample_no - at < 20
    {
        return pss;
    }
    let pss = fs::read_to_string(base.join("smaps_rollup"))
        .ok()
        .and_then(|r| {
            r.lines()
                .find_map(|l| l.strip_prefix("Pss_Shmem:"))
                .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
        })
        .map(|kb| kb * 1024)
        .unwrap_or(rss_shmem);
    cache.pss_shmem.insert(key, (rss_shmem, pss, cache.sample_no));
    pss
}

pub fn clock_ticks() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let t = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if t > 0 { t as u64 } else { 100 }
}

pub fn take(cache: &mut ProcCache) -> Snapshot {
    let taken = Instant::now();
    cache.sample_no += 1;
    let mut procs = HashMap::new();
    if let Ok(rd) = fs::read_dir("/proc") {
        for ent in rd.flatten() {
            let Some(pid) = ent.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            if let Some(p) = read_proc(pid, cache) {
                procs.insert(pid, p);
            }
        }
    }
    cache.retain_live(&procs);

    let mut cgroups = HashMap::new();
    walk_cgroups(Path::new(CGROUP_ROOT), "", &mut cgroups);

    let (cpu_jiffies, ncpu) = read_cpu_stat();
    Snapshot {
        taken,
        procs,
        cgroups,
        meminfo: read_meminfo(),
        cpu_jiffies,
        ncpu,
        pswp: read_pswp(),
        mem_psi: read_psi(Path::new("/proc/pressure/memory")),
    }
}

fn read_proc(pid: u32, cache: &mut ProcCache) -> Option<Proc> {
    let base = PathBuf::from(format!("/proc/{pid}"));
    let stat = fs::read_to_string(base.join("stat")).ok()?;
    // comm may contain spaces and parentheses, so split at the last ')'
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let comm = stat[open + 1..close].to_string();
    let rest: Vec<&str> = stat[close + 2..].split(' ').collect();
    // rest[0] is field 3 (state)
    let field = |n: usize| rest.get(n - 3).and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let ppid = field(4) as u32;
    let flags = field(9);
    let ticks = field(14) + field(15);
    let start_time = field(22);
    let kernel_thread = flags & 0x0020_0000 != 0 || pid == 2 || ppid == 2; // PF_KTHREAD

    let (mut anon, mut shmem, mut swap, mut uid) = (0, 0, 0, u32::MAX);
    if !kernel_thread && let Ok(st) = fs::read_to_string(base.join("status")) {
        for line in st.lines() {
            let kb = |v: &str| {
                v.split_whitespace()
                    .next()
                    .and_then(|x| x.parse::<u64>().ok())
                    .unwrap_or(0)
                    * 1024
            };
            if let Some(v) = line.strip_prefix("RssAnon:") {
                anon = kb(v);
            } else if let Some(v) = line.strip_prefix("RssShmem:") {
                shmem = kb(v);
            } else if let Some(v) = line.strip_prefix("VmSwap:") {
                swap = kb(v);
            } else if let Some(v) = line.strip_prefix("Uid:") {
                uid = v
                    .split_whitespace()
                    .next()
                    .and_then(|x| x.parse().ok())
                    .unwrap_or(u32::MAX);
            }
        }
    }

    // Shared memory mapped by several processes would be counted once per process;
    // the proportional share fixes that, and only matters for big users.
    if shmem > 32 << 20 {
        shmem = pss_shmem(&base, (pid, start_time), shmem, cache);
    }

    let cgroup = fs::read_to_string(base.join("cgroup"))
        .ok()
        .and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix("0::").map(|p| p.trim_start_matches('/').to_string()))
        })
        .unwrap_or_default();

    let (exe, cmdline) = cache
        .static_info
        .entry((pid, start_time))
        .or_insert_with(|| {
            if kernel_thread {
                return (None, Vec::new());
            }
            let exe = fs::read_link(base.join("exe")).ok();
            let mut cmdline: Vec<String> = fs::read(base.join("cmdline"))
                .map(|b| {
                    b.split(|&c| c == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect()
                })
                .unwrap_or_default();
            // Chromium, npm and others rewrite argv into one space-separated string
            if cmdline.len() == 1 && cmdline[0].contains(' ') && !Path::new(&cmdline[0]).exists() {
                cmdline = cmdline[0]
                    .split(' ')
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect();
            }
            (exe, cmdline)
        })
        .clone();
    let cwd = if kernel_thread {
        None
    } else {
        fs::read_link(base.join("cwd")).ok()
    };
    // other users' counters are not readable; skip the failing syscalls
    let io = if kernel_thread || uid != my_uid() {
        None
    } else {
        read_io(&base)
    };

    Some(Proc {
        pid,
        ppid,
        comm,
        exe,
        cmdline,
        cwd,
        cgroup,
        ticks,
        start_time,
        anon,
        shmem,
        swap,
        io,
        kernel_thread,
    })
}

fn walk_cgroups(dir: &Path, rel: &str, out: &mut HashMap<String, CgStat>) {
    let mut st = CgStat::default();
    if let Ok(s) = fs::read_to_string(dir.join("memory.stat")) {
        for line in s.lines() {
            let mut it = line.split(' ');
            let (Some(k), Some(v)) = (it.next(), it.next()) else {
                continue;
            };
            let v: u64 = v.parse().unwrap_or(0);
            match k {
                "anon" => st.anon = v,
                "file" => st.file = v,
                "kernel" => st.kernel = v,
                "shmem" => st.shmem = v,
                "slab_reclaimable" => st.slab_reclaimable = v,
                "slab_unreclaimable" => st.slab_unreclaimable = v,
                "pagetables" => st.pagetables = v,
                "kernel_stack" => st.kernel_stack = v,
                "zswap" => st.zswap = v,
                "swapcached" => st.swapcached = v,
                _ => {}
            }
        }
    }
    if !rel.is_empty() {
        st.mem_psi = read_psi(&dir.join("memory.pressure"));
    }
    if let Ok(s) = fs::read_to_string(dir.join("memory.swap.current")) {
        st.swap = s.trim().parse().unwrap_or(0);
    }
    if let Ok(s) = fs::read_to_string(dir.join("cpu.stat")) {
        st.usage_usec = s
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec ").and_then(|v| v.parse().ok()))
            .unwrap_or(0);
    }
    out.insert(rel.to_string(), st);
    let Ok(rd) = fs::read_dir(dir) else { return };
    for ent in rd.flatten() {
        if !ent.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let name = ent.file_name().to_string_lossy().into_owned();
        let child_rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        walk_cgroups(&ent.path(), &child_rel, out);
    }
}

fn read_meminfo() -> MemInfo {
    let mut m = MemInfo::default();
    if let Ok(s) = fs::read_to_string("/proc/meminfo") {
        for line in s.lines() {
            let mut it = line.split_whitespace();
            let (Some(k), Some(v)) = (it.next(), it.next()) else {
                continue;
            };
            let v: u64 = v.parse::<u64>().unwrap_or(0) * 1024;
            match k {
                "MemTotal:" => m.total = v,
                "MemAvailable:" => m.available = v,
                "SwapTotal:" => m.swap_total = v,
                "SwapFree:" => m.swap_free = v,
                _ => {}
            }
        }
    }
    m
}

fn read_cpu_stat() -> ((u64, u64), usize) {
    let Ok(s) = fs::read_to_string("/proc/stat") else {
        return ((0, 0), 1);
    };
    let mut busy_total = (0, 0);
    let mut ncpu = 0;
    for line in s.lines() {
        if let Some(rest) = line.strip_prefix("cpu ") {
            let v: Vec<u64> = rest.split_whitespace().filter_map(|x| x.parse().ok()).collect();
            let total: u64 = v.iter().take(8).sum();
            let idle = v.get(3).copied().unwrap_or(0) + v.get(4).copied().unwrap_or(0);
            busy_total = (total - idle, total);
        } else if line.starts_with("cpu") {
            ncpu += 1;
        }
    }
    (busy_total, ncpu.max(1))
}

pub fn page_size() -> u64 {
    // SAFETY: sysconf has no preconditions.
    let p = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if p > 0 { p as u64 } else { 4096 }
}

pub fn my_uid() -> u32 {
    // SAFETY: getuid cannot fail.
    unsafe { libc::getuid() }
}

fn read_io(base: &Path) -> Option<(u64, u64)> {
    let s = fs::read_to_string(base.join("io")).ok()?;
    let (mut r, mut w) = (0, 0);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("read_bytes: ") {
            r = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("write_bytes: ") {
            w = v.trim().parse().unwrap_or(0);
        }
    }
    Some((r, w))
}

fn read_psi(path: &Path) -> Option<f64> {
    let s = fs::read_to_string(path).ok()?;
    let line = s.lines().find(|l| l.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|f| f.strip_prefix("avg10="))
        .and_then(|v| v.parse().ok())
}

fn read_pswp() -> (u64, u64) {
    let Ok(s) = fs::read_to_string("/proc/vmstat") else {
        return (0, 0);
    };
    let mut out = (0, 0);
    for line in s.lines() {
        if let Some(v) = line.strip_prefix("pswpin ") {
            out.0 = v.trim().parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("pswpout ") {
            out.1 = v.trim().parse().unwrap_or(0);
        }
    }
    out
}
