use std::collections::HashMap;
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
}

/// Non-NVIDIA GPUs (amdgpu, i915, xe, ...): device totals from sysfs, per-process use from
/// the DRM fdinfo keys every modern driver exposes (drm-resident-*, drm-engine-*).
struct Drm {
    cards: Vec<DrmCard>,
    /// (pid, start time) -> (fds pointing at /dev/dri, sample number of the scan)
    fds: HashMap<(u32, u64), (Vec<String>, u64)>,
    /// (pid, pdev, client id, engine) -> busy ns at the previous sample
    prev_busy: HashMap<(u32, String, u64, String), u64>,
    prev_at: Option<Instant>,
    sample_no: u64,
}

/// Listing a process's fds is the expensive part; the result is reused for this many samples.
const FD_RESCAN: u64 = 15;

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
                cards.push(DrmCard { dir: dev });
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
        for c in &self.cards {
            s.used += read_u64(c.dir.join("mem_info_vram_used")).unwrap_or(0);
            s.total += read_u64(c.dir.join("mem_info_vram_total")).unwrap_or(0);
            s.gtt_used += read_u64(c.dir.join("mem_info_gtt_used")).unwrap_or(0);
            s.gtt_total += read_u64(c.dir.join("mem_info_gtt_total")).unwrap_or(0);
            if let Some(b) = read_u64(c.dir.join("gpu_busy_percent")) {
                s.util = s.util.max(b as u32);
            }
        }

        let now = Instant::now();
        let elapsed_ns = self
            .prev_at
            .map(|t| now.duration_since(t).as_nanos() as f64)
            .unwrap_or(0.0);
        self.prev_at = Some(now);
        let mut busy_now: HashMap<(u32, String, u64, String), u64> = HashMap::new();
        self.fds
            .retain(|(pid, st), _| procs.get(pid).is_some_and(|p| p.start_time == *st));

        // /proc/<pid>/io is readable exactly for the processes whose fdinfo is readable too
        for p in procs.values().filter(|p| p.io.is_some()) {
            let key = (p.pid, p.start_time);
            let stale = self
                .fds
                .get(&key)
                .is_none_or(|(_, at)| self.sample_no - at >= FD_RESCAN);
            if stale {
                self.fds.insert(key, (drm_fds(p.pid), self.sample_no));
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
                for (engine, ns, capacity) in c.engines {
                    let k = (p.pid, pdev.clone(), id, engine);
                    if let Some(prev) = self.prev_busy.get(&k)
                        && elapsed_ns > 0.0
                    {
                        let u = ns.saturating_sub(*prev) as f64 / (elapsed_ns * capacity.max(1) as f64) * 100.0;
                        util = util.max(u);
                    }
                    busy_now.insert(k, ns);
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

struct Client {
    pdev: String,
    id: u64,
    vram: u64,
    gtt: u64,
    /// (engine, busy ns, capacity)
    engines: Vec<(String, u64, u64)>,
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
    let text = fs::read_to_string(format!("/proc/{pid}/fdinfo/{fd}")).ok()?;
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
    // newer drivers report drm-resident-<region>, older ones drm-memory-<region>
    let region = |r: &str| {
        kv.get(format!("drm-resident-{r}").as_str())
            .or_else(|| kv.get(format!("drm-memory-{r}").as_str()))
            .map(|v| parse_size(v))
            .unwrap_or(0)
    };
    let mut engines = Vec::new();
    for (k, v) in &kv {
        if let Some(name) = k.strip_prefix("drm-engine-")
            && !name.starts_with("capacity-")
        {
            let ns = v.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
            let cap = kv
                .get(format!("drm-engine-capacity-{name}").as_str())
                .and_then(|c| c.parse().ok())
                .unwrap_or(1);
            engines.push((name.to_string(), ns, cap));
        }
    }
    Some(Client {
        pdev: kv.get("drm-pdev").unwrap_or(&"").to_string(),
        id: kv.get("drm-client-id").and_then(|v| v.parse().ok()).unwrap_or(0),
        vram: region("vram"),
        gtt: region("gtt"),
        engines,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fdinfo_sizes() {
        assert_eq!(parse_size("96856 KiB"), 96856 << 10);
        assert_eq!(parse_size("12 MiB"), 12 << 20);
        assert_eq!(parse_size("4096"), 4096);
    }
}
