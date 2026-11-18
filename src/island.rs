//! The island: a small pill at the top centre of the monitor under the cursor,
//! carrying the buddy's face. Hover peeks into a panel of running / recent
//! agents; clicking the pill opens the palette.
//!
//! Runs on its own thread with two layered windows (`screenbuddy.island` for
//! the body, click-through `screenbuddy.island-shadow` beneath it). It renders
//! only while something moves; at rest the thread sleeps in
//! `MsgWaitForMultipleObjects` with no timers (a blink wakes it every few
//! seconds once it has dimmed).
#![allow(dead_code)] // shared API; captions, cards and agents use the rest

pub use crate::buddy::BuddyState;
use crate::buddy::{Buddy, Motion};
use crate::config::Presence;
use crate::ui::tokens::{self, Rgba};
use crate::ui::{self, Layer, MonitorInfo, Spring, TextStyle};
use std::cell::RefCell;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};
use tiny_skia::Pixmap;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Threading::{GetCurrentThreadId, INFINITE};
use windows::Win32::UI::Accessibility::{HWINEVENTHOOK, SetWinEventHook};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;

// ---------------------------------------------------------------- public API

/// Lifecycle of an agent as the island shows it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentStatus {
    Pending,
    Running,
    /// Waiting on the user (a question or an approval).
    Asking,
    Done,
    Failed,
    Stopped,
}

/// One row of the peek panel.
#[derive(Clone, Debug, PartialEq)]
pub struct AgentRow {
    pub id: String,
    pub name: String,
    /// One-line status ("Reading the PDF", "Needs your OK").
    pub status: String,
    pub kind: AgentStatus,
    /// When it started (shown as an age: 12S, 4M, 2H).
    pub started: SystemTime,
    /// Finished or asking and not looked at yet; counts in the pill readout.
    pub unseen: bool,
}

/// Things the island asks the app to do (delivered as `AppEvent::Island`).
#[derive(Clone, Debug, PartialEq)]
pub enum IslandEvent {
    OpenAgent(String),
}

/// Cheap to clone; every method is thread-safe and never blocks on the island.
#[derive(Clone)]
pub struct IslandHandle {
    shared: Arc<Shared>,
}

struct Shared {
    tid: Mutex<u32>,
    inbox: Mutex<Inbox>,
    anchor: Mutex<RECT>,
}

#[derive(Default)]
struct Inbox {
    state: Option<BuddyState>,
    agents: Option<Vec<AgentRow>>,
    attach: Option<Option<Attach>>,
    /// Blur strength (0 = solid), from `[appearance] blur`.
    glass: Option<f32>,
}

/// A surface hanging from the island as part of it (the palette): the pill
/// widens to `width` on `mon` and flattens its bottom edge to join it.
#[derive(Clone, Copy, Debug)]
pub struct Attach {
    /// Logical px.
    pub width: f32,
    pub mon: MonitorInfo,
}

const WM_WAKE: u32 = WM_APP + 7;

