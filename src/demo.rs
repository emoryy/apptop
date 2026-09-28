//! A fixed, made-up machine for screenshots (`APPTOP_DEMO=1`), so no real process names,
//! paths or container names end up in published images.

use crate::gpu::GpuSample;
use crate::model::{Kind, Model, Node, Summary};

const M: u64 = 1 << 20;
const G: u64 = 1 << 30;

struct R {
    mem: u64,
    swap: u64,
    cache: Option<u64>,
    cpu: f64,
    gpu: f64,
    vram: u64,
    io: f64,
    procs: usize,
    delta: i64,
    psi: Option<f64>,
}

const fn r(mem: u64, swap: u64, cpu: f64, procs: usize) -> R {
    R {
        mem,
        swap,
        cache: None,
        cpu,
        gpu: 0.0,
        vram: 0,
        io: 0.0,
        procs,
        delta: 0,
        psi: None,
    }
}

fn node(name: &str, detail: &str, kind: Kind, v: R, children: Vec<Node>) -> Node {
    let mut n = Node::new(name, detail, kind);
    n.mem = v.mem;
    n.swap = v.swap;
    n.cache = v.cache;
    n.cpu = v.cpu;
    n.gpu = v.gpu;
    n.vram = v.vram;
    n.io_read = v.io * 0.4;
    n.io_write = v.io * 0.6;
    n.procs = v.procs;
    n.psi = v.psi;
    if v.delta != 0 {
        n.delta = Some((v.delta, true));
    }
    n.children = children;
    n
}

fn leaf(name: &str, detail: &str, v: R) -> Node {
    node(name, detail, Kind::Proc, v, Vec::new())
}

fn counted(mut n: Node, count: usize) -> Node {
    n.count = count;
    n
}

