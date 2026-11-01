//! The palette: a Spotlight-style bar that grows into an answer card.
//! Created on hotkey, destroyed on dismiss.

use crate::capture::{self, Capture};
use crate::config::{self, Config};
use crate::{context, history, voice};
use crate::llm::{self, Conversation, Ev};
use crate::overlay::{self, Overlay};
use crate::win::{self, AppEvent};
use egui::{Color32, FontFamily, FontId, Key, Margin, RichText, TextStyle};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::UI::WindowsAndMessaging::*;

// Logical sizes.
const BAR_H: f32 = 60.0;
const MAX_H: f32 = 560.0;
const PAD: f32 = 18.0;
/// Height of the context-chip row under the bar.
const CHIPS_H: f32 = 32.0;

// Palette (dark, quiet, one accent).
const TINT_SOLID: Color32 = Color32::from_rgb(30, 30, 32);
const TEXT: Color32 = Color32::from_rgb(236, 236, 240);
const TEXT_2: Color32 = Color32::from_rgb(152, 152, 160);
const TEXT_3: Color32 = Color32::from_rgb(105, 105, 112);
const HAIRLINE: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 18);
const ERROR: Color32 = Color32::from_rgb(255, 128, 118);

pub enum Outcome {
    Closed,
    Quit,
}

struct Exchange {
    question: String,
    answer: String,
    error: Option<Friendly>,
    /// Labels of the context chips sent with this question.
    attached: Vec<String>,
}

/// How the palette should open.
#[derive(Default)]
pub struct Start {
    /// Context to send with the first question (selected text, files).
    pub chips: Vec<context::Chip>,
    /// Open on the history browser.
    pub history: bool,
    /// Start recording right away (push-to-talk hotkey).
    pub voice: bool,
}

/// A user-facing error: a short sentence, an optional action, and the raw detail.
struct Friendly {
    text: String,
    detail: String,
    settings: bool,
}

fn friendly(raw: &str) -> Friendly {
    let lower = raw.to_ascii_lowercase();
    let (text, settings) = if lower.contains("api key") && lower.contains("no ") {
        ("Add an API key to get started.", true)
    } else if lower.contains("401")
        || lower.contains("authentication")
        || lower.contains("invalid x-api-key")
    {
        ("Your API key was rejected.", true)
    } else if lower.contains("429") || lower.contains("rate limit") || lower.contains("overloaded")
    {
        ("Busy right now. Try again in a moment.", false)
    } else if lower.contains("not found. install claude") {
        ("Claude Code isn't installed.", true)
    } else if lower.contains("connect") || lower.contains("dns") || lower.contains("timed out") {
        ("Couldn't connect. Check your internet connection.", false)
    } else if lower == "cancelled" {
        ("Stopped.", false)
    } else {
        ("Something went wrong.", false)
    };
    Friendly {
        text: text.into(),
        detail: raw.chars().take(160).collect(),
        settings,
    }
}

pub struct Palette {
    cfg: Config,
    cap: Capture,
    jpeg: Arc<Vec<u8>>,
    thumb: Option<egui::TextureHandle>,
    input: String,
    exchanges: Vec<Exchange>,
    conv: Option<Conversation>,
    rx: Option<Receiver<Ev>>,
    cancel: Arc<AtomicBool>,
    status: String,
    overlay: Option<Overlay>,
    hwnd: Option<HWND>,
    frames: u32,
    height: f32,
    body_h: f32,
    was_focused: bool,
    md: egui_commonmark::CommonMarkCache,
    math: std::rc::Rc<std::cell::RefCell<crate::math::MathCache>>,
    notice: Option<String>,
    quit: bool,
    outcome: Arc<std::sync::Mutex<Outcome>>,
    history_dir: Option<std::path::PathBuf>,
    /// An agent-desktop action waiting for Allow / Don't Allow.
    pending: Option<(String, String, std::sync::mpsc::Sender<bool>)>,
    allow_all: bool,
    accent: Color32,
    width: f32,
    /// Context chips waiting to go out with the next question.
    chips: Vec<context::Chip>,
    /// History browser, while it is showing (the input is its search box).
    browser: Option<history::Browser>,
    /// What was typed before the browser opened.
    draft: String,
    /// Earlier turns of a reopened chat, sent with the next question.
    resume: Option<String>,
    /// When the current chat started (kept when a past chat is continued).
    created: Option<String>,
    rec: Option<voice::Recording>,
    /// Set while recording from a held hotkey: when it was pressed.
    ptt: Option<Instant>,
    transcribing: Option<Receiver<Result<String, String>>>,
    voice_dl: Option<Arc<std::sync::Mutex<voice::Download>>>,
    /// Start recording on the first frame / once the model is downloaded.
    voice_pending: bool,
}

