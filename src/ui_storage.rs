//! Storage tab: free space per filesystem and a "what's filling this disk?" folder breakdown.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use eframe::egui::{self, Color32, RichText, Vec2};

use crate::app::{Ctx, JobResult};
use crate::storage::{Fs, Progress, ScanResult};
use crate::util::fmt_bytes;

const BAR: Color32 = Color32::from_rgb(0x3c, 0xb3, 0x71);
const FULL: Color32 = Color32::from_rgb(0xdd, 0x4b, 0x4b);

#[derive(Default)]
pub struct StorageState {
    fs: Vec<Fs>,
    fs_at: Option<Instant>,
    /// Folder being shown in the breakdown, and finished scans by folder.
    current: Option<PathBuf>,
    results: HashMap<PathBuf, ScanResult>,
    running: Option<(PathBuf, Arc<Progress>, Instant)>,
    back: Vec<PathBuf>,
}

impl StorageState {
    pub fn scanned(&mut self, r: ScanResult) {
        if self.running.as_ref().is_some_and(|(p, ..)| *p == r.path) {
            self.running = None;
        }
        if !r.cancelled {
            self.results.insert(r.path.clone(), r);
        }
    }

    pub fn request_reload(&mut self) {
        self.fs_at = None;
        if let Some(p) = &self.current {
            self.results.remove(p);
        }
    }

    fn open(&mut self, path: PathBuf, c: &Ctx) {
        self.open_with(path, c.jobs);
    }

    #[cfg(test)]
    pub fn open_for_test(&mut self, path: PathBuf, jobs: &crate::app::Jobs) {
        self.open_with(path, jobs);
    }

    fn open_with(&mut self, path: PathBuf, jobs: &crate::app::Jobs) {
        if let Some(cur) = self.current.take() {
            self.back.push(cur);
        }
        self.current = Some(path.clone());
        if !self.results.contains_key(&path) {
            self.scan_with(path, jobs);
        }
    }

    fn start_scan(&mut self, path: PathBuf, c: &Ctx) {
        self.scan_with(path, c.jobs);
    }

    fn scan_with(&mut self, path: PathBuf, jobs: &crate::app::Jobs) {
        if let Some((_, p, _)) = &self.running {
            p.cancel.store(true, Ordering::Relaxed);
        }
        let progress = Arc::new(Progress::default());
        self.running = Some((path.clone(), progress.clone(), Instant::now()));
        let jobs = jobs.clone();
        std::thread::spawn(move || {
            let r = crate::storage::scan(&path, &progress);
            jobs.send(JobResult::Scan(r));
        });
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        if self.fs_at.is_none_or(|t| t.elapsed().as_secs() >= 10) {
            self.fs = crate::storage::filesystems(); // a statvfs per mount: microseconds
            self.fs_at = Some(Instant::now());
        }
        let mut open = None;
        ui.label(RichText::new("Drives").strong().size(16.0));
        egui::Grid::new("fs").num_columns(5).spacing([16.0, 6.0]).striped(true).show(ui, |ui| {
            for f in &self.fs {
                ui.vertical(|ui| {
                    ui.label(RichText::new(&f.mount).strong());
                    ui.label(RichText::new(format!("{}  ·  {}", f.device, f.fstype)).small().weak());
                });
                let pct = f.used_pct();
                usage_bar(ui, Vec2::new(220.0, 14.0), pct / 100.0, if pct >= 90.0 { FULL } else { BAR });
                ui.label(format!("{} of {} used ({pct:.0}%)", fmt_bytes(f.used), fmt_bytes(f.total)));
                ui.label(RichText::new(format!("{} free", fmt_bytes(f.avail))).strong());
                if ui.button("What's using space?").clicked() {
                    open = Some(PathBuf::from(&f.mount));
                }
                ui.end_row();
            }
        });
        ui.horizontal(|ui| {
            if ui.button("Scan my home folder").clicked()
                && let Some(h) = std::env::var_os("HOME")
            {
                open = Some(PathBuf::from(h));
            }
            ui.label(
                RichText::new("Scans stay on one drive, don't follow links, and count hard-linked files once.")
                    .small()
                    .weak(),
            );
        });
        if let Some(p) = open {
            self.back.clear();
            self.open(p, c);
        }
        ui.separator();
        self.breakdown(ui, c);
    }

