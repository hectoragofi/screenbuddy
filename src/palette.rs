//! The egui palette window. Created on hotkey, destroyed on dismiss.

use crate::capture::{self, Capture};
use crate::config::{self, Config};
use crate::llm::{self, Conversation, Ev};
use crate::overlay::{self, Overlay};
use crate::win::{self, AppEvent};
use egui::{Color32, CornerRadius, Key, Margin, RichText, Stroke};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

const BG: Color32 = Color32::from_rgb(24, 24, 27);
const PANEL: Color32 = Color32::from_rgb(34, 34, 38);
const ACCENT: Color32 = Color32::from_rgb(255, 99, 72);
const DIM: Color32 = Color32::from_rgb(140, 140, 150);

pub enum Outcome {
    Closed,
    Quit,
}

struct Exchange {
    question: String,
    answer: String,
    error: Option<String>,
}

pub struct Palette {
    cfg: Config,
    cap: Capture,
    jpeg: Arc<Vec<u8>>,
    input: String,
    exchanges: Vec<Exchange>,
    conv: Option<Conversation>,
    rx: Option<Receiver<Ev>>,
    cancel: Arc<AtomicBool>,
    status: String,
    overlay: Option<Overlay>,
    hwnd: Option<HWND>,
    frames: u32,
    md: egui_commonmark::CommonMarkCache,
    notice: Option<String>,
    quit: bool,
    outcome: Arc<std::sync::Mutex<Outcome>>,
    history_dir: Option<std::path::PathBuf>,
}

/// Opens the palette and blocks until it closes.
pub fn run(cfg: Config, cap: Capture, notice: Option<String>) -> Outcome {
    let outcome = Arc::new(std::sync::Mutex::new(Outcome::Closed));
    let out2 = outcome.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenbuddy")
            .with_inner_size([560.0, 440.0])
            .with_min_inner_size([360.0, 220.0])
            .with_decorations(false)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(true),
        centered: false,
        ..Default::default()
    };
    let res = eframe::run_native(
        "screenbuddy",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx);
            win::set_ctx(Some(cc.egui_ctx.clone()));
            Ok(Box::new(Palette::new(cfg, cap, notice, out2)))
        }),
    );
    win::set_ctx(None);
    if let Err(e) = res {
        config::log(&format!("palette failed: {e}"));
        win::message_box(&format!("screenbuddy could not open its window:\n{e}"));
    }
    std::mem::replace(&mut *outcome.lock().unwrap(), Outcome::Closed)
}

/// Uses Segoe UI for text and Segoe UI Symbol as glyph fallback, read from
/// the Windows font folder (nothing embedded in the exe).
fn setup_fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let mut defs = egui::FontDefinitions::default();
    let mut load = |key: &str, file: &str, primary: bool| {
        let Ok(bytes) = std::fs::read(format!(r"{windir}\Fonts\{file}")) else {
            return;
        };
        defs.font_data
            .insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
        let fam = defs
            .families
            .entry(egui::FontFamily::Proportional)
            .or_default();
        if primary {
            fam.insert(0, key.into());
        } else {
            fam.push(key.into());
        }
        defs.families
            .entry(egui::FontFamily::Monospace)
            .or_default()
            .push(key.into());
    };
    load("segoe", "segoeui.ttf", true);
    load("symbols", "seguisym.ttf", false);
    ctx.set_fonts(defs);
}

fn setup_style(ctx: &egui::Context) {
    setup_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = BG;
    v.window_fill = BG;
    v.extreme_bg_color = PANEL;
    v.selection.bg_fill = ACCENT.linear_multiply(0.6);
    v.hyperlink_color = ACCENT;
    v.override_text_color = Some(Color32::from_rgb(225, 225, 230));
    v.widgets.hovered.corner_radius = CornerRadius::same(6);
    v.widgets.inactive.corner_radius = CornerRadius::same(6);
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 6.0);
        s.spacing.button_padding = egui::vec2(8.0, 4.0);
    });
}

