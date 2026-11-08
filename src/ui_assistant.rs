//! Assistant tab: chat with a local or cloud LLM that can inspect the system through Taskd's tools.

use std::sync::Arc;

use eframe::egui::{self, Align, Key, Layout, RichText};
use serde_json::Value;

use crate::ai::{self, Provider};
use crate::app::{Ctx, JobResult};
use crate::sampler::Shared;

enum Item {
    User(String),
    Assistant(String),
    Tool(String),
    Error(String),
}

#[derive(Default)]
pub struct AssistantState {
    items: Vec<Item>,
    /// Provider-native conversation (reset when the provider changes).
    history: Vec<Value>,
    history_provider: Option<Provider>,
    input: String,
    busy: bool,
    key_edit: String,
    key_loaded_for: Option<Provider>,
    models: Vec<String>,
    models_loading: bool,
    pending_confirm: Option<(String, std::sync::mpsc::Sender<bool>)>,
}

/// Results the background worker sends back to the tab.
pub enum AiMsg {
    Tool(String),
    Confirm(String, std::sync::mpsc::Sender<bool>),
    Done(Result<String, String>, Vec<Value>),
    Models(Result<Vec<String>, String>),
}

impl AssistantState {
    pub fn on_msg(&mut self, m: AiMsg) {
        match m {
            AiMsg::Tool(name) => self.items.push(Item::Tool(name)),
            AiMsg::Confirm(what, tx) => self.pending_confirm = Some((what, tx)),
            AiMsg::Done(res, history) => {
                self.busy = false;
                self.history = history;
                match res {
                    Ok(t) => self.items.push(Item::Assistant(t)),
                    Err(e) => self.items.push(Item::Error(e)),
                }
            }
            AiMsg::Models(res) => {
                self.models_loading = false;
                match res {
                    Ok(m) => self.models = m,
                    Err(e) => self.items.push(Item::Error(format!("Couldn't load models: {e}"))),
                }
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui, c: &mut Ctx, shared: &Arc<Shared>) {
        let provider = Provider::from_key(&c.cfg.ai_provider);
        if self.key_loaded_for != Some(provider) {
            self.key_edit = ai::get_key(provider).unwrap_or_default();
            self.key_loaded_for = Some(provider);
            self.models.clear();
        }
        if c.cfg.ai_base.is_empty() {
            c.cfg.ai_base = provider.default_base().into();
        }
        // Conversations are stored in the provider's own format: switching starts a new one.
        if self.history_provider.is_some_and(|p| p != provider) {
            self.history.clear();
            self.items.push(Item::Tool(format!("switched to {}: new conversation", provider.label())));
        }
        self.history_provider = Some(provider);

        // ---- settings row
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("ai_provider").selected_text(provider.label()).show_ui(ui, |ui| {
                for p in Provider::ALL {
                    if ui.selectable_label(p == provider, p.label()).clicked() && p != provider {
                        c.cfg.ai_provider = p.key().into();
                        c.cfg.ai_base = p.default_base().into();
                        c.cfg.ai_model = p.suggested_models().first().map_or(String::new(), |m| m.to_string());
                    }
                }
            });
            ui.label("Model:");
            ui.add(egui::TextEdit::singleline(&mut c.cfg.ai_model).desired_width(170.0).hint_text("model id"));
            let choices: Vec<String> = if self.models.is_empty() {
                provider.suggested_models().iter().map(|s| s.to_string()).collect()
            } else {
                self.models.clone()
            };
            egui::ComboBox::from_id_salt("ai_models").selected_text("⏷").width(24.0).show_ui(ui, |ui| {
                for m in &choices {
                    if ui.selectable_label(*m == c.cfg.ai_model, m).clicked() {
                        c.cfg.ai_model = m.clone();
                    }
                }
                if choices.is_empty() {
                    ui.label(RichText::new("Use “Load models”").weak());
                }
            });
            if ui.add_enabled(!self.models_loading, egui::Button::new("Load models")).clicked() {
                self.models_loading = true;
                let (p, base, key) = (provider, c.cfg.ai_base.clone(), self.key_edit.trim().to_string());
                let jobs = c.jobs.clone();
                std::thread::spawn(move || jobs.send(JobResult::Ai(AiMsg::Models(ai::list_models(p, &base, &key)))));
            }
            if self.models_loading {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            if provider == Provider::Custom || !provider.default_base().starts_with("https://api.") {
                ui.label("URL:");
                ui.add(egui::TextEdit::singleline(&mut c.cfg.ai_base).desired_width(240.0));
            }
            ui.label("API key:");
            let r = ui.add(
                egui::TextEdit::singleline(&mut self.key_edit)
                    .password(true)
                    .desired_width(200.0)
                    .hint_text(if provider.needs_key() { "required" } else { "optional" }),
            );
            if r.lost_focus() {
                match ai::set_key(provider, self.key_edit.trim()) {
                    Ok(()) => c.info(format!("API key saved in {}", ai::key_storage())),
                    Err(e) => c.error(format!("Couldn't save the key: {e}")),
                }
            }
            ui.checkbox(&mut c.cfg.ai_allow_actions, "Allow ending processes (asks each time)");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.button("New chat").clicked() {
                    self.items.clear();
                    self.history.clear();
                }
            });
        });
        if !provider.is_local(&c.cfg.ai_base) {
            ui.label(
                RichText::new(format!(
                    "Privacy: with {}, process names, (redacted) command lines and system stats the assistant looks at are sent to that service.",
                    provider.label()
                ))
                .small()
                .weak(),
            );
        }
        ui.separator();

