//! Settings window. Every change is saved immediately (no Save button).

use crate::config::{self, Config, hex_rgb, rgb_hex};
use crate::palette::{Outcome, accent_of, setup_style};
use crate::win::{self, AppEvent};
use egui::{Color32, CornerRadius, Margin, RichText, Sense, Stroke, vec2};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::{Arc, Mutex};

const BG: Color32 = Color32::from_rgb(26, 26, 28);
const SIDEBAR: Color32 = Color32::from_rgb(32, 32, 35);
const CARD: Color32 = Color32::from_rgb(38, 38, 42);
const TEXT_2: Color32 = Color32::from_rgb(152, 152, 160);
const TEXT_3: Color32 = Color32::from_rgb(110, 110, 118);
const LINE: Color32 = Color32::from_rgb(52, 52, 57);

const SWATCHES: &[(&str, &str)] = &[
    ("Blue", "#0A84FF"),
    ("Purple", "#BF5AF2"),
    ("Pink", "#FF375F"),
    ("Red", "#FF453A"),
    ("Orange", "#FF9F0A"),
    ("Yellow", "#FFD60A"),
    ("Green", "#30D158"),
    ("Graphite", "#8E8E93"),
];

#[derive(Clone, Copy, PartialEq)]
enum Page {
    General,
    Model,
    Appearance,
    Drawing,
    Voice,
    Agent,
    Advanced,
}

const PAGES: &[(Page, &str, &str)] = &[
    (Page::General, "⌂", "General"),
    (Page::Model, "◆", "Model"),
    (Page::Appearance, "◐", "Appearance"),
    (Page::Drawing, "✎", "Drawing"),
    (Page::Voice, "◉", "Voice"),
    (Page::Agent, "➤", "Agent"),
    (Page::Advanced, "⚙", "Advanced"),
];

/// Background job status shown in the UI (agent-desktop install / version probe).
#[derive(Default, Clone)]
struct Job {
    running: bool,
    text: String,
    /// Installed agent-desktop is older than desktop::MIN_VERSION.
    outdated: bool,
}

struct Settings {
    cfg: Config,
    saved: Config,
    page: Page,
    recording: Option<u8>,
    show_key: bool,
    autostart: bool,
    agent: Arc<Mutex<Job>>,
    claude_found: bool,
    confirm_reset: bool,
    save_error: Option<String>,
    outcome: Arc<Mutex<Outcome>>,
    quit: bool,
    first: bool,
    /// Voice model download in progress.
    voice_dl: Option<Arc<Mutex<crate::voice::Download>>>,
    voice_error: Option<String>,
}

/// Set by "Browse history" so the palette opens on the list once Settings closes.
static BROWSE_AFTER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Opens the Settings window and blocks until it closes.
pub fn run() -> Outcome {
    let (cfg, _) = config::load();
    let outcome = Arc::new(Mutex::new(Outcome::Closed));
    let out2 = outcome.clone();
    // Our own global hotkeys would swallow the keys being recorded.
    win::register_hotkeys("", "");
    win::register_voice_hotkey("");
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenbuddy Settings")
            .with_inner_size([820.0, 600.0])
            .with_min_inner_size([720.0, 480.0]),
        centered: true,
        ..Default::default()
    };
    let res = eframe::run_native(
        "screenbuddy-settings",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx, accent_of(&cfg), 14.0);
            win::set_ctx(Some(cc.egui_ctx.clone()));
            Ok(Box::new(Settings::new(cfg, out2)))
        }),
    );
    win::set_ctx(None);
    if let Err(e) = res {
        config::log(&format!("settings failed: {e}"));
    }
    if BROWSE_AFTER.swap(false, std::sync::atomic::Ordering::Relaxed) {
        win::push(AppEvent::Menu(crate::tray::BROWSE.into()));
    }
    std::mem::replace(&mut *outcome.lock().unwrap(), Outcome::Closed)
}

impl Settings {
    fn new(cfg: Config, outcome: Arc<Mutex<Outcome>>) -> Self {
        let agent = Arc::new(Mutex::new(Job::default()));
        probe_agent(&cfg, agent.clone());
        Self {
            saved: cfg.clone(),
            claude_found: which(&cfg.claude_cli.command),
            cfg,
            page: Page::General,
            recording: None,
            show_key: false,
            autostart: win::autostart_enabled(),
            agent,
            confirm_reset: false,
            save_error: None,
            outcome,
            quit: false,
            first: true,
            voice_dl: None,
            voice_error: None,
        }
    }

    fn persist(&mut self) {
        if self.cfg != self.saved {
            match config::save(&self.cfg) {
                Ok(()) => {
                    self.saved = self.cfg.clone();
                    self.save_error = None;
                }
                Err(e) => self.save_error = Some(format!("Couldn't save settings: {e}")),
            }
        }
    }

