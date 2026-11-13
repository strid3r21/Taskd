//! Draw the tabs headlessly with this machine's real data: catches panics (index errors,
//! unwraps) and runaway layouts in pages that can't be clicked through in CI.

use std::time::{Duration, Instant};

use eframe::egui;

use crate::app::{Ctx, JobResult, Jobs};
use crate::config::Config;
use crate::groups::Grouper;

struct Harness {
    ctx: egui::Context,
    jobs: Jobs,
    rx: std::sync::mpsc::Receiver<JobResult>,
    snap: std::sync::Arc<crate::sampler::Snapshot>,
    grouper: Grouper,
    cfg: Config,
}

impl Harness {
    fn new() -> Self {
        let ctx = egui::Context::default();
        let (jobs, rx) = Jobs::for_test(&ctx);
        let mut sampler = crate::sampler::Sampler::new();
        sampler.sample();
        std::thread::sleep(Duration::from_millis(300));
        let snap = sampler.sample();
        let mut grouper = Grouper::default();
        grouper.update(&snap, 0);
        Harness { ctx, jobs, rx, snap, grouper, cfg: Config::default() }
    }

    /// Run one frame; returns the tallest widget rect drawn (to spot runaway layouts).
    fn frame(&mut self, mut draw: impl FnMut(&mut egui::Ui, &mut Ctx)) -> f32 {
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(1200.0, 800.0))),
            ..Default::default()
        };
        let (mut status, mut elevate, mut switch_to, mut open_rules) = (None, None, None, None);
        let mut out = self.ctx.run_ui(input, |ui| {
            let mut c = Ctx {
                snap: &self.snap,
                cfg: &mut self.cfg,
                jobs: &self.jobs,
                status: &mut status,
                elevate: &mut elevate,
                my_uid: 0,
                switch_to: &mut switch_to,
                open_rules: &mut open_rules,
                groups: &self.grouper.groups,
                grouper: &self.grouper,
            };
            draw(ui, &mut c);
        });
        out.textures_delta.clear();
        out.shapes.iter().map(|s| s.shape.visual_bounding_rect().height()).filter(|h| h.is_finite()).fold(0.0, f32::max)
    }

    /// Wait for the next background job result.
    fn job(&self, within: Duration) -> Option<JobResult> {
        let end = Instant::now() + within;
        while Instant::now() < end {
            if let Ok(r) = self.rx.recv_timeout(Duration::from_millis(50)) {
                return Some(r);
            }
        }
        None
    }
}

#[test]
fn performance_pages_live_and_history() {
    let mut h = Harness::new();
    let mut perf = crate::ui_performance::PerfState::default();
    let mut procs = crate::ui_processes::ProcState::default();
    let mut i = 0;
    while perf.select_nth(&h.snap, i) {
        for range in ["live", "1h"] {
            h.cfg.perf_range = range.into();
            for _ in 0..2 {
                h.frame(|ui, c| perf.ui(ui, c, &mut procs));
            }
            // History is loaded once and reused for every device while fresh.
            if range == "1h"
                && i == 0
                && let Some(JobResult::History(r, recs, ev)) = h.job(Duration::from_secs(10))
            {
                perf.history.loaded(r, recs, ev);
                h.frame(|ui, c| perf.ui(ui, c, &mut procs));
            }
        }
        i += 1;
    }
    assert!(i >= 2, "at least CPU and memory");
}

#[test]
fn timeline_and_storage_tabs() {
    let mut h = Harness::new();
    let mut ev = crate::ui_events::EventsState::default();
    h.frame(|ui, c| ev.ui(ui, c));
    if let Some(JobResult::Events(r, res)) = h.job(Duration::from_secs(10)) {
        ev.loaded(r, res);
    }
    let tallest = h.frame(|ui, c| ev.ui(ui, c));
    assert!(tallest < 2000.0, "timeline rows virtualised ({tallest})");

    let mut st = crate::ui_storage::StorageState::default();
    h.frame(|ui, c| st.ui(ui, c));
    // Scan a small known tree through the real UI flow.
    let dir = std::env::temp_dir().join(format!("taskd-smoke-{}", std::process::id()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/f"), vec![0u8; 50_000]).unwrap();
    st.open_for_test(dir.clone(), &h.jobs);
    h.frame(|ui, c| st.ui(ui, c));
    match h.job(Duration::from_secs(10)) {
        Some(JobResult::Scan(r)) => {
            assert_eq!(r.children[0].name, "sub");
            st.scanned(r);
        }
        _ => panic!("scan finished"),
    }
    h.frame(|ui, c| st.ui(ui, c));
    std::fs::remove_dir_all(&dir).ok();
}