        // ---- conversation
        let input_h = 64.0;
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .max_height(ui.available_height() - input_h)
            .stick_to_bottom(true)
            .show(ui, |ui| {
                if self.items.is_empty() {
                    ui.add_space(8.0);
                    ui.label(RichText::new("Ask about your system. The assistant can look at live processes, history, services and logs.").weak());
                    ui.add_space(4.0);
                    ui.horizontal_wrapped(|ui| {
                        for q in [
                            "Why is my computer slow right now?",
                            "What's using the most memory?",
                            "Has anything been hogging the CPU lately?",
                            "Are any services failing? Why?",
                        ] {
                            if ui.button(q).clicked() {
                                self.input = q.into();
                            }
                        }
                    });
                }
                for it in &self.items {
                    match it {
                        Item::User(t) => {
                            ui.add_space(6.0);
                            ui.label(RichText::new(t).strong());
                        }
                        Item::Assistant(t) => {
                            ui.add_space(2.0);
                            ui.add(egui::Label::new(t.as_str()).wrap().selectable(true));
                        }
                        Item::Tool(t) => {
                            ui.label(RichText::new(format!("🔧 {t}")).small().weak());
                        }
                        Item::Error(e) => {
                            ui.colored_label(ui.visuals().error_fg_color, e);
                        }
                    }
                }
                if self.busy {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("Thinking…").weak());
                    });
                }
            });

        // ---- input
        ui.horizontal(|ui| {
            let r = ui.add_enabled(
                !self.busy,
                egui::TextEdit::multiline(&mut self.input)
                    .desired_rows(2)
                    .desired_width(ui.available_width() - 80.0)
                    .hint_text("Ask something… (Enter to send, Shift+Enter for a new line)"),
            );
            let enter = r.has_focus() && ui.input(|i| i.key_pressed(Key::Enter) && !i.modifiers.shift);
            if (ui.add_enabled(!self.busy && !self.input.trim().is_empty(), egui::Button::new("Send")).clicked()
                || enter)
                && !self.input.trim().is_empty()
            {
                let q = self.input.trim().to_string();
                self.input.clear();
                self.send(q, c, shared, provider);
            }
        });

        // ---- confirmation for actions
        if let Some((what, tx)) = &self.pending_confirm {
            let mut answer = None;
            egui::Window::new("The assistant wants to act")
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .show(ui.ctx(), |ui| {
                    ui.label(format!("Allow the assistant to {what}?"));
                    ui.horizontal(|ui| {
                        if ui.button("Allow").clicked() {
                            answer = Some(true);
                        }
                        if ui.button("Deny").clicked() {
                            answer = Some(false);
                        }
                    });
                });
            if let Some(a) = answer {
                let _ = tx.send(a);
                self.pending_confirm = None;
            }
        }
    }

    fn send(&mut self, q: String, c: &mut Ctx, shared: &Arc<Shared>, provider: Provider) {
        self.items.push(Item::User(q.clone()));
        self.busy = true;
        let settings = ai::Settings {
            provider,
            model: c.cfg.ai_model.trim().to_string(),
            base: c.cfg.ai_base.trim().to_string(),
            key: self.key_edit.trim().to_string(),
        };
        let mut history = self.history.clone();
        let allow = c.cfg.ai_allow_actions;
        let shared = shared.clone();
        let jobs = c.jobs.clone();
        std::thread::spawn(move || {
            let mut server = crate::mcp::Server::with_shared(shared, allow);
            let emit = |e: ai::Event| {
                jobs.send(JobResult::Ai(match e {
                    ai::Event::ToolCall(n) => AiMsg::Tool(n),
                    ai::Event::Confirm(w, tx) => AiMsg::Confirm(w, tx),
                }))
            };
            let before = history.clone();
            let res = ai::ask(&settings, &mut history, &q, &mut server, &emit);
            // On failure keep the conversation as it was (no dangling half-turn).
            let history = if res.is_ok() { history } else { before };
            jobs.send(JobResult::Ai(AiMsg::Done(res, history)));
        });
    }
}