    fn events(&mut self, ctx: &egui::Context) {
        let mut later = Vec::new();
        while let Some(ev) = win::pop() {
            if let AppEvent::Menu(id) = ev {
                if id != crate::tray::QUIT && id != crate::tray::SETTINGS {
                    later.push(AppEvent::Menu(id));
                } else if id == crate::tray::QUIT {
                    self.quit = true;
                    *self.outcome.lock().unwrap() = Outcome::Quit;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                } else if id == crate::tray::SETTINGS {
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
            }
        }
        // Hotkeys are paused while Settings is open; other menu actions are
        // dropped rather than requeued (requeueing would spin the frame loop).
        drop(later);
    }

    fn sidebar(&mut self, ui: &mut egui::Ui, accent: Color32) {
        ui.add_space(14.0);
        ui.horizontal(|ui| {
            ui.add_space(14.0);
            ui.label(RichText::new("●").color(accent).size(15.0));
            ui.label(RichText::new("screenbuddy").strong().size(15.0));
        });
        ui.add_space(14.0);
        for &(page, icon, name) in PAGES {
            let selected = self.page == page;
            let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 32.0), Sense::click());
            let r = rect.shrink2(vec2(8.0, 1.0));
            if selected {
                ui.painter().rect_filled(r, 7, accent.gamma_multiply(0.9));
            } else if resp.hovered() {
                ui.painter().rect_filled(r, 7, Color32::from_white_alpha(8));
            }
            let fg = if selected { Color32::WHITE } else { Color32::from_rgb(220, 220, 226) };
            ui.painter().text(r.left_center() + vec2(12.0, 0.0), egui::Align2::LEFT_CENTER, icon, egui::FontId::proportional(14.0), fg);
            ui.painter().text(r.left_center() + vec2(36.0, 0.0), egui::Align2::LEFT_CENTER, name, egui::FontId::proportional(14.0), fg);
            if resp.clicked() {
                self.page = page;
                self.recording = None;
                self.confirm_reset = false;
            }
        }
    }

    fn page(&mut self, ui: &mut egui::Ui, accent: Color32) {
        let title = PAGES.iter().find(|p| p.0 == self.page).map(|p| p.2).unwrap_or("");
        ui.label(RichText::new(title).size(24.0).strong());
        ui.add_space(14.0);
        match self.page {
            Page::General => self.general(ui, accent),
            Page::Model => self.model(ui, accent),
            Page::Appearance => self.appearance(ui, accent),
            Page::Drawing => self.drawing(ui, accent),
            Page::Voice => self.voice(ui, accent),
            Page::Agent => self.agent(ui, accent),
            Page::Advanced => self.advanced(ui, accent),
        }
    }

    // ------------------------------------------------------------ pages

    fn general(&mut self, ui: &mut egui::Ui, accent: Color32) {
        group(ui, "Shortcuts", |ui| {
            let rec = self.recording;
            row(ui, "Ask about screen", Some("Captures the monitor under your cursor"), |ui| {
                if hotkey_button(ui, &mut self.cfg.hotkey, rec == Some(0), accent) {
                    self.recording = if rec == Some(0) { None } else { Some(0) };
                }
            });
            sep(ui);
            row(ui, "Ask about a region", Some("Drag to select part of the screen first"), |ui| {
                if hotkey_button(ui, &mut self.cfg.region_hotkey, rec == Some(1), accent) {
                    self.recording = if rec == Some(1) { None } else { Some(1) };
                }
            });
        });
        if let Some(slot) = self.recording {
            if let Some(combo) = capture_hotkey(ui) {
                match combo {
                    Ok(s) => match slot {
                        0 => self.cfg.hotkey = s,
                        1 => self.cfg.region_hotkey = s,
                        _ => self.cfg.voice.hotkey = s,
                    },
                    Err(()) => {}
                }
                self.recording = None;
            }
            hint(ui, "Press the new shortcut, with at least one of Ctrl, Alt, Shift or Win. Esc cancels.");
        }
        group(ui, "Startup", |ui| {
            row(ui, "Open at login", Some("Starts quietly in the tray"), |ui| {
                let mut on = self.autostart;
                if toggle(ui, &mut on, accent).changed() {
                    match win::set_autostart(on) {
                        Ok(()) => self.autostart = on,
                        Err(e) => self.save_error = Some(format!("Couldn't change login item: {e}")),
                    }
                }
            });
        });
        group(ui, "History", |ui| {
            row(ui, "Save conversations", Some("Screenshot and transcript, stored only on this PC"), |ui| {
                toggle(ui, &mut self.cfg.history, accent);
            });
            sep(ui);
            row(ui, "Past conversations", Some("Search, read and continue them (Ctrl+H in the palette)"), |ui| {
                if ui.button("Show in Explorer").clicked() {
                    win::shell_open(&config::history_dir());
                }
                if ui.add(primary("Browse history", accent)).clicked() {
                    BROWSE_AFTER.store(true, std::sync::atomic::Ordering::Relaxed);
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
        });
        group(ui, "Context", |ui| {
            row(ui, "Selected text", Some("Attach text you've selected in the app when you press the shortcut"), |ui| {
                segmented(ui, &mut self.cfg.context.selection, &[("off", "Off"), ("uia", "Accessibility"), ("uia+copy", "+ Copy fallback")], accent);
            });
            sep(ui);
            row(ui, "Explorer menu", Some("“Ask screenbuddy” when you right-click a file"), |ui| {
                let mut on = self.cfg.context.explorer_menu;
                if toggle(ui, &mut on, accent).changed() {
                    match crate::context::set_explorer_menu(on) {
                        Ok(()) => self.cfg.context.explorer_menu = on,
                        Err(e) => self.save_error = Some(format!("Couldn't change the Explorer menu: {e}")),
                    }
                }
            });
        });
        if self.cfg.context.selection == "uia+copy" {
            hint(ui, "The copy fallback briefly sends Ctrl+C to apps that don't expose their selection, then puts your clipboard back.");
        }
    }

    fn voice(&mut self, ui: &mut egui::Ui, accent: Color32) {
        use crate::voice;
        if let Some(dl) = &self.voice_dl {
            let d = dl.lock().unwrap().clone();
            if d.done {
                self.voice_dl = None;
                self.voice_error = d.error;
            } else {
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
        group(ui, "Voice input", |ui| {
            row(ui, "Push to talk", Some("Speak your question; transcribed on this PC by Whistle"), |ui| {
                let mut on = self.cfg.voice.enabled;
                if toggle(ui, &mut on, accent).changed() {
                    self.cfg.voice.enabled = on;
                    if on && !voice::model_ready() && self.voice_dl.is_none() {
                        self.voice_error = None;
                        self.voice_dl = Some(voice::start_download(Some(ui.ctx().clone())));
                    }
                }
            });
            sep(ui);
            let rec = self.recording;
            row(ui, "Shortcut", Some("Hold to talk, or tap to start and tap again to stop"), |ui| {
                if hotkey_button(ui, &mut self.cfg.voice.hotkey, rec == Some(2), accent) {
                    self.recording = if rec == Some(2) { None } else { Some(2) };
                }
            });
            sep(ui);
            row(ui, "Send automatically", Some("Ask as soon as the transcript is ready"), |ui| {
                toggle(ui, &mut self.cfg.voice.auto_send, accent);
            });
            sep(ui);
            row(ui, "Language", None, |ui| {
                combo_text(ui, "voice_lang", &mut self.cfg.voice.language, voice::LANGUAGES);
            });
        });
        group(ui, "Alt key", |ui| {
            row(ui, "Hold Alt to ask", Some("Hold Alt alone, speak, let go; the answer appears beside the cursor"), |ui| {
                toggle(ui, &mut self.cfg.input.alt_hold, accent);
            });
            sep(ui);
            row(ui, "Hold for", Some("How long Alt is held before listening starts"), |ui| {
                ui.add(egui::Slider::new(&mut self.cfg.input.alt_hold_ms, 300..=1500).suffix(" ms").step_by(50.0));
            });
            sep(ui);
            row(ui, "Double-tap Alt", Some("Opens the palette to type a question"), |ui| {
                toggle(ui, &mut self.cfg.input.alt_double_tap, accent);
            });
        });
        if let Some(slot) = self.recording.filter(|&s| s == 2) {
            if let Some(combo) = capture_hotkey(ui) {
                if let Ok(s) = combo {
                    let _ = slot;
                    self.cfg.voice.hotkey = s;
                }
                self.recording = None;
            }
            hint(ui, "Press the new shortcut, with at least one of Ctrl, Alt, Shift or Win. Esc cancels.");
        }
        group(ui, "Model", |ui| {
            let status = if !voice::engine_included() {
                "Speech engine not included in this build".to_string()
            } else if let Some(dl) = &self.voice_dl {
                format!("Downloading… {:.0}%", dl.lock().unwrap().progress * 100.0)
            } else if voice::model_ready() {
                "Whistle · 16.9 MB · ready".to_string()
            } else {
                "Not downloaded".to_string()
            };
            row(ui, "Whistle by Cactus Compute", Some(&status), |ui| {
                if self.voice_dl.is_some() {
                    ui.spinner();
                } else if voice::model_ready() {
                    if ui.button("Remove").clicked() {
                        if let Err(e) = voice::remove_model() {
                            self.voice_error = Some(e.to_string());
                        }
                    }
                } else if voice::engine_included() && ui.add(primary("Download", accent)).clicked() {
                    self.voice_error = None;
                    self.voice_dl = Some(voice::start_download(Some(ui.ctx().clone())));
                }
            });
            if let Some(dl) = &self.voice_dl {
                let p = dl.lock().unwrap().progress;
                ui.add(egui::ProgressBar::new(p).desired_height(6.0));
            }
        });
        if let Some(e) = &self.voice_error {
            ui.label(RichText::new(format!("Download failed: {e}")).color(Color32::from_rgb(255, 120, 110)));
        }
        hint(ui, "Stored in %APPDATA%\\screenbuddy\\models. The microphone is only open while you record; nothing stays loaded afterwards.");
    }

    fn model(&mut self, ui: &mut egui::Ui, accent: Color32) {
        group(ui, "Provider", |ui| {
            ui.add_space(4.0);
            segmented(
                ui,
                &mut self.cfg.provider,
                &[("claude-cli", "Claude subscription"), ("anthropic", "Anthropic API"), ("openai", "OpenAI-compatible")],
                accent,
            );
            ui.add_space(4.0);
        });
        match self.cfg.provider.as_str() {
            "claude-cli" => {
                group(ui, "Claude subscription", |ui| {
                    row(ui, "Claude Code", Some("Uses your Claude plan through the claude CLI"), |ui| {
                        if self.claude_found {
                            ui.label(RichText::new("✓ Found").color(Color32::from_rgb(48, 209, 88)));
                        } else {
                            ui.hyperlink_to("Install Claude Code", "https://claude.com/claude-code");
                        }
                    });
                    sep(ui);
                    row(ui, "Model", None, |ui| {
                        combo_text(ui, "cli_model", &mut self.cfg.claude_cli.model, &[("", "Default"), ("opus", "Opus"), ("sonnet", "Sonnet"), ("haiku", "Haiku")]);
                    });
                    sep(ui);
                    row(ui, "Stay ready after closing", Some("Answers start instantly; ~100 MB while waiting"), |ui| {
                        ui.add(egui::Slider::new(&mut self.cfg.claude_cli.keep_warm_minutes, 0..=60).suffix(" min"));
                    });
                });
            }
            "openai" => {
                group(ui, "OpenAI-compatible endpoint", |ui| {
                    row(ui, "Service", None, |ui| {
                        let presets = [
                            ("https://openrouter.ai/api/v1", "OpenRouter"),
                            ("https://api.deepseek.com/v1", "DeepSeek"),
                            ("https://api.openai.com/v1", "OpenAI"),
                            ("http://localhost:11434/v1", "Ollama (local)"),
                        ];
                        combo_text(ui, "oa_base", &mut self.cfg.openai.base_url, &presets);
                    });
                    sep(ui);
                    row(ui, "Base URL", None, |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.openai.base_url).desired_width(300.0));
                    });
                    sep(ui);
                    row(ui, "API key", Some("Empty = OPENAI_API_KEY"), |ui| {
                        secret(ui, &mut self.cfg.openai.api_key, &mut self.show_key);
                    });
                    sep(ui);
                    row(ui, "Model", None, |ui| {
                        ui.add(egui::TextEdit::singleline(&mut self.cfg.openai.model).desired_width(300.0));
                    });
                    sep(ui);
                    row(ui, "Tools", Some("Turn off for models without tool calling"), |ui| {
                        toggle(ui, &mut self.cfg.openai.tools, accent);
                    });
                });
            }
            _ => {
                group(ui, "Anthropic API", |ui| {
                    row(ui, "API key", Some("Empty = ANTHROPIC_API_KEY"), |ui| {
                        secret(ui, &mut self.cfg.anthropic.api_key, &mut self.show_key);
                    });
                    sep(ui);
                    row(ui, "Model", None, |ui| {
                        combo_text(
                            ui,
                            "an_model",
                            &mut self.cfg.anthropic.model,
                            &[
                                ("claude-opus-5-5", "Claude Opus 5.5"),
                                ("claude-sonnet-5-5", "Claude Sonnet 5.5"),
                                ("claude-haiku-5-5", "Claude Haiku 5.5"),
                                ("claude-fable-5-1", "Claude Fable 5.1"),
                            ],
                        );
                    });
                    sep(ui);
                    row(ui, "Effort", Some("Higher thinks longer and costs more"), |ui| {
                        segmented(ui, &mut self.cfg.anthropic.effort, &[("low", "Low"), ("medium", "Medium"), ("high", "High"), ("xhigh", "X-High"), ("max", "Max")], accent);
                    });
                    sep(ui);
                    row(ui, "Refusal fallback", Some("Retry declined requests on a fallback model"), |ui| {
                        toggle(ui, &mut self.cfg.anthropic.fallbacks, accent);
                    });
                });
            }
        }
    }

    fn appearance(&mut self, ui: &mut egui::Ui, accent: Color32) {
        group(ui, "Accent color", |ui| {
            ui.add_space(4.0);
            swatches(ui, &mut self.cfg.appearance.accent);
            ui.add_space(4.0);
        });
        group(ui, "Palette", |ui| {
            row(ui, "Position", None, |ui| {
                segmented(
                    ui,
                    &mut self.cfg.appearance.position,
                    &[
                        ("top", "Top"),
                        ("center", "Center"),
                        ("cursor", "Near cursor"),
                        ("last", "Where I left it"),
                    ],
                    accent,
                );
            });
            sep(ui);
            row(ui, "Width", None, |ui| {
                ui.add(egui::Slider::new(&mut self.cfg.appearance.width, 520.0..=1000.0).step_by(10.0).suffix(" pt"));
            });
            sep(ui);
            row(ui, "Text size", None, |ui| {
                ui.add(egui::Slider::new(&mut self.cfg.appearance.text_size, 12.0..=20.0).step_by(0.5).suffix(" pt"));
            });
            sep(ui);
            row(ui, "Blur strength", Some("How much of the screen shows through. 0 is solid."), |ui| {
                ui.add(egui::Slider::new(&mut self.cfg.appearance.blur, 0.0..=100.0).step_by(5.0).suffix("%"));
            });
            sep(ui);
            row(ui, "Close when clicking elsewhere", Some("Stays open while something is drawn on screen"), |ui| {
                toggle(ui, &mut self.cfg.appearance.dismiss_on_blur, accent);
            });
        });
        group(ui, "Island", |ui| {
            row(ui, "Show the island", Some("The buddy's pill at the top of the screen. Hover to see agents, click to ask."), |ui| {
                toggle(ui, &mut self.cfg.island.enabled, accent);
            });
        });
        hint(ui, "Changes apply the next time you open the palette.");
    }

    fn drawing(&mut self, ui: &mut egui::Ui, accent: Color32) {
        group(ui, "Pointing things out", |ui| {
            row(ui, "Draw on screen", Some("Boxes and arrows when finding something in an app"), |ui| {
                toggle(ui, &mut self.cfg.drawing.enabled, accent);
            });
        });
        if self.cfg.drawing.enabled {
            group(ui, "Color", |ui| {
                ui.add_space(4.0);
                swatches(ui, &mut self.cfg.drawing.color);
                ui.add_space(4.0);
            });
        }
        group(ui, "Seeing your screen", |ui| {
            row(ui, "Read app structure", Some("Sends element names and positions from the accessibility tree, so pointers land exactly"), |ui| {
                toggle(ui, &mut self.cfg.vision.ui_tree, accent);
            });
            sep(ui);
            row(ui, "Zoom in", Some("Lets it look at small text at full resolution"), |ui| {
                toggle(ui, &mut self.cfg.vision.zoom, accent);
            });
            sep(ui);
            row(
                ui,
                "Guide mode",
                Some("\"Walk me through…\": points at each step and waits for you"),
                |ui| {
                    toggle(ui, &mut self.cfg.vision.guide, accent);
                },
            );
            sep(ui);
            row(
                ui,
                "Search the web",
                Some("Looks things up when the answer isn't on screen (Anthropic or Claude CLI)"),
                |ui| {
                    toggle(ui, &mut self.cfg.vision.web_search, accent);
                },
            );
        });
    }

    fn agent(&mut self, ui: &mut egui::Ui, accent: Color32) {
        let job = self.agent.lock().unwrap().clone();
        let installed = self.cfg.desktop_exe().is_some() || crate::desktop::resolve(&self.cfg.agent_desktop.command).is_some();
        group(ui, "Use your apps", |ui| {
            row(ui, "Let screenbuddy act for you", Some("Clicks, types and navigates apps when you ask it to"), |ui| {
                toggle(ui, &mut self.cfg.agent_desktop.enabled, accent);
            });
            sep(ui);
            row(ui, "agent-desktop", Some("Reads apps through accessibility, not screenshots"), |ui| {
                if job.running {
                    ui.spinner();
                    ui.label(RichText::new(&job.text).color(TEXT_2));
                } else if installed && job.outdated {
                    ui.label(RichText::new(&job.text).color(TEXT_2));
                    if ui.add(primary("Update", accent)).clicked() {
                        install_agent(self.agent.clone());
                    }
                } else if installed {
                    ui.label(RichText::new(format!("✓ {}", if job.text.is_empty() { "Installed" } else { &job.text })).color(Color32::from_rgb(48, 209, 88)));
                } else if ui.add(primary("Install", accent)).clicked() {
                    install_agent(self.agent.clone());
                }
            });
        });
        if !installed && !job.running && !job.text.is_empty() {
            hint(ui, &job.text);
        }
        group(ui, "Permission", |ui| {
            ui.add_space(4.0);
            segmented(ui, &mut self.cfg.agent_desktop.approval, &[("ask", "Ask every time"), ("once", "Ask once per chat"), ("never", "Never ask")], accent);
            ui.add_space(2.0);
            let note = match self.cfg.agent_desktop.approval.as_str() {
                "ask" => "You approve each action that deletes, sends, posts or pays.",
                "never" => "Acts right away. Reading apps never asks.",
                _ => "Approve the first delete, send or payment; the rest of that conversation runs on its own.",
            };
            ui.label(RichText::new(note).color(TEXT_3).size(12.5));
            ui.add_space(4.0);
        });
        group(ui, "Background agents", |ui| {
            row(ui, "Hand off multi-step work", Some("The assistant can start an agent that works while you carry on"), |ui| {
                toggle(ui, &mut self.cfg.agents.enabled, accent);
            });
            sep(ui);
            row(ui, "Always approve agent actions", Some("Agents delete, send or pay without asking"), |ui| {
                toggle(ui, &mut self.cfg.agents.always_approve, accent);
            });
            sep(ui);
            row(ui, "Let agents use your mouse", Some("Only when you ask to watch; it asks first and pauses the moment you touch anything"), |ui| {
                let mut on = self.cfg.agents.drive != "never";
                toggle(ui, &mut on, accent);
                self.cfg.agents.drive = if on { "ask" } else { "never" }.into();
            });
        });
        group(ui, "Agent cursor", |ui| {
            row(ui, "Show cursor while acting", Some("A separate pointer, so you can see what it's doing"), |ui| {
                toggle(ui, &mut self.cfg.agent_desktop.cursor, accent);
            });
            if self.cfg.agent_desktop.cursor {
                sep(ui);
                row(ui, "Show what it's doing", Some("A short label beside the cursor"), |ui| {
                    toggle(ui, &mut self.cfg.agent_desktop.cursor_label, accent);
                });
                sep(ui);
                row(ui, "Body", None, |ui| color_hex(ui, &mut self.cfg.agent_desktop.cursor_fill));
                sep(ui);
                row(ui, "Outline", None, |ui| color_hex(ui, &mut self.cfg.agent_desktop.cursor_rim));
                sep(ui);
                row(ui, "Highlight", Some("Ripple and element highlight"), |ui| color_hex(ui, &mut self.cfg.agent_desktop.cursor_accent));
                sep(ui);
                ui.horizontal(|ui| {
                    ui.add_space(4.0);
                    cursor_preview(ui, &self.cfg);
                });
            }
        });
        hint(ui, "Try: “turn on dark mode”, “reply ‘on my way’ in Slack”, “open the last download”.");
    }

    fn advanced(&mut self, ui: &mut egui::Ui, accent: Color32) {
        group(ui, "Custom instructions", |ui| {
            ui.label(RichText::new("Added to every conversation, e.g. “Answer in Spanish” or “I'm a designer”.").color(TEXT_3).size(12.5));
            ui.add(egui::TextEdit::multiline(&mut self.cfg.extra_instructions).desired_rows(4).desired_width(f32::INFINITY));
        });
        group(ui, "Files", |ui| {
            row(ui, "Settings file", Some("config.toml"), |ui| {
                if ui.button("Open").clicked() {
                    win::shell_open(&config::config_path());
                }
            });
            sep(ui);
            row(ui, "Log", None, |ui| {
                if ui.button("Open").clicked() {
                    win::shell_open(&config::data_dir().join("screenbuddy.log"));
                }
            });
        });
        group(ui, "Reset", |ui| {
            row(ui, "Restore defaults", Some("Keeps your API keys"), |ui| {
                let label = if self.confirm_reset { "Click again to reset" } else { "Reset…" };
                let btn = egui::Button::new(RichText::new(label).color(if self.confirm_reset { Color32::WHITE } else { Color32::from_rgb(255, 120, 110) }))
                    .fill(if self.confirm_reset { Color32::from_rgb(200, 60, 50) } else { Color32::TRANSPARENT });
                if ui.add(btn).clicked() {
                    if self.confirm_reset {
                        let keep = (self.cfg.anthropic.api_key.clone(), self.cfg.openai.api_key.clone());
                        self.cfg = Config::default();
                        self.cfg.anthropic.api_key = keep.0;
                        self.cfg.openai.api_key = keep.1;
                        self.confirm_reset = false;
                    } else {
                        self.confirm_reset = true;
                    }
                }
            });
        });
        let _ = accent;
    }
}

impl eframe::App for Settings {
    fn clear_color(&self, _v: &egui::Visuals) -> [f32; 4] {
        BG.to_normalized_gamma_f32()
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.first {
            self.first = false;
            if let Ok(h) = frame.window_handle() {
                if let RawWindowHandle::Win32(h) = h.as_raw() {
                    crate::palette::apply_chrome(windows::Win32::Foundation::HWND(h.hwnd.get() as *mut _));
                }
            }
        }
        self.events(&ctx);
        let accent = accent_of(&self.cfg);
        let full = ui.max_rect();
        ui.painter().rect_filled(full, 0, BG);
        let side_w = 196.0;
        let side = egui::Rect::from_min_size(full.min, vec2(side_w, full.height()));
        ui.painter().rect_filled(side, 0, SIDEBAR);
        ui.painter().vline(side.right(), full.y_range(), Stroke::new(1.0, LINE));

        ui.scope_builder(egui::UiBuilder::new().max_rect(side), |ui| self.sidebar(ui, accent));
        let content = egui::Rect::from_min_max(full.min + vec2(side_w + 1.0, 0.0), full.max);
        ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Frame::new().inner_margin(Margin { left: 28, right: 28, top: 22, bottom: 24 }).show(ui, |ui| {
                    ui.set_max_width(640.0);
                    self.page(ui, accent);
                    if let Some(e) = &self.save_error {
                        ui.add_space(8.0);
                        ui.label(RichText::new(e).color(Color32::from_rgb(255, 120, 110)));
                    }
                });
            });
        });
        self.persist();
        if self.agent.lock().unwrap().running {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
    }
}

