//! The palette: a Spotlight-style bar that grows into an answer card.
//! Created on hotkey, destroyed on dismiss.

use crate::capture::{self, Capture};
use crate::config::{self, Config};
use crate::{context, history, mention, voice};
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

// Logical sizes. The window never changes size: it reserves room for the tabs
// above the pill and the tallest answer below it, with a little margin, and a
// window region keeps everything the surface isn't using click-through.
const PILL_H: f32 = 52.0;
const PILL_RADIUS: f32 = 26.0;
const MAX_H: f32 = 480.0;
/// Padding of the answer inside the surface.
const PAD: f32 = 18.0;
/// Transparent margin, so the surface's edge isn't cut off.
const SIDE: f32 = 4.0;
const BOTTOM: f32 = 4.0;
const TOP: f32 = 12.0;
const TAB_H: f32 = 30.0;
/// A tab's shape reaches this far past its chip.
const TAB_LIP: f32 = 4.0;
/// The pill's top edge, in window coordinates.
const PILL_TOP: f32 = TOP + TAB_H + TAB_LIP * 2.0;
const WINDOW_H: f32 = PILL_TOP + MAX_H + BOTTOM;
/// Most tabs shown; the rest are summed up in a last one.
const MAX_TABS: usize = 4;
/// Where the tabs start, measured from the pill's left edge.
const TAB_INDENT: f32 = 28.0;
const CHIP_H: f32 = 38.0;
const ICON_H: f32 = 30.0;

// Tab motion.
const TAB_RISE: f32 = 0.22;
const TAB_SINK: f32 = 0.14;
const TAB_TEXT_DELAY: f32 = 0.12;
const TAB_TEXT: f32 = 0.12;
/// A plain fade replaces the movement under reduced motion.
const REDUCED_FADE: f32 = 0.12;

const TEXT: Color32 = crate::surface::TEXT;
const TEXT_2: Color32 = crate::surface::TEXT_2;
const SURFACE: Color32 = crate::surface::SURFACE;
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
    /// Background agents the answer spawned (live state comes from `agents`).
    agents: Vec<crate::agents::AgentId>,
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
    /// Blurred screenshot behind the surface; None for a solid surface.
    pub backdrop: Option<Arc<crate::surface::Backdrop>>,
    /// Selected text still being read; becomes a chip before the palette shows.
    pub selection: Option<context::PendingSelection>,
    /// Open on this saved conversation (a clicked quick-ask caption).
    pub open_chat: Option<std::path::PathBuf>,
}

/// A user-facing error: a short sentence, an optional action, and the raw detail.
struct Friendly {
    text: String,
    detail: String,
    settings: bool,
}