impl Palette {
    fn new(
        cfg: Config,
        cap: Capture,
        notice: Option<String>,
        outcome: Arc<std::sync::Mutex<Outcome>>,
    ) -> Self {
        let mut cap = cap;
        let jpeg = Arc::new(std::mem::take(&mut cap.jpeg));
        Self {
            cfg,
            cap,
            jpeg,
            input: String::new(),
            exchanges: Vec::new(),
            conv: Some(Conversation::default()),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            status: String::new(),
            overlay: None,
            hwnd: None,
            frames: 0,
            md: Default::default(),
            notice,
            quit: false,
            outcome,
            history_dir: None,
        }
    }

    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn send(&mut self, ctx: &egui::Context) {
        let q = self.input.trim().to_string();
        if q.is_empty() || self.busy() {
            return;
        }
        let Some(mut conv) = self.conv.take() else {
            return;
        };
        self.input.clear();
        self.notice = None;
        let image = if conv.image_sent {
            None
        } else {
            conv.image_sent = true;
            Some(llm::Image {
                jpeg: self.jpeg.clone(),
                w: self.cap.img_w,
                h: self.cap.img_h,
            })
        };
        self.exchanges.push(Exchange {
            question: q.clone(),
            answer: String::new(),
            error: None,
        });
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.status = "sending…".into();
        llm::spawn(
            llm::Turn {
                cfg: self.cfg.clone(),
                conv,
                question: q,
                image,
                cancel: self.cancel.clone(),
            },
            llm::Out::new(tx, ctx.clone()),
        );
    }

    /// Takes a fresh screenshot (palette + overlay are excluded from capture)
    /// and starts a new conversation about it.
    fn recapture(&mut self) {
        if self.busy() {
            return;
        }
        match capture::grab_monitor_under_cursor().and_then(|shot| capture::encode(&shot, None)) {
            Ok(mut cap) => {
                cap.cursor = self.cap.cursor;
                self.jpeg = Arc::new(std::mem::take(&mut cap.jpeg));
                self.cap = cap;
                self.conv = Some(Conversation::default());
                self.overlay = None;
                self.save_history();
                self.exchanges.clear();
                self.history_dir = None;
                self.notice = Some("New screenshot attached.".into());
            }
            Err(e) => self.notice = Some(format!("Capture failed: {e}")),
        }
    }

    fn draw(&mut self, input: serde_json::Value) -> Result<String, String> {
        let call = overlay::parse_draw(&input, &self.cap)?;
        let n = call.shapes.len();
        if self.overlay.is_none() {
            let ov = Overlay::new(self.cap.monitor.rect, self.cap.monitor.scale)
                .map_err(|e| e.to_string())?;
            self.overlay = Some(ov);
        }
        let ov = self.overlay.as_mut().unwrap();
        ov.apply(call);
        let drawn = ov.drawn_bounds();
        self.dodge(drawn);
        self.raise();
        Ok(format!("Drew {n} annotation(s) on the user's screen."))
    }

