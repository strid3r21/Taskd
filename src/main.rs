mod actions;
mod ai;
mod app;
mod config;
mod events;
mod files;
mod gpu;
mod graph;
mod groups;
mod helper;
mod limits;
mod mcp;
mod power;
mod recorder;
mod remote;
mod report;
mod rules;
mod sampler;
mod sensors;
mod services;
mod shortcut;
mod startup;
mod storage;
mod tray;
mod ui_apps;
mod ui_assistant;
mod ui_events;
mod ui_history;
mod ui_limits;
mod ui_machines;
mod ui_performance;
mod ui_processes;
mod ui_rules;
#[cfg(test)]
mod ui_smoke_tests;
mod ui_storage;
mod ui_tabs;
mod updater;
mod util;

use std::time::{Duration, Instant};

const HELP: &str = "\
Taskd - a fast, lightweight task manager for Linux

USAGE:
    taskdm [OPTIONS]

OPTIONS:
    --bench [N]   Run the sampler N times (default 200) headless and print timings
    --once        Print one sample summary and exit
    --report      Print a shareable system report (Markdown)
    --stream [MS] Print a JSON summary line every MS ms (default 2000); used by the
                  Machines tab over SSH
    --export-history [1h|6h|24h|7d]
                  Print recorded history as CSV (default 24h)
    --update      Check for a new release and install it
    --ask <q>     Ask the configured AI assistant a question from the terminal
    --record      Background recorder for history and alerts (normally run as a user service)
    --mcp         Run as an MCP server on stdio for LLM agents (read-only)
    --mcp-allow-actions
                  With --mcp: also allow ending processes and changing priority
    -V, --version Print version
    -h, --help    Print this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if matches!(
        args.first().map(String::as_str),
        Some("--bench" | "--once" | "--report" | "--export-history" | "--stream")
    ) {
        // Text output may be piped into `head`: exit quietly on a closed pipe instead of panicking.
        // SAFETY: restoring the default disposition of SIGPIPE before any threads start.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    }
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => print!("{HELP}"),
        Some("-V" | "--version") => println!("taskdm {}", env!("CARGO_PKG_VERSION")),
        Some("--bench") => bench(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(200)),
        Some("--once") => once(),
        Some("--report") => std::process::exit(report::cli()),
        Some("--stream") => std::process::exit(remote::stream_cli(args.get(1).map(String::as_str))),
        Some("--export-history") => std::process::exit(report::export_cli(args.get(1).map(String::as_str))),
        Some("--update") => std::process::exit(updater::cli()),
        Some("--record") => std::process::exit(recorder::run()),
        Some("--helper") => std::process::exit(helper::run()),
        Some("--install-helper") => std::process::exit(helper::install()),
        Some("--uninstall-helper") => std::process::exit(helper::uninstall()),
        Some("--ask") => std::process::exit(ask_cli(&args[1..].join(" "))),
        Some("--mcp") => std::process::exit(mcp::run(args.iter().any(|a| a == "--mcp-allow-actions"))),
        Some("--mcp-allow-actions") if args.iter().any(|a| a == "--mcp") => std::process::exit(mcp::run(true)),
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{HELP}");
            std::process::exit(2);
        }
        None => {
            // Already open (e.g. launched again from the keyboard shortcut): bring it forward.
            if shortcut::ask_running_to_show() {
                return;
            }
            if let Err(e) = app::run() {
                eprintln!("taskdm: {e}");
                std::process::exit(1);
            }
        }
    }
}

/// `taskdm --ask "…"`: one question to the assistant configured in the app (read-only tools).
fn ask_cli(question: &str) -> i32 {
    if question.trim().is_empty() {
        eprintln!("usage: taskdm --ask \"why is my computer slow?\"");
        return 2;
    }
    let cfg = config::Config::load();
    let provider = ai::Provider::from_key(&cfg.ai_provider);
    let settings = ai::Settings {
        provider,
        model: cfg.ai_model.clone(),
        base: if cfg.ai_base.is_empty() { provider.default_base().into() } else { cfg.ai_base.clone() },
        key: ai::get_key(provider).unwrap_or_default(),
    };
    let shared = sampler::spawn(1000, || {});
    let mut server = mcp::Server::with_shared(shared, false);
    let emit = |e: ai::Event| match e {
        ai::Event::ToolCall(n) => eprintln!("  [tool] {n}"),
        ai::Event::Confirm(_, tx) => {
            let _ = tx.send(false);
        }
    };
    match ai::ask(&settings, &mut Vec::new(), question, &mut server, &emit) {
        Ok(a) => {
            println!("{a}");
            0
        }
        Err(e) => {
            eprintln!("taskdm: {e}");
            1
        }
    }
}