// ---------------------------------------------------------------- widgets

fn group(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(title.to_uppercase()).size(11.5).color(TEXT_3).strong());
    ui.add_space(2.0);
    egui::Frame::new()
        .fill(CARD)
        .corner_radius(10)
        .stroke(Stroke::new(1.0, LINE))
        .inner_margin(Margin::symmetric(14, 8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(18.0);
}

fn row(ui: &mut egui::Ui, label: &str, sub: Option<&str>, control: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.set_min_height(34.0);
        ui.vertical(|ui| {
            ui.add_space(if sub.is_some() { 2.0 } else { 8.0 });
            ui.label(RichText::new(label).size(14.0));
            if let Some(s) = sub {
                ui.label(RichText::new(s).size(12.0).color(TEXT_3));
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control);
    });
}

fn sep(ui: &mut egui::Ui) {
    let r = ui.available_rect_before_wrap();
    ui.painter().hline(r.x_range(), r.top() + 3.0, Stroke::new(1.0, LINE));
    ui.add_space(7.0);
}

fn hint(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).color(TEXT_3).size(12.5));
    ui.add_space(14.0);
}

fn primary(text: &str, accent: Color32) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text.to_string()).color(Color32::WHITE)).fill(accent).corner_radius(7)
}

/// iOS-style switch.
fn toggle(ui: &mut egui::Ui, on: &mut bool, accent: Color32) -> egui::Response {
    let size = vec2(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_with_time(resp.id, *on, 0.12);
    let bg = Color32::from_rgb(70, 70, 76).lerp_to_gamma(accent, t);
    ui.painter().rect_filled(rect, CornerRadius::same(11), bg);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, t);
    ui.painter().circle_filled(egui::pos2(x, rect.center().y), 9.0, Color32::WHITE);
    resp
}