    /// Moves the palette out of the way if it covers the annotations: tries
    /// the four corners of the work area and picks the least-overlapping one.
    fn dodge(&self, drawn: Option<capture::Rect>) {
        let (Some(h), Some(d)) = (self.hwnd, drawn) else {
            return;
        };
        let overlap = |a: capture::Rect, b: capture::Rect| {
            let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
            let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
            (w.max(0) as i64) * (h.max(0) as i64)
        };
        unsafe {
            let mut r = RECT::default();
            if GetWindowRect(h, &mut r).is_err() {
                return;
            }
            let cur = capture::Rect::from_win(r);
            if overlap(cur, d) == 0 {
                return;
            }
            let wa = self.cap.monitor.work;
            let m = (12.0 * self.cap.monitor.scale) as i32;
            let corners = [
                (wa.x + wa.w - cur.w - m, wa.y + wa.h - cur.h - m),
                (wa.x + m, wa.y + wa.h - cur.h - m),
                (wa.x + wa.w - cur.w - m, wa.y + m),
                (wa.x + m, wa.y + m),
            ];
            let best = corners
                .iter()
                .map(|&(x, y)| {
                    (
                        overlap(
                            capture::Rect {
                                x,
                                y,
                                w: cur.w,
                                h: cur.h,
                            },
                            d,
                        ),
                        x,
                        y,
                    )
                })
                .min_by_key(|c| c.0)
                .unwrap();
            if best.0 < overlap(cur, d) {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    best.1,
                    best.2,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    /// Keeps the palette above the overlay (both are topmost).
    fn raise(&self) {
        if let Some(h) = self.hwnd {
            unsafe {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    fn pump(&mut self) {
        let Some(rx) = self.rx.take() else { return };
        let mut keep = true;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Ev::Delta(t) => {
                    if let Some(x) = self.exchanges.last_mut() {
                        x.answer.push_str(&t);
                    }
                    self.status.clear();
                }
                Ev::Status(s) => self.status = s,
                Ev::Draw(input, reply) => {
                    let r = self.draw(input);
                    if let Err(e) = &r {
                        config::log(&format!("draw rejected: {e}"));
                    }
                    let _ = reply.send(r);
                }
                Ev::Error(e) => {
                    if let Some(x) = self.exchanges.last_mut() {
                        x.error = Some(e);
                    }
                }
                Ev::Done(conv) => {
                    self.conv = Some(conv);
                    self.status.clear();
                    keep = false;
                    self.save_history();
                }
            }
        }
        if keep {
            self.rx = Some(rx);
        }
    }

    fn save_history(&mut self) {
        if !self.cfg.history || self.exchanges.is_empty() {
            return;
        }
        let dir = self
            .history_dir
            .get_or_insert_with(|| config::history_dir().join(crate::util::file_stamp()))
            .clone();
        if std::fs::create_dir_all(&dir).is_err() {
            return;
        }
        let shot = dir.join("screenshot.jpg");
        if !shot.exists() {
            let _ = std::fs::write(&shot, &*self.jpeg);
        }
        let mut md = format!(
            "# screenbuddy · {}\n\n_{}_\n\n![screenshot](screenshot.jpg)\n\n",
            crate::util::timestamp(),
            self.cfg.provider_label()
        );
        for x in &self.exchanges {
            md.push_str(&format!("## {}\n\n{}\n\n", x.question, x.answer));
            if let Some(e) = &x.error {
                md.push_str(&format!("> error: {e}\n\n"));
            }
        }
        let _ = std::fs::write(dir.join("chat.md"), md);
    }

    fn first_frame(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(h.hwnd.get() as *mut _);
        self.hwnd = Some(hwnd);
        unsafe {
            if crate::win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let work = capture::monitor_at(self.cap.cursor.0, self.cap.cursor.1).work;
            let gap = (16.0 * self.cap.monitor.scale) as i32;
            let mut x = self.cap.cursor.0 + gap;
            let mut y = self.cap.cursor.1 + gap;
            if x + w > work.x + work.w {
                x = self.cap.cursor.0 - gap - w;
            }
            if y + h > work.y + work.h {
                y = self.cap.cursor.1 - gap - h;
            }
            x = x.clamp(work.x, (work.x + work.w - w).max(work.x));
            y = y.clamp(work.y, (work.y + work.h - h).max(work.y));
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE);
            let _ = SetForegroundWindow(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.save_history();
        if self.quit {
            *self.outcome.lock().unwrap() = Outcome::Quit;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    fn handle_app_events(&mut self, ctx: &egui::Context) {
        let mut requeue = Vec::new();
        while let Some(ev) = win::pop() {
            match ev {
                // Pressing the hotkey again (or clicking the tray) toggles the palette off.
                AppEvent::Hotkey(_) | AppEvent::TrayClick => self.close(ctx),
                AppEvent::Menu(id) => match crate::tray::handle_passive(&id) {
                    crate::tray::Passive::Handled => {}
                    crate::tray::Passive::Quit => {
                        self.quit = true;
                        self.close(ctx);
                    }
                    crate::tray::Passive::NeedsIdle => {
                        requeue.push(AppEvent::Menu(id));
                        self.close(ctx);
                    }
                },
            }
        }
        for ev in requeue {
            win::push(ev);
        }
    }

    fn header(&mut self, ui: &mut egui::Ui) {
        let resp = ui
            .horizontal(|ui| {
                ui.label(RichText::new("●").color(ACCENT).size(14.0));
                ui.label(RichText::new("screenbuddy").strong());
                ui.label(RichText::new(self.cfg.provider_label()).color(DIM).small());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(egui::Button::new("✕").frame(false))
                        .on_hover_text("Close (Esc)")
                        .clicked()
                    {
                        self.close(ui.ctx());
                    }
                    let has_marks = self.overlay.as_ref().is_some_and(|o| !o.is_empty());
                    if ui
                        .add_enabled(has_marks, egui::Button::new("⌫").frame(false))
                        .on_hover_text("Clear annotations")
                        .clicked()
                    {
                        if let Some(o) = self.overlay.as_mut() {
                            o.clear();
                        }
                    }
                    if ui
                        .add_enabled(!self.busy(), egui::Button::new("⟳").frame(false))
                        .on_hover_text("New screenshot")
                        .clicked()
                    {
                        self.recapture();
                    }
                    // Remaining space is a drag handle.
                    let rest = ui.available_rect_before_wrap();
                    ui.allocate_rect(rest, egui::Sense::click_and_drag())
                })
                .inner
            })
            .inner;
        if resp.drag_started() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
    }

    fn transcript(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.exchanges.is_empty() {
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(format!(
                            "Screenshot attached ({}×{}). Ask anything about what's on screen.",
                            self.cap.img_w, self.cap.img_h
                        ))
                        .color(DIM),
                    );
                }
                let n = self.exchanges.len();
                for (i, x) in self.exchanges.iter().enumerate() {
                    ui.add_space(4.0);
                    egui::Frame::new()
                        .fill(PANEL)
                        .corner_radius(8)
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(RichText::new(&x.question).strong());
                        });
                    ui.add_space(2.0);
                    if !x.answer.is_empty() {
                        egui_commonmark::CommonMarkViewer::new().show(ui, &mut self.md, &x.answer);
                    }
                    if let Some(e) = &x.error {
                        ui.label(
                            RichText::new(format!("⚠ {e}")).color(Color32::from_rgb(255, 120, 110)),
                        );
                    }
                    if i + 1 == n && self.rx.is_some() {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label(
                                RichText::new(if self.status.is_empty() {
                                    "…"
                                } else {
                                    &self.status
                                })
                                .color(DIM),
                            );
                        });
                    }
                }
                if let Some(n) = &self.notice {
                    ui.add_space(4.0);
                    ui.label(RichText::new(n).color(DIM).small());
                }
            });
    }
}

