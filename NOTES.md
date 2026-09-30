# apptop design notes

How apptop decides what a "program" is and where each number comes from. For usage, see
the README.

`apptop --dump --depth 3` prints the tree once (sorted by memory), handy for checking grouping
changes without the TUI.

## Where the numbers come from

| Column | Top-level row (a cgroup) | Rows below it (processes) |
|---|---|---|
| Memory | `memory.stat`: anon + shmem + (kernel - slab_reclaimable) | RssAnon + PSS share of shmem |
| Swap | `memory.swap.current` | VmSwap |
| Cache | `memory.stat`: file - shmem | not shown |
| CPU% | `cpu.stat` usage_usec delta (includes exited children) | utime+stime delta |
| VRAM | NVML per-pid usage summed | same |
| GPU% | NVML process utilization samples (sm_util), averaged per interval | same |
| Pressure | `memory.pressure` some avg10 (max over merged units) | not available |
| Disk/s | `/proc/pid/io` read_bytes + write_bytes deltas, own processes only | same |
| Δ5m | memory now minus memory 5 minutes ago (history, 10 s resolution) | same |

On a typical systemd desktop the io controller is not delegated to the user manager, so user
cgroups have no `io.stat`; hence the per-process I/O counters. Other users' processes (root
services, container users) show no I/O.

The cgroup totals are exact and include kernel memory and exited children, which no process
row can show, so each breakdown ends with an "other" row holding the difference. It expands into the parts `memory.stat` names: zswap (compressed swap in RAM, charged as kernel memory), page tables, unreclaimable slab, kernel stacks, other kernel memory, the swap cache (pages read back into RAM that keep their swap slot, so they count in both Memory and Swap), and an "unattributed" remainder. The remainder is mostly swap charged to the cgroup that no live process's `VmSwap` explains: cgroup v2 keeps charges with the cgroup where memory was allocated, so pages of exited processes and of processes that moved into their own scope stay on it.

Shared memory is counted per process by its PSS share (`Pss_Shmem` from `smaps_rollup`), but
only for processes with more than 32 MB RssShmem. `smaps_rollup` costs up to ~100 ms for a
process with gigabytes mapped (an LLM server holding 7 GB of CUDA host memory took 84 ms), so
the value is cached until RssShmem moves by 5% or 20 samples pass. Without this, one sample
cost ~250 ms of system time.

## GPUs other than NVIDIA (src/gpu.rs)

NVIDIA goes through NVML. Every other DRM driver (amdgpu, i915, xe, ...) is read the way nvtop does it: device totals from sysfs (`mem_info_vram_used/total`, `mem_info_gtt_used/total`, `gpu_busy_percent` where the driver has it), per process from `/proc/<pid>/fdinfo/<fd>` of the fds that point at `/dev/dri/*`. Memory is `drm-resident-vram` + `drm-resident-gtt` (older kernels: `drm-memory-*`); GPU% is the busiest engine's `drm-engine-<name>` ns delta over the interval, divided by `drm-engine-capacity-<name>`. Several fds of one process can share a DRM client, so clients are counted once per (`drm-pdev`, `drm-client-id`).

GTT is system RAM the driver maps for the GPU. It is not charged to the memory cgroup (no GPU/TTM field in `memory.stat`) and not part of RssAnon/RssShmem, so adding it to the VRAM column counts nothing twice; only the summary RAM meter includes it, as all used RAM. Buffers shared between a program and the compositor are counted for both, as in nvtop.

On i915/xe (and other drivers with shmem-backed GEM objects) GPU buffers are shmem, charged to the memory cgroup of the process that allocated them. There they show both in the VRAM column (fdinfo `system0`) and in the cgroup's memory; for programs lifted out of a login session, whose own rows count mapped shmem only, they land in the session's "other" row as unattributed shared memory (Hyprland on an HD 520: 160 MB shmem in the session, 129 MB `system0` for Hyprland itself).

Listing a process's fds is the costly part, so the list of DRM fds is cached per process and rescanned every 15 samples. Only processes whose `/proc/<pid>/io` is readable are scanned: the same permission check guards fdinfo. Cards bound to `nvidia*` are skipped, so a machine with only an NVIDIA GPU does no fd scanning at all. On an AMD laptop with ~490 processes apptop used 1 tick of CPU in 20 s.

## Grouping rules (src/model.rs)

- A unit is the first `.service`/`.scope` in the cgroup path (`user@<uid>.service` is descended
  into), or an `app-*.slice` directly under `app.slice`. The root cgroup is "Kernel".
- App units are named from their desktop entry (`app-<desktop-id>@<hex>.service`,
  `app-<desktop-id>-<pid>.scope`), then by the Exec binary, then by process name. Services use
  the unit name plus its `Description=` as detail. Units with the same name merge into one row
  ("N instances").
- Chromium browsers move the main process into its own `app-com.vivaldi.Vivaldi-<pid>.scope`
  (or the Chrome/Chromium equivalent) while renderers stay in the launcher's unit. Units are
  tied to a profile by walking a chromium process's parents up to the main process and taking
  its `--user-data-dir`; all units of one profile merge, named from whichever has a desktop
  entry, so each profile launcher gets its own row.
- Chromium, npm and others rewrite argv into one space-separated string; a single-element
  cmdline containing spaces that is not an existing path is split. An interpreter whose argv0 is
  not the interpreter ("npm run dev") is named by that title.