impl IslandHandle {
    fn wake(&self) {
        let tid = *self.shared.tid.lock().unwrap();
        if tid != 0 {
            unsafe {
                let _ = PostThreadMessageW(tid, WM_WAKE, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// What the buddy is doing. `Done` and `Error` fall back to rest by
    /// themselves; rest is `Working(n)` while agents run, else `Idle`.
    pub fn set_state(&self, s: BuddyState) {
        self.shared.inbox.lock().unwrap().state = Some(s);
        self.wake();
    }

    /// Replaces the agent rows shown on peek (newest first; 5 shown).
    pub fn set_agents(&self, rows: Vec<AgentRow>) {
        self.shared.inbox.lock().unwrap().agents = Some(rows);
        self.wake();
    }

    /// Sets the glass strength (0 = solid ink).
    pub fn set_glass(&self, strength: f32) {
        self.shared.inbox.lock().unwrap().glass = Some(strength);
        self.wake();
    }

    /// Screen rect (physical px) of the island's body as it is drawn now, so
    /// cards can hang from it. Empty while hidden.
    pub fn anchor(&self) -> RECT {
        *self.shared.anchor.lock().unwrap()
    }
}

/// Merges the island with the surface below it (`Some`) or lets go (`None`).
/// While attached the island stays shown whatever else would hide it (the
/// setting, full-screen apps, auto-hide), doesn't peek or dim, and stays on
/// `mon`. No-op if the island isn't running.
pub fn attach(a: Option<Attach>) {
    if let Some(h) = HANDLE.get() {
        h.shared.inbox.lock().unwrap().attach = Some(a);
        h.wake();
    }
}

/// Where the island's body sits (physical px) once attached at `width` on `mon`.
pub fn header_rect(mon: &MonitorInfo, width: f32) -> RECT {
    let s = mon.scale;
    let (bw, bh) = ((width * s).ceil() as i32, (PILL_H * s).ceil() as i32);
    let m = mon.rect;
    let x = m.x + (m.w - bw) / 2;
    let y = m.y + (TOP_GAP * s).round() as i32;
    RECT {
        left: x,
        top: y,
        right: x + bw,
        bottom: y + bh,
    }
}

static HANDLE: OnceLock<IslandHandle> = OnceLock::new();
type Waker = Box<dyn Fn() + Send + Sync>;
static ANCHOR_WAKER: Mutex<Option<Waker>> = Mutex::new(None);

/// Called (on the island thread; keep it short) whenever [`IslandHandle::anchor`]
/// changes, so surfaces hanging from the island can follow without polling.
pub fn on_anchor_change(f: impl Fn() + Send + Sync + 'static) {
    *ANCHOR_WAKER.lock().unwrap() = Some(Box::new(f));
}

fn set_anchor(shared: &Shared, r: RECT) {
    let changed = {
        let mut a = shared.anchor.lock().unwrap();
        let changed = *a != r;
        *a = r;
        changed
    };
    if changed {
        if let Some(f) = ANCHOR_WAKER.lock().unwrap().as_ref() {
            f();
        }
    }
}

/// Spawns the island thread (once; later calls return the same handle).
pub fn start() -> IslandHandle {
    HANDLE
        .get_or_init(|| {
            let h = IslandHandle {
                shared: Arc::new(Shared {
                    tid: Mutex::new(0),
                    inbox: Mutex::new(Inbox::default()),
                    anchor: Mutex::new(RECT::default()),
                }),
            };
            let shared = h.shared.clone();
            let (ready_tx, ready_rx) = std::sync::mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("island".into())
                .spawn(move || {
                    unsafe {
                        // Create this thread's message queue before anyone posts to it.
                        let mut m = MSG::default();
                        let _ = PeekMessageW(&mut m, None, 0, 0, PM_NOREMOVE);
                        *shared.tid.lock().unwrap() = GetCurrentThreadId();
                    }
                    let _ = ready_tx.send(());
                    if let Err(e) = run(shared) {
                        crate::config::log(&format!("island: {e:#}"));
                    }
                });
            if spawned.is_ok() {
                let _ = ready_rx.recv_timeout(Duration::from_secs(2));
            }
            h
        })
        .clone()
}

/// The running island, if it was started.
/// The island's glass (blurred backdrop + how much ink it keeps), shared so
/// the cards hanging below it are the same material.
static GLASS: Mutex<Option<(Arc<crate::surface::Backdrop>, f32)>> = Mutex::new(None);

fn publish_glass(b: &Option<Arc<crate::surface::Backdrop>>, opacity: f32) {
    let mut g = GLASS.lock().unwrap();
    let same = match (&*g, b) {
        (Some((a, o)), Some(b)) => Arc::ptr_eq(a, b) && *o == opacity,
        (None, None) => true,
        _ => false,
    };
    if !same {
        *g = b.clone().map(|b| (b, opacity));
    }
}

/// The island's current glass, if it has one (None = solid ink).
pub fn glass() -> Option<(Arc<crate::surface::Backdrop>, f32)> {
    GLASS.lock().unwrap().clone()
}

pub fn handle() -> Option<IslandHandle> {
    HANDLE.get().cloned()
}

/// Sets the buddy's state if the island is running (no-op otherwise).
pub fn buddy(s: BuddyState) {
    if let Some(h) = HANDLE.get() {
        h.set_state(s);
    }
}

// ---------------------------------------------------------------- layout (logical px)

const TOP_GAP: f32 = 8.0;
/// Rest pill height (logical px); the anchor is taller while peeking.
pub const PILL_H: f32 = 36.0;
const PEEK_W: f32 = 360.0;
const PAD_X: f32 = 13.0;
const PITCH: f32 = 3.2;
const GAP: f32 = 9.0;
const ROW_H: f32 = 44.0;
const ASK_H: f32 = 38.0;
const EMPTY_H: f32 = 30.0;
const MAX_ROWS: usize = 5;
const PEEK_DELAY: Duration = Duration::from_millis(120);
const UNPEEK_DELAY: Duration = Duration::from_millis(220);
/// Dim to DIM_ALPHA after this long with nothing happening.
const DIM_AFTER: Duration = Duration::from_secs(10);
/// With auto-hide on, tuck away after this long with nothing happening.
const HIDE_AFTER: Duration = Duration::from_secs(4);
/// While tucked away, how often the cursor is checked for the top-edge reveal.
const EDGE_POLL: Duration = Duration::from_millis(120);
/// Half-width (logical px) of the top-edge zone that brings the island back.
const EDGE_HALF_W: f32 = 160.0;

/// `[island] presence`: 0 = hotkey, 1 = auto-hide, 2 = always.
static PRESENCE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// When the island is on screen (from config `[island] presence`).
pub fn set_presence(p: Presence) {
    let v = match p {
        Presence::Hotkey => 0,
        Presence::AutoHide => 1,
        Presence::Always => 2,
    };
    PRESENCE.store(v, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = handle() {
        h.wake();
    }
}

fn presence() -> Presence {
    match PRESENCE.load(std::sync::atomic::Ordering::Relaxed) {
        1 => Presence::AutoHide,
        2 => Presence::Always,
        _ => Presence::Hotkey,
    }
}

/// Only-when-called: the island is out while it is the palette's header or
/// the buddy is listening, thinking, answering or showing an error; a done
/// face after the palette closed doesn't bring it back.
fn summoned(attached: bool, state: BuddyState) -> bool {
    attached
        || matches!(
            state,
            BuddyState::Listening(_) | BuddyState::Thinking | BuddyState::Streaming | BuddyState::Error
        )
}

static GLASS_STRENGTH: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0x4270_0000); // 60.0

/// The blur strength surfaces use (0 = solid), for the cards' own glass
/// while the island is away.
pub fn glass_strength() -> f32 {
    f32::from_bits(GLASS_STRENGTH.load(std::sync::atomic::Ordering::Relaxed))
}

/// How much of the screen shows through the island (0 = solid).
pub fn set_glass(strength: f32) {
    GLASS_STRENGTH.store(strength.clamp(0.0, 100.0).to_bits(), std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = handle() {
        h.set_glass(strength);
    }
}

/// True when the cursor sits on the top edge of its monitor, near the centre.
fn cursor_on_top_edge() -> bool {
    let mut p = POINT::default();
    if unsafe { GetCursorPos(&mut p) }.is_err() {
        return false;
    }
    let m = ui::monitor_under_cursor();
    let r = m.rect;
    let cx = r.x + r.w / 2;
    p.y <= r.y + 1 && ((p.x - cx).abs() as f32) <= EDGE_HALF_W * m.scale
}
const DIM_ALPHA: f32 = 0.36;
/// Shadow-layer margins around the body.
const SHADOW_SIDE: f32 = 28.0;
const SHADOW_BELOW: f32 = 40.0;

fn face_w() -> f32 {
    crate::buddy::COLS as f32 * PITCH
}

/// What a click at a logical point inside the body means.
#[derive(Clone, Debug, PartialEq)]
enum Hit {
    Pill,
    Agent(usize),
    Ask,
    Nothing,
}

/// Everything about how the island looks, minus the windows (so tests can
/// render it headless).
struct View {
    buddy: Buddy,
    agents: Vec<AgentRow>,
    /// Transient state started (for the elapsed readout).
    busy_since: Option<Instant>,
    w: Spring,
    h: Spring,
    /// 0 = pill, 1 = peek panel.
    reveal: Spring,
    alpha: Spring,
    label: String,
    label_since: Instant,
    hover: Option<Hit>,
    peek: bool,
    /// Width (logical) while a surface hangs from the island.
    attached: Option<f32>,
    /// 0 = round pill, 1 = flat bottom edge joined to the attached surface.
    flat: Spring,
}

impl View {
    fn new(now: Instant) -> Self {
        let mut v = Self {
            buddy: Buddy::new(now),
            agents: Vec::new(),
            busy_since: None,
            w: Spring::new(0.0),
            h: Spring::new(PILL_H),
            reveal: Spring::new(0.0),
            alpha: Spring::new(1.0).with(90.0, 19.0),
            label: String::new(),
            label_since: now,
            hover: None,
            peek: false,
            attached: None,
            flat: Spring::new(0.0).with(520.0, 46.0),
        };
        v.layout(now);
        v.w.snap(v.w.target);
        v
    }

    fn running(&self) -> u32 {
        self.agents
            .iter()
            .filter(|a| matches!(a.kind, AgentStatus::Running | AgentStatus::Pending))
            .count() as u32
    }

    /// The state to return to after a transient one.
    fn rest_state(&self) -> BuddyState {
        match self.running() {
            0 => BuddyState::Idle,
            n => BuddyState::Working(n),
        }
    }

    fn set_state(&mut self, s: BuddyState, now: Instant) {
        let timed = matches!(s, BuddyState::Thinking | BuddyState::Streaming);
        if !timed {
            self.busy_since = None;
        } else if self.busy_since.is_none() {
            self.busy_since = Some(now);
        }
        self.buddy.set(s, now);
    }

    fn set_agents(&mut self, rows: Vec<AgentRow>, now: Instant) {
        self.agents = rows;
        if matches!(
            self.buddy.state(),
            BuddyState::Idle | BuddyState::Working(_)
        ) {
            self.buddy.set(self.rest_state(), now);
        }
    }

    fn label_for(&self) -> (String, Rgba) {
        let s = match self.buddy.state() {
            BuddyState::Listening(_) => ("Listening", tokens::SIGNAL),
            // The face and the elapsed readout say enough.
            BuddyState::Thinking | BuddyState::Streaming => ("", tokens::MUTE),
            BuddyState::Done => ("Done", tokens::PAPER),
            BuddyState::Error => ("Error", tokens::ERR),
            BuddyState::Idle | BuddyState::Working(_) => ("", tokens::MUTE),
        };
        (s.0.to_string(), s.1)
    }

    /// Right-aligned Doto readout: elapsed while busy, else the agent count.
    fn readout(&self, now: Instant) -> Option<(String, Rgba)> {
        if let Some(t0) = self.busy_since {
            let s = now.saturating_duration_since(t0).as_secs();
            return Some((
                format!("{:02}:{:02}", s / 60, s % 60),
                tokens::PAPER.fade(0.8),
            ));
        }
        let n = self.running()
            + self
                .agents
                .iter()
                .filter(|a| a.unseen && a.kind != AgentStatus::Running)
                .count() as u32;
        (n > 0).then(|| (format!("{n:02}"), tokens::PAPER))
    }

    fn rows_h(&self) -> f32 {
        let n = self.agents.len().min(MAX_ROWS);
        let list = if n == 0 { EMPTY_H } else { n as f32 * ROW_H };
        PILL_H + 1.0 + 6.0 + list + ASK_H + 6.0
    }

    /// Rest-pill width for the current content.
    fn rest_w(&self, now: Instant) -> f32 {
        let mut w = PAD_X + face_w() + PAD_X;
        if matches!(self.buddy.state(), BuddyState::Working(_)) {
            w += 4.0; // room for the badge
        }
        let (label, _) = self.label_for();
        if !label.is_empty() {
            w += GAP + ui::text_width(&label, &TextStyle::label(1.0)) - 2.0;
        }
        if let Some((r, _)) = self.readout(now) {
            w += GAP + 3.0 + ui::text_width(&r, &TextStyle::readout(1.0)) - 2.0;
        }
        w.max(56.0)
    }

    /// Updates spring targets from the current content.
    fn layout(&mut self, now: Instant) {
        let (label, _) = self.label_for();
        if label != self.label {
            self.label = label;
            self.label_since = now;
        }
        if let Some(w) = self.attached {
            self.peek = false;
            self.w.set(w);
            self.h.set(PILL_H);
            self.reveal.set(0.0);
            self.flat.set(1.0);
            return;
        }
        self.flat.set(0.0);
        let peek = self.peek;
        self.w.set(if peek { PEEK_W } else { self.rest_w(now) });
        self.h.set(if peek { self.rows_h() } else { PILL_H });
        self.reveal.set(if peek { 1.0 } else { 0.0 });
    }

    fn tick(&mut self, dt: f32, now: Instant) -> bool {
        let mut moving = false;
        for s in [
            &mut self.w,
            &mut self.h,
            &mut self.reveal,
            &mut self.alpha,
            &mut self.flat,
        ] {
            moving |= s.tick(dt);
        }
        self.buddy.tick(dt, now);
        moving
    }

    /// Row rects (logical, inside the body): (y, h, hit).
    fn rows(&self) -> Vec<(f32, f32, Hit)> {
        let mut out = Vec::new();
        let mut y = PILL_H + 1.0 + 6.0;
        if self.agents.is_empty() {
            y += EMPTY_H;
        }
        for i in 0..self.agents.len().min(MAX_ROWS) {
            out.push((y, ROW_H, Hit::Agent(i)));
            y += ROW_H;
        }
        out.push((y, ASK_H, Hit::Ask));
        out
    }

    fn hit(&self, x: f32, y: f32) -> Hit {
        if x < 0.0 || y < 0.0 || x > self.w.value || y > self.h.value {
            return Hit::Nothing;
        }
        if y < PILL_H || self.reveal.value < 0.5 {
            return Hit::Pill;
        }
        for (ry, rh, hit) in self.rows() {
            if y >= ry && y < ry + rh && x >= 8.0 && x <= self.w.value - 8.0 {
                return hit;
            }
        }
        Hit::Nothing
    }

    fn radius(&self) -> f32 {
        let r = 18.0 + (22.0 - 18.0) * self.reveal.value.clamp(0.0, 1.0);
        r.min(self.h.value / 2.0)
    }

    /// Bottom corner radius: the top one, flattening as the island attaches.
    fn bottom_radius(&self) -> f32 {
        self.radius() * (1.0 - self.flat.value.clamp(0.0, 1.0))
    }

    /// Body size in physical px.
    fn body_size(&self, s: f32) -> (u32, u32) {
        (
            (self.w.value * s).ceil().max(1.0) as u32,
            (self.h.value * s).ceil().max(1.0) as u32,
        )
    }

    fn render_body(&mut self, s: f32, now: Instant) -> Option<Pixmap> {
        self.render_body_glass(s, now, None)
    }

    fn render_body_glass(&mut self, s: f32, now: Instant, glass: Option<Glass<'_>>) -> Option<Pixmap> {
        let (bw, bh) = self.body_size(s);
        let mut pm = Pixmap::new(bw, bh)?;
        let (w, h) = (bw as f32, bh as f32);
        let r = self.radius() * s;
        let rb = self.bottom_radius() * s;
        let flat = self.flat.value.clamp(0.0, 1.0);
        let body = body_path(0.0, 0.0, w, h, r, rb);
        match (body, glass) {
            (Some(path), Some(g)) => fill_glass(&mut pm, &path, &g),
            (body, _) => fill_path(&mut pm, body, tokens::INK),
        }
        let inner = body_path(0.5, 0.5, w - 1.0, h - 1.0, (r - 0.5).max(0.0), (rb - 0.5).max(0.0));
        stroke_path(&mut pm, inner, tokens::LINE);
        if flat > 0.0 {
            // The bottom edge is the seam with the attached surface: no hairline.
            ui::fill_rect(&mut pm, 1.0, h - 2.0, w - 2.0, 2.0, tokens::INK.fade(flat));
        }

        // ---- header: face · label ······ readout
        let mid = PILL_H * s / 2.0;
        let (_, fh) = Buddy::size(PITCH * s);
        let fx = PAD_X * s;
        let fy = (mid - fh / 2.0).round();
        self.buddy.draw(&mut pm, fx, fy, PITCH * s, 1.0, now);

        let fit = |need: f32, have: f32| (1.0 - (need - have) / 14.0).clamp(0.0, 1.0);
        let rest_w = self.rest_w(now);
        let content = fit(rest_w, self.w.value);
        let mut lx = fx + face_w() * s + GAP * s;
        if matches!(self.buddy.state(), BuddyState::Working(_)) {
            lx += 4.0 * s;
        }
        let (label, lcolor) = self.label_for();
        let lab_in = ease_out(
            (now.saturating_duration_since(self.label_since)
                .as_secs_f32()
                / 0.2)
                .min(1.0),
        );
        let reveal = self.reveal.value.clamp(0.0, 1.0);
        let (label, lcolor, la) = if label.is_empty() {
            ("screenbuddy".to_string(), tokens::MUTE, reveal.max(flat))
        } else {
            (label, lcolor, lab_in * content.max(reveal))
        };
        // Readout first (right-aligned), so the label can fade instead of colliding.
        let mut read_left = f32::MAX;
        if let Some((text, color)) = self.readout(now) {
            let rs = TextStyle::readout(s).color(color.fade(content.max(reveal)));
            let tw = ui::text_width(&text, &rs);
            let base = (mid + ui::cap_height(&rs) / 2.0).round();
            let mut rx = w - PAD_X * s - tw;
            if reveal > 0.0 && self.busy_since.is_none() {
                // In the panel the count reads "03 AGENTS".
                let unit = TextStyle::label(s).color(tokens::MUTE.fade(reveal));
                let uw = ui::text_width("agents", &unit);
                ui::draw_text_baseline(
                    &mut pm,
                    "agents",
                    w - PAD_X * s - uw,
                    base - 0.5 * s,
                    &unit,
                );
                rx -= (uw + 6.0 * s) * ease_out(reveal);
            }
            ui::draw_text_baseline(&mut pm, &text, rx, base, &rs);
            read_left = rx;
        }
        let ls = TextStyle::label(s);
        let room = read_left - (lx + ui::text_width(&label, &ls)) - 8.0 * s;
        let la = la * (room / (10.0 * s) + 1.0).clamp(0.0, 1.0);
        if la > 0.01 {
            let base = (mid + ui::cap_height(&ls) / 2.0).round();
            ui::draw_text_baseline(&mut pm, &label, lx, base, &ls.color(lcolor.fade(la)));
        }

        // ---- peek panel
        let ra = ((reveal - 0.3) / 0.7).clamp(0.0, 1.0);
        if ra > 0.0 {
            let rise = (1.0 - ease_out(ra)) * 6.0 * s; // rows settle up into place
            ui::fill_rect(
                &mut pm,
                14.0 * s,
                PILL_H * s,
                w - 28.0 * s,
                1.0,
                tokens::LINE.fade(ra),
            );
            if self.agents.is_empty() {
                let es = TextStyle::label(s).color(tokens::MUTE.fade(0.7 * ra));
                let y = (PILL_H + 7.0) * s + rise;
                ui::draw_text_baseline(
                    &mut pm,
                    "nothing running",
                    22.0 * s,
                    y + (EMPTY_H * s + ui::cap_height(&es)) / 2.0,
                    &es,
                );
            }
            for (ry, rh, hit) in self.rows() {
                let (y, rh) = (ry * s + rise, rh * s);
                let hovered = self.hover.as_ref() == Some(&hit);
                if hovered {
                    ui::fill_rounded(
                        &mut pm,
                        8.0 * s,
                        y + 2.0 * s,
                        w - 16.0 * s,
                        rh - 4.0 * s,
                        tokens::RADIUS_CHIP * s,
                        tokens::INK_2.fade(ra),
                    );
                }
                match hit {
                    Hit::Agent(i) => self.draw_agent(&mut pm, i, y, rh, w, s, ra, hovered),
                    Hit::Ask => {
                        if !hovered {
                            ui::fill_rect(
                                &mut pm,
                                14.0 * s,
                                y,
                                w - 28.0 * s,
                                1.0,
                                tokens::LINE.fade(ra * 0.7),
                            );
                        }
                        let c = if hovered { tokens::PAPER } else { tokens::MUTE };
                        let bs = TextStyle::body(s).color(c.fade(ra));
                        let base = (y + rh / 2.0 + ui::cap_height(&bs) / 2.0).round();
                        // A small chevron where the status dots sit.
                        ui::draw_text_baseline(&mut pm, "›", 18.5 * s, base, &bs);
                        ui::draw_text_baseline(&mut pm, "Ask…", 34.0 * s, base, &bs);
                    }
                    _ => {}
                }
            }
        }

        // Clip everything to the body shape.
        if let (Some(mut mask), Some(path)) = (
            tiny_skia::Mask::new(bw, bh),
            body_path(0.0, 0.0, w, h, r, rb),
        ) {
            mask.fill_path(
                &path,
                tiny_skia::FillRule::Winding,
                true,
                tiny_skia::Transform::identity(),
            );
            pm.apply_mask(&mask);
        }
        Some(pm)
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_agent(
        &self,
        pm: &mut Pixmap,
        i: usize,
        y: f32,
        rh: f32,
        w: f32,
        s: f32,
        ra: f32,
        hovered: bool,
    ) {
        let a = &self.agents[i];
        let dot = match a.kind {
            AgentStatus::Running => tokens::SIGNAL,
            AgentStatus::Asking => tokens::ASK,
            AgentStatus::Done => tokens::OK,
            AgentStatus::Failed => tokens::ERR,
            AgentStatus::Pending | AgentStatus::Stopped => tokens::MUTE,
        };
        let name_s = TextStyle::body(s).color(tokens::PAPER.fade(ra));
        let stat_s = TextStyle::label(s).color(tokens::MUTE.fade(ra));
        let stat_s = TextStyle {
            caps: false,
            tracking: 0.0,
            face: ui::Face::Mono,
            ..stat_s
        };
        // Two lines centred in the row: name on top, status below.
        let name_base = (y + rh / 2.0 - 2.0 * s).round();
        let stat_base = (name_base + 15.0 * s).round();
        let dy = name_base - ui::cap_height(&name_s) / 2.0;
        if a.kind == AgentStatus::Running {
            ui::fill_circle(pm, 22.0 * s, dy, 5.0 * s, dot.fade(0.16 * ra));
        }
        ui::fill_circle(pm, 22.0 * s, dy, 2.6 * s, dot.fade(ra));
        // Seen and finished rows step back a little.
        let quiet = !a.unseen
            && matches!(
                a.kind,
                AgentStatus::Done | AgentStatus::Failed | AgentStatus::Stopped
            );
        let name_s = if quiet {
            name_s.color(tokens::PAPER.mix(tokens::MUTE, 0.45).fade(ra))
        } else {
            name_s
        };

        let age = age_label(a.started);
        let ag = TextStyle::label(s).color(tokens::MUTE.fade(ra));
        let aw = ui::text_width(&age, &ag);
        let right = w - 22.0 * s;
        ui::draw_text_baseline(pm, &age, right - aw, name_base, &ag);
        let x = 34.0 * s;
        let name = ui::ellipsize(&a.name, &name_s, right - aw - 12.0 * s - x);
        ui::draw_text_baseline(pm, &name, x, name_base, &name_s);
        let stat = ui::ellipsize(&a.status, &stat_s, right - x);
        let stat_s = if hovered {
            stat_s.color(tokens::PAPER.mix(tokens::MUTE, 0.35).fade(ra))
        } else {
            stat_s
        };
        ui::draw_text_baseline(pm, &stat, x, stat_base, &stat_s);
    }

    /// The soft shadow layer: body rect inset by the margins.
    fn render_shadow(&self, s: f32) -> Option<Pixmap> {
        let (bw, bh) = self.body_size(s);
        let (side, top, below) = shadow_margins(s);
        let mut pm = Pixmap::new(bw + 2 * side, bh + top + below)?;
        ui::shadow(
            &mut pm,
            side as f32,
            top as f32,
            bw as f32,
            bh as f32,
            self.radius() * s,
            s,
        );
        Some(pm)
    }
}

/// A rounded rect with separate top (`rt`) and bottom (`rb`) corner radii.
fn body_path(x: f32, y: f32, w: f32, h: f32, rt: f32, rb: f32) -> Option<tiny_skia::Path> {
    let lim = (w / 2.0).min(h / 2.0).max(0.0);
    let (rt, rb) = (rt.clamp(0.0, lim), rb.clamp(0.0, lim));
    let (x1, y1) = (x + w, y + h);
    let (kt, kb) = (0.552_284_8 * rt, 0.552_284_8 * rb);
    let mut pb = tiny_skia::PathBuilder::new();
    pb.move_to(x + rt, y);
    pb.line_to(x1 - rt, y);
    pb.cubic_to(x1 - rt + kt, y, x1, y + rt - kt, x1, y + rt);
    pb.line_to(x1, y1 - rb);
    pb.cubic_to(x1, y1 - rb + kb, x1 - rb + kb, y1, x1 - rb, y1);
    pb.line_to(x + rb, y1);
    pb.cubic_to(x + rb - kb, y1, x, y1 - rb + kb, x, y1 - rb);
    pb.line_to(x, y + rt);
    pb.cubic_to(x, y + rt - kt, x + rt - kt, y, x + rt, y);
    pb.close();
    pb.finish()
}

fn paint_of(c: Rgba) -> tiny_skia::Paint<'static> {
    let mut p = tiny_skia::Paint::default();
    p.set_color(c.skia());
    p.anti_alias = true;
    p
}

fn fill_path(pm: &mut Pixmap, path: Option<tiny_skia::Path>, c: Rgba) {
    if let Some(p) = path {
        let id = tiny_skia::Transform::identity();
        pm.fill_path(&p, &paint_of(c), tiny_skia::FillRule::Winding, id, None);
    }
}

fn stroke_path(pm: &mut Pixmap, path: Option<tiny_skia::Path>, c: Rgba) {
    if let Some(p) = path {
        let st = tiny_skia::Stroke {
            width: 1.0,
            ..Default::default()
        };
        let id = tiny_skia::Transform::identity();
        pm.stroke_path(&p, &paint_of(c), &st, id, None);
    }
}

/// A blurred screenshot behind the island, plus where the body sits on screen.
struct Glass<'a> {
    backdrop: &'a crate::surface::Backdrop,
    /// The body's top-left in physical screen pixels.
    x: i32,
    y: i32,
    /// How much of the surface keeps its own ink (0 = pure glass).
    opacity: f32,
}

/// Fills `path` with the backdrop, darkened and mixed with the island's ink the
/// same way the palette shader does, so the two read as the same glass.
fn fill_glass(pm: &mut Pixmap, path: &tiny_skia::Path, g: &Glass<'_>) {
    let (w, h) = (pm.width(), pm.height());
    let Some(mut tex) = Pixmap::new(w, h) else {
        return;
    };
    let ink = tokens::INK;
    let (ir, ig, ib) = (
        ink.r as f32 / 255.0,
        ink.g as f32 / 255.0,
        ink.b as f32 / 255.0,
    );
    let px = tex.pixels_mut();
    for y in 0..h {
        let sy = g.y as f32 + y as f32 + 0.5;
        for x in 0..w {
            let sx = g.x as f32 + x as f32 + 0.5;
            let c = g.backdrop.sample_screen(sx, sy);
            let peak = c[0].max(c[1]).max(c[2]).max(0.001);
            let limit = (crate::surface::PEAK / peak).min(1.0);
            let mix = |v: f32, tint: f32| v * limit * (1.0 - g.opacity) + tint * g.opacity;
            let r = (mix(c[0], ir) * 255.0).round().clamp(0.0, 255.0) as u8;
            let gg = (mix(c[1], ig) * 255.0).round().clamp(0.0, 255.0) as u8;
            let b = (mix(c[2], ib) * 255.0).round().clamp(0.0, 255.0) as u8;
            if let Some(c) = tiny_skia::PremultipliedColorU8::from_rgba(r, gg, b, 255) {
                px[(y * w + x) as usize] = c;
            }
        }
    }
    let paint = tiny_skia::Paint {
        shader: tiny_skia::Pattern::new(
            tex.as_ref(),
            tiny_skia::SpreadMode::Pad,
            tiny_skia::FilterQuality::Nearest,
            1.0,
            tiny_skia::Transform::identity(),
        ),
        anti_alias: true,
        ..Default::default()
    };
    pm.fill_path(
        path,
        &paint,
        tiny_skia::FillRule::Winding,
        tiny_skia::Transform::identity(),
        None,
    );
}

fn shadow_margins(s: f32) -> (u32, u32, u32) {
    (
        (SHADOW_SIDE * s).ceil() as u32,
        (TOP_GAP * s).floor() as u32,
        (SHADOW_BELOW * s).ceil() as u32,
    )
}

fn age_label(t: SystemTime) -> String {
    let s = SystemTime::now()
        .duration_since(t)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m", s / 60),
        3600..86400 => format!("{}h", s / 3600),
        _ => format!("{}d", s / 86400),
    }
}

fn ease_out(k: f32) -> f32 {
    1.0 - (1.0 - k.clamp(0.0, 1.0)).powi(3)
}

// ---------------------------------------------------------------- window thread

#[derive(Default)]
struct Input {
    hovered: bool,
    changed_at: Option<Instant>,
    mouse: (i32, i32),
    down: bool,
    clicks: Vec<(i32, i32)>,
    foreground: bool,
    tracking: bool,
}

thread_local! {
    static INPUT: RefCell<Input> = RefCell::new(Input::default());
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    (
        (lp.0 & 0xFFFF) as i16 as i32,
        ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
    )
}

unsafe extern "system" fn body_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
            WM_MOUSEMOVE => {
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    i.mouse = lparam_xy(lp);
                    if !i.hovered {
                        i.hovered = true;
                        i.changed_at = Some(Instant::now());
                    }
                    if !i.tracking {
                        let mut t = TRACKMOUSEEVENT {
                            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                            dwFlags: TME_LEAVE,
                            hwndTrack: hwnd,
                            dwHoverTime: 0,
                        };
                        i.tracking = TrackMouseEvent(&mut t).is_ok();
                    }
                });
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    i.hovered = false;
                    i.tracking = false;
                    i.down = false;
                    i.changed_at = Some(Instant::now());
                });
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                INPUT.with(|i| i.borrow_mut().down = true);
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    if i.down {
                        i.clicks.push(lparam_xy(lp));
                    }
                    i.down = false;
                });
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(c));
                }
                return LRESULT(1);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