    fn breakdown(&mut self, ui: &mut egui::Ui, c: &mut Ctx) {
        let Some(cur) = self.current.clone() else {
            ui.add_space(12.0);
            ui.label(
                RichText::new("Pick a drive or your home folder to see which folders take the most space.").weak(),
            );
            return;
        };
        let mut go = None::<PathBuf>;
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.back.is_empty(), egui::Button::new("⬅ Back")).clicked() {
                self.current = self.back.pop();
                return;
            }
            if let Some(parent) = cur.parent()
                && ui.button("⬆ Up").clicked()
            {
                go = Some(parent.to_path_buf());
            }
            // Clickable breadcrumb.
            let mut acc = PathBuf::new();
            for comp in cur.components() {
                acc.push(comp);
                let label = comp.as_os_str().to_string_lossy();
                let label = if label == "/" { "/".to_string() } else { format!("{label} /") };
                if ui.link(label).clicked() && acc != cur {
                    go = Some(acc.clone());
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳ Rescan").clicked() {
                    self.results.remove(&cur);
                    self.start_scan(cur.clone(), c);
                }
                if ui.button("Open folder").clicked() {
                    open_in_files(&cur);
                }
            });
        });
        if let Some(p) = go {
            self.open(p, c);
            return;
        }

        if let Some((path, prog, started)) = &self.running
            && *path == cur
        {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "Scanning… {} files, {} so far ({:.0} s)",
                    prog.files.load(Ordering::Relaxed),
                    fmt_bytes(prog.bytes.load(Ordering::Relaxed)),
                    started.elapsed().as_secs_f32()
                ));
                if ui.button("Stop").clicked() {
                    prog.cancel.store(true, Ordering::Relaxed);
                }
            });
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            return;
        }
        let Some(r) = self.results.get(&cur) else {
            if self.running.is_none() {
                self.start_scan(cur, c);
            }
            return;
        };
        ui.label(format!("{} in {} items", fmt_bytes(r.total), r.children.len()));
        if r.denied > 0 {
            ui.label(
                RichText::new(format!(
                    "{} folder{} couldn't be read (they belong to other users or the system), so totals may be low.",
                    r.denied,
                    if r.denied == 1 { "" } else { "s" }
                ))
                .small()
                .color(ui.visuals().warn_fg_color),
            );
        }
        let biggest = r.children.first().map_or(1, |c| c.size.max(1));
        let row_h = 22.0;
        let mut drill = None;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_rows(ui, row_h, r.children.len(), |ui, rows| {
            for e in &r.children[rows] {
                let resp = ui
                    .horizontal(|ui| {
                        ui.set_height(row_h);
                        usage_bar(ui, Vec2::new(140.0, 10.0), e.size as f32 / biggest as f32, BAR);
                        ui.add_sized([90.0, row_h], egui::Label::new(RichText::new(fmt_bytes(e.size)).strong()));
                        let icon = if e.is_dir { "📁" } else { "  " };
                        ui.add(egui::Label::new(format!("{icon} {}", e.name)).truncate());
                        if e.is_dir {
                            ui.label(RichText::new(format!("{} files", e.files)).small().weak());
                        }
                    })
                    .response
                    .interact(egui::Sense::click());
                if e.is_dir && resp.double_clicked() {
                    drill = Some(e.path.clone());
                }
                let resp = if e.is_dir { resp.on_hover_text("Double-click to look inside") } else { resp };
                resp.context_menu(|ui| {
                    if e.is_dir && ui.button("Look inside").clicked() {
                        drill = Some(e.path.clone());
                        ui.close();
                    }
                    if ui.button("Show in file manager").clicked() {
                        open_in_files(if e.is_dir { &e.path } else { e.path.parent().unwrap_or(&e.path) });
                        ui.close();
                    }
                    if ui.button("Copy path").clicked() {
                        ui.ctx().copy_text(e.path.to_string_lossy().into_owned());
                        ui.close();
                    }
                });
            }
        });
        if let Some(p) = drill {
            self.open(p, c);
        }
    }
}

fn usage_bar(ui: &mut egui::Ui, size: Vec2, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, ui.visuals().faint_bg_color.gamma_multiply(2.0));
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, 3.0, color);
}

fn open_in_files(p: &Path) {
    let _ = std::process::Command::new(crate::actions::sys_bin("xdg-open"))
        .arg(p)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
}