fn segmented(ui: &mut egui::Ui, value: &mut String, options: &[(&str, &str)], accent: Color32) {
    egui::Frame::new().fill(Color32::from_rgb(28, 28, 31)).corner_radius(8).inner_margin(Margin::same(2)).show(ui, |ui| {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 2.0;
            for &(v, label) in options {
                let sel = value == v;
                let btn = egui::Button::new(RichText::new(label).size(13.0).color(if sel { Color32::WHITE } else { TEXT_2 }))
                    .fill(if sel { accent } else { Color32::TRANSPARENT })
                    .corner_radius(6)
                    .min_size(vec2(0.0, 26.0));
                if ui.add(btn).clicked() {
                    *value = v.to_string();
                }
            }
        });
    });
}

/// Dropdown of presets that also allows any custom value already set.
fn combo_text(ui: &mut egui::Ui, id: &str, value: &mut String, options: &[(&str, &str)]) {
    let shown = options.iter().find(|o| o.0 == value.as_str()).map(|o| o.1.to_string()).unwrap_or_else(|| value.clone());
    egui::ComboBox::from_id_salt(id).selected_text(shown).width(220.0).show_ui(ui, |ui| {
        for &(v, label) in options {
            ui.selectable_value(value, v.to_string(), label);
        }
    });
}