unsafe extern "system" fn on_foreground(
    _: HWINEVENTHOOK,
    _: u32,
    _: HWND,
    _: i32,
    _: i32,
    _: u32,
    _: u32,
) {
    INPUT.with(|i| i.borrow_mut().foreground = true);
}

/// True when the foreground window covers the whole monitor (video, game,
/// presentation): the island stays out of the way.
fn fullscreen_on(mon: &MonitorInfo) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        if fg.0.is_null() || fg == GetShellWindow() || fg == GetDesktopWindow() {
            return false;
        }
        let mut cls = [0u16; 32];
        let n = GetClassNameW(fg, &mut cls) as usize;
        let cls = String::from_utf16_lossy(&cls[..n]);
        if cls == "WorkerW" || cls == "Progman" || cls.starts_with("screenbuddy") {
            return false;
        }
        // Maximized windows can overhang the monitor by their resize border,
        // and some apps (Flock) draw their own frame while still being a
        // normal framed window. Those are not fullscreen: the island belongs
        // on top of them. Real fullscreen apps drop the caption and frame.
        let style = GetWindowLongW(fg, GWL_STYLE) as u32;
        let framed = (style & (WS_CAPTION.0 | WS_THICKFRAME.0)) != 0;
        let mut wp = WINDOWPLACEMENT {
            length: std::mem::size_of::<WINDOWPLACEMENT>() as u32,
            ..Default::default()
        };
        let maximized = GetWindowPlacement(fg, &mut wp).is_ok() && wp.showCmd == SW_SHOWMAXIMIZED.0 as u32;
        if framed || maximized {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(fg, &mut r).is_err() {
            return false;
        }
        let m = mon.rect;
        r.left <= m.x && r.top <= m.y && r.right >= m.x + m.w && r.bottom >= m.y + m.h
    }
}