- Terminals (yakuake, konsole, vte-spawn scopes, ...): every process under an interactive shell,
  zellij client or zellij server is a job. By default jobs become top-level rows of their own,
  merged by name ("Claude Code ×27", children by working directory); `t` or `--split` keeps
  them under the terminal, grouped by zellij session. A job is named after its first Claude Code
  or ComfyUI process, else its top process; a GUI app started from a shell takes its desktop
  name so it merges with the menu-started instance.
- A process in a terminal's cgroup whose parent sits in another, non-terminal unit belongs to
  that unit (a Chrome started by a CLI tool moves its main process into its own scope while its
  children stay in the terminal tab's scope).
- GNOME, Flatpak and uwsm put the launcher before the desktop id (`app-gnome-<id>-<pid>.scope`, `app-flatpak-<id>-<n>.scope`); when the full id has no desktop entry, the first segment is dropped and the lookup repeated. D-Bus activated units without a desktop entry are named by their bus name. Services with reverse-DNS names (`org.gnome.SettingsDaemon.Power`) take their `Description=` as name and the unit name as detail.
- Flatpak starts sandboxed helpers (zypak, `flatpak-spawn --sandbox`) in scopes of their own, so all scopes of one app id merge into one row, named "<app> (Flatpak)" so it does not merge with a native install of the same app. Instance labels skip bwrap's own arguments (everything up to `--`).
- GNOME Terminal (`vte-spawn-*`), Ptyxis (`ptyxis-spawn-*`) and kitty (`kitty-<pid>-<n>`) run every tab's shell in a scope of its own. Those processes are moved into the unit of the process that spawned the shell (the terminal), wherever that runs (an app scope or a login session), so the terminal rules apply to them there. All units of one terminal program merge into its row. Console (kgx), Ptyxis and kitty's `kitten` helpers count as terminal processes.
- Graphical login sessions (`session-N.scope` with logind `TYPE=x11|wayland`) are containers when the desktop does not give apps scopes of their own (XFCE, sway, i3, Cinnamon, ...). Session plumbing (display-manager worker, `xfce4-session`, `startxfce4`, `dbus-launch`, `ssh-agent`, `sh -c` wrappers, interactive shells) stays in the session row; every other process becomes a program row, keeping children of the same executable (and xfce4-panel's `wrapper-2.0` plugins, named by their display name). A terminal inside a session follows the terminal rules. A session scope is never itself treated as a terminal, even when a terminal runs in it. Other sessions (SSH, console) are named from `/run/systemd/sessions/<id>`.
- Docker scopes are named through the socket (`/containers/json`, `DOCKER_HOST` if it is a unix
  socket) and grouped by compose project.

## Layout

Data columns first, the name last (like htop's Command column). With the name on the left and
the numbers right-aligned, a wide terminal left a gap that made rows hard to follow.

## Stopping programs (`k` / F9, src/kill.rs)

The confirmation panel is built from live state when it opens: cgroup targets are re-read from
`cgroup.procs`, process targets are (pid, start time) pairs and are dropped if the pid now
belongs to another process. It names the row kind and detail, since a filtered view can leave
the selection on something unexpected (a filter for "tail" matched a `tail` inside a docker
container during testing).

| Row | SIGTERM | SIGKILL |
|---|---|---|
| app / service in the user's cgroups | kill(2) to every process in the cgroup | write `1` to `cgroup.kill` |
| terminal job, process row, zellij session | kill(2) to the listed processes | same with SIGKILL |
| docker container / compose group | `docker stop` in a background thread | `docker kill` |
| system cgroup, other user's process | refused (no sudo) | refused |

Other signals (SIGHUP, SIGINT, SIGQUIT, SIGSTOP, SIGCONT, SIGTSTP, SIGUSR1, SIGUSR2) come from a drop-down in the panel; they go to every process with kill(2) (never `cgroup.kill`, which only sends SIGKILL) and to containers as `docker kill --signal`. A signal sent to a stopped process stays pending until SIGCONT, so SIGTERM, SIGINT, SIGQUIT and SIGHUP are followed by SIGCONT, which is what systemd does when it stops a unit. Found while testing: SIGTERM to a process paused with SIGSTOP did nothing until SIGCONT arrived.

Group rows ("Claude Code ×26") are allowed; the panel states how many programs, processes and
containers stop, and warns when apptop's own ancestry (its terminal) is included.

Tested on a throwaway `tail -f /dev/null` (SIGTERM, process target) and on a
`systemd-run --user --scope -u apptop-killtest` scope (SIGKILL through `cgroup.kill`).

## Language

`src/i18n.rs`: `tr("English", "magyar")` at each string, `count()` for plurals. The language
comes from `--lang`, else `LC_ALL` / `LC_MESSAGES` / `LANG` (a `hu` prefix selects Hungarian).

## Screenshot

`docs/screenshot.png` shows the built-in demo data (`APPTOP_DEMO=1`, `src/demo.rs`), never a
real machine. `scripts/screenshot.sh` runs the demo in a private tmux server, expands the
Claude Code row and converts `tmux capture-pane -e` output to `docs/screenshot.html` with
`scripts/ansi2html.py`; the PNG is a 2x browser screenshot of that page's terminal box.

## Settings

`~/.config/apptop/config` keeps sort column, direction, view mode and the details panel,
written on quit. `--split` overrides the saved view mode for that run.

## Cost

About 2-3% of one core at the default 2 s interval, ~40 MB RSS, measured with ~2100 processes
and 240 cgroups.