fn secret(ui: &mut egui::Ui, value: &mut String, show: &mut bool) {
    if ui.button(if *show { "Hide" } else { "Show" }).clicked() {
        *show = !*show;
    }
    ui.add(egui::TextEdit::singleline(value).password(!*show).desired_width(260.0).hint_text("sk-…"));
}

fn swatches(ui: &mut egui::Ui, value: &mut String) {
    ui.horizontal(|ui| {
        for &(name, hex) in SWATCHES {
            let [r, g, b] = hex_rgb(hex).unwrap();
            let c = Color32::from_rgb(r, g, b);
            let (rect, resp) = ui.allocate_exact_size(vec2(30.0, 30.0), Sense::click());
            ui.painter().circle_filled(rect.center(), 11.0, c);
            if value.eq_ignore_ascii_case(hex) {
                ui.painter().circle_stroke(rect.center(), 14.0, Stroke::new(2.0, c));
            }
            if resp.on_hover_text(name).clicked() {
                *value = hex.to_string();
            }
        }
        ui.add_space(8.0);
        color_hex(ui, value);
    });
}

fn color_hex(ui: &mut egui::Ui, value: &mut String) {
    let mut c = hex_rgb(value).unwrap_or([255, 255, 255]);
    if egui::widgets::color_picker::color_edit_button_srgb(ui, &mut c).changed() {
        *value = rgb_hex(c);
    }
}

