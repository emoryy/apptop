use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::PathBuf;
use std::time::Instant;

use nvml_wrapper::Nvml;
use nvml_wrapper::enums::device::UsedGpuMemory;

use crate::collect::Proc;

#[derive(Clone, Debug, Default)]
pub struct GpuSample {
    /// dedicated video memory in use / available, over all GPUs
    pub used: u64,
    pub total: u64,
    /// system memory the GPU drivers have mapped for the GPU (GTT), over all non-NVIDIA GPUs
    pub gtt_used: u64,
    pub gtt_total: u64,
    /// some GPU maps system memory without a known pool size (i915/xe), so shares use all RAM
    pub gtt_unbounded: bool,
    pub util: u32,
    /// GPU memory per pid (VRAM + GTT), summed over devices
    pub per_pid: HashMap<u32, u64>,
    /// the GTT part of `per_pid`
    pub gtt_per_pid: HashMap<u32, u64>,
    /// utilization % per pid since the previous sample
    pub util_per_pid: HashMap<u32, u32>,
}

pub struct Gpu {
    nvml: Option<Nvml>,
    /// NVML timestamp (µs) of the newest utilization sample already seen, per device
    last_seen: Vec<u64>,
    drm: Drm,
}

impl Gpu {
    pub fn new() -> Self {
        Gpu {
            nvml: Nvml::init().ok(),
            last_seen: Vec::new(),
            drm: Drm::new(),
        }
    }

    pub fn available(&self) -> bool {
        self.nvml.is_some() || !self.drm.cards.is_empty()
    }

    pub fn sample(&mut self, procs: &HashMap<u32, Proc>) -> Option<GpuSample> {
        if !self.available() {
            return None;
        }
        let mut s = GpuSample::default();
        self.sample_nvml(&mut s);
        self.drm.sample(procs, &mut s);
        Some(s)
    }

    fn sample_nvml(&mut self, s: &mut GpuSample) {
        let Some(nvml) = self.nvml.as_ref() else { return };
        let Ok(count) = nvml.device_count() else { return };
        self.last_seen.resize(count as usize, 0);
        for i in 0..count {
            let Ok(dev) = nvml.device_by_index(i) else { continue };
            // several samples per pid can arrive per interval; keep the average
            let mut util: HashMap<u32, (u32, u32)> = HashMap::new();
            if let Ok(samples) = dev.process_utilization_stats(self.last_seen[i as usize]) {
                for smp in samples {
                    self.last_seen[i as usize] = self.last_seen[i as usize].max(smp.timestamp);
                    let e = util.entry(smp.pid).or_default();
                    e.0 += smp.sm_util;
                    e.1 += 1;
                }
            }
            for (pid, (sum, n)) in util {
                *s.util_per_pid.entry(pid).or_default() += sum / n.max(1);
            }
            if let Ok(m) = dev.memory_info() {
                s.used += m.used;
                s.total += m.total;
            }
            if let Ok(u) = dev.utilization_rates() {
                s.util = s.util.max(u.gpu);
            }
            // a process doing both compute and graphics is listed twice with the same number
            let mut seen: HashMap<u32, u64> = HashMap::new();
            let lists = [dev.running_compute_processes(), dev.running_graphics_processes()];
            for list in lists.into_iter().flatten() {
                for p in list {
                    if let UsedGpuMemory::Used(b) = p.used_gpu_memory {
                        let e = seen.entry(p.pid).or_default();
                        *e = (*e).max(b);
                    }
                }
            }
            for (pid, b) in seen {
                *s.per_pid.entry(pid).or_default() += b;
            }
        }
    }
}

struct DrmCard {
    dir: PathBuf,
    /// PCI address, matched against fdinfo drm-pdev
    pdev: String,
}

/// Non-NVIDIA GPUs (amdgpu, i915, xe, ...): device totals from sysfs, per-process use from
/// the DRM fdinfo keys every modern driver exposes (drm-resident-*, drm-engine-*).
struct Drm {
    cards: Vec<DrmCard>,
    /// (pid, start time) -> (fds pointing at /dev/dri, sample number of the first sighting)
    fds: HashMap<(u32, u64), (Vec<String>, u64)>,
    prev_busy: BusyCounters,
    prev_at: Option<Instant>,
    sample_no: u64,
}

