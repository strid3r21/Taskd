//! System tray icon (StatusNotifierItem over D-Bus): a live mini CPU graph with a memory bar,
//! a tooltip, and Show / Pause / Quit.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use eframe::egui;
use ksni::blocking::TrayMethods;

pub enum TrayCmd {
    Show,
    TogglePause,
    Quit,
}

pub struct TaskdTray {
    cpu: VecDeque<f32>,
    mem: f32,
    paused: bool,
    tx: Sender<TrayCmd>,
    ctx: egui::Context,
}

const SIZE: i32 = 32;

impl TaskdTray {
    fn send(&self, c: TrayCmd) {
        let _ = self.tx.send(c);
        self.ctx.request_repaint();
    }
}

/// ARGB32 (network byte order) icon: dark tile, CPU history bars, memory bar along the bottom.
pub fn render_icon(cpu: &[f32], mem: f32) -> Vec<u8> {
    let n = SIZE as usize;
    let mut px = vec![0u8; n * n * 4];
    let mut put = |x: usize, y: usize, a: u8, r: u8, g: u8, b: u8| {
        let i = (y * n + x) * 4;
        px[i..i + 4].copy_from_slice(&[a, r, g, b]);
    };
    for y in 1..n - 1 {
        for x in 1..n - 1 {
            put(x, y, 0xff, 0x1e, 0x29, 0x3b);
        }
    }
    // CPU: one 2-px bar per recent sample (newest on the right), above the memory bar.
    let graph_h = n - 9;
    for (i, v) in cpu.iter().rev().take((n - 4) / 2).enumerate() {
        let h = ((v / 100.0).clamp(0.0, 1.0) * graph_h as f32).round().max(1.0) as usize;
        let x = n - 3 - i * 2;
        for y in (2 + graph_h - h)..(2 + graph_h) {
            put(x, y, 0xff, 0x3b, 0x8e, 0xea);
            put(x - 1, y, 0xff, 0x3b, 0x8e, 0xea);
        }
    }
    // Memory: purple fill along the bottom.
    let w = ((mem / 100.0).clamp(0.0, 1.0) * (n - 4) as f32).round() as usize;
    for y in n - 6..n - 2 {
        for x in 2..n - 2 {
            if x - 2 < w {
                put(x, y, 0xff, 0xa3, 0x5c, 0xe0);
            } else {
                put(x, y, 0xff, 0x33, 0x3b, 0x4d);
            }
        }
    }
    px
}

impl ksni::Tray for TaskdTray {
    fn id(&self) -> String {
        "taskd-monitor".into()
    }
    fn title(&self) -> String {
        "Taskd".into()
    }
    fn icon_name(&self) -> String {
        String::new() // use the live pixmap
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        let cpu: Vec<f32> = self.cpu.iter().copied().collect();
        vec![ksni::Icon { width: SIZE, height: SIZE, data: render_icon(&cpu, self.mem) }]
    }
    fn tool_tip(&self) -> ksni::ToolTip {
        ksni::ToolTip {
            title: "Taskd".into(),
            description: format!(
                "CPU {:.0}% · Memory {:.0}%{}",
                self.cpu.back().copied().unwrap_or(0.0),
                self.mem,
                if self.paused { " · paused" } else { "" }
            ),
            icon_name: String::new(),
            icon_pixmap: Vec::new(),
        }
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        self.send(TrayCmd::Show);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "Show Taskd".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Show)),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: if self.paused { "Resume updates".into() } else { "Pause updates".into() },
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::TogglePause)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "Quit".into(),
                activate: Box::new(|t: &mut Self| t.send(TrayCmd::Quit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

pub struct Tray {
    handle: ksni::blocking::Handle<TaskdTray>,
}

impl Tray {
    /// Register the tray icon; None when the desktop has no tray (StatusNotifier) host.
    pub fn start(ctx: egui::Context, tx: Sender<TrayCmd>) -> Option<Tray> {
        let t = TaskdTray { cpu: VecDeque::with_capacity(16), mem: 0.0, paused: false, tx, ctx };
        t.spawn().ok().map(|handle| Tray { handle })
    }

    pub fn update(&self, cpu: f32, mem: f32, paused: bool) {
        self.handle.update(|t| {
            if t.cpu.len() == 14 {
                t.cpu.pop_front();
            }
            t.cpu.push_back(cpu);
            t.mem = mem;
            t.paused = paused;
        });
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.handle.shutdown().wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_is_argb_and_reflects_values() {
        let idle = render_icon(&[0.0; 14], 0.0);
        let busy = render_icon(&[100.0; 14], 100.0);
        assert_eq!(idle.len(), (SIZE * SIZE * 4) as usize);
        let blue = |px: &[u8]| px.chunks(4).filter(|p| p == &[0xff, 0x3b, 0x8e, 0xea]).count();
        assert!(blue(&busy) > blue(&idle) * 5, "a busy CPU draws taller bars");
    }
}