struct Island {
    view: View,
    body: Layer,
    shadow: Layer,
    shared: Arc<Shared>,
    mon: MonitorInfo,
    fullscreen: bool,
    /// 0..1 fade for hide/show (multiplies alpha).
    shown: Spring,
    activity: Instant,
    last: Instant,
    dirty: bool,
    shadow_key: Option<(i32, i32, u32, u32, u32)>,
    shadow_alpha: f32,
    /// A click opened the palette: no peeking again until the cursor leaves.
    no_peek: bool,
    /// Auto-hidden because nothing is happening (polls for the top-edge reveal).
    tucked: bool,
    /// The blurred screen behind the island (None = solid ink).
    backdrop: Option<Arc<crate::surface::Backdrop>>,
    /// Glass strength from settings: 0 = solid.
    glass: f32,
    /// When the backdrop was last captured, so it follows the screen.
    glass_at: Instant,
    /// Last time the foreground window was checked for true fullscreen.
    full_at: Instant,
    /// What the last frame decided dimming and tucking from (for `pace`).
    idle: Idle,
}

/// What dimming and auto-hide depend on. `frame` decides from it and `pace`
/// wakes for its next flip, so the two can never disagree.
#[derive(Clone, Copy, Debug)]
struct Idle {
    resting: bool,
    attached: bool,
    hovered: bool,
    peek: bool,
    /// An agent is asking: the island stays out.
    needs_you: bool,
    auto_hide: bool,
    activity: Instant,
}