/// (pid, pdev, client id, engine key) -> (busy ns or cycles, total cycles) at the previous sample
type BusyCounters = HashMap<(u32, String, u64, String), (u64, Option<u64>)>;

/// Listing a process's fds is the expensive part; the result is reused for this many samples.
const FD_RESCAN: u64 = 15;
/// For this many samples after a process appears, it is checked for a GPU fd more often,
/// since programs open the render node some time after they start.
const YOUNG: u64 = 10;

impl Drm {
    fn new() -> Self {
        let mut cards = Vec::new();
        if let Ok(rd) = fs::read_dir("/sys/class/drm") {
            for e in rd.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                // "card1" is a GPU, "card1-DP-1" a connector
                if !name
                    .strip_prefix("card")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|c| c.is_ascii_digit()))
                {
                    continue;
                }
                let dev = e.path().join("device");
                let driver = fs::read_link(dev.join("driver"))
                    .ok()
                    .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                // NVIDIA is covered by NVML, and its DRM node exposes no usage keys
                if driver.is_empty() || driver.starts_with("nvidia") {
                    continue;
                }
                let pdev = fs::read_link(&dev)
                    .ok()
                    .and_then(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()))
                    .unwrap_or_default();
                cards.push(DrmCard { dir: dev, pdev });
            }
        }
        Drm {
            cards,
            fds: HashMap::new(),
            prev_busy: HashMap::new(),
            prev_at: None,
            sample_no: 0,
        }
    }

    fn sample(&mut self, procs: &HashMap<u32, Proc>, s: &mut GpuSample) {
        if self.cards.is_empty() {
            return;
        }
        self.sample_no += 1;
        let read_u64 = |p: PathBuf| fs::read_to_string(p).ok().and_then(|v| v.trim().parse::<u64>().ok());
        let mut have_busy = false;
        // cards whose driver counts its GTT pool in sysfs; for the others the clients are summed
        let mut sysfs_gtt: HashSet<String> = HashSet::new();
        for c in &self.cards {
            s.used += read_u64(c.dir.join("mem_info_vram_used")).unwrap_or(0);
            s.total += read_u64(c.dir.join("mem_info_vram_total")).unwrap_or(0);
            if let Some(g) = read_u64(c.dir.join("mem_info_gtt_used")) {
                s.gtt_used += g;
                sysfs_gtt.insert(c.pdev.clone());
            }
            s.gtt_total += read_u64(c.dir.join("mem_info_gtt_total")).unwrap_or(0);
            if let Some(b) = read_u64(c.dir.join("gpu_busy_percent")) {
                s.util = s.util.max(b as u32);
                have_busy = true;
            }
        }

        let now = Instant::now();
        let elapsed_ns = self
            .prev_at
            .map(|t| now.duration_since(t).as_nanos() as f64)
            .unwrap_or(0.0);
        self.prev_at = Some(now);
        let mut busy_now: BusyCounters = HashMap::new();
        // one entry per DRM client, however many processes share its fd
        let mut client_load: HashMap<(String, u64, String), f64> = HashMap::new();
        let mut client_gtt: HashMap<(String, u64), u64> = HashMap::new();
        self.fds
            .retain(|(pid, st), _| procs.get(pid).is_some_and(|p| p.start_time == *st));

        // /proc/<pid>/io is readable exactly for the processes whose fdinfo is readable too
        for p in procs.values().filter(|p| p.io.is_some()) {
            let key = (p.pid, p.start_time);
            // rescans are spread over the interval by pid
            let phase = self.sample_no + p.pid as u64;
            let stale = match self.fds.get(&key) {
                None => true,
                Some((fds, first)) if fds.is_empty() && self.sample_no - first < YOUNG => phase % 2 == 0,
                Some((fds, _)) if fds.is_empty() => phase % (FD_RESCAN * 2) == 0,
                Some(_) => phase % FD_RESCAN == 0,
            };
            if stale {
                let first = self.fds.get(&key).map_or(self.sample_no, |(_, f)| *f);
                self.fds.insert(key, (drm_fds(p.pid), first));
            }
            let fds = &self.fds[&key].0;
            if fds.is_empty() {
                continue;
            }
            // several fds can share one DRM client; count each client once
            let mut clients: HashMap<(String, u64), Client> = HashMap::new();
            for fd in fds {
                if let Some(c) = read_client(p.pid, fd) {
                    clients.insert((c.pdev.clone(), c.id), c);
                }
            }
            let (mut mem, mut gtt, mut util) = (0u64, 0u64, 0f64);
            for ((pdev, id), c) in clients {
                mem += c.vram + c.gtt;
                gtt += c.gtt;
                if !sysfs_gtt.contains(&pdev) {
                    // buffers shared with other processes (a compositor's imported windows) once
                    client_gtt.insert((pdev.clone(), id), c.gtt.saturating_sub(c.gtt_shared));
                }
                for e in c.engines {
                    let k = (p.pid, pdev.clone(), id, e.key.clone());
                    if let Some(&(prev, prev_total)) = self.prev_busy.get(&k) {
                        let busy = e.busy.saturating_sub(prev) as f64;
                        // cycles (xe) are measured against the engine's own total; ns against wall time
                        let span = match (e.total, prev_total) {
                            (Some(t), Some(pt)) => t.saturating_sub(pt) as f64,
                            _ => elapsed_ns,
                        };
                        if span > 0.0 {
                            let u = busy / (span * e.capacity.max(1) as f64) * 100.0;
                            util = util.max(u);
                            client_load.insert((pdev.clone(), id, e.key.clone()), u);
                        }
                    }
                    busy_now.insert(k, (e.busy, e.total));
                }
            }
            if mem > 0 {
                *s.per_pid.entry(p.pid).or_default() += mem;
                *s.gtt_per_pid.entry(p.pid).or_default() += gtt;
            }
            if util >= 0.5 {
                *s.util_per_pid.entry(p.pid).or_default() += util.min(100.0).round() as u32;
            }
        }
        self.prev_busy = busy_now;
        if !have_busy {
            // i915/xe have no device-wide busy file: the clients' load on the busiest engine stands in
            let mut engine_load: HashMap<(String, String), f64> = HashMap::new();
            for ((pdev, _, engine), u) in client_load {
                *engine_load.entry((pdev, engine)).or_default() += u;
            }
            let load = engine_load.values().fold(0.0f64, |a, b| a.max(*b)).min(100.0);
            s.util = s.util.max(load.round() as u32);
        }
        let fallback: u64 = client_gtt.values().sum();
        if fallback > 0 {
            // mapped system memory of a GPU without a pool counter: its size is bounded only by RAM
            s.gtt_used += fallback;
            s.gtt_unbounded = true;
        }
    }
}