fn bench(n: usize) {
    let n = n.max(1);
    let t = Instant::now();
    let mut s = sampler::Sampler::new();
    let init = t.elapsed();
    let mut costs: Vec<Duration> = Vec::with_capacity(n);
    let mut procs = 0;
    for _ in 0..n {
        let snap = s.sample();
        procs = snap.procs.len();
        costs.push(snap.cost);
    }
    costs.sort();
    let avg = costs.iter().sum::<Duration>() / n.max(1) as u32;
    let pct = |p: f64| costs[((costs.len() as f64 - 1.0) * p) as usize];
    println!("Taskd sampler benchmark ({n} samples, {procs} processes)");
    println!("  init  {:>9.3?}", init);
    println!("  avg   {:>9.3?}", avg);
    println!("  p50   {:>9.3?}", pct(0.5));
    println!("  p99   {:>9.3?}", pct(0.99));
    println!("  max   {:>9.3?}", costs.last().copied().unwrap_or_default());
    println!("  => {:.3}% of one core at a 1s refresh interval", avg.as_secs_f64() * 100.0);
}

fn once() {
    let mut s = sampler::Sampler::new();
    std::thread::sleep(Duration::from_millis(500));
    let snap = s.sample();
    println!("CPU    {} ({} threads) {:.1}%", snap.cpu.model, snap.cpu.logical, snap.cpu.usage);
    println!("Memory {} / {}", util::fmt_bytes(snap.mem.used), util::fmt_bytes(snap.mem.total));
    for g in &snap.gpus {
        println!("GPU    {} {:?}%", g.name, g.util);
    }
    for d in &snap.disks {
        println!("Disk   {} {} {}", d.name, d.model, util::fmt_bytes(d.size));
    }
    for n in &snap.nets {
        println!("Net    {}", n.name);
    }
    let mut p = snap.procs.clone();
    p.sort_by(|a, b| b.cpu.total_cmp(&a.cpu));
    println!("{} processes, top by CPU:", p.len());
    for x in p.iter().take(8) {
        println!("  {:>7} {:<24} {:>5.1}% {:>10}", x.pid, x.name, x.cpu, util::fmt_bytes(x.mem));
    }
    let mut g = groups::Grouper::default();
    g.update(&snap, unsafe { libc::getuid() });
    let mut apps: Vec<_> = g
        .groups
        .iter()
        .filter(|x| {
            (x.kind == groups::Kind::App && x.mine) || matches!(x.kind, groups::Kind::Container | groups::Kind::Vm)
        })
        .collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.mem));
    println!("{} apps:", apps.len());
    for a in apps {
        println!(
            "  {:<32} {:<9} {:>3} procs {:>10} {:>5.1}% cpu",
            a.name,
            a.kind.label(),
            a.members.len(),
            util::fmt_bytes(a.mem),
            a.cpu
        );
    }
    let n_svc = g.groups.iter().filter(|x| x.kind == groups::Kind::Service).count();
    println!(
        "{n_svc} services, {} lone processes",
        g.groups.iter().filter(|x| x.kind == groups::Kind::Process).count()
    );
    p.sort_by(|a, b| b.gpu.total_cmp(&a.gpu));
    let gpu: Vec<_> = p.iter().filter(|x| x.gpu > 0.0).take(8).collect();
    if !gpu.is_empty() {
        println!("Top by GPU:");
        for x in gpu {
            println!("  {:>7} {:<24} {:>5.1}%", x.pid, x.name, x.gpu);
        }
    }
}
