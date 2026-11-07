# Taskd

A fast, lightweight task manager for Linux. It's a single ~8 MB native binary with no runtime dependencies beyond libc and OpenGL.

## Features

- **Apps**: running applications with each one's processes grouped together, and totals for CPU, memory, GPU and disk. Apps are recognized by their systemd app scope (Flatpaks, VS Code, apps launched by GNOME/KDE) or by matching their executable to an installed `.desktop` entry (menu launches on Cinnamon and others). Expand an app to see its processes, or end, kill, suspend or resume the whole app.
- **Processes**: a sortable, virtualized table. You can filter by name, command, user or PID, show a tree view, hide kernel threads, show only your own processes, and choose which columns appear. Each process shows CPU, memory (private), GPU, RSS, disk read/write rates, threads, nice value, state, how long it has been running, and its command line. Busy CPU and memory cells are tinted.
- **Process actions**: End (TERM), Kill (KILL), Suspend/Resume (STOP/CONT), HUP and INT, priority (nice), and copying the PID, name or command. If permission is denied, Taskd offers to retry through `pkexec`.
- **Process details**: the executable, working directory, command line, cgroup, open fds, fd limit, environment size, memory breakdown and context switches.
- **Performance**: live graphs for CPU (overall or per logical processor, plus frequency, temperature, load, context switches), memory (composition and swap), each disk (active time, read/write throughput), each network interface (receive/send, link speed) and each GPU (NVIDIA via NVML; AMD/Intel via sysfs). It shows utilization, VRAM, temperature and power. Below the graphs is a **top 10 apps & services** list for the selected resource (CPU, memory, GPU, disk).
- **Per-process GPU usage**: NVML per-process samples on NVIDIA; DRM `fdinfo` engine counters on AMD/Intel.
- **Users**: CPU, memory and disk usage per user. Double-click a user to filter the Processes tab to them.
- **Services**: systemd system and user services. You can filter, start, stop, restart, enable and disable them; polkit prompts when needed.
- **Startup**: XDG autostart entries. Toggling one writes a per-user override and never edits system files.
- **Auto-update**: at launch Taskd checks GitHub for a newer release (one API request). Tarball installs download it, verify it against `SHA256SUMS`, check that the new binary runs, swap it in atomically, and restart, all automatically. `.deb` installs show an **Update & restart** banner, which asks for your password. Use the ⚙ menu to turn the launch check off or check now, or run `taskdm --update` from a terminal.
- **Settings**: refresh interval (250 ms to 10 s), pause, and remembered UI state (`~/.config/taskd/config`).

