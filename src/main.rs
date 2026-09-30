mod collect;
mod config;
mod demo;
mod docker;
mod gpu;
mod history;
mod i18n;
mod kill;
mod model;
mod names;
mod ui;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;

use crate::collect::{ProcCache, Snapshot};
use crate::docker::DockerNames;
use crate::gpu::Gpu;
use crate::history::History;
use crate::model::{Ctx, Model};
use crate::names::{DesktopIndex, UnitDescriptions};

pub struct Sampler {
    cache: ProcCache,
    prev: Option<Snapshot>,
    desktop: DesktopIndex,
    unit_desc: UnitDescriptions,
    docker: DockerNames,
    gpu: Gpu,
    clk_tck: u64,
    pub split_terminals: bool,
    pub history: Arc<Mutex<History>>,
    demo: bool,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

impl Sampler {
    pub fn new() -> Self {
        Sampler {
            cache: ProcCache::default(),
            prev: None,
            desktop: DesktopIndex::load(),
            unit_desc: UnitDescriptions::default(),
            docker: DockerNames::default(),
            gpu: Gpu::new(),
            clk_tck: collect::clock_ticks(),
            split_terminals: false,
            history: Arc::new(Mutex::new(History::default())),
            demo: std::env::var_os("APPTOP_DEMO").is_some_and(|v| v == "1"),
        }
    }

    pub fn gpu_available(&self) -> bool {
        self.demo || self.gpu.available()
    }

    pub fn sample(&mut self) -> Model {
        if self.demo {
            return demo::model();
        }
        let cur = collect::take(&mut self.cache);
        let mut model = model::build(Ctx {
            cur: &cur,
            prev: self.prev.as_ref(),
            desktop: &self.desktop,
            unit_desc: &mut self.unit_desc,
            docker: &mut self.docker,
            gpu: self.gpu.sample(&cur.procs),
            clk_tck: self.clk_tck,
            split_terminals: self.split_terminals,
        });
        if let Ok(mut h) = self.history.lock() {
            h.update(&mut model.roots, cur.taken);
        }
        self.prev = Some(cur);
        model
    }
}

struct Args {
    dump: bool,
    split: bool,
    lang: Option<i18n::Lang>,
    depth: usize,
    interval: Duration,
}

fn parse_args() -> Result<Args> {
    let mut a = Args {
        dump: false,
        split: false,
        lang: None,
        depth: 1,
        interval: Duration::from_secs(2),
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--dump" => a.dump = true,
            "--split" => a.split = true,
            "--lang" => {
                let v = it.next().unwrap_or_default();
                a.lang = Some(i18n::parse(&v).ok_or_else(|| anyhow::anyhow!("--lang takes en or hu, not {v:?}"))?);
            }
            "--depth" => a.depth = it.next().and_then(|v| v.parse().ok()).unwrap_or(1),
            "-d" | "--delay" => {
                let secs: f64 = it.next().and_then(|v| v.parse().ok()).unwrap_or(2.0);
                a.interval = Duration::from_secs_f64(secs.max(0.5));
            }
            "-h" | "--help" => {
                println!(
                    "apptop: resource use per application\n\n\
                     usage: apptop [-d SECONDS] [--split] [--lang en|hu] [--dump [--depth N]]\n\n  \
                     -d, --delay SECONDS  refresh interval (default 2)\n  \
                     --split              keep programs started in terminals under the terminal\n  \
                     --lang en|hu         interface language (default: from LANG)\n  \
                     --dump               print the tree once and exit\n  \
                     --depth N            levels to print with --dump (default 1)"
                );
                std::process::exit(0);
            }
            other => anyhow::bail!("unknown argument: {other}"),
        }
    }
    Ok(a)
}

fn main() -> Result<()> {
    // Rust ignores SIGPIPE, which turns `apptop --dump | head` into a panic; die quietly like other CLI tools
    // SAFETY: resetting a signal disposition before any threads exist.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    let args = parse_args()?;
    i18n::set(args.lang.unwrap_or_else(i18n::from_env));
    let mut cfg = config::Config::load();
    if args.split {
        cfg.split = true;
    }
    let mut sampler = Sampler::new();
    sampler.split_terminals = cfg.split;
    if args.dump {
        sampler.sample();
        std::thread::sleep(Duration::from_millis(500));
        let model = sampler.sample();
        ui::dump(&model, args.depth);
        return Ok(());
    }
    ui::run(sampler, args.interval, cfg)
}