/// Opens the palette and blocks until it closes.
#[allow(dead_code)]
pub fn run(cfg: Config, cap: Capture, notice: Option<String>) -> Outcome {
    run_with(cfg, cap, notice, Start::default())
}

/// Like `run`, with extra context or a different starting view.
pub fn run_with(cfg: Config, cap: Capture, notice: Option<String>, start: Start) -> Outcome {
    let outcome = Arc::new(std::sync::Mutex::new(Outcome::Closed));
    let accent = accent_of(&cfg);
    let text_size = cfg.appearance.text_size.clamp(11.0, 22.0);
    let width = cfg.appearance.width.clamp(480.0, 1100.0);
    let out2 = outcome.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenbuddy")
            .with_inner_size([width, BAR_H])
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_taskbar(false)
            .with_resizable(false),
        centered: false,
        ..Default::default()
    };
    let res = eframe::run_native(
        "screenbuddy",
        options,
        Box::new(move |cc| {
            setup_style(&cc.egui_ctx, accent, text_size);
            win::set_ctx(Some(cc.egui_ctx.clone()));
            Ok(Box::new(Palette::new(cfg, cap, notice, out2, start)))
        }),
    );
    win::set_ctx(None);
    if let Err(e) = res {
        config::log(&format!("palette failed: {e}"));
        win::message_box(&format!("screenbuddy could not open its window:\n{e}"));
    }
    std::mem::replace(&mut *outcome.lock().unwrap(), Outcome::Closed)
}

/// Segoe UI Variable (Windows 11) or Segoe UI, with Segoe UI Symbol as glyph
/// fallback and Cascadia/Consolas for code. Read from the Windows font folder.
pub fn setup_fonts(ctx: &egui::Context) {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let read = |file: &str| std::fs::read(format!(r"{windir}\Fonts\{file}")).ok();
    let mut defs = egui::FontDefinitions::default();
    let mut put = |key: &str, bytes: Vec<u8>, fam: FontFamily, front: bool| {
        defs.font_data
            .insert(key.into(), Arc::new(egui::FontData::from_owned(bytes)));
        let list = defs.families.entry(fam).or_default();
        if front {
            list.insert(0, key.into());
        } else {
            list.push(key.into());
        }
    };
    if let Some(b) = read("SegUIVar.ttf").or_else(|| read("segoeui.ttf")) {
        put("ui", b, FontFamily::Proportional, true);
    }
    if let Some(b) = read("CascadiaMono.ttf").or_else(|| read("consola.ttf")) {
        put("mono", b, FontFamily::Monospace, true);
    }
    if let Some(b) = read("seguisym.ttf") {
        put("sym", b.clone(), FontFamily::Proportional, false);
        put("sym", b, FontFamily::Monospace, false);
    }
    ctx.set_fonts(defs);
}

pub fn accent_of(cfg: &Config) -> Color32 {
    let [r, g, b] = config::hex_rgb(&cfg.appearance.accent).unwrap_or([10, 132, 255]);
    Color32::from_rgb(r, g, b)
}

pub fn setup_style(ctx: &egui::Context, accent: Color32, text_size: f32) {
    setup_fonts(ctx);
    let mut v = egui::Visuals::dark();
    v.panel_fill = Color32::TRANSPARENT;
    v.window_fill = Color32::TRANSPARENT;
    v.extreme_bg_color = Color32::from_white_alpha(10);
    v.code_bg_color = Color32::from_white_alpha(16);
    v.selection.bg_fill = accent.linear_multiply(0.45);
    v.selection.stroke.color = accent;
    v.hyperlink_color = accent;
    v.override_text_color = Some(TEXT);
    v.text_cursor.stroke.color = accent;
    v.widgets.noninteractive.bg_stroke.color = HAIRLINE;
    ctx.set_visuals(v);
    ctx.global_style_mut(|s| {
        s.spacing.item_spacing = egui::vec2(8.0, 7.0);
        s.text_styles = [
            (TextStyle::Heading, FontId::proportional(text_size + 4.0)),
            (TextStyle::Body, FontId::proportional(text_size)),
            (TextStyle::Button, FontId::proportional(14.0)),
            (TextStyle::Small, FontId::proportional(12.5)),
            (TextStyle::Monospace, FontId::monospace(13.5)),
        ]
        .into();
        s.interaction.selectable_labels = true;
    });
}

/// Windows 11 chrome: rounded corners and the system shadow.
pub fn apply_chrome(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::*;
    unsafe {
        let dark: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            (&dark as *const i32).cast(),
            4,
        );
        let round = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&round as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            4,
        );
    }
}