pub fn model() -> Model {
    let firefox = node(
        "Firefox",
        "",
        Kind::App,
        R {
            cache: Some(1100 * M),
            gpu: 3.0,
            vram: 620 * M,
            io: 180e3,
            delta: 120 * M as i64,
            ..r(7800 * M, 410 * M, 18.4, 38)
        },
        vec![
            counted(leaf("Isolated Web Co", "", r(5600 * M, 300 * M, 11.2, 24)), 24),
            leaf(
                "firefox",
                "",
                R {
                    gpu: 3.0,
                    vram: 620 * M,
                    ..r(1300 * M, 70 * M, 5.1, 1)
                },
            ),
            leaf("WebExtensions", "", r(420 * M, 20 * M, 0.8, 1)),
            counted(leaf("Web Content", "", r(250 * M, 10 * M, 0.9, 6)), 6),
            leaf("RDD Process", "", r(60 * M, 0, 0.3, 1)),
        ],
    );
    let comfy = node(
        "ComfyUI",
        "~/ComfyUI",
        Kind::Job,
        R {
            gpu: 97.0,
            vram: 10900 * M,
            psi: Some(2.1),
            delta: 1400 * M as i64,
            ..r(6200 * M, 0, 104.0, 2)
        },
        Vec::new(),
    );
    let sessions = [
        ("~/src/website", 1100, 2, 3.1),
        ("~/src/apptop", 610, 1, 4.8),
        ("~/src/infra", 540, 1, 0.6),
        ("~/notes", 480, 1, 0.4),
        ("~/src/api", 430, 1, 2.2),
        ("~/src/mobile", 390, 1, 0.9),
        ("~/src/scripts", 280, 1, 0.6),
    ];
    let claude = counted(
        node(
            "Claude Code",
            "",
            Kind::Job,
            R {
                delta: 85 * M as i64,
                ..r(3830 * M, 640 * M, 12.6, 96)
            },
            sessions
                .iter()
                .map(|&(dir, mb, n, cpu)| {
                    counted(
                        node(dir, "", Kind::Job, r(mb * M, mb * M / 6, cpu, 12 * n), Vec::new()),
                        n,
                    )
                })
                .collect(),
        ),
        8,
    );
    let shop = node(
        "Docker: shop",
        "6 containers",
        Kind::Docker,
        R {
            cache: Some(310 * M),
            io: 2.4e6,
            ..r(3100 * M, 240 * M, 5.2, 41)
        },
        [
            "shop-db-1",
            "shop-api-1",
            "shop-worker-1",
            "shop-web-1",
            "shop-redis-1",
            "shop-mailpit-1",
        ]
        .iter()
        .zip([1400u64, 720, 460, 310, 140, 70])
        .map(|(n, mb)| node(n, "", Kind::Docker, r(mb * M, 40 * M, 0.8, 7), Vec::new()))
        .collect(),
    );
    let mut roots = vec![
        firefox,
        comfy,
        claude,
        shop,
        node(
            "Visual Studio Code",
            "",
            Kind::App,
            R {
                cache: Some(240 * M),
                ..r(1900 * M, 180 * M, 2.3, 27)
            },
            vec![],
        ),
        node(
            "Chromium",
            "",
            Kind::Browser,
            R {
                cache: Some(120 * M),
                vram: 140 * M,
                ..r(1700 * M, 90 * M, 1.9, 22)
            },
            vec![],
        ),
        node(
            "Konsole",
            "",
            Kind::Terminal,
            R {
                cache: Some(90 * M),
                ..r(640 * M, 20 * M, 0.7, 31)
            },
            vec![],
        ),
        node(
            "kwin_wayland",
            "KDE Wayland Compositor",
            Kind::UserService,
            R {
                cache: Some(23 * M),
                gpu: 2.0,
                vram: 580 * M,
                ..r(520 * M, 0, 3.1, 3)
            },
            vec![],
        ),
        node(
            "plasmashell",
            "KDE Plasma Workspace",
            Kind::UserService,
            R {
                vram: 170 * M,
                ..r(410 * M, 30 * M, 0.9, 6)
            },
            vec![],
        ),
        counted(node("npm run dev", "", Kind::Job, r(350 * M, 0, 0.4, 12), vec![]), 3),
        node(
            "Spotify",
            "",
            Kind::App,
            R {
                cache: Some(160 * M),
                ..r(330 * M, 0, 1.1, 9)
            },
            vec![],
        ),
        node(
            "Thunderbird",
            "",
            Kind::App,
            R {
                cache: Some(210 * M),
                ..r(310 * M, 45 * M, 0.3, 5)
            },
            vec![],
        ),
        node(
            "postgresql",
            "PostgreSQL database server",
            Kind::SystemService,
            R {
                cache: Some(820 * M),
                ..r(280 * M, 0, 0.8, 9)
            },
            vec![],
        ),
        node(
            "docker",
            "Docker Application Container Engine",
            Kind::SystemService,
            r(190 * M, 12 * M, 1.4, 28),
            vec![],
        ),
        node(
            "Dolphin",
            "2 instances",
            Kind::App,
            R {
                cache: Some(60 * M),
                ..r(180 * M, 0, 0.0, 2)
            },
            vec![],
        ),
        node(
            "kde-baloo",
            "Baloo File Indexer Daemon",
            Kind::UserService,
            R {
                cache: Some(1400 * M),
                io: 3.1e6,
                ..r(160 * M, 0, 6.2, 1)
            },
            vec![],
        ),
        node(
            "python: backup.py",
            "~/bin",
            Kind::Job,
            R {
                io: 12.5e6,
                ..r(90 * M, 0, 8.7, 1)
            },
            vec![],
        ),
        node(
            "tailscaled",
            "Tailscale node agent",
            Kind::SystemService,
            r(70 * M, 0, 0.2, 1),
            vec![],
        ),
        node(
            "systemd-journald",
            "Journal Service",
            Kind::SystemService,
            R {
                cache: Some(40 * M),
                ..r(60 * M, 0, 0.1, 1)
            },
            vec![],
        ),
        node(
            "pipewire",
            "PipeWire Multimedia Service",
            Kind::UserService,
            r(22 * M, 0, 0.6, 1),
            vec![],
        ),
        node(
            "NetworkManager",
            "Network Manager",
            Kind::SystemService,
            r(18 * M, 0, 0.0, 1),
            vec![],
        ),
        node("Kernel", "", Kind::Kernel, r(0, 0, 1.2, 312), vec![]),
    ];
    for r in &mut roots {
        r.set_keys("");
    }
    Model {
        roots,
        summary: Summary {
            swap_in: 0.0,
            swap_out: 0.0,
            mem_psi: Some(0.3),
            cpu_pct: 23.0,
            ncpu: 16,
            mem_used: 38600 * M,
            mem_total: 62 * G + 700 * M,
            swap_used: 2100 * M,
            swap_total: 16 * G,
            gpu: Some(GpuSample {
                used: 12100 * M,
                total: 16 * G,
                util: 94,
                ..Default::default()
            }),
        },
    }
}
