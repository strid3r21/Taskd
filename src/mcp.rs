//! MCP (Model Context Protocol) server over stdio: `taskdm --mcp [--mcp-allow-actions]`.
//!
//! Lets a local LLM agent inspect the system through structured, pre-aggregated data
//! (current rates, history, app/service grouping) instead of parsing `ps`/`top` output.
//! Read-only by default; process actions are only *advertised* with `--mcp-allow-actions`.
//!
//! Transport: newline-delimited JSON-RPC 2.0 on stdin/stdout. Logs go to stderr only.

use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::actions::{self, ActionError, Signal};
use crate::groups::{Group, Grouper};
use crate::sampler::{self, History, ProcInfo, Shared, Snapshot, now_epoch};
use crate::util::{fmt_bytes, fmt_duration, fmt_rate};

const PROTOCOL_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26", "2024-11-05"];
const CMD_MAX: usize = 300;

pub struct Server {
    shared: Arc<Shared>,
    grouper: Grouper,
    my_uid: u32,
    allow_actions: bool,
    started: Instant,
}

pub fn run(allow_actions: bool) -> i32 {
    let shared = sampler::spawn(1000, || {});
    let mut server = Server {
        shared,
        grouper: Grouper::default(),
        // SAFETY: getuid never fails.
        my_uid: unsafe { libc::getuid() },
        allow_actions,
        started: Instant::now(),
    };
    eprintln!(
        "taskdm MCP server {} on stdio ({})",
        env!("CARGO_PKG_VERSION"),
        if allow_actions { "process actions ENABLED" } else { "read-only" }
    );
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.iter().filter_map(|m| server.handle(m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => server.handle(&msg),
            Err(e) => Some(error_reply(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(r) = reply {
            if writeln!(out, "{r}").and_then(|_| out.flush()).is_err() {
                break;
            }
        }
    }
    0
}

fn error_reply(id: Value, code: i64, msg: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
}

fn tool_text(v: Value, is_error: bool) -> Value {
    let text = serde_json::to_string_pretty(&v).unwrap_or_default();
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

impl Server {
    /// Handle one JSON-RPC message; `None` for notifications.
    pub fn handle(&mut self, msg: &Value) -> Option<Value> {
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        let id = id?; // notifications (no id) get no reply
        let result = match method {
            "initialize" => {
                let asked = params["protocolVersion"].as_str().unwrap_or("");
                let version = if PROTOCOL_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSIONS[0] };
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": "taskd", "title": "Taskd", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                })
            }
            "ping" => json!({}),
            "tools/list" => json!({"tools": self.tool_list()}),
            "tools/call" => {
                let name = params["name"].as_str().unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match self.call(name, &args) {
                    Ok(v) => tool_text(v, false),
                    Err(e) => tool_text(json!({"error": e}), true),
                }
            }
            _ => return Some(error_reply(id, -32601, &format!("method not found: {method}"))),
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }

    fn tool_list(&self) -> Vec<Value> {
        let ro = json!({"readOnlyHint": true, "openWorldHint": false});
        let mut tools = vec![
            json!({
                "name": "health_check",
                "title": "Health check",
                "description": "Analyse the system for performance problems right now and over the last minutes: CPU saturation, single-core bottlenecks, memory pressure, swapping, I/O wait, busy disks, GPU load/heat, temperatures, runaway or memory-hogging processes, zombie/stuck processes and failed services. Returns findings with severity and the apps/processes responsible. Start here when asked why the computer is slow.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": ro,
            }),
            json!({
                "name": "system_overview",
                "title": "System overview",
                "description": "Current CPU, memory/swap, disks, network, GPUs, load average, uptime and process/thread counts.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": ro,
            }),
            json!({
                "name": "top_consumers",
                "title": "Top resource consumers",
                "description": "The apps, services or processes using the most of a resource. group_by='app' (default) groups processes into applications and systemd services; 'process' lists individual processes.",
                "inputSchema": {"type": "object", "properties": {
                    "resource": {"type": "string", "enum": ["cpu", "memory", "gpu", "disk_io"]},
                    "group_by": {"type": "string", "enum": ["app", "process"], "default": "app"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 50, "default": 10}
                }, "required": ["resource"]},
                "annotations": ro,
            }),
            json!({
                "name": "list_processes",
                "title": "List processes",
                "description": "Processes with CPU, memory, GPU, disk I/O, state and command line. Filter by text (name/command), user or state; sort and limit.",
                "inputSchema": {"type": "object", "properties": {
                    "filter": {"type": "string", "description": "Case-insensitive substring of name or command line"},
                    "user": {"type": "string"},
                    "state": {"type": "string", "description": "Process state letter, e.g. R (running), D (disk wait), Z (zombie), T (stopped)"},
                    "sort_by": {"type": "string", "enum": ["cpu", "memory", "gpu", "disk_io", "pid", "threads", "age"], "default": "cpu"},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 200, "default": 25},
                    "include_kernel_threads": {"type": "boolean", "default": false}
                }},
                "annotations": ro,
            }),
            json!({
                "name": "process_details",
                "title": "Process details",
                "description": "Everything about one process: resources, executable, working dir, command line, parent chain, children, cgroup/unit, memory breakdown, open files, limits and context switches.",
                "inputSchema": {"type": "object", "properties": {"pid": {"type": "integer"}}, "required": ["pid"]},
                "annotations": ro,
            }),
            json!({
                "name": "resource_history",
                "title": "Resource history",
                "description": "Recent history (up to ~2 minutes at 1 s) of a resource: min/avg/max/latest plus a downsampled series, to tell spikes from sustained load.",
                "inputSchema": {"type": "object", "properties": {
                    "resource": {"type": "string", "enum": ["cpu", "memory", "swap", "gpu", "disk", "network"]},
                    "device": {"type": "string", "description": "Disk name (e.g. nvme0n1), interface (e.g. enp6s0) or GPU index; default: all"}
                }, "required": ["resource"]},
                "annotations": ro,
            }),
            json!({
                "name": "failed_services",
                "title": "Failed services",
                "description": "systemd system and user units currently in the failed state.",
                "inputSchema": {"type": "object", "properties": {}},
                "annotations": ro,
            }),
            json!({
                "name": "service_logs",
                "title": "Service logs",
                "description": "Recent journal lines for a systemd unit (e.g. to see why it failed or what it is doing).",
                "inputSchema": {"type": "object", "properties": {
                    "unit": {"type": "string"},
                    "user": {"type": "boolean", "default": false, "description": "A user (--user) unit"},
                    "lines": {"type": "integer", "minimum": 1, "maximum": 200, "default": 40}
                }, "required": ["unit"]},
                "annotations": ro,
            }),
        ];
        if self.allow_actions {
            tools.push(json!({
                "name": "signal_process",
                "title": "Signal a process",
                "description": "Send a signal to a process: term (ask to quit), kill (force), stop (suspend), cont (resume), hup, int. Only use when the user explicitly asked for it.",
                "inputSchema": {"type": "object", "properties": {
                    "pid": {"type": "integer"},
                    "signal": {"type": "string", "enum": ["term", "kill", "stop", "cont", "hup", "int"]}
                }, "required": ["pid", "signal"]},
                "annotations": {"readOnlyHint": false, "destructiveHint": true, "openWorldHint": false},
            }));
            tools.push(json!({
                "name": "set_priority",
                "title": "Set process priority",
                "description": "Change a process's nice value (-20 highest .. 19 lowest). Raising priority (negative values) normally needs root and will fail. Only use when the user explicitly asked for it.",
                "inputSchema": {"type": "object", "properties": {
                    "pid": {"type": "integer"},
                    "nice": {"type": "integer", "minimum": -20, "maximum": 19}
                }, "required": ["pid", "nice"]},
                "annotations": {"readOnlyHint": false, "destructiveHint": false, "openWorldHint": false},
            }));
        }
        tools
    }

    /// Latest snapshot; right after start, wait briefly so rates cover a full interval.
    fn snapshot(&mut self) -> Arc<Snapshot> {
        let deadline = self.started + Duration::from_millis(2500);
        loop {
            let s = self.shared.latest();
            if s.generation >= 3 || Instant::now() >= deadline {
                self.grouper.update(&s, self.my_uid);
                return s;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    pub fn call(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        match name {
            "health_check" => Ok(self.health_check()),
            "system_overview" => Ok(self.overview()),
            "top_consumers" => self.top_consumers(args),
            "list_processes" => Ok(self.list_processes(args)),
            "process_details" => self.process_details(args),
            "resource_history" => self.history(args),
            "failed_services" => Ok(failed_services()),
            "service_logs" => service_logs(args),
            "signal_process" | "set_priority" if !self.allow_actions => {
                Err("process actions are disabled; start the server with --mcp-allow-actions".into())
            }
            "signal_process" => self.signal(args),
            "set_priority" => self.set_priority(args),
            _ => Err(format!("unknown tool: {name}")),
        }
    }

    // ------------------------------------------------------------ helpers

    fn group_of(&self) -> HashMap<u32, &Group> {
        let mut m = HashMap::new();
        for g in &self.grouper.groups {
            for &i in &g.members {
                m.insert(i, g);
            }
        }
        m
    }

    fn proc_json(&self, s: &Snapshot, i: usize, groups: &HashMap<u32, &Group>) -> Value {
        let p = &s.procs[i];
        let ncpu = s.cpu.logical.max(1) as f32;
        let since_boot = now_epoch().saturating_sub(s.boot_time) as f64;
        let mut cmd = p.cmd.to_string();
        if cmd.len() > CMD_MAX {
            let mut cut = CMD_MAX;
            while !cmd.is_char_boundary(cut) {
                cut -= 1;
            }
            cmd.truncate(cut);
            cmd.push('…');
        }
        let g = groups.get(&(i as u32));
        json!({
            "pid": p.pid,
            "ppid": p.ppid,
            "name": &*p.name,
            "user": &*p.user,
            "state": state_name(p.state),
            "cpu_percent_of_machine": round1(p.cpu),
            "cpu_percent_of_one_core": round1(p.cpu * ncpu),
            "memory": fmt_bytes(p.mem),
            "memory_bytes": p.mem,
            "gpu_percent": round1(p.gpu),
            "disk_read": fmt_rate(p.read_rate),
            "disk_write": fmt_rate(p.write_rate),
            "threads": p.threads,
            "nice": p.nice,
            "running_for": fmt_duration((since_boot - p.start).max(0.0) as u64),
            "belongs_to": g.map(|g| format!("{} ({})", g.name, g.kind.label().to_lowercase())),
            "command": if cmd.is_empty() { Value::Null } else { Value::String(cmd) },
        })
    }

    fn group_json(&self, s: &Snapshot, g: &Group, resource: &str) -> Value {
        let mut members: Vec<&ProcInfo> = g.members.iter().map(|&i| &s.procs[i as usize]).collect();
        members.sort_by(|a, b| metric(b, resource).total_cmp(&metric(a, resource)));
        json!({
            "name": g.name,
            "kind": g.kind.label().to_lowercase(),
            "processes": g.members.len(),
            "cpu_percent_of_machine": round1(g.cpu),
            "memory": fmt_bytes(g.mem),
            "memory_bytes": g.mem,
            "gpu_percent": round1(g.gpu),
            "disk_io": fmt_rate(g.read + g.write),
            "top_processes": members.iter().take(5).map(|p| json!({
                "pid": p.pid, "name": &*p.name,
                "cpu_percent_of_machine": round1(p.cpu), "memory": fmt_bytes(p.mem)
            })).collect::<Vec<_>>(),
        })
    }

    // ------------------------------------------------------------ tools

    fn overview(&mut self) -> Value {
        let s = self.snapshot();
        let m = &s.mem;
        json!({
            "cpu": {
                "model": s.cpu.model,
                "cores": s.cpu.physical, "logical_processors": s.cpu.logical,
                "usage_percent": round1(s.cpu.usage),
                "user_percent": round1(s.cpu.user), "system_percent": round1(s.cpu.system),
                "iowait_percent": round1(s.cpu.iowait),
                "busiest_core_percent": round1(s.cpu.per_core.iter().copied().fold(0.0, f32::max)),
                "frequency_ghz": s.cpu.freq_mhz.map(|f| round2(f / 1000.0)),
                "temperature_c": s.cpu.temp_c.map(round1),
                "load_average_1_5_15": s.load,
                "context_switches_per_s": s.cpu.ctxt_rate.round(),
            },
            "memory": {
                "total": fmt_bytes(m.total), "used": fmt_bytes(m.used), "available": fmt_bytes(m.available),
                "used_percent": round1(pct(m.used, m.total)),
                "cached": fmt_bytes(m.cached), "dirty": fmt_bytes(m.dirty),
                "swap_total": fmt_bytes(m.swap_total), "swap_used": fmt_bytes(m.swap_used),
            },
            "disks": s.disks.iter().map(|d| json!({
                "device": d.name, "model": d.model, "type": if d.rotational { "HDD" } else { "SSD" },
                "size": fmt_bytes(d.size), "active_percent": round1(d.busy),
                "read": fmt_rate(d.read_rate), "write": fmt_rate(d.write_rate),
            })).collect::<Vec<_>>(),
            "network": s.nets.iter().map(|n| json!({
                "interface": n.name, "receive": fmt_rate(n.rx_rate), "send": fmt_rate(n.tx_rate),
                "link_mbps": n.speed_mbps,
            })).collect::<Vec<_>>(),
            "gpus": s.gpus.iter().enumerate().map(|(i, g)| json!({
                "index": i, "name": g.name, "usage_percent": g.util.map(round1),
                "memory_used": g.mem_used.map(fmt_bytes), "memory_total": g.mem_total.map(fmt_bytes),
                "temperature_c": g.temp_c.map(round1), "power_w": g.power_w.map(round1),
            })).collect::<Vec<_>>(),
            "processes": s.procs.len(),
            "threads": s.threads,
            "running_now": s.running,
            "uptime": fmt_duration(s.uptime as u64),
        })
    }

    fn top_consumers(&mut self, args: &Value) -> Result<Value, String> {
        let resource = args["resource"].as_str().ok_or("resource is required")?;
        if !["cpu", "memory", "gpu", "disk_io"].contains(&resource) {
            return Err(format!("unknown resource: {resource}"));
        }
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let s = self.snapshot();
        if args["group_by"].as_str() == Some("process") {
            let groups = self.group_of();
            let mut idx: Vec<usize> = (0..s.procs.len()).collect();
            idx.sort_by(|&a, &b| metric(&s.procs[b], resource).total_cmp(&metric(&s.procs[a], resource)));
            let list: Vec<Value> = idx.into_iter().take(limit).map(|i| self.proc_json(&s, i, &groups)).collect();
            return Ok(json!({"resource": resource, "processes": list}));
        }
        let mut gs: Vec<&Group> = self.grouper.groups.iter().collect();
        gs.sort_by(|a, b| group_metric(b, resource).total_cmp(&group_metric(a, resource)));
        let list: Vec<Value> = gs.into_iter().take(limit).map(|g| self.group_json(&s, g, resource)).collect();
        let mut out = json!({"resource": resource, "consumers": list});
        if resource == "disk_io" && self.my_uid != 0 {
            out["note"] = json!("Disk I/O of other users' processes (most services) is only visible to root.");
        }
        Ok(out)
    }

    fn list_processes(&mut self, args: &Value) -> Value {
        let s = self.snapshot();
        let groups = self.group_of();
        let filter = args["filter"].as_str().unwrap_or("").to_lowercase();
        let user = args["user"].as_str();
        let state = args["state"].as_str().and_then(|x| x.bytes().next()).map(|b| b.to_ascii_uppercase());
        let kernel = args["include_kernel_threads"].as_bool().unwrap_or(false);
        let limit = args["limit"].as_u64().unwrap_or(25).clamp(1, 200) as usize;
        let sort = args["sort_by"].as_str().unwrap_or("cpu");
        let mut idx: Vec<usize> = (0..s.procs.len())
            .filter(|&i| {
                let p = &s.procs[i];
                (kernel || !p.kernel)
                    && user.is_none_or(|u| *p.user == *u)
                    && state.is_none_or(|st| p.state.to_ascii_uppercase() == st)
                    && (filter.is_empty()
                        || p.name.to_lowercase().contains(&filter)
                        || p.cmd.to_lowercase().contains(&filter))
            })
            .collect();
        let total = idx.len();
        idx.sort_by(|&a, &b| {
            let (pa, pb) = (&s.procs[a], &s.procs[b]);
            match sort {
                "pid" => pa.pid.cmp(&pb.pid),
                "threads" => pb.threads.cmp(&pa.threads),
                "age" => pa.start.total_cmp(&pb.start),
                r => metric(pb, r).total_cmp(&metric(pa, r)),
            }
        });
        let list: Vec<Value> = idx.into_iter().take(limit).map(|i| self.proc_json(&s, i, &groups)).collect();
        json!({"matching": total, "shown": list.len(), "sorted_by": sort, "processes": list})
    }

    fn process_details(&mut self, args: &Value) -> Result<Value, String> {
        let pid = args["pid"].as_u64().ok_or("pid is required")? as u32;
        let s = self.snapshot();
        let groups = self.group_of();
        let i = s.procs.iter().position(|p| p.pid == pid).ok_or_else(|| format!("no process with pid {pid}"))?;
        let mut v = self.proc_json(&s, i, &groups);
        let d = actions::read_details(pid);
        v["command"] = json!(d.cmdline);
        v["executable"] = json!(d.exe);
        v["working_dir"] = json!(d.cwd);
        v["cgroup"] = json!(d.cgroup);
        v["unit"] = json!(s.procs[i].unit.as_deref());
        v["open_files"] = json!(d.open_fds);
        v["open_files_limit"] = json!(d.limits_open_files);
        v["status"] = Value::Object(d.status.into_iter().map(|(k, v)| (k, Value::String(v))).collect());
        // Parent chain up to init, and direct children.
        let by_pid: HashMap<u32, &ProcInfo> = s.procs.iter().map(|p| (p.pid, p)).collect();
        let mut chain = Vec::new();
        let mut cur = s.procs[i].ppid;
        while let Some(p) = by_pid.get(&cur) {
            chain.push(json!({"pid": p.pid, "name": &*p.name}));
            if p.pid <= 1 || chain.len() > 32 {
                break;
            }
            cur = p.ppid;
        }
        v["parents"] = json!(chain);
        v["children"] = json!(
            s.procs
                .iter()
                .filter(|p| p.ppid == pid)
                .map(|p| json!({"pid": p.pid, "name": &*p.name}))
                .collect::<Vec<_>>()
        );
        Ok(v)
    }

    fn history(&mut self, args: &Value) -> Result<Value, String> {
        let resource = args["resource"].as_str().ok_or("resource is required")?;
        let device = args["device"].as_str();
        let s = self.snapshot();
        let secs = s.elapsed.max(0.25);
        let series = |name: String, unit: &str, h: &History| summarize(name, unit, h, secs);
        let out: Vec<Value> = match resource {
            "cpu" => vec![series("cpu".into(), "percent", &s.cpu.hist)],
            "memory" => vec![series("memory_used".into(), "percent", &s.mem.hist)],
            "swap" => vec![series("swap_used".into(), "percent", &s.mem.swap_hist)],
            "gpu" => s
                .gpus
                .iter()
                .enumerate()
                .filter(|(i, _)| device.is_none_or(|d| d == i.to_string()))
                .map(|(i, g)| series(format!("gpu{i} ({})", g.name), "percent", &g.hist))
                .collect(),
            "disk" => s
                .disks
                .iter()
                .filter(|d| device.is_none_or(|x| x == d.name))
                .flat_map(|d| {
                    [
                        series(format!("{} active", d.name), "percent", &d.busy_hist),
                        series(format!("{} read", d.name), "bytes/s", &d.read_hist),
                        series(format!("{} write", d.name), "bytes/s", &d.write_hist),
                    ]
                })
                .collect(),
            "network" => s
                .nets
                .iter()
                .filter(|n| device.is_none_or(|x| x == n.name))
                .flat_map(|n| {
                    [
                        series(format!("{} receive", n.name), "bytes/s", &n.rx_hist),
                        series(format!("{} send", n.name), "bytes/s", &n.tx_hist),
                    ]
                })
                .collect(),
            other => return Err(format!("unknown resource: {other}")),
        };
        if out.is_empty() {
            return Err("no such device".into());
        }
        Ok(json!({"interval_seconds": round2(secs as f32), "series": out}))
    }

    fn health_check(&mut self) -> Value {
        let s = self.snapshot();
        let mut f: Vec<Value> = Vec::new();
        let mut add = |sev: &str, area: &str, summary: String, details: Value| {
            f.push(json!({"severity": sev, "area": area, "summary": summary, "details": details}));
        };
        let recent = |h: &History, n: usize| -> f32 {
            let v: Vec<f32> = h.iter().collect();
            let tail = &v[v.len().saturating_sub(n)..];
            if tail.is_empty() { 0.0 } else { tail.iter().sum::<f32>() / tail.len() as f32 }
        };
        let top = |res: &str, n: usize| -> Vec<Value> {
            let mut gs: Vec<&Group> = self.grouper.groups.iter().collect();
            gs.sort_by(|a, b| group_metric(b, res).total_cmp(&group_metric(a, res)));
            gs.into_iter()
                .take(n)
                .filter(|g| group_metric(g, res) > 0.0)
                .map(|g| {
                    json!({"name": g.name, "kind": g.kind.label().to_lowercase(),
                    "cpu_percent_of_machine": round1(g.cpu), "memory": fmt_bytes(g.mem),
                    "gpu_percent": round1(g.gpu), "disk_io": fmt_rate(g.read + g.write)})
                })
                .collect()
        };
        let ncpu = s.cpu.logical.max(1) as f32;
        let m = &s.mem;

        // CPU
        let cpu30 = recent(&s.cpu.hist, 30);
        if cpu30 > 85.0 {
            add(
                "critical",
                "cpu",
                format!("CPU saturated: {cpu30:.0}% average over the last ~30 samples"),
                json!({"top": top("cpu", 5)}),
            );
        } else if cpu30 > 60.0 {
            add(
                "warning",
                "cpu",
                format!("High CPU load: {cpu30:.0}% average recently"),
                json!({"top": top("cpu", 5)}),
            );
        }
        if s.load[0] > ncpu * 1.5 {
            add(
                "warning",
                "cpu",
                format!(
                    "Load average {:.1} is well above the {} logical CPUs: work is queueing",
                    s.load[0], s.cpu.logical
                ),
                json!({"load_1_5_15": s.load}),
            );
        }
        for (i, h) in s.cpu.core_hist.iter().enumerate() {
            if recent(h, 10) > 95.0 && cpu30 < 60.0 {
                add(
                    "info",
                    "cpu",
                    format!(
                        "Logical CPU {i} is pegged near 100% while overall load is moderate: likely a single-threaded bottleneck"
                    ),
                    json!({"top": top("cpu", 3)}),
                );
                break;
            }
        }
        for p in s.procs.iter().filter(|p| p.cpu * ncpu > 90.0) {
            add(
                "warning",
                "process",
                format!("{} (pid {}) is using a full CPU core ({:.0}% of one core)", p.name, p.pid, p.cpu * ncpu),
                json!({"pid": p.pid, "user": &*p.user, "command": truncate(&p.cmd)}),
            );
        }
        if s.cpu.iowait > 20.0 {
            add(
                "warning",
                "disk",
                format!("CPU is waiting on I/O {:.0}% of the time: storage is a bottleneck", s.cpu.iowait),
                json!({"top_disk_io": top("disk_io", 5)}),
            );
        }
        // Memory
        let avail = pct(m.available, m.total);
        if avail < 10.0 {
            add(
                "critical",
                "memory",
                format!("Only {avail:.0}% of memory available ({})", fmt_bytes(m.available)),
                json!({"top": top("memory", 5)}),
            );
        } else if avail < 20.0 {
            add(
                "warning",
                "memory",
                format!("Memory is getting low: {avail:.0}% available"),
                json!({"top": top("memory", 5)}),
            );
        }
        if m.swap_total > 0 {
            let swap_now = pct(m.swap_used, m.swap_total);
            let first = m.swap_hist.iter().next().unwrap_or(swap_now);
            if swap_now - first > 5.0 {
                add(
                    "warning",
                    "memory",
                    format!(
                        "Swap use is climbing ({first:.0}% → {swap_now:.0}%): the system is swapping, which makes everything slow"
                    ),
                    json!({"top": top("memory", 5)}),
                );
            } else if swap_now > 50.0 {
                add("info", "memory", format!("Swap is {swap_now:.0}% used ({})", fmt_bytes(m.swap_used)), json!({}));
            }
        }
        for p in s.procs.iter().filter(|p| m.total > 0 && p.mem as f32 > m.total as f32 * 0.25) {
            add(
                "warning",
                "process",
                format!("{} (pid {}) uses {} ({:.0}% of RAM)", p.name, p.pid, fmt_bytes(p.mem), pct(p.mem, m.total)),
                json!({"pid": p.pid}),
            );
        }
        // Disks
        for d in &s.disks {
            let busy = recent(&d.busy_hist, 15);
            if busy > 90.0 {
                add(
                    "warning",
                    "disk",
                    format!("Disk {} is {busy:.0}% busy: anything reading or writing it will be slow", d.name),
                    json!({"read": fmt_rate(d.read_rate), "write": fmt_rate(d.write_rate), "top_disk_io": top("disk_io", 5)}),
                );
            }
        }
        // GPU
        for (i, g) in s.gpus.iter().enumerate() {
            if recent(&g.hist, 15) > 95.0 {
                add("info", "gpu", format!("GPU {i} ({}) is fully loaded", g.name), json!({"top": top("gpu", 5)}));
            }
            if g.temp_c.is_some_and(|t| t > 85.0) {
                add("warning", "temperature", format!("GPU {i} is hot: {:.0} °C", g.temp_c.unwrap_or(0.0)), json!({}));
            }
        }
        if let Some(t) = s.cpu.temp_c.filter(|t| *t > 90.0) {
            add(
                "warning",
                "temperature",
                format!("CPU is hot: {t:.0} °C; it may be throttling"),
                json!({"frequency_ghz": s.cpu.freq_mhz.map(|f| round2(f / 1000.0))}),
            );
        }
        // Process states
        let zombies: Vec<Value> = s
            .procs
            .iter()
            .filter(|p| p.state == b'Z')
            .map(|p| json!({"pid": p.pid, "name": &*p.name, "parent_pid": p.ppid}))
            .collect();
        if zombies.len() >= 5 {
            add(
                "info",
                "process",
                format!("{} zombie processes: their parent isn't reaping them", zombies.len()),
                json!({"zombies": zombies.iter().take(20).collect::<Vec<_>>()}),
            );
        }
        let stuck: Vec<Value> = s
            .procs
            .iter()
            .filter(|p| p.state == b'D' && !p.kernel)
            .map(|p| json!({"pid": p.pid, "name": &*p.name}))
            .collect();
        if !stuck.is_empty() {
            add(
                "info",
                "disk",
                format!("{} process(es) in uninterruptible I/O wait (D state) right now", stuck.len()),
                json!({"processes": stuck}),
            );
        }
        // Services
        if let Some(failed) = failed_services()["failed"].as_array().filter(|a| !a.is_empty()) {
            add(
                "warning",
                "services",
                format!("{} failed systemd unit(s)", failed.len()),
                json!({"units": failed, "hint": "use service_logs to see why"}),
            );
        }

        let worst = if f.iter().any(|x| x["severity"] == "critical") {
            "critical"
        } else if f.iter().any(|x| x["severity"] == "warning") {
            "warning"
        } else {
            "ok"
        };
        json!({
            "status": worst,
            "observed_seconds": round1((s.cpu.hist.len() as f64 * s.elapsed.max(0.25)) as f32),
            "summary": {
                "cpu_percent": round1(s.cpu.usage), "memory_used_percent": round1(pct(m.used, m.total)),
                "load_1m": s.load[0], "processes": s.procs.len(),
            },
            "findings": f,
            "heaviest_apps_now": {"cpu": top("cpu", 3), "memory": top("memory", 3)},
        })
    }

    fn signal(&mut self, args: &Value) -> Result<Value, String> {
        let pid = args["pid"].as_u64().ok_or("pid is required")? as u32;
        let sig = match args["signal"].as_str().unwrap_or("") {
            "term" => Signal::Term,
            "kill" => Signal::Kill,
            "stop" => Signal::Stop,
            "cont" => Signal::Cont,
            "hup" => Signal::Hup,
            "int" => Signal::Int,
            other => return Err(format!("unknown signal: {other}")),
        };
        let s = self.snapshot();
        let p = guard_target(&s, pid)?;
        let name = p.name.to_string();
        match actions::send_signal(pid, sig) {
            Ok(()) => Ok(json!({"ok": true, "pid": pid, "name": name, "signal": format!("SIG{}", sig.name())})),
            Err(ActionError::Permission) => Err(format!("permission denied: {name} (pid {pid}) belongs to {}", p.user)),
            Err(ActionError::Other(e)) => Err(e),
        }
    }

    fn set_priority(&mut self, args: &Value) -> Result<Value, String> {
        let pid = args["pid"].as_u64().ok_or("pid is required")? as u32;
        let nice = args["nice"].as_i64().ok_or("nice is required")?.clamp(-20, 19) as i32;
        let s = self.snapshot();
        let p = guard_target(&s, pid)?;
        match actions::set_nice(pid, nice) {
            Ok(()) => Ok(json!({"ok": true, "pid": pid, "name": &*p.name, "nice": nice})),
            Err(ActionError::Permission) => {
                Err("permission denied (raising priority or other users' processes needs root)".into())
            }
            Err(ActionError::Other(e)) => Err(e),
        }
    }
}

/// Refuse obviously dangerous targets even when actions are enabled.
fn guard_target(s: &Snapshot, pid: u32) -> Result<&ProcInfo, String> {
    let p = s.procs.iter().find(|p| p.pid == pid).ok_or_else(|| format!("no process with pid {pid}"))?;
    if pid <= 1 || p.kernel {
        return Err("refusing to act on init or kernel threads".into());
    }
    if pid == std::process::id() {
        return Err("refusing to act on the Taskd MCP server itself".into());
    }
    Ok(p)
}

const INSTRUCTIONS: &str = "Taskd exposes live Linux system and process data. To diagnose slowness, call health_check first, \
then drill down with top_consumers, resource_history and process_details. CPU percentages are of the whole machine \
unless the field says 'of_one_core'. Apps group an application's processes; services are systemd units.";

fn metric(p: &ProcInfo, resource: &str) -> f64 {
    match resource {
        "memory" => p.mem as f64,
        "gpu" => p.gpu as f64,
        "disk_io" => p.read_rate + p.write_rate,
        _ => p.cpu as f64,
    }
}

fn group_metric(g: &Group, resource: &str) -> f64 {
    match resource {
        "memory" => g.mem as f64,
        "gpu" => g.gpu as f64,
        "disk_io" => g.read + g.write,
        _ => g.cpu as f64,
    }
}

fn summarize(name: String, unit: &str, h: &History, interval: f64) -> Value {
    let v: Vec<f32> = h.iter().collect();
    if v.is_empty() {
        return json!({"name": name, "unit": unit, "samples": 0});
    }
    let (min, max) = v.iter().fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
    let avg = v.iter().sum::<f32>() / v.len() as f32;
    // Downsample to at most 30 points (averaging buckets) to keep the payload small.
    let buckets = v.len().min(30);
    let series: Vec<f32> = (0..buckets)
        .map(|b| {
            let (lo, hi) = (b * v.len() / buckets, ((b + 1) * v.len() / buckets).max(b * v.len() / buckets + 1));
            round1(v[lo..hi].iter().sum::<f32>() / (hi - lo) as f32)
        })
        .collect();
    json!({
        "name": name, "unit": unit, "samples": v.len(),
        "covers_seconds": (v.len() as f64 * interval).round(),
        "min": round1(min), "avg": round1(avg), "max": round1(max), "latest": round1(*v.last().unwrap()),
        "series_oldest_to_newest": series,
    })
}

fn failed_services() -> Value {
    let mut failed = Vec::new();
    for user in [false, true] {
        let mut cmd = Command::new("systemctl");
        if user {
            cmd.arg("--user");
        }
        let Ok(out) = cmd.args(["list-units", "--state=failed", "--no-legend", "--no-pager", "--plain"]).output()
        else {
            continue;
        };
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let mut it = line.trim_start_matches(['●', '*', ' ']).split_whitespace();
            if let Some(unit) = it.next() {
                let rest: Vec<&str> = it.skip(3).collect();
                failed.push(
                    json!({"unit": unit, "scope": if user { "user" } else { "system" }, "description": rest.join(" ")}),
                );
            }
        }
    }
    json!({"failed": failed})
}

fn service_logs(args: &Value) -> Result<Value, String> {
    let unit = args["unit"].as_str().ok_or("unit is required")?;
    if unit.is_empty() || unit.starts_with('-') || unit.contains(char::is_whitespace) {
        return Err("invalid unit name".into());
    }
    let lines = args["lines"].as_u64().unwrap_or(40).clamp(1, 200).to_string();
    let mut cmd = Command::new("journalctl");
    if args["user"].as_bool().unwrap_or(false) {
        cmd.arg("--user");
    }
    let out = cmd
        .args(["-u", unit, "-n", &lines, "--no-pager", "-o", "short-iso"])
        .output()
        .map_err(|e| format!("journalctl: {e}"))?;
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Ok(
        json!({"unit": unit, "lines": text.lines().collect::<Vec<_>>(), "note": if err.is_empty() { Value::Null } else { Value::String(err) }}),
    )
}

fn state_name(s: u8) -> &'static str {
    match s {
        b'R' => "running",
        b'S' => "sleeping",
        b'D' => "disk wait",
        b'Z' => "zombie",
        b'T' => "stopped",
        b't' => "traced",
        b'I' => "idle",
        _ => "other",
    }
}

fn truncate(s: &str) -> String {
    s.chars().take(CMD_MAX).collect()
}

fn pct(a: u64, b: u64) -> f32 {
    if b == 0 { 0.0 } else { 100.0 * a as f32 / b as f32 }
}

fn round1(v: f32) -> f32 {
    (v * 10.0).round() / 10.0
}

fn round2(v: f32) -> f32 {
    (v * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(allow: bool) -> Server {
        Server {
            shared: sampler::spawn(200, || {}),
            grouper: Grouper::default(),
            my_uid: unsafe { libc::getuid() },
            allow_actions: allow,
            started: Instant::now() - Duration::from_secs(10),
        }
    }

    fn call(s: &mut Server, name: &str, args: Value) -> Value {
        let r = s
            .handle(&json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}))
            .unwrap();
        r["result"].clone()
    }

    #[test]
    fn handshake_and_listing() {
        let mut s = server(false);
        let init = s
            .handle(&json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {"protocolVersion": "2025-06-18"}}))
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
        assert!(s.handle(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let tools = s.handle(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})).unwrap();
        let names: Vec<&str> =
            tools["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"health_check"));
        assert!(!names.contains(&"signal_process"), "actions must not be advertised by default");
        let unknown = s.handle(&json!({"jsonrpc": "2.0", "id": 3, "method": "nope"})).unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn read_only_tools_work() {
        let mut s = server(false);
        for (name, args) in [
            ("health_check", json!({})),
            ("system_overview", json!({})),
            ("top_consumers", json!({"resource": "memory"})),
            ("top_consumers", json!({"resource": "cpu", "group_by": "process", "limit": 3})),
            ("list_processes", json!({"limit": 5, "sort_by": "memory"})),
            ("process_details", json!({"pid": std::process::id()})),
            ("resource_history", json!({"resource": "cpu"})),
        ] {
            let r = call(&mut s, name, args);
            assert_eq!(r["isError"], false, "{name}: {r}");
            let text = r["content"][0]["text"].as_str().unwrap();
            assert!(serde_json::from_str::<Value>(text).is_ok(), "{name} returns JSON text");
        }
    }

    #[test]
    fn actions_are_gated_and_guarded() {
        let mut ro = server(false);
        let r = call(&mut ro, "signal_process", json!({"pid": 999999, "signal": "kill"}));
        assert_eq!(r["isError"], true);
        assert!(r["content"][0]["text"].as_str().unwrap().contains("disabled"));

        let mut rw = server(true);
        let r = call(&mut rw, "signal_process", json!({"pid": 1, "signal": "term"}));
        assert_eq!(r["isError"], true);
        let r = call(&mut rw, "signal_process", json!({"pid": std::process::id(), "signal": "term"}));
        assert_eq!(r["isError"], true);

        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        std::thread::sleep(Duration::from_millis(1500)); // let the sampler see it
        let r = call(&mut rw, "signal_process", json!({"pid": child.id(), "signal": "term"}));
        assert_eq!(r["isError"], false, "{r}");
        assert!(!child.wait().unwrap().success());
    }
}