fn thumbnail(ctx: &egui::Context, jpeg: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory_with_format(jpeg, image::ImageFormat::Jpeg).ok()?;
    let img = img.thumbnail(240, 160).to_rgb8();
    let ci =
        egui::ColorImage::from_rgb([img.width() as usize, img.height() as usize], img.as_raw());
    Some(ctx.load_texture("shot", ci, egui::TextureOptions::LINEAR))
}

fn status_text(s: &str) -> &'static str {
    match s {
        "drawing…" => "Pointing it out",
        "acting…" => "Working",
        _ => "Thinking",
    }
}

impl Palette {
    fn new(
        cfg: Config,
        cap: Capture,
        notice: Option<String>,
        outcome: Arc<std::sync::Mutex<Outcome>>,
        start: Start,
    ) -> Self {
        let mut cap = cap;
        let jpeg = Arc::new(std::mem::take(&mut cap.jpeg));
        let conv = Conversation::default();
        llm::prewarm(&cfg, &conv);
        let accent = accent_of(&cfg);
        let width = cfg.appearance.width.clamp(480.0, 1100.0);
        Self {
            cfg,
            cap,
            jpeg,
            thumb: None,
            input: String::new(),
            exchanges: Vec::new(),
            conv: Some(conv),
            rx: None,
            cancel: Arc::new(AtomicBool::new(false)),
            status: String::new(),
            overlay: None,
            hwnd: None,
            frames: 0,
            height: BAR_H,
            body_h: 0.0,
            was_focused: false,
            md: Default::default(),
            math: Default::default(),
            notice,
            quit: false,
            outcome,
            history_dir: None,
            pending: None,
            allow_all: false,
            accent,
            width,
            chips: start.chips,
            browser: start.history.then(history::Browser::open),
            draft: String::new(),
            resume: None,
            created: None,
            rec: None,
            ptt: None,
            transcribing: None,
            voice_dl: None,
            voice_pending: start.voice,
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
        let chips = std::mem::take(&mut self.chips);
        let attached: Vec<String> = chips.iter().map(|c| c.label.clone()).collect();
        let attachments: Vec<llm::Image> = chips.iter().filter_map(|c| c.image.clone()).collect();
        let mut prompt = context::compose(&chips, &q);
        if let Some(pre) = self.resume.take() {
            prompt = pre + &prompt;
        }
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
            question: q,
            answer: String::new(),
            error: None,
            attached,
        });
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.status = "sending…".into();
        llm::spawn(
            llm::Turn {
                cfg: self.cfg.clone(),
                conv,
                question: prompt,
                image,
                attachments,
                cancel: self.cancel.clone(),
            },
            llm::Out::new(tx, ctx.clone()),
        );
    }

    /// Takes a fresh screenshot (palette + overlay are excluded from capture)
    /// and starts a new conversation about it.
    fn recapture(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        match capture::grab_monitor_under_cursor().and_then(|shot| capture::encode(&shot, None)) {
            Ok(mut cap) => {
                cap.cursor = self.cap.cursor;
                self.save_history();
                self.jpeg = Arc::new(std::mem::take(&mut cap.jpeg));
                self.cap = cap;
                self.thumb = thumbnail(ctx, &self.jpeg);
                self.conv = Some({
                    let c = Conversation::default();
                    llm::prewarm(&self.cfg, &c);
                    c
                });
                self.overlay = None;
                self.exchanges.clear();
                self.math.borrow_mut().clear();
                self.history_dir = None;
                self.resume = None;
                self.created = None;
                self.notice = None;
            }
            Err(e) => self.notice = Some(format!("Couldn't take a new screenshot ({e}).")),
        }
    }

    fn draw(&mut self, input: serde_json::Value) -> Result<String, String> {
        let color = config::hex_rgb(&self.cfg.drawing.color).unwrap_or([10, 132, 255]);
        let call = overlay::parse_draw(&input, &self.cap, color)?;
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
            // Account for the height the card will grow to.
            let mut cur = capture::Rect::from_win(r);
            cur.h = cur.h.max((MAX_H * self.cap.monitor.scale) as i32);
            if overlap(cur, d) == 0 {
                return;
            }
            let wa = self.cap.monitor.work;
            let m = (16.0 * self.cap.monitor.scale) as i32;
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
                }
                Ev::Status(s) => self.status = s,
                Ev::Draw(input, reply) => {
                    let r = self.draw(input);
                    if let Err(e) = &r {
                        config::log(&format!("draw rejected: {e}"));
                    }
                    let _ = reply.send(r);
                }
                Ev::Approve(intent, detail, reply) => {
                    if self.allow_all || self.cfg.agent_desktop.approval == "never" {
                        let _ = reply.send(true);
                    } else {
                        self.pending = Some((intent, detail, reply));
                    }
                }
                Ev::Error(e) => {
                    if let Some(x) = self.exchanges.last_mut() {
                        x.error = Some(friendly(&e));
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
        let chat = history::Chat {
            created: self.created.get_or_insert_with(crate::util::timestamp).clone(),
            provider: self.cfg.provider_label(),
            exchanges: self
                .exchanges
                .iter()
                .map(|x| history::SavedExchange {
                    question: x.question.clone(),
                    answer: x.answer.clone(),
                    error: x.error.as_ref().map(|e| e.detail.clone()),
                    attached: x.attached.clone(),
                })
                .collect(),
        };
        if let Err(e) = history::write(&dir, &chat) {
            config::log(&format!("saving history failed: {e}"));
        }
    }

    /// Shows a saved chat and lets the next question continue it.
    fn open_chat(&mut self, ctx: &egui::Context, dir: std::path::PathBuf) {
        if self.busy() {
            return;
        }
        let Some(chat) = history::load(&dir) else {
            self.notice = Some("Couldn't open that conversation.".into());
            return;
        };
        self.save_history();
        self.exchanges = chat
            .exchanges
            .iter()
            .map(|x| Exchange {
                question: x.question.clone(),
                answer: x.answer.clone(),
                error: x.error.as_deref().map(friendly),
                attached: x.attached.clone(),
            })
            .collect();
        self.resume = Some(chat.preamble());
        self.created = Some(chat.created).filter(|c| !c.is_empty());
        self.history_dir = Some(dir);
        self.conv = Some({
            let c = Conversation::default();
            llm::prewarm(&self.cfg, &c);
            c
        });
        self.overlay = None;
        self.math.borrow_mut().clear();
        self.browser = None;
        self.input = std::mem::take(&mut self.draft);
        self.notice = None;
        ctx.request_repaint();
    }

    fn toggle_browser(&mut self) {
        if self.browser.take().is_some() {
            self.input = std::mem::take(&mut self.draft);
        } else {
            self.draft = std::mem::take(&mut self.input);
            self.browser = Some(history::Browser::open());
        }
    }

    // ------------------------------------------------------------ voice

    /// Mic button / hotkey: start recording, or stop if already recording.
    fn mic(&mut self, ctx: &egui::Context, hold: bool) {
        if self.rec.is_some() {
            self.stop_rec();
            return;
        }
        if self.transcribing.is_some() {
            return;
        }
        if !voice::engine_included() {
            self.notice = Some("Voice input isn't available in this build (speech engine not included).".into());
            return;
        }
        if !voice::model_ready() {
            if self.voice_dl.is_none() {
                self.voice_dl = Some(voice::start_download(Some(ctx.clone())));
            }
            self.voice_pending = true;
            self.notice = Some("Downloading the voice model (17 MB, once)…".into());
            return;
        }
        self.voice_pending = false;
        match voice::Recording::start(&self.cfg.voice.language, ctx.clone()) {
            Ok(r) => {
                self.rec = Some(r);
                self.ptt = hold.then(Instant::now);
                self.notice = None;
            }
            Err(e) => self.notice = Some(e),
        }
    }

    fn stop_rec(&mut self) {
        self.ptt = None;
        if let Some(r) = self.rec.take() {
            self.transcribing = Some(r.finish());
        }
    }

    /// Hold-to-talk release, model download progress, transcripts.
    fn pump_voice(&mut self, ctx: &egui::Context) {
        if let Some(t0) = self.ptt {
            if self.rec.is_some() && !win::hotkey_held(&self.cfg.voice.hotkey) {
                if t0.elapsed() < Duration::from_millis(350) {
                    // A quick tap: keep recording until the next tap or click.
                    self.ptt = None;
                } else {
                    self.stop_rec();
                }
            }
        }
        if let Some(dl) = &self.voice_dl {
            let d = dl.lock().unwrap().clone();
            if d.done {
                self.voice_dl = None;
                match d.error {
                    Some(e) => {
                        self.voice_pending = false;
                        self.notice = Some(format!("Couldn't download the voice model ({e})."));
                    }
                    None => {
                        self.notice = None;
                        if self.voice_pending {
                            self.mic(ctx, false);
                        }
                    }
                }
            } else {
                self.notice = Some(format!("Downloading the voice model (17 MB, once)… {:.0}%", d.progress * 100.0));
            }
        }
        if let Some(rx) = &self.transcribing {
            match rx.try_recv() {
                Ok(r) => {
                    self.transcribing = None;
                    match r {
                        Ok(t) if !t.is_empty() => {
                            if !self.input.trim().is_empty() {
                                self.input.push(' ');
                            }
                            self.input.push_str(&t);
                            if self.cfg.voice.auto_send && self.browser.is_none() {
                                self.send(ctx);
                            }
                        }
                        Ok(_) => self.notice = Some("Didn't catch that.".into()),
                        Err(e) if e == "discarded" => {}
                        Err(e) => {
                            config::log(&format!("voice: {e}"));
                            self.notice = Some(format!("Voice input failed: {e}"));
                        }
                    }
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
                Err(std::sync::mpsc::TryRecvError::Disconnected) => self.transcribing = None,
            }
        }
        if self.rec.is_some() || self.transcribing.is_some() || self.voice_dl.is_some() {
            ctx.request_repaint_after(Duration::from_millis(33));
        }
    }

    /// Places the bar Spotlight-style: centered, upper third of the monitor
    /// the user is working on.
    fn first_frame(&mut self, frame: &eframe::Frame, ctx: &egui::Context) {
        self.thumb = thumbnail(ctx, &self.jpeg);
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(h.hwnd.get() as *mut _);
        self.hwnd = Some(hwnd);
        apply_chrome(hwnd);
        unsafe {
            if win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let mut r = RECT::default();
            let _ = GetWindowRect(hwnd, &mut r);
            let w = r.right - r.left;
            let work = capture::monitor_at(self.cap.cursor.0, self.cap.cursor.1).work;
            let s = self.cap.monitor.scale;
            let (x, y) = match self.cfg.appearance.position.as_str() {
                "cursor" => {
                    let gap = (16.0 * s) as i32;
                    let max_h = (MAX_H * s) as i32;
                    let (cx, cy) = self.cap.cursor;
                    let x = (cx - w / 2).clamp(work.x, (work.x + work.w - w).max(work.x));
                    let y = if cy + gap + max_h <= work.y + work.h { cy + gap } else { (cy - gap - max_h).max(work.y) };
                    (x, y)
                }
                "center" => (work.x + (work.w - w) / 2, work.y + (work.h - (MAX_H * s) as i32).max(0) / 2),
                _ => (work.x + (work.w - w) / 2, work.y + (work.h as f32 * 0.2) as i32),
            };
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE);
            let _ = SetForegroundWindow(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.rec = None; // discards the recording
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
                // The push-to-talk hotkey records into this palette.
                AppEvent::Hotkey(win::HK_VOICE) => {
                    if self.rec.is_some() && self.ptt.is_none() {
                        self.stop_rec();
                    } else if self.rec.is_none() {
                        self.mic(ctx, true);
                    }
                }
                // Pressing the hotkey again (or clicking the tray) toggles the palette off.
                AppEvent::Hotkey(_) | AppEvent::TrayClick => self.close(ctx),
                // More files from Explorer join the current question.
                AppEvent::OpenFile(p) => {
                    self.chips.push(context::file_chip(&p));
                    if let Some(h) = self.hwnd {
                        unsafe {
                            let _ = SetForegroundWindow(h);
                        }
                    }
                }
                AppEvent::Menu(id) if id == crate::tray::BROWSE => {
                    if self.browser.is_none() {
                        self.toggle_browser();
                    }
                }
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

    /// Like Spotlight: losing focus dismisses — unless annotations are showing,
    /// so the user can click the thing that was pointed out.
    fn dismiss_on_blur(&mut self, ctx: &egui::Context) {
        let focused = ctx.input(|i| i.viewport().focused).unwrap_or(true);
        if focused {
            self.was_focused = true;
        } else if self.was_focused
            && self.cfg.appearance.dismiss_on_blur
            && !self.busy()
            && self.rec.is_none()
            && self.transcribing.is_none()
            && self.overlay.as_ref().is_none_or(|o| o.is_empty())
        {
            self.close(ctx);
        }
    }

    fn bar(&mut self, ui: &mut egui::Ui, enter: bool) {
        ui.horizontal(|ui| {
            ui.set_height(BAR_H - 2.0 * 10.0);
            // Screenshot chip: what the question is about. Click to retake.
            if let Some(t) = &self.thumb {
                let size = t.size_vec2();
                let h = 30.0;
                let w = (size.x / size.y * h).clamp(30.0, 54.0);
                let img = egui::Image::new((t.id(), egui::vec2(w, h)))
                    .corner_radius(5)
                    .sense(egui::Sense::click());
                let r = ui
                    .add(img)
                    .on_hover_text("Screenshot attached · click to retake");
                ui.painter().rect_stroke(
                    r.rect,
                    5,
                    egui::Stroke::new(1.0, HAIRLINE),
                    egui::StrokeKind::Outside,
                );
                if r.clicked() {
                    self.recapture(ui.ctx());
                }
                ui.add_space(4.0);
            }
            let hint = if self.rec.is_some() {
                "Listening…"
            } else if self.transcribing.is_some() {
                "Transcribing…"
            } else if self.browser.is_some() {
                "Search history"
            } else if self.exchanges.is_empty() {
                "Ask about your screen"
            } else {
                "Ask a follow-up"
            };
            let edit = egui::TextEdit::singleline(&mut self.input)
                .hint_text(RichText::new(hint).color(TEXT_3))
                .font(TextStyle::Heading)
                .frame(egui::Frame::NONE)
                .desired_width(f32::INFINITY);
            let voice_on = self.cfg.voice.enabled;
            let gear_w = 26.0 + 34.0 + if voice_on { 34.0 } else { 0.0 };
            let resp = ui.add_sized([ui.available_width() - gear_w, 28.0], edit);
            let hist = icon_button(ui, Icon::History, self.browser.is_some(), self.accent)
                .on_hover_text("History (Ctrl+H)");
            if hist.clicked() {
                self.toggle_browser();
            }
            if voice_on {
                let tip = if self.rec.is_some() {
                    "Stop and transcribe"
                } else {
                    "Speak your question"
                };
                let level = self.rec.as_ref().map(|r| r.level());
                let mic = icon_button(ui, Icon::Mic(level), self.rec.is_some(), self.accent)
                    .on_hover_text(format!("{tip} (hold {})", self.cfg.voice.hotkey));
                if mic.clicked() {
                    self.mic(ui.ctx(), false);
                }
            }
            let gear = ui
                .add(egui::Button::new(RichText::new("⚙").color(TEXT_3).size(16.0)).frame(false))
                .on_hover_text("Settings");
            if gear.clicked() {
                self.open_settings(ui.ctx());
            }
            if !resp.has_focus() && self.frames < 8 {
                resp.request_focus();
            }
            if enter && self.browser.is_none() {
                self.send(ui.ctx());
                resp.request_focus();
            }
        });
        if !self.chips.is_empty() {
            self.chip_row(ui);
        }
    }

    /// Context chips: what goes along with the next question. ✕ removes one.
    fn chip_row(&mut self, ui: &mut egui::Ui) {
        let mut remove = None;
        ui.horizontal(|ui| {
            ui.set_height(CHIPS_H - 8.0);
            ui.add_space(2.0);
            for (i, c) in self.chips.iter().enumerate() {
                egui::Frame::new()
                    .fill(Color32::from_white_alpha(14))
                    .corner_radius(6)
                    .inner_margin(Margin { left: 8, right: 4, top: 2, bottom: 2 })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        let mark = if c.image.is_some() { "▣" } else if c.label == "Selected text" { "❝" } else { "▤" };
                        let label: String = if c.label.chars().count() > 28 {
                            format!("{}…", c.label.chars().take(27).collect::<String>())
                        } else {
                            c.label.clone()
                        };
                        ui.label(RichText::new(format!("{mark} {label}")).size(12.5).color(TEXT_2))
                            .on_hover_text(&c.hover);
                        if ui
                            .add(egui::Button::new(RichText::new("✕").size(11.0).color(TEXT_3)).frame(false))
                            .on_hover_text("Remove")
                            .clicked()
                        {
                            remove = Some(i);
                        }
                    });
            }
        });
        if let Some(i) = remove {
            self.chips.remove(i);
        }
    }

    fn top_h(&self) -> f32 {
        BAR_H + if self.chips.is_empty() { 0.0 } else { CHIPS_H }
    }

    /// The history list, in place of the conversation.
    fn history_body(&mut self, ui: &mut egui::Ui, keys: (bool, bool, bool)) {
        let accent = self.accent;
        let query = self.input.clone();
        let max_h = MAX_H - self.top_h() - 24.0;
        let Some(b) = self.browser.as_mut() else { return };
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(max_h)
            .show(ui, |ui| {
                ui.add_space(8.0);
                let a = b.show(ui, &query, keys, accent, TEXT_2, TEXT_3);
                ui.add_space(10.0);
                a
            });
        self.body_h = out.content_size.y;
        if let Some(history::Action::Open(dir)) = out.inner {
            let ctx = ui.ctx().clone();
            self.open_chat(&ctx, dir);
        }
    }

    fn body(&mut self, ui: &mut egui::Ui) {
        let time = ui.input(|i| i.time);
        let n = self.exchanges.len();
        let busy = self.busy();
        let mut open_settings = false;
        let mut decision: Option<(bool, bool)> = None;
        let accent = self.accent;
        let once = self.cfg.agent_desktop.approval == "once";
        let out = egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .max_height(MAX_H - self.top_h() - 24.0)
            .show(ui, |ui| {
                ui.add_space(10.0);
                for (i, x) in self.exchanges.iter().enumerate() {
                    let last = i + 1 == n;
                    // Earlier turns and the current question read as quiet context.
                    if n > 1 || !x.answer.is_empty() || x.error.is_some() || busy {
                        ui.label(RichText::new(&x.question).color(TEXT_2).size(13.0));
                        ui.add_space(2.0);
                    }
                    if !x.answer.is_empty() {
                        let math = self.math.clone();
                        let render = move |ui: &mut egui::Ui, tex: &str, inline: bool| {
                            crate::math::show(ui, &math, tex, inline)
                        };
                        egui_commonmark::CommonMarkViewer::new()
                            .render_math_fn(Some(&render))
                            .show(ui, &mut self.md, &crate::math::normalize(&x.answer));
                    }
                    if last && busy && x.answer.is_empty() && self.pending.is_none() {
                        // Soft pulsing status instead of a spinner.
                        let a = 0.45 + 0.35 * ((time * 3.2).sin() as f32 * 0.5 + 0.5);
                        ui.label(
                            RichText::new(status_text(&self.status))
                                .color(TEXT_2.gamma_multiply(a)),
                        );
                    }
                    if let Some(e) = &x.error {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&e.text).color(ERROR));
                            if e.settings && ui.link("Open settings").clicked() {
                                open_settings = true;
                            }
                        });
                        if e.text != "Stopped." {
                            ui.label(RichText::new(&e.detail).color(TEXT_3).small());
                        }
                    }
                    if !last {
                        ui.add_space(14.0);
                    }
                }
                if let Some((intent, detail, _)) = &self.pending {
                    ui.add_space(6.0);
                    egui::Frame::new()
                        .fill(Color32::from_white_alpha(12))
                        .stroke(egui::Stroke::new(1.0, HAIRLINE))
                        .corner_radius(10)
                        .inner_margin(Margin::same(14))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(format!(
                                    "Allow screenbuddy to {}?",
                                    lowercase_first(intent)
                                ))
                                .strong(),
                            );
                            ui.label(RichText::new(detail).monospace().color(TEXT_3).size(12.0));
                            ui.add_space(6.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .link(RichText::new("Always allow").size(13.0))
                                    .on_hover_text("Never ask again. Change this in Settings → Agent.")
                                    .clicked()
                                {
                                    decision = Some((true, true));
                                }
                                if once {
                                    ui.label(RichText::new("Allowing covers the rest of this conversation").color(TEXT_3).size(12.0));
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let allow = egui::Button::new(
                                            RichText::new("Allow").color(Color32::WHITE),
                                        )
                                        .fill(accent)
                                        .corner_radius(7)
                                        .min_size(egui::vec2(84.0, 28.0));
                                        if ui.add(allow).clicked() {
                                            decision = Some((true, false));
                                        }
                                        let deny = egui::Button::new("Don't Allow")
                                            .corner_radius(7)
                                            .min_size(egui::vec2(100.0, 28.0));
                                        if ui.add(deny).clicked() {
                                            decision = Some((false, false));
                                        }
                                    },
                                );
                            });
                        });
                }
                if let Some(n) = &self.notice {
                    ui.add_space(4.0);
                    ui.label(RichText::new(n).color(TEXT_3).small());
                }
                ui.add_space(14.0);
            });
        self.body_h = out.content_size.y;
        if let Some((allow, all)) = decision {
            self.decide(allow, all);
        }
        if open_settings {
            self.open_settings(ui.ctx());
        }
    }

    /// Resolves the pending approval. `always` switches approvals off for good.
    fn decide(&mut self, allow: bool, always: bool) {
        if let Some((_, _, reply)) = self.pending.take() {
            let _ = reply.send(allow);
        }
        if allow && self.cfg.agent_desktop.approval == "once" {
            self.allow_all = true;
        }
        if always {
            self.allow_all = true;
            self.cfg.agent_desktop.approval = "never".into();
            let (mut disk, _) = config::load();
            disk.agent_desktop.approval = "never".into();
            if let Err(e) = config::save(&disk) {
                config::log(&format!("saving approval mode failed: {e}"));
            }
        }
    }

    /// Closes the palette and opens the Settings window.
    fn open_settings(&mut self, ctx: &egui::Context) {
        win::push(AppEvent::Menu(crate::tray::SETTINGS.into()));
        self.close(ctx);
    }

    fn has_body(&self) -> bool {
        !self.exchanges.is_empty() || self.notice.is_some() || self.browser.is_some()
    }

    /// Grows/shrinks the window smoothly toward its content height.
    fn animate_height(&mut self, ctx: &egui::Context) {
        let has_body = self.has_body();
        let target = if has_body {
            (self.top_h() + 1.0 + self.body_h).min(MAX_H)
        } else {
            self.top_h()
        };
        let diff = target - self.height;
        if diff.abs() > 0.5 {
            self.height += diff * 0.35;
            if diff.abs() < 2.0 {
                self.height = target;
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                self.width,
                self.height.round(),
            )));
            ctx.request_repaint();
        }
    }
}