fn hotkey_button(ui: &mut egui::Ui, value: &mut String, recording: bool, accent: Color32) -> bool {
    let text = if recording { "Press keys…".to_string() } else { value.replace('+', " + ") };
    let btn = egui::Button::new(RichText::new(text).monospace().color(if recording { Color32::WHITE } else { Color32::from_rgb(230, 230, 235) }))
        .fill(if recording { accent } else { Color32::from_rgb(52, 52, 58) })
        .corner_radius(6)
        .min_size(vec2(170.0, 28.0));
    ui.add(btn).clicked()
}

/// Returns Some(Ok(combo)) once a modifier+key is pressed, Some(Err) on Esc.
fn capture_hotkey(ui: &mut egui::Ui) -> Option<Result<String, ()>> {
    ui.input(|i| {
        for e in &i.events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = e {
                if *key == egui::Key::Escape && !modifiers.any() {
                    return Some(Err(()));
                }
                let mut parts = Vec::new();
                if modifiers.ctrl {
                    parts.push("Ctrl".to_string());
                }
                if modifiers.alt {
                    parts.push("Alt".to_string());
                }
                if modifiers.shift {
                    parts.push("Shift".to_string());
                }
                if parts.is_empty() {
                    continue;
                }
                let name = match key.name() {
                    "Backtick" => "`".to_string(),
                    n => n.to_string(),
                };
                parts.push(name);
                let combo = parts.join("+");
                if win::parse_hotkey(&combo).is_ok() {
                    return Some(Ok(combo));
                }
            }
        }
        None
    })
}

