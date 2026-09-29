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