impl eframe::App for Palette {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn ui(&mut self, ui: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.frames == 0 {
            self.first_frame(frame, &ctx);
        }
        self.frames += 1;
        self.pump();
        self.handle_app_events(&ctx);
        self.dismiss_on_blur(&ctx);
        if self.voice_pending && self.frames == 2 {
            self.mic(&ctx, true);
        }
        self.pump_voice(&ctx);
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::H)) {
            self.toggle_browser();
        }
        let mut keys = (false, false, false);
        if self.browser.is_some() {
            keys = ctx.input_mut(|i| {
                (
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                    i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                    i.consume_key(egui::Modifiers::NONE, Key::Enter),
                )
            });
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.toggle_browser();
            }
        }
        if self.rec.is_some() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.rec = None; // discard
                self.ptt = None;
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                self.stop_rec();
            }
        }

        if self.pending.is_some() {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
                self.decide(false, false);
            } else if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter)) {
                self.decide(true, false);
            }
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if self.busy() {
                // The worker notices the flag and hands the conversation back via Done.
                self.cancel.store(true, Ordering::Relaxed);
                if let Some(x) = self.exchanges.last_mut() {
                    x.error = Some(friendly("cancelled"));
                }
            } else {
                self.close(&ctx);
                return;
            }
        }
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));

        // Background: solid card; DWM rounds the corners.
        let full = ui.max_rect();
        let fill = TINT_SOLID;
        ui.painter().rect_filled(full, 8, fill);

        egui::Frame::new()
            .inner_margin(Margin {
                left: PAD as i8 - 4,
                right: PAD as i8,
                top: 10,
                bottom: 10,
            })
            .show(ui, |ui| {
                ui.set_width(full.width() - 2.0 * PAD + 4.0);
                self.bar(ui, enter);
            });
        if self.has_body() {
            let y = ui.cursor().top();
            ui.painter()
                .hline(full.x_range(), y, egui::Stroke::new(1.0, HAIRLINE));
            egui::Frame::new()
                .inner_margin(Margin::symmetric(PAD as i8, 0))
                .show(ui, |ui| {
                    ui.set_width(full.width() - 2.0 * PAD);
                    if self.browser.is_some() {
                        self.history_body(ui, keys);
                    } else {
                        self.body(ui);
                    }
                });
        }
        self.animate_height(&ctx);
        if self.busy() {
            ctx.request_repaint_after(std::time::Duration::from_millis(33));
        }
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

