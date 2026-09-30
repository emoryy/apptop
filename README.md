# apptop

A per-application resource monitor for Linux. Where htop shows one row per process, apptop shows one row per program: a browser with 90 renderer processes, a terminal running two dozen sessions, a docker compose stack, a Flatpak app with its sandbox helpers or a Python app with worker processes each become a single row with totals you can compare, and expand into their parts when you want the detail.

![apptop showing programs sorted by memory, with the Claude Code group expanded](docs/screenshot.png)

<sub>Demo data (`APPTOP_DEMO=1`); regenerate with `scripts/screenshot.sh`.</sub>

## What it shows

- **One row per program**, built from the cgroups that systemd, the desktop, Flatpak and docker already create: apps launched from the desktop, user and system services, docker containers grouped by compose project, the kernel.
- **Desktops without app scopes work too.** On XFCE, sway, i3, Hyprland and the like everything runs in the login session, so apptop splits the session into its programs and keeps only the session plumbing (display manager, session manager, launch wrappers) in the session row.
- **Readable names** from `.desktop` entries and unit descriptions. Interpreters are named by their script (`python: server.py`, `java: app.war`, `npm run dev`), each Chromium-based browser profile gets its own row, Flatpak apps are marked `(Flatpak)` so they do not merge with a native install, and login sessions say what they are (`desktop session #2 · lightdm · wayland`, `SSH session #7 · sshd · tty · pts/1 · 192.168.0.10`).
- **Programs started in terminals get their own rows** (merged by name, e.g. `Claude Code ×26` with one child per working directory) instead of hiding under the terminal. The per-tab scopes of GNOME Terminal, Ptyxis and kitty are folded into their terminal first. Press `t` to show these programs under their terminal instead, grouped by zellij session.
- **Columns:** memory the kernel cannot reclaim, share of the total, change over 5 minutes, swap, memory pressure (PSI), CPU%, GPU% and GPU memory (NVIDIA, AMD, Intel), disk I/O, file cache, process count. Click a header or press its key to sort.
- **Details panel** (`i`): the largest process's command line, working directory, parent, age, a 10-minute memory sparkline, and the VRAM / GTT split of the GPU memory.
- **Stopping programs** (`k`): a confirmation panel lists exactly what stops, built from live state, with SIGTERM, SIGKILL and other signals. See [Stopping programs](#stopping-programs).
- **Filter** (`/`) by name or directory.

## Requirements

- Linux with the unified cgroup v2 hierarchy (the default on current systemd distributions) and a systemd user session.
- Optional: a GPU for the GPU columns (NVIDIA through NVML, i.e. `nvidia-utils`; AMD, Intel and other DRM drivers through `/proc/<pid>/fdinfo` and sysfs), and access to the docker socket for container names.

Run it as your user. Run it as root to see disk I/O and GPU numbers for other users' processes and to stop them as well.

### Tested on

| Machine | Desktop | GPU |
|---|---|---|
| Arch-based desktop | KDE Plasma 6, Wayland | NVIDIA RTX (NVML) |
| ThinkPad, Arch-based | KDE Plasma X11, GNOME 50 Wayland; Flatpak apps | AMD Renoir iGPU (amdgpu) |
| Chromebook, Arch-based | XFCE (X11), sway, Hyprland (plain and uwsm) | Intel HD 520 (i915) |

## Install

**Arch Linux:** build a package with the PKGBUILD in `packaging/arch` (it builds the current GitHub `master`):

```bash
git clone https://github.com/emoryy/apptop
cd apptop/packaging/arch
makepkg -si
```

This installs `/usr/bin/apptop` and a menu entry that opens it in your terminal. To update later, run `git pull && makepkg -si` in the same directory.

**Any distribution, with Cargo:**

```bash
cargo install --git https://github.com/emoryy/apptop
```

The binary lands in `~/.cargo/bin`, which has to be on `PATH`. Or build from a checkout and copy it wherever you like:

```bash
cargo build --release
install -m755 target/release/apptop ~/.local/bin/
```

Rust 1.88 or newer.

## Usage

```
apptop [-d SECONDS] [--split] [--lang en|hu] [--dump [--depth N]]

  -d, --delay SECONDS  refresh interval (default 2)
  --split              keep programs started in terminals under the terminal
  --lang en|hu         interface language (default: from LANG)
  --dump               print the tree once and exit
  --depth N            levels to print with --dump (default 1)
```

| Key | Action |
|---|---|
| `↑` `↓` `PgUp` `PgDn` `Home` `End` | move |
| `→` `←` `Enter` | expand, collapse |
| `e` / `E` | expand / collapse everything |
| `M` `D` `S` `W` `P` | sort by memory, change, swap, pressure, CPU |
| `G` `V` `O` `C` `N` | sort by GPU%, GPU memory, disk, cache, name |
| `<` `>` / `I` | previous / next sort column, invert direction |
| `k` / `F9` | stop the selected row (asks first) |
| `i` / `F3` | details panel |
| `/` | filter, `Esc` clears |
| `t` | programs started in terminals: own rows / under the terminal |
| `?` / `F1` | help |
| `q` / `F10` | quit |

The mouse works too: click a column header to sort, double-click a row to expand it. The selected row carries `k` (stop) and `i` (details) buttons at its right end, labelled when there is room, and every footer hint and every chip in the stop panel is clickable.

Sort column, direction, view mode and the details panel are remembered in `~/.config/apptop/config`.

## How the numbers are computed

| Column | Program row (a cgroup) | Rows below it (processes) |
|---|---|---|
| Memory | anon + shmem + unreclaimable kernel memory from `memory.stat` | RssAnon + proportional share of shared memory |
| Swap | `memory.swap.current` | VmSwap |
| Cache | file cache from `memory.stat` | not shown |
| CPU% | `cpu.stat` (includes exited children); 100% = one core | per-process CPU time |
| Pressure | `memory.pressure` some avg10 | not available |
| GPU% | NVIDIA: NVML. Other GPUs: busy time of the busiest engine from fdinfo (`drm-engine-*`, or `drm-cycles-*` on xe) | same |
| VRAM | NVIDIA: NVML. Other GPUs: `drm-resident-*` from fdinfo, video memory plus system memory mapped for the GPU (GTT) | same |
| Disk/s | `/proc/<pid>/io` read and write bytes | same |

The cgroup totals include kernel memory and exited children, which no single process accounts for, so each breakdown ends with an "other" row holding the difference; expand it to see zswap, page tables, slab, kernel stacks, the swap cache and what is left unattributed.

On integrated GPUs the VRAM column is mostly GTT, system memory the driver maps for the GPU. On AMD it is not part of the Memory column. On Intel (i915, xe) GPU buffers are shared memory charged to the program's cgroup, so they also count in Memory. Buffers shared between a program and the compositor show under both, as in nvtop.

More detail on the grouping rules and the measurements is in [NOTES.md](NOTES.md).

## Stopping programs

`k` opens a panel that names the row, its kind, the cgroup or containers affected, and the first processes with their command lines. SIGTERM is the default; `Tab` cycles SIGTERM, SIGKILL and the last pick from the other-signals menu (`↓`): SIGHUP, SIGINT, SIGQUIT, SIGSTOP, SIGCONT, SIGTSTP, SIGUSR1, SIGUSR2, each with a one-line explanation. SIGTERM, SIGINT, SIGQUIT and SIGHUP are followed by SIGCONT, as systemd does, so a paused program acts on them at once.

| Row | SIGTERM | SIGKILL |
|---|---|---|
| application or user service | every process in its cgroup | `cgroup.kill` |
| program started in a terminal or a desktop session, single process | those processes | same |
| docker container or compose group | `docker stop` | `docker kill` (other signals: `docker kill --signal`) |
| system service, another user's process | refused unless apptop runs as root | same |

Processes are identified by PID and start time, so a PID reused in the meantime is never signalled. Rows that stand for several programs (`Claude Code ×26`, a whole compose project) can be stopped too; the panel states how many programs, processes and containers that means, and warns if apptop's own terminal is among them. apptop never calls sudo.

## Language

English by default; Hungarian when the locale starts with `hu` (`LC_ALL`, `LC_MESSAGES` or `LANG`), or with `--lang hu`.

## Development

`cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` run in CI on the latest stable Rust. `apptop --dump --depth 3` prints the tree once, which is the quickest way to check a grouping change on a real machine. `APPTOP_DEMO=1` shows fixed, made-up data (used for the screenshot).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT) at your option.