fn friendly(raw: &str) -> Friendly {
    let lower = raw.to_ascii_lowercase();
    let (text, settings) = if raw.starts_with(llm::INTERRUPTED) {
        ("The answer stopped partway.", false)
    } else if lower.contains("api key") && lower.contains("no ") {
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
        detail: raw
            .strip_prefix(llm::INTERRUPTED)
            .unwrap_or(raw)
            .chars()
            .take(160)
            .collect(),
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
    /// The surface's height, following the measured content.
    pour: crate::surface::Smooth,
    /// A narrower column that gets there first, so the surface pours.
    lead: crate::surface::Smooth,
    body_h: f32,
    /// None if the shader didn't build; the surface is then plain rectangles.
    renderer: Option<Arc<crate::surface::Renderer>>,
    backdrop: Option<Arc<crate::surface::Backdrop>>,
    selection: Option<context::PendingSelection>,
    reduce: bool,
    /// Window pixels per point right now.
    scale: f32,
    last_time: f64,
    tabs: Vec<Tab>,
    buds: Vec<Bud>,
    /// The window region now set: (top, bottom) in pixels.
    region: Option<(i32, i32)>,
    was_focused: bool,
    md: egui_commonmark::CommonMarkCache,
    math: std::rc::Rc<std::cell::RefCell<crate::math::MathCache>>,
    diagrams: crate::diagram::DiagramCache,
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
    open_on_start: Option<std::path::PathBuf>,
    /// egui time (s) at which to hand cold pages back to the OS; see `trim_when_idle`.
    trim_at: f64,
    /// The user dragged the palette; its spot is saved on close.
    moved: bool,
    /// The `@` menu, while an `@word` is being typed.
    mention: Option<mention::Menu>,
    /// Mentions still being fetched (web pages, screenshots, clipboard).
    attaching: Vec<mention::Pending>,
    /// Send as soon as `attaching` is done.
    send_when_ready: bool,
    /// Give the input focus back on the next frame.
    refocus: bool,
    /// Move the input's cursor to the end of the text on the next frame.
    cursor_end: bool,
    /// Recent mic levels for the recording waveform, oldest first.
    wave: Vec<f32>,
    wave_t: f64,
    /// Typed answers to agents' questions, by agent id.
    agent_drafts: std::collections::HashMap<String, String>,
}

/// A context tab's motion. It outlives its chip by the time it takes to sink.
struct Tab {
    key: String,
    born: f64,
    gone: Option<f64>,
    x: crate::surface::Smooth,
    width: f32,
    /// Index into `chips`; None for the "Attaching…" tab and after removal.
    chip: Option<usize>,
}

/// The droplet as it breaks away from the surface, before the overlay takes over.
struct Bud {
    from: egui::Pos2,
    to: egui::Pos2,
    start: f64,
}

/// Opens the palette and blocks until it closes.
#[allow(dead_code)]
pub fn run(cfg: Config, cap: Capture, notice: Option<String>) -> Outcome {
    run_with(cfg, move || Ok(cap), notice, Start::default())
}

/// Like `run`, with extra context or a different starting view.
///
/// `cap` hands over the screenshot once the window exists, so it can still be
/// encoding while the window is being made.
pub fn run_with(
    cfg: Config,
    cap: impl FnOnce() -> anyhow::Result<Capture> + 'static,
    notice: Option<String>,
    start: Start,
) -> Outcome {
    let outcome = Arc::new(std::sync::Mutex::new(Outcome::Closed));
    let accent = accent_of(&cfg);
    let text_size = cfg.appearance.text_size.clamp(11.0, 22.0);
    let width = cfg.appearance.width.clamp(480.0, 1100.0);
    let out2 = outcome.clone();
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("screenbuddy")
            .with_inner_size([width + SIDE * 2.0, WINDOW_H])
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
            let cap = cap().map_err(|e| format!("Screenshot encoding failed: {e}"))?;
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
/// Seconds after opening at which start-up memory (GL driver init, shader
/// compilation, font loading) is trimmed from the working set.
const TRIM_AFTER_OPEN: f64 = 1.0;

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
            (TextStyle::Heading, FontId::proportional(text_size + 1.0)),
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

/// The surface draws its own corners, and Windows shouldn't add a shadow either.
fn plain_chrome(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::*;
    let square = DWMWCP_DONOTROUND;
    let _ = unsafe {
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            (&square as *const DWM_WINDOW_CORNER_PREFERENCE).cast(),
            4,
        )
    };
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
        "zooming…" => "Looking closer",
        "looking…" => "Looking",
        "watching…" => "Watching for your next step · Esc stops",
        "searching the web…" => "Searching the web",
        "reading results…" => "Reading results",
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
            pour: crate::surface::Smooth::new(PILL_H),
            lead: crate::surface::Smooth::new(PILL_H),
            body_h: 0.0,
            renderer: None,
            backdrop: start.backdrop,
            selection: start.selection,
            reduce: crate::surface::reduced_motion(),
            scale: 1.0,
            last_time: 0.0,
            tabs: Vec::new(),
            buds: Vec::new(),
            region: None,
            was_focused: false,
            md: Default::default(),
            math: Default::default(),
            diagrams: Default::default(),
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
            open_on_start: start.open_chat,
            trim_at: TRIM_AFTER_OPEN,
            moved: false,
            mention: None,
            attaching: Vec::new(),
            send_when_ready: false,
            refocus: false,
            cursor_end: false,
            wave: Vec::new(),
            wave_t: 0.0,
            agent_drafts: Default::default(),
        }
    }


    fn busy(&self) -> bool {
        self.rx.is_some()
    }

    fn send(&mut self, ctx: &egui::Context) {
        if let Some((start, url)) = mention::typed_url(&format!("{} ", self.input.trim_end())) {
            self.input.truncate(start);
            self.attaching.push(mention::resolve_url(url));
        }
        if !self.attaching.is_empty() {
            self.send_when_ready = !self.input.trim().is_empty();
            return;
        }
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
            agents: Vec::new(),
        });
        let (tx, rx) = channel();
        self.rx = Some(rx);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.status = "sending…".into();
        crate::island::buddy(crate::island::BuddyState::Thinking);
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

    /// Most of the open palette's working set is touched once while the GL
    /// driver and egui start up (or while an answer streams) and then never
    /// again. Trimming once things are idle returns those pages; anything still
    /// in use soft-faults back in from RAM, so there is no visible cost.
    fn trim_when_idle(&mut self, ctx: &egui::Context) {
        let now = ctx.input(|i| i.time);
        if self.frames < 3 {
            ctx.request_repaint_after(std::time::Duration::from_secs_f64(TRIM_AFTER_OPEN));
        } else if now >= self.trim_at && !self.busy() {
            self.trim_at = f64::INFINITY;
            win::trim_memory();
        }
    }

    /// Stops the answer in progress. The worker notices the flag and hands the
    /// conversation back via Done.
    fn stop_turn(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        crate::island::buddy(crate::island::BuddyState::Idle);
        if let Some(x) = self.exchanges.last_mut() {
            x.error = Some(friendly("cancelled"));
        }
    }

    /// Asks the last question again after it failed or was stopped. The
    /// worker already rolled the conversation back to before that question.
    fn retry(&mut self, ctx: &egui::Context) {
        if self.busy() || self.exchanges.last().is_none_or(|x| x.error.is_none()) {
            return;
        }
        let Some(x) = self.exchanges.pop() else { return };
        let typed = std::mem::replace(&mut self.input, x.question);
        self.send(ctx);
        self.input = typed;
    }

    /// Takes a fresh screenshot (palette + overlay are excluded from capture)
    /// and starts a new conversation about it.
    fn recapture(&mut self, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let shot = capture::grab_monitor_under_cursor();
        let backdrop = shot.as_ref().ok().and_then(|s| {
            crate::surface::Backdrop::new(s, self.cfg.appearance.blur)
        });
        match shot.and_then(|shot| capture::encode(&shot, None)) {
            Ok(mut cap) => {
                self.backdrop = backdrop.clone();
                if let Some(r) = &self.renderer {
                    r.set_backdrop(backdrop);
                }
                cap.cursor = self.cap.cursor;
                crate::vision::set_capture(&cap, &self.cfg);
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
                self.diagrams.clear();
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
        ov.apply(call, !self.reduce);
        let drawn = ov.drawn_bounds();
        self.dodge(drawn);
        self.raise();
        self.launch_droplets();
        Ok(format!("Drew {n} annotation(s) on the user's screen."))
    }

    /// Window pixels per point, as this monitor uses them.
    fn ppp(&self) -> f32 {
        self.scale
    }

    /// Top-left of the window in virtual-screen pixels.
    fn window_pos(&self) -> (i32, i32) {
        let mut r = RECT::default();
        if let Some(h) = self.hwnd {
            let _ = unsafe { GetWindowRect(h, &mut r) };
        }
        (r.left, r.top)
    }

    /// Lets droplets go toward whatever was just drawn, each from a bud at the
    /// edge of the surface nearest its target.
    fn launch_droplets(&mut self) {
        let (wx, wy) = self.window_pos();
        let ppp = self.ppp();
        let left = wx as f32 + SIDE * ppp;
        let top = wy as f32 + PILL_TOP * ppp;
        let surface = (left, top, left + self.width * ppp, top + self.pour.value * ppp);
        let now = self.last_time;
        let Some(ov) = self.overlay.as_mut() else { return };
        for (edge, to, wait) in ov.launch(surface) {
            let pt = |p: (f32, f32)| egui::pos2((p.0 - wx as f32) / ppp, (p.1 - wy as f32) / ppp);
            let (edge, to) = (pt(edge), pt(to));
            let out = (to - edge).normalized();
            self.buds.push(Bud {
                from: edge - out * 14.0,
                to: edge + out * 8.0,
                start: now + wait as f64,
            });
        }
    }

    /// Moves the palette out of the way if it covers the annotations: tries the
    /// four corners of the work area and picks the least-overlapping one.
    fn dodge(&self, drawn: Option<capture::Rect>) {
        let (Some(h), Some(d)) = (self.hwnd, drawn) else {
            return;
        };
        let overlap = |a: capture::Rect, b: capture::Rect| {
            let w = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
            let h = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);
            (w.max(0) as i64) * (h.max(0) as i64)
        };
        let s = self.ppp();
        let (off_x, off_y) = ((SIDE * s) as i32, (PILL_TOP * s) as i32);
        let (wx, wy) = self.window_pos();
        // The surface, not the window: most of the window is see-through. Account
        // for the height the surface can grow to.
        let cur = capture::Rect {
            x: wx + off_x,
            y: wy + off_y,
            w: (self.width * s) as i32,
            h: (MAX_H * s) as i32,
        };
        if overlap(cur, d) == 0 {
            return;
        }
        let wa = self.cap.monitor.work;
        let m = (16.0 * s) as i32;
        let top = wa.y + ((PILL_TOP - TOP) * s) as i32;
        let corners = [
            (wa.x + wa.w - cur.w - m, wa.y + wa.h - cur.h - m),
            (wa.x + m, wa.y + wa.h - cur.h - m),
            (wa.x + wa.w - cur.w - m, top),
            (wa.x + m, top),
        ];
        let best = corners
            .iter()
            .map(|&(x, y)| (overlap(capture::Rect { x, y, w: cur.w, h: cur.h }, d), x, y))
            .min_by_key(|c| c.0)
            .unwrap();
        if best.0 < overlap(cur, d) {
            unsafe {
                let _ = SetWindowPos(
                    h,
                    Some(HWND_TOPMOST),
                    best.1 - off_x,
                    best.2 - off_y,
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
                        if x.answer.is_empty() {
                            crate::island::buddy(crate::island::BuddyState::Streaming);
                        }
                        x.answer.push_str(&t);
                    }
                }
                Ev::Status(s) => {
                    // Guide mode: the user clicks into their app; don't treat that as dismissal.
                    if s == "watching…" {
                        self.was_focused = false;
                    }
                    self.status = s
                }
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
                    crate::island::buddy(crate::island::BuddyState::Error);
                    if let Some(x) = self.exchanges.last_mut() {
                        x.error = Some(friendly(&e));
                    }
                }
                Ev::Spawned(id) => {
                    if let Some(x) = self.exchanges.last_mut() {
                        x.agents.push(id);
                    }
                }
                Ev::Done(conv) => {
                    if self.exchanges.last().is_some_and(|x| x.error.is_none()) {
                        crate::island::buddy(crate::island::BuddyState::Done);
                    }
                    self.trim_at = 0.0; // the answer's working memory is now cold
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
                agents: Vec::new(),
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
        self.diagrams.clear();
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
        // Before anything can take focus away from the app it's reading.
        if let Some(chip) = self.selection.take().and_then(|s| s.wait()) {
            self.chips.insert(0, chip);
        }
        self.thumb = thumbnail(ctx, &self.jpeg);
        let opacity = crate::surface::glass_opacity(self.cfg.appearance.blur);
        self.renderer = frame
            .gl()
            .and_then(|gl| crate::surface::Renderer::new(gl, self.backdrop.clone(), opacity));
        let Ok(handle) = frame.window_handle() else {
            return;
        };
        let RawWindowHandle::Win32(h) = handle.as_raw() else {
            return;
        };
        let hwnd = HWND(h.hwnd.get() as *mut _);
        self.hwnd = Some(hwnd);
        plain_chrome(hwnd);
        unsafe {
            if win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let work = capture::monitor_at(self.cap.cursor.0, self.cap.cursor.1).work;
            let s = self.cap.monitor.scale;
            // (x, y) below place the pill; the window sits around it.
            let w = (self.width * s) as i32;
            let (x, y) = match self.cfg.appearance.position.as_str() {
                "cursor" => {
                    let gap = (16.0 * s) as i32;
                    let max_h = (MAX_H * s) as i32;
                    let (cx, cy) = self.cap.cursor;
                    let x = (cx - w / 2).clamp(work.x, (work.x + work.w - w).max(work.x));
                    let top = work.y + ((PILL_TOP - TOP) * s) as i32;
                    let y = if cy + gap + max_h <= work.y + work.h { cy + gap } else { (cy - gap - max_h).max(top) };
                    (x, y)
                }
                "center" => (
                    work.x + (work.w - w) / 2,
                    work.y + (work.h - (MAX_H * s) as i32).max(0) / 2,
                ),
                "last" if self.cfg.appearance.saved_pos.is_some_and(on_screen) => {
                    let [x, y] = self.cfg.appearance.saved_pos.unwrap();
                    let wa = capture::monitor_at(x, y).work;
                    let h = (PILL_H * s) as i32;
                    (
                        x.clamp(wa.x, (wa.x + wa.w - w).max(wa.x)),
                        y.clamp(wa.y, (wa.y + wa.h - h).max(wa.y)),
                    )
                }
                _ => (
                    work.x + (work.w - w) / 2,
                    work.y + (work.h as f32 * 0.2) as i32,
                ),
            };
            let (x, y) = (x - (SIDE * s) as i32, y - (PILL_TOP * s) as i32);
            let _ = SetWindowPos(hwnd, Some(HWND_TOPMOST), x, y, 0, 0, SWP_NOSIZE);
            let _ = SetForegroundWindow(hwnd);
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
    }

    fn close(&mut self, ctx: &egui::Context) {
        self.cancel.store(true, Ordering::Relaxed);
        self.rec = None; // discards the recording
        self.save_history();
        self.save_position();
        if self.quit {
            *self.outcome.lock().unwrap() = Outcome::Quit;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }

    /// After a drag, remembers the spot and opens there from now on.
    fn save_position(&mut self) {
        let (true, Some(h)) = (self.moved, self.hwnd) else {
            return;
        };
        let mut r = RECT::default();
        if unsafe { GetWindowRect(h, &mut r) }.is_err() {
            return;
        }
        // Re-read so a Settings window saved meanwhile is not clobbered.
        let (mut cfg, err) = config::load();
        if err.is_some() {
            return;
        }
        cfg.appearance.position = "last".into();
        // The pill's corner, not the window's, which has room around it.
        let s = self.ppp();
        cfg.appearance.saved_pos = Some([r.left + (SIDE * s) as i32, r.top + (PILL_TOP * s) as i32]);
        if let Err(e) = config::save(&cfg) {
            config::log(&format!("saving palette position failed: {e}"));
        }
    }

    /// Keeps the bottom of the surface on screen as it grows (e.g. after it was
    /// dragged near the bottom of the monitor).
    fn keep_on_screen(&self, height: f32) {
        let Some(h) = self.hwnd else {
            return;
        };
        let s = self.ppp();
        let (wx, wy) = self.window_pos();
        let top = wy + (PILL_TOP * s) as i32;
        let wa = capture::monitor_at(wx + (SIDE * s) as i32, top).work;
        let px = (height * s).round() as i32;
        let bottom = wa.y + wa.h;
        if top + px > bottom {
            let new_top = (bottom - px).max(wa.y);
            if new_top != top {
                unsafe {
                    let _ = SetWindowPos(
                        h,
                        Some(HWND_TOPMOST),
                        wx,
                        new_top - (PILL_TOP * s) as i32,
                        0,
                        0,
                        SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }
    }

    /// Collects finished mentions as chips; sends a question that was waiting on them.
    fn pump_mentions(&mut self, ctx: &egui::Context) {
        if let Some((start, url)) = mention::typed_url(&self.input) {
            self.input.truncate(start);
            self.attaching.push(mention::resolve_url(url));
        }
        let mut i = 0;
        while i < self.attaching.len() {
            match self.attaching[i].try_recv() {
                Ok(r) => {
                    match r {
                        Ok(chip) => self.chips.push(chip),
                        Err(e) => self.notice = Some(e),
                    }
                    self.attaching.remove(i);
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.attaching.remove(i);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => i += 1,
            }
        }
        if !self.attaching.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(50));
        } else if self.send_when_ready {
            self.send_when_ready = false;
            self.send(ctx);
        }
    }

    /// Opens/closes the `@` menu as the input changes and handles its keys.
    fn mention_keys(&mut self, ctx: &egui::Context) {
        let token = if self.browser.is_none() && self.rec.is_none() {
            mention::active_token(&self.input).map(|(s, q)| (s, q.to_string()))
        } else {
            None
        };
        let Some((start, q)) = token else {
            self.mention = None;
            return;
        };
        let menu = self
            .mention
            .get_or_insert_with(|| mention::Menu::open(&self.cfg, self.cap.monitor.rect));
        let n = menu.filtered(&q).len();
        let (up, down, pick, esc) = ctx.input_mut(|i| {
            (
                i.consume_key(egui::Modifiers::NONE, Key::ArrowUp),
                i.consume_key(egui::Modifiers::NONE, Key::ArrowDown),
                i.consume_key(egui::Modifiers::NONE, Key::Tab)
                    || (n > 0 && i.consume_key(egui::Modifiers::NONE, Key::Enter)),
                i.consume_key(egui::Modifiers::NONE, Key::Escape),
            )
        });
        if n > 0 {
            if up {
                menu.selected = (menu.selected + n - 1) % n;
            }
            if down {
                menu.selected = (menu.selected + 1) % n;
            }
            menu.selected = menu.selected.min(n - 1);
        }
        if esc {
            self.input.truncate(start);
            self.mention = None;
        } else if pick && n > 0 {
            let sel = menu.selected;
            self.pick_mention(start, &q, sel);
        }
    }

    fn pick_mention(&mut self, start: usize, q: &str, idx: usize) {
        let Some(item) = self.mention.as_ref().and_then(|m| m.filtered(q).get(idx).map(|i| (*i).clone())) else {
            return;
        };
        if item.disabled {
            self.notice = Some(item.desc);
            return;
        }
        self.input.truncate(start);
        self.mention = None;
        self.refocus = true;
        if let Some(rx) = mention::resolve(item.kind, self.hwnd) {
            self.attaching.push(rx);
        }
        // A file dialog took focus; don't treat that as clicking away.
        self.was_focused = false;
    }

    /// The `@` menu, in place of the conversation.
    fn mention_body(&mut self, ui: &mut egui::Ui) {
        let Some((start, q)) = mention::active_token(&self.input).map(|(s, q)| (s, q.to_string())) else {
            return;
        };
        let Some(menu) = self.mention.as_ref() else { return };
        let accent = self.accent;
        let selected = menu.selected;
        let mut clicked = None;
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(MAX_H - PILL_H - 8.0)
            .show(ui, |ui| {
                ui.add_space(6.0);
                let items = menu.filtered(&q);
                if items.is_empty() {
                    ui.label(RichText::new("No matches").color(TEXT_2));
                }
                for (i, it) in items.iter().enumerate() {
                    let fill = if i == selected { Color32::from_white_alpha(14) } else { Color32::TRANSPARENT };
                    let r = egui::Frame::new()
                        .fill(fill)
                        .corner_radius(10)
                        .inner_margin(Margin::symmetric(10, 6))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                let name = RichText::new(format!("@{}", it.name)).strong();
                                ui.label(if it.disabled { name.color(TEXT_2) } else { name.color(accent) });
                                ui.add_space(6.0);
                                ui.label(RichText::new(&it.desc).color(TEXT_2));
                            });
                        })
                        .response
                        .interact(egui::Sense::click());
                    if r.clicked() {
                        clicked = Some(i);
                    }
                }
                ui.add_space(8.0);
            });
        self.body_h = out.content_size.y;
        if let Some(i) = clicked {
            self.pick_mention(start, &q, i);
        }
    }

    fn handle_app_events(&mut self, ctx: &egui::Context) {
        if let Some(dir) = self.open_on_start.take() {
            self.open_chat(ctx, dir);
        }
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
                AppEvent::Hotkey(_)
                | AppEvent::TrayClick
                | AppEvent::IslandClick
                | AppEvent::AltDoubleTap => self.close(ctx),
                // Agent rows are the app's business once the palette is gone.
                ev @ AppEvent::Island(_) => requeue.push(ev),
                AppEvent::OpenChat(dir) => self.open_chat(ctx, dir),
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
        } else if self.was_focused && self.may_dismiss() {
            self.close(ctx);
        }
    }

    /// True when clicking away should close the palette right now.
    fn may_dismiss(&self) -> bool {
        self.cfg.appearance.dismiss_on_blur
            && !self.busy()
            && self.rec.is_none()
            && self.transcribing.is_none()
            && self.overlay.as_ref().is_none_or(|o| o.is_empty())
    }

    fn start_drag(&mut self, ctx: &egui::Context) {
        self.moved = true;
        ctx.send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }

    /// The chip at the left of the pill: what the question is about. Click to
    /// retake the screenshot. It says Agent while waiting on an approval, and
    /// shows a dot loader during an answer.
    fn screen_chip(&mut self, ui: &mut egui::Ui) {
        let busy = self.busy();
        let agent = self.pending.is_some();
        let label = if agent { "Agent" } else { "Screen" };
        let galley = ui.painter().layout_no_wrap(label.to_string(), FontId::proportional(13.0), TEXT);
        let pic = 24.0;
        let size = egui::vec2(7.0 + pic + 7.0 + galley.size().x + 12.0, CHIP_H);
        let idle = !busy && !agent;
        let sense = if idle { egui::Sense::click_and_drag() } else { egui::Sense::drag() };
        let (rect, resp) = ui.allocate_exact_size(size, sense);
        if resp.drag_started() {
            self.start_drag(ui.ctx());
        }
        let fill = if resp.hovered() && idle { 40 } else { 28 };
        ui.painter().rect_filled(rect, CHIP_H / 2.0, Color32::from_white_alpha(fill));
        let spot = egui::Rect::from_center_size(
            egui::pos2(rect.left() + 7.0 + pic / 2.0, rect.center().y),
            egui::vec2(pic, pic),
        );
        if busy {
            dot_loader(ui, spot.center(), TEXT, self.reduce);
        } else if agent {
            let c = spot.center();
            let p = [c + egui::vec2(-5.0, -6.0), c + egui::vec2(6.0, -1.0), c + egui::vec2(1.0, 1.0), c + egui::vec2(-1.0, 6.0)];
            ui.painter().add(egui::Shape::closed_line(p.to_vec(), egui::Stroke::new(1.6, TEXT)));
        } else if let Some(t) = &self.thumb {
            let tex = t.size_vec2();
            let side = tex.x.min(tex.y);
            let uv = egui::Rect::from_center_size(
                egui::pos2(0.5, 0.5),
                egui::vec2(side / tex.x, side / tex.y),
            );
            egui::Image::new((t.id(), spot.size()))
                .uv(uv)
                .corner_radius(pic / 2.0)
                .paint_at(ui, spot);
        }
        let text_pos = egui::pos2(spot.right() + 7.0, rect.center().y - galley.size().y / 2.0);
        ui.painter().galley(text_pos, galley, TEXT);
        if idle {
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            let clicked = resp.clicked();
            resp.on_hover_text("Screenshot attached · click to retake · drag to move");
            if clicked {
                self.recapture(ui.ctx());
            }
        }
    }

    /// Mic levels as a row of dots that swell with your voice, newest on the
    /// right, in place of the text field while recording.
    fn waveform(&mut self, ui: &mut egui::Ui) {
        let now = ui.input(|i| i.time);
        if let Some(r) = &self.rec {
            if now - self.wave_t >= 0.05 {
                self.wave_t = now;
                self.wave.push(r.level());
                if self.wave.len() > 256 {
                    self.wave.remove(0);
                }
            }
        }
        let size = egui::vec2(ui.available_width(), CHIP_H);
        let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
        let p = ui.painter();
        let y = rect.center().y;
        let step = 5.0;
        let n = (rect.width() / step) as usize;
        for i in 0..n {
            let x = rect.right() - 2.0 - i as f32 * step;
            let level = self.wave.iter().rev().nth(i).copied();
            match level {
                Some(l) if l > 0.02 => {
                    let h = 3.0 + l.clamp(0.0, 1.0) * (CHIP_H - 14.0);
                    let bar = egui::Rect::from_center_size(egui::pos2(x, y), egui::vec2(2.5, h));
                    p.rect_filled(bar, 1.25, TEXT);
                }
                Some(_) => {
                    p.circle_filled(egui::pos2(x, y), 1.25, TEXT_2);
                }
                None => {
                    p.circle_filled(egui::pos2(x, y), 1.0, TEXT_2.gamma_multiply(0.5));
                }
            }
        }
    }

    /// The text field, or the waveform while recording.
    fn input_field(&mut self, ui: &mut egui::Ui, enter: bool) {
        if self.rec.is_some() {
            self.refocus = true; // back to the text when recording ends
            self.waveform(ui);
            return;
        }
        self.wave.clear();
        let hint = if self.transcribing.is_some() {
            "Transcribing…"
        } else if self.browser.is_some() {
            "Search history"
        } else if self.exchanges.is_empty() {
            "Ask about your screen"
        } else {
            "Ask a follow-up"
        };
        let edit = egui::TextEdit::singleline(&mut self.input)
            .id_salt("composer")
            .hint_text(RichText::new(hint).color(TEXT_2).text_style(TextStyle::Heading))
            .font(TextStyle::Heading)
            .frame(egui::Frame::NONE)
            .align(egui::Align2::LEFT_CENTER)
            .desired_width(f32::INFINITY);
        let resp = ui.add_sized([ui.available_width(), ICON_H], edit);
        if std::mem::take(&mut self.cursor_end) {
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), resp.id) {
                let end = egui::text::CCursor::new(self.input.chars().count());
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                st.store(ui.ctx(), resp.id);
            }
        }
        if self.refocus {
            self.refocus = false;
            resp.request_focus();
        }
        if !resp.has_focus() && self.frames < 8 {
            resp.request_focus();
        }
        if enter && self.browser.is_none() {
            self.send(ui.ctx());
            resp.request_focus();
        }
    }

    /// Attach, history and settings, shown only while the pointer is over the
    /// palette so the bar stays quiet. Added right to left.
    fn quiet_tools(&mut self, ui: &mut egui::Ui) {
        let browsing = self.browser.is_some();
        let over = ui.input(|i| i.pointer.hover_pos().is_some());
        let shown = ui.ctx().animate_bool(ui.scope_id().with("tools"), over || browsing);
        ui.scope(|ui| {
            ui.set_opacity(shown);
            let gear = icon_button(ui, Icon::Gear, false, self.accent).on_hover_text("Settings");
            if gear.clicked() {
                self.open_settings(ui.ctx());
            }
            let hist = icon_button(ui, Icon::History, browsing, self.accent)
                .on_hover_text("History (Ctrl+H)");
            if hist.clicked() {
                self.toggle_browser();
            }
            if browsing || self.rec.is_some() {
                return;
            }
            let add = icon_button(ui, Icon::Plus, false, self.accent)
                .on_hover_text("Attach a file, page, or the clipboard (@)");
            if add.clicked() {
                if !self.input.is_empty() && !self.input.ends_with(char::is_whitespace) {
                    self.input.push(' ');
                }
                self.input.push('@');
                self.refocus = true;
                self.cursor_end = true;
            }
        });
    }

    /// White send arrow; a stop square during an answer or a recording.
    fn send_button(&mut self, ui: &mut egui::Ui) {
        let recording = self.rec.is_some();
        let stop = self.busy() || recording;
        let enabled = stop
            || (self.browser.is_none()
                && self.transcribing.is_none()
                && !self.input.trim().is_empty());
        let sense = if enabled { egui::Sense::click() } else { egui::Sense::hover() };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(CHIP_H, CHIP_H), sense);
        let c = rect.center();
        let p = ui.painter();
        let (fill, fg) = if enabled {
            let fill = if resp.hovered() { Color32::from_gray(214) } else { Color32::WHITE };
            (fill, SURFACE)
        } else {
            (Color32::from_white_alpha(28), TEXT_2)
        };
        p.circle_filled(c, CHIP_H / 2.0, fill);
        if stop {
            p.rect_filled(egui::Rect::from_center_size(c, egui::vec2(10.0, 10.0)), 2, fg);
        } else {
            let st = egui::Stroke::new(1.9, fg);
            let top = c + egui::vec2(0.0, -5.5);
            p.line_segment([top, c + egui::vec2(0.0, 5.5)], st);
            p.line_segment([top, top + egui::vec2(-4.5, 4.5)], st);
            p.line_segment([top, top + egui::vec2(4.5, 4.5)], st);
        }
        if enabled && resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        let clicked = resp.clicked();
        resp.on_hover_text(if recording {
            "Stop and transcribe (Enter)"
        } else if stop {
            "Stop (Esc)"
        } else {
            "Send (Enter)"
        });
        if clicked {
            if recording {
                self.stop_rec();
            } else if stop {
                self.stop_turn();
            } else {
                self.send(ui.ctx());
                self.refocus = true;
            }
        }
    }

    /// The pill: chip, text, tools, send.
    fn bar(&mut self, ui: &mut egui::Ui, enter: bool) {
        ui.horizontal_centered(|ui| {
            self.screen_chip(ui);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                self.send_button(ui);
                if self.cfg.voice.enabled {
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
                self.quiet_tools(ui);
                ui.add_space(4.0);
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    self.input_field(ui, enter);
                });
            });
        });
    }

    /// Matches the tabs to the chips: new chips rise, removed chips start to sink.
    fn reconcile_tabs(&mut self, now: f64, pill_left: f32) {
        let mut want: Vec<(String, Option<usize>)> =
            self.chips.iter().enumerate().map(|(i, c)| (c.label.clone(), Some(i))).collect();
        if !self.attaching.is_empty() {
            want.push(("Attaching…".to_string(), None));
        }
        let mut fresh = 0;
        let mut open: Vec<usize> = (0..self.tabs.len()).filter(|&i| self.tabs[i].gone.is_none()).collect();
        for (key, chip) in &want {
            if let Some(at) = open.iter().position(|&i| &self.tabs[i].key == key) {
                self.tabs[open.remove(at)].chip = *chip;
            } else {
                // Tabs already there when the hotkey opens the palette just appear:
                // this runs on nearly every use, so it shouldn't make you wait.
                let born = if self.frames <= 1 { f64::MIN } else { now + crate::surface::stagger(fresh) as f64 };
                fresh += 1;
                self.tabs.push(Tab {
                    key: key.clone(),
                    born,
                    gone: None,
                    x: crate::surface::Smooth::new(pill_left + TAB_INDENT),
                    width: 0.0,
                    chip: *chip,
                });
            }
        }
        for i in open {
            self.tabs[i].gone = Some(now);
            self.tabs[i].chip = None;
        }
        self.tabs.retain(|t| t.gone.is_none_or(|g| now - g < TAB_SINK as f64));
    }

    /// How far a tab has risen out of the pill, 0..=1, and its label's opacity.
    fn tab_state(&self, t: &Tab, now: f64) -> (f32, f32) {
        use crate::surface::{EASE_OUT, ease};
        if self.reduce {
            let fade = match t.gone {
                Some(g) => 1.0 - ((now - g) as f32 / TAB_SINK).clamp(0.0, 1.0),
                None if t.born == f64::MIN => 1.0,
                None => ((now - t.born) as f32 / REDUCED_FADE).clamp(0.0, 1.0),
            };
            return (if t.gone.is_some() || fade > 0.0 { 1.0 } else { 0.0 }, fade);
        }
        if let Some(g) = t.gone {
            let out = 1.0 - ease(EASE_OUT, now, g, TAB_SINK);
            return (out, out);
        }
        if t.born == f64::MIN {
            return (1.0, 1.0);
        }
        let text = ((now - t.born) as f32 - TAB_TEXT_DELAY) / TAB_TEXT;
        (ease(EASE_OUT, now, t.born, TAB_RISE), text.clamp(0.0, 1.0))
    }

    /// The context tabs, rising out of the top of the pill. Adds their shapes to
    /// `blobs`; true while any are moving.
    fn tabs_ui(&mut self, ui: &mut egui::Ui, pill: egui::Rect, now: f64, dt: f32, blobs: &mut Vec<crate::surface::Blob>) -> bool {
        self.reconcile_tabs(now, pill.left());
        if self.tabs.is_empty() {
            return false;
        }
        let font = FontId::proportional(12.0);
        let room = pill.width() - TAB_INDENT * 2.0;
        let label_of = |key: &str| -> String {
            if key.chars().count() > 28 {
                format!("{}…", key.chars().take(27).collect::<String>())
            } else {
                key.to_string()
            }
        };

        // Which tabs fit; anything past them is summed up as "+N".
        let mut x = pill.left() + TAB_INDENT;
        let mut shown = 0;
        let live = self.tabs.iter().filter(|t| t.gone.is_none()).count();
        for t in self.tabs.iter_mut().filter(|t| t.gone.is_none()) {
            let g = ui.painter().layout_no_wrap(label_of(&t.key), font.clone(), TEXT);
            t.width = 11.0 + g.size().x + 27.0;
            if shown >= MAX_TABS || (x + t.width > pill.left() + TAB_INDENT + room && shown > 0) {
                break;
            }
            if t.x.target != x {
                t.x.target = x;
            }
            if self.reduce {
                t.x.snap(x);
            }
            x += t.width + TAB_LIP;
            shown += 1;
        }
        let hidden = live - shown;

        let mut moving = false;
        let mut remove = None;
        let tabs = std::mem::take(&mut self.tabs);
        let mut sinking = false;
        let mut shown_seen = 0;
        let mut more: Vec<String> = Vec::new();
        let mut new_tabs = Vec::with_capacity(tabs.len());
        for mut t in tabs {
            let live_index = if t.gone.is_none() {
                shown_seen += 1;
                Some(shown_seen - 1)
            } else {
                None
            };
            if live_index.is_some_and(|i| i >= shown) {
                more.push(t.key.clone());
                new_tabs.push(t);
                continue;
            }
            t.x.step(dt, 0.06);
            let (rise, text) = self.tab_state(&t, now);
            moving |= !t.x.settled() || rise < 1.0 || text < 1.0 || t.gone.is_some() || now < t.born;
            let top = pill.top() - TAB_LIP - TAB_H;
            let shift = (1.0 - rise) * (TAB_H + TAB_LIP);
            let chip = egui::Rect::from_min_size(egui::pos2(t.x.value, top + shift), egui::vec2(t.width, TAB_H));
            if t.gone.is_none() || !std::mem::replace(&mut sinking, true) {
                blobs.push(crate::surface::Blob {
                    rect: egui::Rect::from_min_max(
                        chip.min - egui::vec2(TAB_LIP, TAB_LIP),
                        egui::pos2(chip.right() + TAB_LIP, pill.top() + PILL_RADIUS),
                    ),
                    radius: 19.0,
                    merge: 20.0,
                });
            }
            if text > 0.0 {
                let chip_index = t.chip;
                ui.scope(|ui| {
                    ui.set_opacity(text);
                    ui.painter().rect_filled(chip, TAB_H / 2.0, Color32::from_white_alpha(20));
                    let g = ui.painter().layout_no_wrap(label_of(&t.key), font.clone(), TEXT);
                    ui.painter().galley(
                        egui::pos2(chip.left() + 11.0, chip.center().y - g.size().y / 2.0),
                        g,
                        TEXT,
                    );
                    let Some(i) = chip_index.filter(|&i| i < self.chips.len()) else { return };
                    let hover = self.chips[i].hover.clone();
                    let close = egui::Rect::from_center_size(
                        egui::pos2(chip.right() - 15.0, chip.center().y),
                        egui::vec2(TAB_H, TAB_H),
                    );
                    let id = ui.scope_id().with(("tab", &t.key, i));
                    let area = ui.interact(chip, id.with("body"), egui::Sense::hover());
                    area.on_hover_text(hover);
                    let r = ui.interact(close, id.with("close"), egui::Sense::click()).on_hover_text("Remove");
                    if r.hovered() {
                        ui.painter().circle_filled(close.center(), 10.0, Color32::from_white_alpha(20));
                        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                    }
                    let c = close.center();
                    let st = egui::Stroke::new(1.6, TEXT_2);
                    ui.painter().line_segment([c + egui::vec2(-3.0, -3.0), c + egui::vec2(3.0, 3.0)], st);
                    ui.painter().line_segment([c + egui::vec2(3.0, -3.0), c + egui::vec2(-3.0, 3.0)], st);
                    if r.clicked() {
                        remove = Some(i);
                    }
                });
            }
            new_tabs.push(t);
        }
        self.tabs = new_tabs;

        if hidden > 0 {
            let names = more;
            let g = ui.painter().layout_no_wrap(format!("+{hidden}"), font.clone(), TEXT);
            let w = g.size().x + 22.0;
            let top = pill.top() - TAB_LIP - TAB_H;
            let chip = egui::Rect::from_min_size(egui::pos2(x, top), egui::vec2(w, TAB_H));
            blobs.push(crate::surface::Blob {
                rect: egui::Rect::from_min_max(
                    chip.min - egui::vec2(TAB_LIP, TAB_LIP),
                    egui::pos2(chip.right() + TAB_LIP, pill.top() + PILL_RADIUS),
                ),
                radius: 19.0,
                merge: 20.0,
            });
            ui.painter().rect_filled(chip, TAB_H / 2.0, Color32::from_white_alpha(20));
            ui.painter().galley(
                egui::pos2(chip.left() + 11.0, chip.center().y - g.size().y / 2.0),
                g,
                TEXT,
            );
            ui.interact(chip, ui.scope_id().with("tab-more"), egui::Sense::hover())
                .on_hover_text(names.join("\n"));
        }
        if let Some(i) = remove {
            self.chips.remove(i);
            self.refocus = true;
        }
        moving
    }

    /// The history list, in place of the conversation.
    fn history_body(&mut self, ui: &mut egui::Ui, keys: (bool, bool, bool)) {
        let accent = self.accent;
        let query = self.input.clone();
        let max_h = MAX_H - PILL_H - 8.0;
        let Some(b) = self.browser.as_mut() else { return };
        let out = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .max_height(max_h)
            .show(ui, |ui| {
                ui.add_space(8.0);
                let a = b.show(ui, &query, keys, accent, TEXT_2, TEXT_2);
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
        let mut retry = false;
        let mut decision: Option<(bool, bool)> = None;
        let mut pin: Option<String> = None;
        let accent = self.accent;
        let once = self.cfg.agent_desktop.approval == "once";
        let out = egui::ScrollArea::vertical()
            .stick_to_bottom(true)
            .auto_shrink([false, true])
            .max_height(MAX_H - PILL_H - 8.0)
            .show(ui, |ui| {
                ui.add_space(10.0);
                for (i, x) in self.exchanges.iter().enumerate() {
                    let last = i + 1 == n;
                    // Earlier turns and the current question read as quiet context.
                    if n > 1 || !x.answer.is_empty() || x.error.is_some() || busy {
                        ui.label(RichText::new(&x.question).color(TEXT_2).size(13.5));
                        ui.add_space(4.0);
                    }
                    if !x.answer.is_empty() {
                        let math = self.math.clone();
                        let render = move |ui: &mut egui::Ui, tex: &str, inline: bool| {
                            crate::math::show(ui, &math, tex, inline)
                        };
                        let theme = crate::diagram::Theme { bg: SURFACE, fg: TEXT, muted: TEXT_2, accent };
                        for seg in crate::diagram::split(&x.answer) {
                            match seg {
                                crate::diagram::Seg::Md(md) => {
                                    egui_commonmark::CommonMarkViewer::new()
                                        .render_math_fn(Some(&render))
                                        .show(ui, &mut self.md, &crate::math::normalize(md));
                                }
                                crate::diagram::Seg::Mermaid(src) => {
                                    if crate::diagram::show(ui, &mut self.diagrams, &mut self.md, src, theme) {
                                        pin = Some(src.to_string());
                                    }
                                }
                            }
                        }
                    }
                    for id in &x.agents {
                        agent_line(ui, id, &mut self.agent_drafts, accent);
                    }
                    let watching = self.status == "watching…";
                    if last && busy && (x.answer.is_empty() || watching) && self.pending.is_none() {
                        // Soft pulsing status instead of a spinner.
                        let a = 0.45 + 0.35 * ((time * 3.2).sin() as f32 * 0.5 + 0.5);
                        let (color, text) = if watching {
                            (accent.gamma_multiply(a + 0.2), format!("◉ {}", status_text(&self.status)))
                        } else {
                            (TEXT_2.gamma_multiply(a), status_text(&self.status).to_string())
                        };
                        ui.label(RichText::new(text).color(color));
                    }
                    if let Some(e) = &x.error {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(&e.text).color(ERROR));
                            if e.settings && ui.link("Open settings").clicked() {
                                open_settings = true;
                            }
                            if last && !busy && ui.link("Try again").clicked() {
                                retry = true;
                            }
                        });
                        if e.text != "Stopped." {
                            ui.label(RichText::new(&e.detail).color(TEXT_2).small());
                        }
                    }
                    if !last {
                        ui.add_space(14.0);
                    }
                }
                if let Some((intent, detail, _)) = &self.pending {
                    ui.add_space(6.0);
                    // The surface carries the shape change; only the text fades.
                    let shown = ui.ctx().animate_bool_with_time(ui.scope_id().with("approval"), true, 0.14);
                    egui::Frame::new()
                        .show(ui, |ui| {
                            ui.set_opacity(shown);
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new(format!(
                                    "Allow screenbuddy to {}?",
                                    lowercase_first(intent)
                                ))
                                .size(15.0),
                            );
                            ui.label(RichText::new(detail).monospace().color(TEXT_2).size(12.5));
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .link(RichText::new("Always allow").size(13.0))
                                    .on_hover_text("Never ask again. Change this in Settings → Agent.")
                                    .clicked()
                                {
                                    decision = Some((true, true));
                                }
                                if once {
                                    ui.label(RichText::new("Allowing covers the rest of this conversation").color(TEXT_2).size(12.5));
                                }
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        let allow = egui::Button::new(
                                            RichText::new("Allow").color(SURFACE).size(13.0),
                                        )
                                        .fill(Color32::WHITE)
                                        .corner_radius(CHIP_H / 2.0)
                                        .min_size(egui::vec2(76.0, ICON_H + 2.0));
                                        if ui.add(allow).clicked() {
                                            decision = Some((true, false));
                                        }
                                        let deny = egui::Button::new(RichText::new("Don't Allow").size(13.0))
                                            .fill(Color32::TRANSPARENT)
                                            .stroke(egui::Stroke::new(1.0, Color32::from_white_alpha(46)))
                                            .corner_radius(CHIP_H / 2.0)
                                            .min_size(egui::vec2(94.0, ICON_H + 2.0));
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
                    ui.label(RichText::new(n).color(TEXT_2).small());
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
        if retry {
            self.retry(ui.ctx());
        }
        if let Some(src) = pin {
            // Top-right of the captured area; the overlay clamps the card on screen.
            let (w, h) = (self.cap.img_w as f32, self.cap.img_h as f32);
            let shape = serde_json::json!({"kind": "diagram", "mermaid": src, "x": w, "y": h * 0.06});
            if let Err(e) = self.draw(serde_json::json!({"shapes": [shape]})) {
                self.notice = Some(format!("Couldn't show the diagram on screen ({e})."));
            }
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
        !self.exchanges.is_empty() || self.notice.is_some() || self.browser.is_some() || self.mention.is_some()
    }

    /// Moves the surface toward its measured height. True while it's moving.
    fn pour_surface(&mut self, dt: f32) -> bool {
        let target = if self.has_body() {
            (PILL_H + 4.0 + self.body_h).min(MAX_H)
        } else {
            PILL_H
        };
        if target != self.pour.target {
            if target > self.pour.target {
                self.keep_on_screen(target);
            }
            self.pour.target = target;
            self.lead.target = target;
        }
        if self.reduce {
            self.pour.snap(target);
            self.lead.snap(target);
            return false;
        }
        let time = crate::surface::smooth_time_for(target - self.pour.value);
        self.pour.step(dt, time);
        // The center column leads when pouring out; on the way back up it just
        // follows the surface instead of leaving a gap.
        self.lead.step(dt, time * 0.6);
        !self.pour.settled() || !self.lead.settled()
    }

    /// Everything that makes up the surface this frame, in window points.
    fn main_blobs(&self, pill: egui::Rect) -> Vec<crate::surface::Blob> {
        let mut blobs = vec![crate::surface::Blob {
            rect: egui::Rect::from_min_size(pill.min, egui::vec2(pill.width(), self.pour.value)),
            radius: PILL_RADIUS,
            merge: 0.0,
        }];
        let ahead = self.lead.value - self.pour.value;
        if ahead > 0.5 {
            let inset = pill.width() * 0.15;
            blobs.push(crate::surface::Blob {
                rect: egui::Rect::from_min_size(
                    pill.min + egui::vec2(inset, 0.0),
                    egui::vec2(pill.width() - inset * 2.0, self.lead.value),
                ),
                radius: PILL_RADIUS,
                // Fades out as the surface catches up, so nothing pops or bulges.
                merge: 30.0 * (ahead / 20.0).min(1.0),
            });
        }
        blobs
    }

    /// The buds breaking away toward what was just drawn.
    fn bud_blobs(&mut self, now: f64, blobs: &mut Vec<crate::surface::Blob>) {
        self.buds.retain(|b| now - b.start < overlay::BUD_TIME as f64);
        for b in &self.buds {
            if blobs.len() >= crate::surface::MAX_BLOBS {
                break;
            }
            let t = crate::surface::ease(crate::surface::EASE_IN_OUT, now, b.start, overlay::BUD_TIME);
            if now >= b.start {
                let c = b.from + (b.to - b.from) * t;
                blobs.push(crate::surface::Blob {
                    rect: egui::Rect::from_center_size(c, egui::vec2(20.0, 20.0)),
                    radius: 10.0,
                    merge: 26.0,
                });
            }
        }
    }

    /// Clips the window to the part the surface uses, so everything else stays
    /// click-through. It grows right away and only shrinks once settled, so this
    /// isn't redone on every frame of an animation.
    fn update_region(&mut self, settled: bool, hovering: bool) {
        let Some(hwnd) = self.hwnd else { return };
        let s = self.ppp();
        let tabs_up = !self.tabs.is_empty() || !self.buds.is_empty();
        let top = if tabs_up { 0.0 } else { PILL_TOP - TOP };
        // Tooltips can be several lines long, so give them the whole window while
        // the pointer is over the surface. Moving off it closes the region again.
        let h = self.pour.value.max(self.pour.target).max(self.lead.value);
        let bottom = if hovering { WINDOW_H } else { (PILL_TOP + h + BOTTOM).min(WINDOW_H) };
        let want = ((top * s) as i32, (bottom * s).ceil() as i32);
        let grows = self.region.is_none_or(|r| want.0 < r.0 || want.1 > r.1);
        if self.region != Some(want) && (grows || settled) {
            let w = ((self.width + SIDE * 2.0) * s).ceil() as i32 + 2;
            unsafe {
                use windows::Win32::Graphics::Gdi::{CreateRectRgn, SetWindowRgn};
                let rgn = CreateRectRgn(0, want.0, w, want.1);
                SetWindowRgn(hwnd, Some(rgn), true);
            }
            self.region = Some(want);
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
        self.pump_mentions(&ctx);
        self.mention_keys(&ctx);
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
                self.stop_turn();
            } else {
                self.close(&ctx);
                return;
            }
        }
        let enter = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter));

        let full = ui.max_rect();
        self.scale = ctx.pixels_per_point();
        let now = ctx.input(|i| i.time);
        let dt = ((now - self.last_time) as f32).clamp(0.0, 0.05);
        self.last_time = now;
        let pill = egui::Rect::from_min_size(
            full.min + egui::vec2(SIDE, PILL_TOP),
            egui::vec2(self.width, PILL_H),
        );
        let mut moving = self.pour_surface(dt);
        let surface = egui::Rect::from_min_size(pill.min, egui::vec2(pill.width(), self.pour.value));

        // Paint the surface first; its shapes are finished further down, through
        // a shared slot, once the tabs have said where they are this frame.
        let bg = ui.painter().add(egui::Shape::Noop);

        // Dragging any empty part of the surface moves it; widgets on top win.
        let drag = ui.interact(surface, ui.scope_id().with("drag-bg"), egui::Sense::drag());
        if drag.drag_started() {
            self.start_drag(&ctx);
        }
        let mut blobs = self.main_blobs(pill);
        moving |= self.tabs_ui(ui, pill, now, dt, &mut blobs);
        // The margin is part of the window, but isn't part of the palette.
        let clicked_away = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer.interact_pos().is_some_and(|p| !blobs.iter().any(|b| b.rect.contains(p)))
        });
        if clicked_away && self.may_dismiss() {
            self.close(&ctx);
            return;
        }
        self.bud_blobs(now, &mut blobs);
        blobs.truncate(crate::surface::MAX_BLOBS);
        moving |= !self.buds.is_empty();
        if self.overlay.as_mut().is_some_and(|o| o.tick()) {
            moving = true;
        }

        ui.scope_builder(
            egui::UiBuilder::new().max_rect(pill.shrink2(egui::vec2((PILL_H - CHIP_H) / 2.0, (PILL_H - CHIP_H) / 2.0))),
            |ui| self.bar(ui, enter),
        );

        let has_body = self.has_body();
        let shown = ctx.animate_bool_with_time(ui.scope_id().with("body-fade"), has_body, 0.12);
        if has_body || shown > 0.0 {
            let room = egui::Rect::from_min_max(
                egui::pos2(pill.left() + PAD, pill.bottom()),
                egui::pos2(pill.right() - PAD, pill.top() + MAX_H),
            );
            ui.scope_builder(egui::UiBuilder::new().max_rect(room), |ui| {
                ui.set_clip_rect(surface);
                ui.set_opacity(shown);
                if self.browser.is_some() {
                    self.history_body(ui, keys);
                } else if self.mention.is_some() {
                    self.mention_body(ui);
                } else if has_body {
                    self.body(ui);
                }
            });
            // Text comes up from under the moving edge instead of being cut off.
            let edge = (self.pour.target - self.pour.value).abs().min(16.0) / 16.0;
            if edge > 0.0 {
                let (y1, y0) = (surface.bottom(), (surface.bottom() - 18.0).max(pill.bottom()));
                let mut mesh = egui::Mesh::default();
                let solid = SURFACE.gamma_multiply(edge);
                mesh.colored_vertex(egui::pos2(surface.left(), y0), Color32::TRANSPARENT);
                mesh.colored_vertex(egui::pos2(surface.right(), y0), Color32::TRANSPARENT);
                mesh.colored_vertex(egui::pos2(surface.right(), y1), solid);
                mesh.colored_vertex(egui::pos2(surface.left(), y1), solid);
                mesh.add_triangle(0, 1, 2);
                mesh.add_triangle(0, 2, 3);
                ui.painter().with_clip_rect(surface.shrink(1.0)).add(mesh);
            }
        }

        let hovering = ctx.input(|i| i.pointer.hover_pos())
            .is_some_and(|p| blobs.iter().any(|b| b.rect.contains(p)));
        // Now the shapes are known, fill the slot under the widgets.
        let (wx, wy) = self.window_pos();
        let scene = crate::surface::Scene { blobs, window_px: (wx as f32, wy as f32) };
        match &self.renderer {
            Some(r) => {
                let painter = ui.painter().clone();
                let cb = r.callback(full, scene);
                painter.set(bg, cb);
            }
            None => {
                let shapes: Vec<egui::Shape> = crate::surface::fallback_shapes(&scene);
                ui.painter().set(bg, egui::Shape::Vec(shapes));
            }
        }

        self.update_region(!moving, hovering);
        if moving {
            ctx.request_repaint();
        }
        self.trim_when_idle(&ctx);
        if self.busy() {
            // Guide mode can wait for minutes: pulse slowly instead of at 30 fps.
            let ms = if self.status == "watching…" { 250 } else { 33 };
            ctx.request_repaint_after(std::time::Duration::from_millis(ms));
        }
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if self.rx.is_some() {
            crate::island::buddy(crate::island::BuddyState::Idle); // closed mid-answer
        }
    }
}