fn lowercase_first(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) if !s.chars().nth(1).is_some_and(char::is_uppercase) => {
            f.to_lowercase().chain(c).collect()
        }
        Some(f) => std::iter::once(f).chain(c).collect(),
        None => String::new(),
    }
}

enum Icon {
    History,
    /// Microphone; `Some(level)` while recording.
    Mic(Option<f32>),
}

/// A small painted icon button (no font glyph dependency).
fn icon_button(ui: &mut egui::Ui, icon: Icon, active: bool, accent: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
    let c = rect.center();
    let p = ui.painter();
    let fg = if active { Color32::WHITE } else if resp.hovered() { TEXT_2 } else { TEXT_3 };
    if active {
        let bg = match icon {
            Icon::Mic(_) => Color32::from_rgb(220, 60, 60),
            Icon::History => accent,
        };
        p.circle_filled(c, 12.0, bg);
    } else if resp.hovered() {
        p.circle_filled(c, 12.0, Color32::from_white_alpha(10));
    }
    let st = egui::Stroke::new(1.5, fg);
    match icon {
        Icon::History => {
            p.circle_stroke(c, 7.0, st);
            p.line_segment([c, c + egui::vec2(0.0, -4.5)], st);
            p.line_segment([c, c + egui::vec2(3.5, 1.5)], st);
        }
        Icon::Mic(level) => {
            if let Some(l) = level {
                p.circle_stroke(c, 12.0 + 3.0 * l, egui::Stroke::new(1.5, Color32::from_rgb(220, 60, 60).gamma_multiply(0.6)));
            }
            let body = egui::Rect::from_center_size(c + egui::vec2(0.0, -2.5), egui::vec2(6.0, 10.0));
            p.rect_stroke(body, 3, st, egui::StrokeKind::Middle);
            let arc: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::PI * i as f32 / 12.0;
                    c + egui::vec2(-5.5 * a.cos(), 5.5 * a.sin() - 1.0)
                })
                .collect();
            p.add(egui::Shape::line(arc, st));
            p.line_segment([c + egui::vec2(0.0, 4.5), c + egui::vec2(0.0, 7.5)], st);
        }
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp
}