fn drm_fds(pid: u32) -> Vec<String> {
    let Ok(rd) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| fs::read_link(e.path()).is_ok_and(|t| t.starts_with("/dev/dri/")))
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect()
}

struct Engine {
    /// "ns:<engine>" or "cycles:<engine>", so drivers reporting both never mix the two
    key: String,
    /// busy ns (drm-engine-*) or busy cycles (drm-cycles-*, xe)
    busy: u64,
    /// total cycles for cycle counters (drm-total-cycles-*)
    total: Option<u64>,
    capacity: u64,
}

struct Client {
    pdev: String,
    id: u64,
    vram: u64,
    gtt: u64,
    /// the part of `gtt` shared with other clients
    gtt_shared: u64,
    engines: Vec<Engine>,
}

/// "96856 KiB" / "12 MiB" / "4096" -> bytes
fn parse_size(v: &str) -> u64 {
    let mut it = v.split_whitespace();
    let n: u64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
    match it.next() {
        Some("KiB") => n << 10,
        Some("MiB") => n << 20,
        Some("GiB") => n << 30,
        _ => n,
    }
}

fn read_client(pid: u32, fd: &str) -> Option<Client> {
    parse_client(&fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?)
}

fn is_vram_region(r: &str) -> bool {
    r.starts_with("vram") || r.starts_with("local")
}

fn is_system_region(r: &str) -> bool {
    r == "gtt" || r.starts_with("system") || r.starts_with("stolen")
}