/// Little preview of the agent cursor with the chosen colors.
fn cursor_preview(ui: &mut egui::Ui, cfg: &Config) {
    let col = |s: &str, d: [u8; 3]| {
        let [r, g, b] = hex_rgb(s).unwrap_or(d);
        Color32::from_rgb(r, g, b)
    };
    let a = &cfg.agent_desktop;
    let (fill, rim, acc) = (col(&a.cursor_fill, [255; 3]), col(&a.cursor_rim, [17, 19, 24]), col(&a.cursor_accent, [10, 132, 255]));
    let (rect, _) = ui.allocate_exact_size(vec2(300.0, 70.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 8, Color32::from_rgb(30, 30, 33));
    let target = egui::Rect::from_center_size(rect.left_center() + vec2(70.0, 0.0), vec2(90.0, 30.0));
    p.rect_stroke(target, 6, Stroke::new(2.0, acc), egui::StrokeKind::Outside);
    p.text(target.center(), egui::Align2::CENTER_CENTER, "Send", egui::FontId::proportional(13.0), Color32::from_rgb(220, 220, 226));
    let o = target.center() + vec2(18.0, 4.0);
    let pts = [vec2(0.0, 0.0), vec2(0.0, 22.0), vec2(6.0, 17.0), vec2(10.0, 26.0), vec2(14.0, 24.0), vec2(10.0, 15.5), vec2(17.0, 15.5)];
    let poly: Vec<egui::Pos2> = pts.iter().map(|v| o + *v).collect();
    p.circle_stroke(o, 12.0, Stroke::new(2.0, acc.gamma_multiply(0.6)));
    p.add(egui::Shape::convex_polygon(poly.clone(), fill, Stroke::NONE));
    p.add(egui::Shape::closed_line(poly, Stroke::new(1.5, rim)));
    if a.cursor_label {
        let lp = o + vec2(26.0, 18.0);
        let galley = p.layout_no_wrap("Click Send".into(), egui::FontId::proportional(12.0), Color32::WHITE);
        let bg = egui::Rect::from_min_size(lp, galley.size() + vec2(14.0, 8.0));
        p.rect_filled(bg, 10, acc);
        p.galley(lp + vec2(7.0, 4.0), galley, Color32::WHITE);
    }
}

// ---------------------------------------------------------------- jobs

fn which(cmd: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else { return false };
    std::env::split_paths(&path).any(|d| ["", ".exe", ".cmd"].iter().any(|ext| d.join(format!("{cmd}{ext}")).is_file()))
}

fn probe_agent(cfg: &Config, job: Arc<Mutex<Job>>) {
    let Some(exe) = crate::desktop::resolve(&cfg.agent_desktop.command) else { return };
    std::thread::spawn(move || {
        let out = crate::desktop::run(&exe, &["version".to_string()]);
        let v = serde_json::from_str::<serde_json::Value>(&out).ok().and_then(|v| v["data"]["version"].as_str().map(str::to_owned));
        if let Some(v) = v {
            let mut j = job.lock().unwrap();
            j.outdated = crate::desktop::outdated(&v);
            j.text = if j.outdated { format!("v{v} · update to v{}+", crate::desktop::MIN_VERSION) } else { format!("Installed · v{v}") };
        }
    });
}

fn install_agent(job: Arc<Mutex<Job>>) {
    use std::os::windows::process::CommandExt;
    *job.lock().unwrap() = Job { running: true, text: "Installing…".into(), outdated: false };
    std::thread::spawn(move || {
        let res = std::process::Command::new("cmd")
            .args(["/C", "npm", "install", "-g", "agent-desktop@latest"])
            .creation_flags(0x0800_0000)
            .output();
        let text = match res {
            Ok(o) if o.status.success() => "Installed".to_string(),
            Ok(o) => format!("Install failed: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("npm error")),
            Err(_) => "Install needs Node.js (npm). Get it from nodejs.org, then try again.".to_string(),
        };
        *job.lock().unwrap() = Job { running: false, text, outdated: false };
    });
}