/// True when the point lies on some monitor (a saved spot can outlive a
/// disconnected display).
fn on_screen([x, y]: [i32; 2]) -> bool {
    let r = capture::monitor_at(x, y).rect;
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}

/// One spawned agent inside the conversation: its live status, any question
/// or approval it is waiting on, and its result. Cards replace this later.
fn agent_line(ui: &mut egui::Ui, id: &str, drafts: &mut std::collections::HashMap<String, String>, accent: Color32) {
    use crate::agents::{Answer, AskKind, Status};
    let Some(a) = crate::agents::get(id) else { return };
    if a.status.is_active() {
        // No event wakes egui from the agent threads; poll gently while it works.
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
    }
    ui.add_space(6.0);
    let name = format!("\u{2039}{}\u{203a}", a.name);
    ui.horizontal_wrapped(|ui| {
        let (dot, text) = match a.status {
            Status::Pending => (TEXT_2, format!("Spawned agent {name} \u{2014} starting\u{2026}")),
            Status::Running => {
                let step = a.steps.last().map(|s| format!(" \u{b7} {s}")).unwrap_or_default();
                (accent, format!("Spawned agent {name} \u{2014} working\u{2026}{step}"))
            }
            Status::Done => (Color32::from_rgb(61, 220, 132), format!("Agent {name} is done")),
            Status::Failed => (ERROR, format!("Agent {name} failed: {}", a.error.as_deref().unwrap_or("something went wrong"))),
            Status::Stopped => (TEXT_2, format!("Agent {name} stopped{}", if a.counted { "" } else { " (didn't count)" })),
        };
        ui.label(RichText::new("\u{25cf}").color(dot).size(11.0));
        ui.label(RichText::new(text).color(TEXT_2).size(13.5));
        match a.status {
            Status::Pending => {
                if ui.link(RichText::new("Cancel").size(13.0)).clicked() {
                    crate::agents::cancel(id);
                }
            }
            Status::Running => {
                if ui.link(RichText::new("Stop").size(13.0)).clicked() {
                    crate::agents::stop(id);
                }
            }
            Status::Failed | Status::Stopped => {
                if ui.link(RichText::new("Retry").size(13.0)).clicked() {
                    crate::agents::retry(id);
                }
            }
            Status::Done => {}
        }
    });
    if let Some(q) = &a.ask {
        match q.kind {
            AskKind::Approval => {
                ui.label(RichText::new(format!("Allow it to {}?", lowercase_first(&q.text))).size(14.0));
                ui.label(RichText::new(&q.detail).monospace().color(TEXT_2).size(12.0));
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        crate::agents::answer(id, q.id, Answer::Allow);
                    }
                    if ui.button("Don't Allow").clicked() {
                        crate::agents::answer(id, q.id, Answer::Deny);
                    }
                });
            }
            AskKind::Questions => {
                for line in &q.questions {
                    ui.label(RichText::new(line).size(14.0));
                }
                let draft = drafts.entry(id.to_string()).or_default();
                let mut send = false;
                ui.horizontal(|ui| {
                    let r = ui.add(egui::TextEdit::singleline(draft).hint_text("Answer\u{2026}").desired_width(ui.available_width() - 70.0));
                    send = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                    send |= ui.button("Send").clicked();
                });
                if send && !draft.trim().is_empty() {
                    crate::agents::answer(id, q.id, Answer::Text(std::mem::take(draft)));
                }
            }
        }
    }
    if a.status == Status::Done && !a.result.is_empty() {
        ui.label(RichText::new(&a.result).color(TEXT).size(14.0));
    }
    if !a.status.is_active() && !a.files.is_empty() {
        ui.horizontal_wrapped(|ui| {
            for f in &a.files {
                let label = f.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                if ui.link(RichText::new(label).size(13.0)).on_hover_text(f.display().to_string()).clicked() {
                    win::shell_open(f);
                }
            }
        });
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
    Plus,
    History,
    Gear,
    /// Microphone; `Some(level)` while recording.
    Mic(Option<f32>),
}