impl Idle {
    fn new(now: Instant) -> Self {
        Self {
            resting: true,
            attached: false,
            hovered: false,
            peek: false,
            needs_you: false,
            auto_hide: false,
            activity: now,
        }
    }

    /// Full opacity (and breathing); a resting island dims after DIM_AFTER.
    fn awake(&self, now: Instant) -> bool {
        !self.resting
            || self.attached
            || self.hovered
            || self.peek
            || now.saturating_duration_since(self.activity) < DIM_AFTER
    }

    /// Auto-hidden because nothing is happening.
    fn tucked(&self, now: Instant) -> bool {
        !self.attached
            && self.auto_hide
            && self.resting
            && !self.hovered
            && !self.peek
            && !self.needs_you
            && now.saturating_duration_since(self.activity) >= HIDE_AFTER
    }

    /// When `awake` or `tucked` next changes by itself. Only a deadline still
    /// ahead that flips something counts: one that passed without a flip (an
    /// agent asking keeps the island out) would ask for a 1 ms wait forever.
    fn next_flip(&self, now: Instant) -> Option<Instant> {
        [self.activity + HIDE_AFTER, self.activity + DIM_AFTER]
            .into_iter()
            .filter(|&t| {
                t > now && (self.awake(t) != self.awake(now) || self.tucked(t) != self.tucked(now))
            })
            .min()
    }
}

fn run(shared: Arc<Shared>) -> anyhow::Result<()> {
    let now = Instant::now();
    let body = Layer::new("island", Some(body_proc), false)?;
    let shadow = Layer::new("island-shadow", None, true)?;
    let mon = ui::monitor_under_cursor();
    let mut isl = Island {
        view: View::new(now),
        body,
        shadow,
        shared,
        mon,
        fullscreen: false,
        shown: Spring::new(0.0).with(160.0, 26.0),
        activity: now,
        last: now,
        dirty: true,
        shadow_key: None,
        shadow_alpha: -1.0,
        no_peek: false,
        tucked: false,
        backdrop: None,
        glass: 60.0,
        glass_at: now,
        full_at: now,
        idle: Idle::new(now),
    };
    isl.shown.set(1.0);
    unsafe {
        let _ = SetWinEventHook(
            EVENT_SYSTEM_FOREGROUND,
            EVENT_SYSTEM_FOREGROUND,
            None,
            Some(on_foreground),
            0,
            0,
            WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
        );
    }
    let mut msg = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                if msg.message == WM_QUIT {
                    return Ok(());
                }
                if msg.hwnd.0.is_null() && msg.message == WM_WAKE {
                    continue;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let now = Instant::now();
        isl.frame(now);
        let wait = |ms: u32| unsafe {
            MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT);
        };
        match isl.pace(now) {
            Motion::Fast => unsafe {
                if DwmFlush().is_err() {
                    wait(8);
                }
            },
            Motion::Slow => wait(33),
            Motion::Still(Some(t)) => wait(
                t.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(60_000) as u32
                    + 1,
            ),
            Motion::Still(None) => wait(INFINITE),
        }
    }
}

impl Island {
    fn frame(&mut self, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last)
            .as_secs_f32()
            .min(0.05);
        self.last = now;

        // ---- inbox
        let inbox = std::mem::take(&mut *self.shared.inbox.lock().unwrap());
        if let Some(g) = inbox.glass {
            let g = g.clamp(0.0, 100.0);
            if (g - self.glass).abs() > 0.01 {
                self.glass = g;
                self.backdrop = None;
                self.dirty = true;
            }
        }
        if let Some(a) = inbox.attach {
            if let Some(a) = a {
                if a.mon.rect != self.mon.rect || a.mon.scale != self.mon.scale {
                    self.mon = a.mon;
                    self.fullscreen = false;
                }
                self.no_peek = true;
            }
            self.view.attached = a.map(|a| a.width);
            self.activity = now;
            self.dirty = true;
        }
        if let Some(rows) = inbox.agents {
            if rows != self.view.agents {
                self.view.set_agents(rows, now);
                self.dirty = true;
            }
        }
        if let Some(s) = inbox.state {
            let s = match s {
                // Callers say Idle; we know whether agents are still working.
                BuddyState::Idle => self.view.rest_state(),
                s => s,
            };
            if s != self.view.buddy.state() {
                if !matches!(s, BuddyState::Listening(_))
                    || !matches!(self.view.buddy.state(), BuddyState::Listening(_))
                {
                    self.activity = now;
                    if self.view.attached.is_none() {
                        self.follow_cursor();
                    }
                }
                self.view.set_state(s, now);
                self.dirty = true;
            }
        }

        // ---- transient states fall back to rest by themselves
        let age = self.view.buddy.age(now);
        match self.view.buddy.state() {
            BuddyState::Done if age > Duration::from_millis(2600) => {
                let r = self.view.rest_state();
                self.view.set_state(r, now);
                self.dirty = true;
            }
            BuddyState::Error if age > Duration::from_secs(6) => {
                let r = self.view.rest_state();
                self.view.set_state(r, now);
                self.dirty = true;
            }
            _ => {}
        }

        // ---- input
        let (hovered_raw, changed, mouse, clicks, fg) = INPUT.with(|i| {
            let mut i = i.borrow_mut();
            let fg = std::mem::take(&mut i.foreground);
            (
                i.hovered,
                i.changed_at,
                i.mouse,
                std::mem::take(&mut i.clicks),
                fg,
            )
        });
        // A layered window only sees WM_MOUSEMOVE while it is visible, so while
        // the island is tucked away its own hover area is checked here. That is
        // what makes "move the mouse back to it" work, not just the top edge.
        let presence = presence();
        let over_pill = presence != Presence::Hotkey && !self.fullscreen && self.cursor_on_pill();
        let hovered = hovered_raw || over_pill;
        if self.view.attached.is_none() {
            // Re-check true fullscreen now and then: the foreground window can
            // go fullscreen (or leave it) without another foreground event.
            let every = if self.fullscreen {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            };
            if fg || now.saturating_duration_since(self.full_at) >= every {
                self.full_at = now;
                if fg {
                    self.follow_cursor();
                }
                let full = fullscreen_on(&self.mon);
                if full != self.fullscreen {
                    self.fullscreen = full;
                    self.dirty = true;
                }
            }
        }
        let since = changed.map_or(Duration::MAX, |t| now.saturating_duration_since(t));
        if !hovered {
            self.no_peek = false;
        }
        let peek = if hovered {
            !self.no_peek && (self.view.peek || since >= PEEK_DELAY)
        } else {
            self.view.peek && since < UNPEEK_DELAY
        };
        if peek != self.view.peek {
            self.view.peek = peek;
            self.dirty = true;
        }
        if hovered {
            self.activity = now;
        }
        let s = self.mon.scale;
        let (lx, ly) = (mouse.0 as f32 / s, mouse.1 as f32 / s);
        let hover = hovered
            .then(|| self.view.hit(lx, ly))
            .filter(|h| *h != Hit::Nothing);
        if hover != self.view.hover {
            self.view.hover = hover;
            self.dirty = true;
        }
        for (x, y) in clicks {
            match self.view.hit(x as f32 / s, y as f32 / s) {
                Hit::Pill | Hit::Ask => {
                    self.view.peek = false;
                    self.no_peek = true;
                    crate::win::push(crate::win::AppEvent::IslandClick);
                }
                Hit::Agent(i) => {
                    if let Some(a) = self.view.agents.get(i) {
                        crate::win::push(crate::win::AppEvent::Island(IslandEvent::OpenAgent(
                            a.id.clone(),
                        )));
                    }
                }
                Hit::Nothing => {}
            }
            self.dirty = true;
        }

