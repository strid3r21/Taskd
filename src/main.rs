mod actions;
mod app;
mod config;
mod gpu;
mod graph;
mod groups;
mod sampler;
mod services;
mod startup;
mod ui_apps;
mod ui_performance;
mod ui_processes;
mod ui_tabs;
mod util;

use std::time::{Duration, Instant};

const HELP: &str = "\
Taskd - a fast, lightweight task manager for Linux

USAGE:
    taskdm [OPTIONS]

OPTIONS:
    --bench [N]   Run the sampler N times (default 200) headless and print timings
    --once        Print one sample summary and exit
    -V, --version Print version
    -h, --help    Print this help
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("-h" | "--help") => print!("{HELP}"),
        Some("-V" | "--version") => println!("taskdm {}", env!("CARGO_PKG_VERSION")),
        Some("--bench") => bench(args.get(1).and_then(|n| n.parse().ok()).unwrap_or(200)),
        Some("--once") => once(),
        Some(other) => {
            eprintln!("unknown option: {other}\n\n{HELP}");
            std::process::exit(2);
        }
        None => {
            if let Err(e) = app::run() {
                eprintln!("taskdm: {e}");
                std::process::exit(1);
            }
        }
    }
}

fn bench(n: usize) {
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
    let mut apps: Vec<_> = g.groups.iter().filter(|x| x.kind == groups::Kind::App && x.mine).collect();
    apps.sort_by_key(|a| std::cmp::Reverse(a.mem));
    println!("{} apps:", apps.len());
    for a in apps {
        println!("  {:<32} {:>3} procs {:>10} {:>5.1}% cpu", a.name, a.members.len(), util::fmt_bytes(a.mem), a.cpu);
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