/// Four dots that take turns swelling: the composer's "working" mark. Under
/// reduced motion they only change opacity.
fn dot_loader(ui: &egui::Ui, c: egui::Pos2, color: Color32, reduce: bool) {
    let t = ui.input(|i| i.time) as f32;
    let corners = [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)];
    for (i, (dx, dy)) in corners.into_iter().enumerate() {
        let phase = (t * 5.0 - i as f32 * std::f32::consts::FRAC_PI_2).sin() * 0.5 + 0.5;
        let pos = c + egui::vec2(dx, dy) * 4.5;
        let size = if reduce { 1.8 } else { 1.0 + 1.3 * phase };
        ui.painter().circle_filled(pos, size, color.gamma_multiply(0.45 + 0.55 * phase));
    }
}

/// A small painted icon button (no font glyph dependency).
fn icon_button(ui: &mut egui::Ui, icon: Icon, active: bool, accent: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ICON_H, ICON_H), egui::Sense::click());
    let c = rect.center();
    let p = ui.painter();
    let fg = if active { Color32::WHITE } else if resp.hovered() { TEXT } else { TEXT_2 };
    if active {
        let bg = match icon {
            Icon::Mic(_) => Color32::from_rgb(220, 60, 60),
            _ => accent,
        };
        p.circle_filled(c, ICON_H / 2.0, bg);
    } else if resp.hovered() {
        p.circle_filled(c, ICON_H / 2.0, Color32::from_white_alpha(16));
    }
    let st = egui::Stroke::new(1.5, fg);
    match icon {
        Icon::History => {
            p.circle_stroke(c, 7.0, st);
            p.line_segment([c, c + egui::vec2(0.0, -4.5)], st);
            p.line_segment([c, c + egui::vec2(3.5, 1.5)], st);
        }
        Icon::Plus => {
            p.line_segment([c + egui::vec2(-6.0, 0.0), c + egui::vec2(6.0, 0.0)], st);
            p.line_segment([c + egui::vec2(0.0, -6.0), c + egui::vec2(0.0, 6.0)], st);
        }
        Icon::Gear => {
            p.circle_stroke(c, 3.6, st);
            for i in 0..8 {
                let a = std::f32::consts::FRAC_PI_4 * i as f32;
                let d = egui::vec2(a.cos(), a.sin());
                p.line_segment([c + d * 5.2, c + d * 7.4], st);
            }
        }
        Icon::Mic(level) => {
            if let Some(l) = level {
                p.circle_stroke(c, ICON_H / 2.0 + 3.0 * l, egui::Stroke::new(1.5, Color32::from_rgb(220, 60, 60).gamma_multiply(0.6)));
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