        // ---- targets
        let state = self.view.buddy.state();
        let resting = matches!(state, BuddyState::Idle | BuddyState::Working(_));
        let attached = self.view.attached.is_some();
        if attached {
            // Count the palette as activity, so closing it doesn't tuck at once.
            self.activity = now;
        }
        if self.tucked && cursor_on_top_edge() {
            self.activity = now;
            self.follow_cursor();
            self.dirty = true;
        }
        self.idle = Idle {
            resting,
            attached,
            hovered,
            peek: self.view.peek,
            needs_you: self.view.agents.iter().any(|a| a.kind == AgentStatus::Asking),
            auto_hide: presence == Presence::AutoHide,
            activity: self.activity,
        };
        let awake = self.idle.awake(now);
        self.view.buddy.set_awake(awake);
        self.view.alpha.set(if awake { 1.0 } else { DIM_ALPHA });
        self.tucked = self.idle.tucked(now);
        // Attached, the island is the palette's header: nothing hides it.
        let called = presence != Presence::Hotkey || summoned(attached, state);
        let hide = !attached && (!called || (self.fullscreen && resting) || self.tucked);
        self.shown.set(if hide { 0.0 } else { 1.0 });

        // ---- glass: a small blurred copy of what is behind the island
        let glass_on = self.glass > 0.0 && !hide;
        if glass_on {
            if self.backdrop.is_none() || self.glass_at.elapsed() >= Duration::from_secs(2) {
                let rect = self.glass_rect();
                match crate::capture::grab_rect(rect) {
                    Ok(shot) => {
                        self.backdrop = crate::surface::Backdrop::new(&shot, self.glass);
                    }
                    Err(e) => {
                        crate::config::log(&format!("island glass: {e:#}"));
                        self.backdrop = None;
                    }
                }
                self.glass_at = now;
            }
        } else if self.backdrop.is_some() {
            self.backdrop = None;
        }
        publish_glass(&self.backdrop, crate::surface::glass_opacity(self.glass));

        self.view.layout(now);

        // ---- animate + draw
        let moving = self.view.tick(dt, now) | self.shown.tick(dt);
        let animating = moving || !matches!(self.view.buddy.motion(now), Motion::Still(_));
        if !(animating || self.dirty) {
            return;
        }
        self.dirty = false;
        let opacity = self.view.alpha.value * self.shown.value;
        if self.shown.value <= 0.001 && self.shown.target == 0.0 {
            self.body.hide();
            self.shadow.hide();
            self.body.release_memory();
            self.shadow.release_memory();
            self.shadow_key = None;
            set_anchor(&self.shared, RECT::default());
            return;
        }
        self.draw(now, opacity);
    }

    fn draw(&mut self, now: Instant, opacity: f32) {
        let s = self.mon.scale;
        let m = self.mon.rect;
        let (bw, _bh) = self.view.body_size(s);
        let x = m.x + (m.w - bw as i32) / 2;
        let y = m.y + (TOP_GAP * s).round() as i32;
        let glass = self.backdrop.as_deref().map(|b| Glass {
            backdrop: b,
            x,
            y,
            opacity: crate::surface::glass_opacity(self.glass),
        });
        let Some(pm) = self.view.render_body_glass(s, now, glass) else {
            return;
        };
        let (bw, bh) = (pm.width() as i32, pm.height() as i32);
        let x = m.x + (m.w - bw) / 2;
        let y = m.y + (TOP_GAP * s).round() as i32;
        let first = !self.body.visible();
        self.body.present(&pm, x, y, opacity);
        set_anchor(
            &self.shared,
            RECT {
                left: x,
                top: y,
                right: x + bw,
                bottom: y + bh,
            },
        );

        let (side, top, _) = shadow_margins(s);
        let key = (x, y, bw as u32, bh as u32, self.view.radius().to_bits());
        if self.shadow_key != Some(key) {
            if let Some(sp) = self.view.render_shadow(s) {
                self.shadow
                    .present(&sp, x - side as i32, y - top as i32, opacity);
                self.shadow_key = Some(key);
                self.shadow_alpha = opacity;
            }
        } else if (self.shadow_alpha - opacity).abs() > 0.002 {
            self.shadow.set_opacity(opacity);
            self.shadow_alpha = opacity;
        }
        if first {
            self.shadow.place_below(self.body.hwnd());
        }
    }

    fn pace(&self, now: Instant) -> Motion {
        if !self.view.w.settled()
            || !self.view.h.settled()
            || !self.view.reveal.settled()
            || !self.view.alpha.settled()
            || !self.view.flat.settled()
            || !self.shown.settled()
        {
            return Motion::Fast;
        }
        if !self.body.visible() {
            return if self.tucked {
                Motion::Still(Some(now + EDGE_POLL))
            } else if self.fullscreen && presence() != Presence::Hotkey {
                // Poll for the fullscreen app leaving, so the island comes back.
                Motion::Still(Some(now + Duration::from_millis(500)))
            } else {
                Motion::Still(None)
            };
        }
        let input = INPUT.with(|i| {
            let i = i.borrow();
            (i.hovered, i.changed_at)
        });
        wake(&self.view, &self.idle, self.fullscreen, input, self.no_peek, now)
    }

}

/// When a visible, settled island next needs a frame: the buddy's own motion,
/// the fullscreen poll, the pending peek / unpeek, the dim / tuck and the
/// Done/Error fallback. `input` is the window's (hovered, changed_at).
fn wake(
    view: &View,
    idle: &Idle,
    fullscreen: bool,
    (hovered, changed): (bool, Option<Instant>),
    no_peek: bool,
    now: Instant,
) -> Motion {
    let mut m = view.buddy.motion(now);
    let mut at: Vec<Instant> = Vec::new();
    if view.attached.is_none() {
        at.push(
            now + if fullscreen {
                Duration::from_millis(500)
            } else {
                Duration::from_secs(2)
            },
        );
    }
    if let Some(c) = changed {
        if hovered && !view.peek && !no_peek {
            at.push(c + PEEK_DELAY);
        } else if !hovered && view.peek {
            at.push(c + UNPEEK_DELAY);
        }
    }
    at.extend(idle.next_flip(now));
    match view.buddy.state() {
        BuddyState::Done => {
            at.push(now + Duration::from_millis(2600).saturating_sub(view.buddy.age(now)))
        }
        BuddyState::Error => {
            at.push(now + Duration::from_secs(6).saturating_sub(view.buddy.age(now)))
        }
        _ => {}
    }
    // The frame just run at `now` already acted on (or deliberately kept) any
    // earlier deadline, e.g. the unpeek while the cursor sits just beside the
    // pill; waking for it again would be a 1 ms wait on every loop.
    if let Some(t) = at.into_iter().filter(|&t| t >= now).min() {
        m = match m {
            Motion::Still(None) => Motion::Still(Some(t)),
            Motion::Still(Some(b)) => Motion::Still(Some(b.min(t))),
            Motion::Slow if t <= now + Duration::from_millis(33) => Motion::Fast,
            other => other,
        };
    }
    m
}

impl Island {
    /// True when the cursor is over where the rest pill sits, even while the
    /// window is hidden. Lets a tucked island come back when the pointer is
    /// moved to it, not only to the very top edge of the monitor.
    fn cursor_on_pill(&self) -> bool {
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_err() {
            return false;
        }
        let s = self.mon.scale;
        let (bw, bh) = self.view.body_size(s);
        let m = self.mon.rect;
        let x = m.x + (m.w - bw as i32) / 2;
        let y = m.y + (TOP_GAP * s).round() as i32;
        let pad = (10.0 * s) as i32;
        p.x >= x - pad
            && p.x <= x + bw as i32 + pad
            && p.y >= y - pad
            && p.y <= y + bh as i32 + pad
    }

    /// The screen area the glass captures: wide and tall enough for the pill,
    /// the peek panel and an attached palette, with room for the blur to bleed.
    fn glass_rect(&self) -> crate::capture::Rect {
        let s = self.mon.scale;
        let m = self.mon.rect;
        let pad = (30.0 * s).ceil() as i32;
        let w = (self
            .view
            .attached
            .unwrap_or(0.0)
            .max(PEEK_W)
            .max(self.view.w.value)
            * s)
            .ceil() as i32
            + 2 * pad;
        // Tall enough for the peek panel and the agent cards hanging below.
        let h = ((PILL_H + 560.0) * s).ceil() as i32 + 2 * pad;
        let x = m.x + (m.w - w) / 2;
        let y = m.y + (TOP_GAP * s).round() as i32 - pad;
        let left = x.max(m.x);
        let top = y.max(m.y);
        let right = (x + w).min(m.x + m.w);
        let bottom = (y + h).min(m.y + m.h);
        crate::capture::Rect {
            x: left,
            y: top,
            w: (right - left).max(1),
            h: (bottom - top).max(1),
        }
    }