fn parse_client(text: &str) -> Option<Client> {
    let mut kv: HashMap<&str, &str> = HashMap::new();
    for line in text.lines() {
        if let Some((k, v)) = line.split_once(':') {
            kv.insert(k.trim(), v.trim());
        }
    }
    let driver = kv.get("drm-driver")?;
    if driver.starts_with("nvidia") {
        return None;
    }
    // newer drivers report drm-resident-<region>, older ones drm-memory-<region>; region names
    // differ per driver: amdgpu vram/gtt, i915/xe local0 (device memory) and system0/stolen-*
    let (mut vram, mut gtt, mut gtt_shared) = (0, 0, 0);
    let resident: Vec<(&str, &str)> = kv
        .iter()
        .filter_map(|(k, v)| k.strip_prefix("drm-resident-").map(|r| (r, *v)))
        .collect();
    let regions: Vec<(&str, &str)> = if resident.is_empty() {
        kv.iter()
            .filter_map(|(k, v)| k.strip_prefix("drm-memory-").map(|r| (r, *v)))
            .collect()
    } else {
        resident
    };
    for (r, v) in regions {
        if is_vram_region(r) {
            vram += parse_size(v);
        } else if is_system_region(r) {
            gtt += parse_size(v);
        }
    }
    for (k, v) in &kv {
        if let Some(r) = k.strip_prefix("drm-shared-")
            && is_system_region(r)
        {
            gtt_shared += parse_size(v);
        }
    }
    let num = |v: &str| {
        v.split_whitespace()
            .next()
            .and_then(|x| x.parse::<u64>().ok())
            .unwrap_or(0)
    };
    let capacity = |name: &str| {
        kv.get(format!("drm-engine-capacity-{name}").as_str())
            .and_then(|c| c.parse().ok())
            .unwrap_or(1)
    };
    let mut engines = Vec::new();
    for (k, v) in &kv {
        if let Some(name) = k.strip_prefix("drm-engine-")
            && !name.starts_with("capacity-")
        {
            engines.push(Engine {
                key: format!("ns:{name}"),
                busy: num(v),
                total: None,
                capacity: capacity(name),
            });
        } else if let Some(name) = k.strip_prefix("drm-cycles-")
            // cycles mean nothing without the matching total (msm and panfrost report a clock instead)
            && let Some(total) = kv.get(format!("drm-total-cycles-{name}").as_str())
        {
            engines.push(Engine {
                key: format!("cycles:{name}"),
                busy: num(v),
                total: Some(num(total)),
                capacity: capacity(name),
            });
        }
    }
    Some(Client {
        pdev: kv.get("drm-pdev").unwrap_or(&"").to_string(),
        id: kv.get("drm-client-id").and_then(|v| v.parse().ok()).unwrap_or(0),
        vram,
        gtt,
        gtt_shared,
        engines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn i915_regions_and_shared_part() {
        let c = parse_client(
            "drm-driver:\ti915\ndrm-client-id:\t8\ndrm-pdev:\t0000:00:02.0\ndrm-total-system0:\t13076 KiB\n\
             drm-resident-system0:\t13076 KiB\ndrm-shared-system0:\t4000 KiB\ndrm-resident-stolen-system0:\t0\n\
             drm-engine-render:\t224515 ns\n",
        )
        .unwrap();
        assert_eq!((c.vram, c.gtt, c.gtt_shared), (0, 13076 << 10, 4000 << 10));
        assert_eq!(c.engines.len(), 1);
    }

    #[test]
    fn cycles_need_a_total_and_never_share_a_key_with_ns() {
        // msm style: ns and cycles for the same engine, no total cycles
        let msm = parse_client("drm-driver:\tmsm\ndrm-engine-gpu:\t100 ns\ndrm-cycles-gpu:\t5000\n").unwrap();
        assert_eq!(
            msm.engines.iter().map(|e| e.key.as_str()).collect::<Vec<_>>(),
            ["ns:gpu"]
        );
        // xe style
        let xe = parse_client("drm-driver:\txe\ndrm-cycles-rcs:\t50\ndrm-total-cycles-rcs:\t100\n").unwrap();
        assert_eq!(xe.engines[0].key, "cycles:rcs");
        assert_eq!(xe.engines[0].total, Some(100));
    }

    #[test]
    fn parses_fdinfo_sizes() {
        assert_eq!(parse_size("96856 KiB"), 96856 << 10);
        assert_eq!(parse_size("12 MiB"), 12 << 20);
        assert_eq!(parse_size("4096"), 4096);
    }
}