- **MCP server for local LLM agents**: `taskdm --mcp` lets an agent inspect the system (see [below](#mcp-server-for-llm-agents)).

### Keyboard

| Key | Action |
| --- | --- |
| `Ctrl+1`…`Ctrl+6` | Switch tab |
| `Ctrl+F` | Focus the process filter |
| `↑` / `↓` | Move selection |
| `Enter` | Process details |
| `Delete` / `Shift+Delete` | End / kill the selected process or app |
| `Ctrl+P` | Pause or resume updates |
| `F5` | Refresh now |

## Performance

Taskd is built to cost almost nothing while it runs:

- **It reads `/proc` and `/sys` directly** instead of using a general-purpose system-info crate. Parsing works on byte slices in reused buffers, so there's no per-tick allocation in the hot paths.
- **It keeps fds open and uses `pread`.** procfs regenerates a file's content on every read at offset 0, so Taskd keeps each process's `stat`/`statm`/`io` fds open and re-reads them with a single `pread`. That replaces `open`+`read`+`close` and skips the costly `/proc/<pid>` path lookup. System files (`/proc/stat`, `meminfo`, cpufreq, hwmon, …) get the same treatment. Std's `read_to_end` was avoided too, because it adds `statx`+`lseek` to every file.
- **Sampling runs on its own thread.** The UI only reads the latest immutable snapshot (an `Arc` swap), so a slow frame never delays sampling and vice versa.
- **It repaints only when there's new data or input.** There's no continuous render loop and exactly one frame per sample. egui paints two frames for a zero-delay repaint request, so the sampler requests a 1 ms delay instead.
- **The process table is virtualized.** Only visible rows are laid out, and the sorted/filtered/tree order is cached per snapshot.
- **GPU telemetry loads NVML with `dlopen`** at runtime, so there's no link dependency. NVML's per-process query costs 1–3 ms, so it runs at most every ~2 s and its values are reused in between (NVML buffers samples, so nothing is lost). AMD/Intel DRM fds are found by a round-robin scan that only runs when such a GPU is present.
- **Grouping into apps and services only runs while the Apps or Performance tab is visible**, once per snapshot. Services and startup entries load only when you open their tab.

Measured on a Ryzen 9 5900X with ~590 processes:

| | |
| --- | --- |
| Sampler cost per tick | **~3.3 ms** (was 8.8 ms before the pread/fd-cache work) |
| Whole app, window visible, 1 s interval | **~1% of one core** |
| Private memory | **~33 MB** (RSS ~95 MB, mostly the GL driver) |

Run the benchmark yourself:

```sh
taskdm --bench 300   # headless sampler benchmark
taskdm --once        # one-shot text summary
```

## MCP server for LLM agents

`taskdm --mcp` runs Taskd headless as a [Model Context Protocol](https://modelcontextprotocol.io) server on stdio. Any MCP-capable agent (LM Studio, Ollama front-ends, Goose, Open WebUI via mcpo, Claude Desktop, …) can then look through your running tasks for problems. It uses the same sampler as the GUI, so the agent gets current rates, recent history, and processes grouped into apps and services, not one-off `ps` output (whose CPU% is a lifetime average).

| Tool | What it gives the agent |
| --- | --- |
| `health_check` | Findings with severity and the apps responsible: CPU saturation, single-core bottlenecks, memory pressure, swapping, I/O wait, busy disks, GPU load/heat, temperatures, runaway or memory-hogging processes, zombie/stuck processes, failed services. **Start here.** |
| `system_overview` | CPU, memory/swap, disks, network, GPUs, load, uptime |
| `top_consumers` | Top apps/services (or processes) by CPU, memory, GPU or disk I/O |
| `list_processes` | Filter by text, user or state; sort and limit |
| `process_details` | Executable, command, parents/children, cgroup, memory breakdown, open files, limits |
| `resource_history` | Min/avg/max/latest and a downsampled series for the last ~2 minutes |
| `failed_services`, `service_logs` | Failed systemd units and their recent journal lines |

**It's read-only by default.** The action tools `signal_process` (end/kill/suspend/resume) and `set_priority` only exist when you start it with `--mcp-allow-actions`; otherwise they aren't even offered to the model. Even then, PID 1, kernel threads and Taskd itself are refused, and you can't act on other users' processes without root. Nothing listens on the network. Command lines are included (truncated) and environment variables never are.

Client configuration (the usual `mcpServers` format; use the full path, since GUI apps often don't have `~/.local/bin` on their `PATH`):

```json
{
  "mcpServers": {
    "taskd": { "command": "/home/YOU/.local/bin/taskdm", "args": ["--mcp"] }
  }
}
```

For a `.deb` install the command is `/usr/bin/taskdm`. Add `"--mcp-allow-actions"` to `args` only if you want the agent to be able to end processes.

## Install

The package is called **`taskd-monitor`** and the command is **`taskdm`**. Ubuntu/Debian already ship an unrelated `taskd` package (the Taskwarrior sync server).

From a release, either install the `.deb`:

```sh
sudo apt install ./taskd-monitor_*_amd64.deb
```

or use the tarball (installs to `~/.local` by default, no root needed):

```sh
tar xzf taskd-monitor-*-x86_64-linux.tar.gz && ./taskd-monitor-*/install.sh
```

From source:

```sh
cargo build --release
./target/release/taskdm
```

Taskd runs on X11 and Wayland. It needs no `-dev` packages to build, because the windowing libraries are loaded at runtime.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

| Path | What |
| --- | --- |
| `src/sampler.rs` | `/proc` + `/sys` sampling, snapshots, sampler thread |
| `src/gpu.rs` | NVML (dlopen) and amdgpu sysfs |
| `src/util.rs` | Raw/pread file readers, byte parsers, formatting |
| `src/groups.rs` | Grouping processes into apps (desktop entries/app scopes) and services |
| `src/mcp.rs` | MCP server (stdio JSON-RPC): tools, health-check heuristics |
| `src/updater.rs` | Self-update from GitHub releases |
| `src/actions.rs` | Signals, renice, pkexec fallback, process details |
| `src/services.rs`, `src/startup.rs` | systemd and XDG autostart |
| `src/app.rs` | Window, tabs, status bar, background jobs |
| `src/ui_*.rs`, `src/graph.rs` | Tabs and the hand-painted graph widget |

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, a release build and the sampler benchmark on our self-hosted Unraid runner (`[self-hosted, linux, unraid]`). Pushing a `v*` tag that matches the version in `Cargo.toml` builds the tarball and `.deb` and publishes a GitHub release (`release.yml`).

## License

MIT