    /// Moves to the monitor under the cursor if it changed.
    fn follow_cursor(&mut self) {
        let m = ui::monitor_under_cursor();
        if m.rect != self.mon.rect || m.scale != self.mon.scale {
            self.mon = m;
            self.fullscreen = fullscreen_on(&self.mon);
            self.dirty = true;
        }
    }
}

// ---------------------------------------------------------------- headless previews

#[cfg(test)]
mod tests {
    use super::*;

    fn settle(v: &mut View, now: Instant) {
        v.layout(now);
        v.label_since = now - Duration::from_secs(1); // label already faded in
        for s in [&mut v.w, &mut v.h, &mut v.reveal, &mut v.alpha, &mut v.flat] {
            s.snap(s.target);
        }
    }

    /// The attached island over a stand-in for the palette below it (same
    /// geometry and tokens as the palette's attached surface), to judge the
    /// seam: one object, header on top, input row and answer below.
    fn composite_attached(v: &mut View, s: f32, now: Instant, below_h: f32) -> Pixmap {
        let body = v.render_body(s, now).unwrap();
        let (bw, bh) = (body.width() as f32, body.height() as f32);
        let pad = (40.0 * s) as u32;
        let ph = (below_h * s).ceil();
        let mut pm =
            Pixmap::new(body.width() + 2 * pad, (bh + ph) as u32 + 2 * pad).unwrap();
        let (a, b) = (Rgba::rgb(0xECEAE5), Rgba::rgb(0xD6DAE2));
        for y in 0..pm.height() {
            let k = y as f32 / pm.height() as f32;
            let pw = pm.width() as f32;
            ui::fill_rect(&mut pm, 0.0, y as f32, pw, 1.0, a.mix(b, k));
        }
        let (ox, oy) = (pad as f32, pad as f32);
        // The palette's shadow covers the whole object.
        ui::shadow(&mut pm, ox, oy, bw, bh + ph, 22.0 * s, s);
        let pp = tiny_skia::PixmapPaint::default();
        let id = tiny_skia::Transform::identity();
        pm.draw_pixmap(pad as i32, pad as i32, body.as_ref(), &pp, id, None);
        // Palette surface: flat top under the header, round bottom.
        let top = oy + bh;
        fill_path(&mut pm, body_path(ox, top, bw, ph, 0.0, 22.0 * s), tokens::INK);
        // Its hairline runs down the sides and round the bottom only.
        stroke_path(
            &mut pm,
            body_path(ox + 0.5, top - 30.0, bw - 1.0, ph + 29.5, 0.0, 21.5 * s),
            tokens::LINE,
        );
        ui::fill_rect(&mut pm, ox + 1.0, top - 31.0, bw - 2.0, 31.0, tokens::INK);
        // Input row: screen chip, placeholder, send button.
        let row = 52.0 * s;
        let mid = top + row / 2.0;
        let chip_h = 34.0 * s;
        ui::fill_rounded(&mut pm, ox + 9.0 * s, mid - chip_h / 2.0, 92.0 * s, chip_h, tokens::RADIUS_CHIP * s, tokens::INK_2);
        let ls = TextStyle::label(s).color(tokens::PAPER);
        let base = (mid + ui::cap_height(&ls) / 2.0).round();
        ui::draw_text_baseline(&mut pm, "screen", ox + 40.0 * s, base, &ls);
        ui::fill_rounded(&mut pm, ox + 15.0 * s, mid - 11.0 * s, 22.0 * s, 22.0 * s, 5.0 * s, Rgba::rgb(0x3A3F48));
        let hs = TextStyle::body(s).size(14.0 * s).color(tokens::MUTE);
        let base = (mid + ui::cap_height(&hs) / 2.0).round();
        ui::draw_text_baseline(&mut pm, "Ask about your screen", ox + 112.0 * s, base, &hs);
        ui::fill_circle(&mut pm, ox + bw - 26.0 * s, mid, 15.0 * s, tokens::INK_2);
        if below_h > 60.0 {
            ui::fill_rect(&mut pm, ox + 18.0 * s, top + row, bw - 36.0 * s, 1.0, tokens::LINE);
            let bs = TextStyle::body(s).color(tokens::PAPER);
            let mut y = top + row + 28.0 * s;
            for line in [
                "The chart shows revenue per quarter; Q3",
                "dips because the EU launch slipped.",
            ] {
                ui::draw_text_baseline(&mut pm, line, ox + 18.0 * s, y, &bs);
                y += 20.0 * s;
            }
        }
        pm
    }

    #[test]
    fn only_when_called_shows_for_the_palette_and_voice() {
        assert!(summoned(true, BuddyState::Idle));
        assert!(summoned(false, BuddyState::Listening(0.3)));
        assert!(summoned(false, BuddyState::Thinking));
        assert!(summoned(false, BuddyState::Streaming));
        assert!(!summoned(false, BuddyState::Idle));
        assert!(!summoned(false, BuddyState::Working(2)), "running agents stay off screen");
        assert!(!summoned(false, BuddyState::Done));
    }

    #[test]
    fn glass_shows_the_screen_behind_the_body() {
        use crate::capture::{MonitorInfo, RawShot, Rect};
        let rect = Rect { x: 0, y: 0, w: 200, h: 200 };
        let mon = MonitorInfo { rect, work: rect, scale: 1.0 };
        let shot = RawShot {
            monitor: mon,
            cursor: (0, 0),
            bgra: vec![255u8; 200 * 200 * 4],
            windows: Vec::new(),
        };
        let bd = crate::surface::Backdrop::new(&shot, 60.0).unwrap();
        let mut pm = Pixmap::new(100, 100).unwrap();
        let path = tiny_skia::PathBuilder::from_rect(
            tiny_skia::Rect::from_xywh(0.0, 0.0, 100.0, 100.0).unwrap(),
        );
        fill_glass(
            &mut pm,
            &path,
            &Glass {
                backdrop: &bd,
                x: 50,
                y: 50,
                opacity: crate::surface::glass_opacity(60.0),
            },
        );
        let px = pm.pixel(50, 50).unwrap();
        let ink = tokens::INK;
        assert!(
            px.red() > ink.r && px.green() > ink.g && px.blue() > ink.b,
            "the blurred screen should show through the ink"
        );
    }

    fn rows() -> Vec<AgentRow> {
        let ago = |s: u64| SystemTime::now() - Duration::from_secs(s);
        vec![
            AgentRow {
                id: "a1".into(),
                name: "Flight prices to Lisbon".into(),
                status: "Comparing 14 fares across three sites".into(),
                kind: AgentStatus::Running,
                started: ago(41),
                unseen: false,
            },
            AgentRow {
                id: "a2".into(),
                name: "Tidy the Downloads folder".into(),
                status: "Wants to delete 212 files".into(),
                kind: AgentStatus::Asking,
                started: ago(320),
                unseen: true,
            },
            AgentRow {
                id: "a3".into(),
                name: "Summarise the Q3 deck".into(),
                status: "Wrote summary.md".into(),
                kind: AgentStatus::Done,
                started: ago(4000),
                unseen: true,
            },
        ]
    }

    /// Composites shadow + body over a backdrop, like the desktop would.
    fn composite(v: &mut View, s: f32, now: Instant, light: bool) -> Pixmap {
        let body = v.render_body(s, now).unwrap();
        let shadow = v.render_shadow(s).unwrap();
        let (side, top, _) = shadow_margins(s);
        let pad = 24;
        let mut pm = Pixmap::new(shadow.width() + 2 * pad, shadow.height() + 2 * pad).unwrap();
        let (a, b) = if light {
            (0xECEAE5, 0xD6DAE2)
        } else {
            (0x282C34, 0x16181C)
        };
        let (a, b) = (Rgba::rgb(a), Rgba::rgb(b));
        for y in 0..pm.height() {
            let k = y as f32 / pm.height() as f32;
            let pw = pm.width() as f32;
            ui::fill_rect(&mut pm, 0.0, y as f32, pw, 1.0, a.mix(b, k));
        }
        let pp = tiny_skia::PixmapPaint::default();
        let id = tiny_skia::Transform::identity();
        pm.draw_pixmap(pad as i32, pad as i32, shadow.as_ref(), &pp, id, None);
        pm.draw_pixmap(
            (pad + side) as i32,
            (pad + top) as i32,
            body.as_ref(),
            &pp,
            id,
            None,
        );
        pm
    }

