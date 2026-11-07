# Taskd

A fast, lightweight task manager for Linux. It's a single ~8 MB native binary with no runtime dependencies beyond libc and OpenGL.

## Features

- **Processes**: a sortable, virtualized table. You can filter by name, command, user or PID, show a tree view, hide kernel threads, show only your own processes, and choose which columns appear. Each process shows CPU, memory (private), RSS, disk read/write rates, threads, nice value, state, how long it has been running, and its command line. Busy CPU and memory cells are tinted.
- **Process actions**: End (TERM), Kill (KILL), Suspend/Resume (STOP/CONT), HUP and INT, priority (nice), and copying the PID, name or command. If permission is denied, Taskd offers to retry through `pkexec`.
- **Process details**: the executable, working directory, command line, cgroup, open fds, fd limit, environment size, memory breakdown and context switches.
- **Performance**: live graphs for CPU (overall or per logical processor, plus frequency, temperature, load, context switches), memory (composition and swap), each disk (active time, read/write throughput), each network interface (receive/send, link speed) and each GPU (NVIDIA via NVML; AMD/Intel via sysfs). It shows utilization, VRAM, temperature and power.
- **Users**: CPU, memory and disk usage per user. Double-click a user to filter the Processes tab to them.
- **Services**: systemd system and user services. You can filter, start, stop, restart, enable and disable them; polkit prompts when needed.
- **Startup**: XDG autostart entries. Toggling one writes a per-user override and never edits system files.
- **Settings**: refresh interval (250 ms to 10 s), pause, and remembered UI state (`~/.config/taskd/config`).

### Keyboard

| Key | Action |
| --- | --- |
| `Ctrl+1`…`Ctrl+5` | Switch tab |
| `Ctrl+F` | Focus the process filter |
| `↑` / `↓` | Move selection |
| `Enter` | Process details |
| `Delete` / `Shift+Delete` | End / kill the selected process |
| `Ctrl+P` | Pause or resume updates |
| `F5` | Refresh now |

## Performance

Taskd is built to cost almost nothing while it runs:

- **It reads `/proc` and `/sys` directly** instead of using a general-purpose system-info crate. Parsing works on byte slices in reused buffers, so there's no per-tick allocation in the hot paths.
- **It keeps fds open and uses `pread`.** procfs regenerates a file's content on every read at offset 0, so Taskd keeps each process's `stat`/`statm`/`io` fds open and re-reads them with a single `pread`. That replaces `open`+`read`+`close` and skips the costly `/proc/<pid>` path lookup. System files (`/proc/stat`, `meminfo`, cpufreq, hwmon, …) get the same treatment. Std's `read_to_end` was avoided too, because it adds `statx`+`lseek` to every file.
- **Sampling runs on its own thread.** The UI only reads the latest immutable snapshot (an `Arc` swap), so a slow frame never delays sampling and vice versa.
- **It repaints only when there's new data or input.** There's no continuous render loop and exactly one frame per sample. egui paints two frames for a zero-delay repaint request, so the sampler requests a 1 ms delay instead.
- **The process table is virtualized.** Only visible rows are laid out, and the sorted/filtered/tree order is cached per snapshot.
- **GPU telemetry loads NVML with `dlopen`** at runtime, so there's no link dependency. Services and startup entries load only when you open their tab.

Measured on a Ryzen 9 5900X with ~590 processes:

| | |
| --- | --- |
| Sampler cost per tick | **~3.3 ms** (was 8.8 ms before the pread/fd-cache work) |
| Whole app, window visible, 1 s interval | **~1% of one core** |
| Private memory | **~33 MB** (RSS ~95 MB, mostly the GL driver) |

Run the benchmark yourself:

```sh
taskd --bench 300   # headless sampler benchmark
taskd --once        # one-shot text summary
```

## Install

From a release, either install the `.deb`:

```sh
sudo apt install ./taskd_*_amd64.deb
```

or use the tarball (installs to `~/.local` by default, no root needed):

```sh
tar xzf taskd-*-x86_64-linux.tar.gz && ./taskd-*/install.sh
```

From source:

```sh
cargo build --release
./target/release/taskd
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
| `src/actions.rs` | Signals, renice, pkexec fallback, process details |
| `src/services.rs`, `src/startup.rs` | systemd and XDG autostart |
| `src/app.rs` | Window, tabs, status bar, background jobs |
| `src/ui_*.rs`, `src/graph.rs` | Tabs and the hand-painted graph widget |

CI (`.github/workflows/ci.yml`) runs fmt, clippy, tests, a release build and the sampler benchmark on our self-hosted Unraid runner (`[self-hosted, linux, unraid]`). Pushing a `v*` tag that matches the version in `Cargo.toml` builds the tarball and `.deb` and publishes a GitHub release (`release.yml`).

## License

MIT