impl eframe::App for Palette {
    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.frames == 0 {
            self.first_frame(frame, &ctx);
        }
        self.frames += 1;
        self.pump();
        self.handle_app_events(&ctx);

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if self.busy() {
                // The worker notices the flag and hands the conversation back via Done.
                self.cancel.store(true, Ordering::Relaxed);
                if let Some(x) = self.exchanges.last_mut() {
                    x.error = Some("cancelled".into());
                }
                self.status.clear();
            } else {
                self.close(&ctx);
                return;
            }
        }
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));

        egui::Frame::new()
            .fill(BG)
            .stroke(Stroke::new(1.0, Color32::from_rgb(60, 60, 66)))
            .inner_margin(Margin::same(10))
            .show(ui, |ui| {
                ui.set_min_size(ui.available_size());
                self.header(ui);
                ui.separator();
                let input_h = 64.0;
                let avail = ui.available_height() - input_h - 8.0;
                ui.allocate_ui(egui::vec2(ui.available_width(), avail.max(40.0)), |ui| {
                    self.transcript(ui)
                });
                ui.add_space(4.0);
                let hint = if self.exchanges.is_empty() {
                    "Ask about your screen…   Enter to send · Shift+Enter newline · Esc close"
                } else {
                    "Follow up…"
                };
                let edit = egui::TextEdit::multiline(&mut self.input)
                    .hint_text(hint)
                    .desired_rows(2)
                    .desired_width(f32::INFINITY);
                let resp = ui.add_sized([ui.available_width(), input_h - 8.0], edit);
                if !self.busy() && !resp.has_focus() && self.frames < 5 {
                    resp.request_focus();
                }
                if enter {
                    self.send(&ctx);
                    resp.request_focus();
                }
            });
        // Keep pumping while the worker streams (it also requests repaints).
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        }
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