    #[test]
    fn island_hit_testing_and_layout() {
        let now = Instant::now();
        let mut v = View::new(now);
        settle(&mut v, now);
        assert_eq!(v.h.value, PILL_H);
        assert!(v.w.value >= 56.0 && v.w.value < 80.0, "{}", v.w.value);
        assert_eq!(v.hit(10.0, 10.0), Hit::Pill);
        assert_eq!(v.hit(-1.0, 10.0), Hit::Nothing);

        v.set_agents(rows(), now);
        assert_eq!(v.buddy.state(), BuddyState::Working(1));
        settle(&mut v, now);
        let rest = v.w.value;
        assert!(rest > 80.0 && rest < 140.0, "{rest}");

        v.peek = true;
        settle(&mut v, now);
        assert_eq!(v.w.value, PEEK_W);
        assert_eq!(v.hit(100.0, PILL_H + 7.0 + 10.0), Hit::Agent(0));
        assert_eq!(v.hit(100.0, PILL_H + 7.0 + 3.0 * ROW_H + 10.0), Hit::Ask);
        assert!(v.render_body(1.5, now).is_some());
    }

    /// A resting island must not spin. Runs what `frame` decides (`Idle`, the
    /// view's tick) and what `pace` then waits (`wake`) with time moving on, for
    /// each resting state: with nothing animating no wait under 10 ms repeats
    /// (one can, when a real deadline falls just after a poll), and breathing
    /// never turns into a run of display-rate frames.
    #[test]
    fn island_idle_never_spins() {
        let asking: Vec<AgentRow> = rows()
            .into_iter()
            .filter(|r| r.kind == AgentStatus::Asking)
            .collect();
        let t0 = Instant::now();
        // (name, agents, auto-hide, seconds since the last activity at the start)
        let cases = [
            ("visible-idle", Vec::new(), false, 0),
            ("visible-idle auto-hide", Vec::new(), true, 0),
            ("dimmed", Vec::new(), false, 12),
            ("dimmed auto-hide", Vec::new(), true, 12),
            ("tucked", Vec::new(), true, 5),
            ("working", rows(), false, 0),
            ("working auto-hide", rows(), true, 0),
            ("asking", asking.clone(), false, 0),
            ("asking auto-hide", asking, true, 0),
        ];
        for (name, agents, auto_hide, ago) in cases {
            let mut v = View::new(t0);
            v.set_agents(agents, t0);
            let start = t0 + Duration::from_secs(20);
            let activity = start - Duration::from_secs(ago);
            let (mut now, mut last, mut fast_run, mut short_run) = (start, start, 0, 0);
            while now < start + Duration::from_secs(40) {
                // What `frame` decides…
                let idle = Idle {
                    resting: matches!(v.buddy.state(), BuddyState::Idle | BuddyState::Working(_)),
                    attached: false,
                    hovered: false,
                    peek: false,
                    needs_you: v.agents.iter().any(|a| a.kind == AgentStatus::Asking),
                    auto_hide,
                    activity,
                };
                let awake = idle.awake(now);
                v.buddy.set_awake(awake);
                v.alpha.set(if awake { 1.0 } else { DIM_ALPHA });
                v.tick(now.saturating_duration_since(last).as_secs_f32().min(0.05), now);
                settle(&mut v, now);
                last = now;
                // …and how long `pace` then waits.
                let at = now - start;
                let own = v.buddy.motion(now);
                let paced = wake(&v, &idle, false, (false, None), false, now);
                if !matches!((paced, own), (Motion::Fast, Motion::Slow)) {
                    fast_run = 0;
                }
                let step = match paced {
                    Motion::Fast => {
                        assert!(!matches!(own, Motion::Still(_)), "{name}: Fast with nothing animating at {at:?}");
                        if matches!(own, Motion::Slow) {
                            fast_run += 1;
                            assert!(fast_run < 5, "{name}: breathing stuck at display rate at {at:?}");
                        }
                        Duration::from_millis(16)
                    }
                    Motion::Slow => Duration::from_millis(33),
                    Motion::Still(Some(t)) => {
                        // A real deadline can fall just after a poll; a stale
                        // one asks for the same short wait again and again.
                        let w = t.saturating_duration_since(now);
                        short_run = if w < Duration::from_millis(10) { short_run + 1 } else { 0 };
                        assert!(short_run < 2, "{name}: waits {w:?} again at {at:?}");
                        w
                    }
                    Motion::Still(None) => Duration::from_secs(1),
                };
                if !matches!(paced, Motion::Still(Some(_))) {
                    short_run = 0;
                }
                // The island's loop waits 1 ms past the deadline.
                now += step + Duration::from_millis(1);
            }
        }
    }

    /// `SCREENBUDDY_ISLAND_PNG=<dir> cargo test island` writes previews of the
    /// rest pill, the peek panel and every buddy state.
    #[test]
    fn island_renders_previews() {
        let dir = std::env::var("SCREENBUDDY_ISLAND_PNG").ok();
        let s = 3.0;
        let t0 = Instant::now();
        let mut shots: Vec<(String, Pixmap)> = Vec::new();
        let states: [(&str, BuddyState, f32); 9] = [
            ("idle", BuddyState::Idle, 2.0),
            ("listening-quiet", BuddyState::Listening(0.08), 1.0),
            ("listening-loud", BuddyState::Listening(0.85), 1.0),
            ("thinking", BuddyState::Thinking, 4.3),
            ("thinking-b", BuddyState::Thinking, 4.75),
            ("streaming", BuddyState::Streaming, 7.2),
            ("done", BuddyState::Done, 0.3),
            ("done-settled", BuddyState::Done, 1.8),
            ("error", BuddyState::Error, 1.0),
        ];
        for (name, st, at) in states {
            let mut v = View::new(t0);
            v.set_state(st, t0);
            let now = t0 + Duration::from_secs_f32(at);
            if let BuddyState::Listening(l) = st {
                v.buddy.set(BuddyState::Listening(l), now);
                for _ in 0..200 {
                    v.buddy.tick(0.01, now);
                }
            }
            settle(&mut v, now);
            let pm = composite(&mut v, s, now, true);
            shots.push((format!("state-{name}"), pm));
        }

        let mut v = View::new(t0);
        v.set_agents(rows(), t0);
        let now = t0 + Duration::from_secs(3);
        settle(&mut v, now);
        shots.push(("rest-working".into(), composite(&mut v, s, now, true)));
        shots.push(("rest-working-dark".into(), composite(&mut v, s, now, false)));
        v.peek = true;
        v.hover = Some(Hit::Agent(1));
        settle(&mut v, now);
        shots.push(("peek".into(), composite(&mut v, s, now, true)));
        shots.push(("peek-dark".into(), composite(&mut v, s, now, false)));
        // Mid-grow frame.
        v.peek = false;
        v.layout(now);
        v.peek = true;
        v.layout(now);
        v.w.snap(200.0);
        v.h.snap(120.0);
        v.reveal.snap(0.55);
        shots.push(("peek-mid".into(), composite(&mut v, s, now, true)));

        let mut v = View::new(t0);
        v.peek = true;
        settle(&mut v, now);
        shots.push(("peek-empty".into(), composite(&mut v, s, now, true)));

        // Attached to the palette: the island is its header.
        let mut v = View::new(t0);
        v.set_agents(rows(), t0);
        v.attached = Some(560.0);
        settle(&mut v, now);
        assert_eq!(v.w.value, 560.0);
        assert_eq!(v.bottom_radius(), 0.0);
        shots.push(("attached".into(), composite_attached(&mut v, s, now, 52.0)));
        v.set_state(BuddyState::Thinking, now);
        let later = now + Duration::from_secs_f32(4.3);
        settle(&mut v, later);
        shots.push((
            "attached-answering".into(),
            composite_attached(&mut v, s, later, 140.0),
        ));
        // Mid-morph: widening, corners half flat.
        v.w.snap(300.0);
        v.flat.snap(0.5);
        shots.push(("attached-mid".into(), composite(&mut v, s, later, true)));

        if let Some(dir) = dir {
            let _ = std::fs::create_dir_all(&dir);
            for (name, pm) in shots {
                pm.save_png(std::path::Path::new(&dir).join(format!("{name}.png")))
                    .unwrap();
            }
        }
    }

    /// Live smoke test on the real desktop: `cargo test island_live -- --ignored`.
    #[test]
    #[ignore]
    fn island_live() {
        crate::win::set_dpi_awareness();
        let h = start();
        let step = |st: BuddyState, ms: u64| {
            h.set_state(st);
            std::thread::sleep(Duration::from_millis(ms));
        };
        step(BuddyState::Idle, 1500);
        for i in 0..40 {
            step(BuddyState::Listening((i as f32 * 0.4).sin().abs()), 50);
        }
        step(BuddyState::Thinking, 2500);
        step(BuddyState::Streaming, 2500);
        step(BuddyState::Done, 3000);
        h.set_agents(rows());
        std::thread::sleep(Duration::from_millis(2000));
        let a = h.anchor();
        assert!(a.right > a.left, "island never showed");
        step(BuddyState::Error, 2500);
        std::thread::sleep(Duration::from_secs(
            std::env::var("ISLAND_LIVE_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(2),
        ));
    }
}
