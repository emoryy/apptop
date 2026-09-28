use std::collections::HashMap;

use nvml_wrapper::Nvml;
use nvml_wrapper::enums::device::UsedGpuMemory;

#[derive(Clone, Debug, Default)]
pub struct GpuSample {
    pub used: u64,
    pub total: u64,
    pub util: u32,
    /// VRAM per pid, summed over devices
    pub per_pid: HashMap<u32, u64>,
    /// SM utilization % per pid since the previous sample
    pub util_per_pid: HashMap<u32, u32>,
}

pub struct Gpu {
    nvml: Option<Nvml>,
    /// NVML timestamp (µs) of the newest utilization sample already seen, per device
    last_seen: Vec<u64>,
}

impl Gpu {
    pub fn new() -> Self {
        Gpu {
            nvml: Nvml::init().ok(),
            last_seen: Vec::new(),
        }
    }

    pub fn available(&self) -> bool {
        self.nvml.is_some()
    }

    pub fn sample(&mut self) -> Option<GpuSample> {
        let nvml = self.nvml.as_ref()?;
        let mut s = GpuSample::default();
        let count = nvml.device_count().ok()?;
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
        Some(s)
    }
}
