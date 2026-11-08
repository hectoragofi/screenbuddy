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
    visible: Option<bool>,
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

    /// Shows or hides the island (the `[island] enabled` setting).
    pub fn set_visible(&self, on: bool) {
        self.shared.inbox.lock().unwrap().visible = Some(on);
        self.wake();
    }

    /// Screen rect (physical px) of the island's body as it is drawn now, so
    /// cards can hang from it. Empty while hidden.
    pub fn anchor(&self) -> RECT {
        *self.shared.anchor.lock().unwrap()
    }
}

static HANDLE: OnceLock<IslandHandle> = OnceLock::new();

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
const PILL_H: f32 = 36.0;
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
        let peek = self.peek;
        self.w.set(if peek { PEEK_W } else { self.rest_w(now) });
        self.h.set(if peek { self.rows_h() } else { PILL_H });
        self.reveal.set(if peek { 1.0 } else { 0.0 });
    }

    fn tick(&mut self, dt: f32) -> bool {
        let mut moving = false;
        for s in [&mut self.w, &mut self.h, &mut self.reveal, &mut self.alpha] {
            moving |= s.tick(dt);
        }
        self.buddy.tick(dt);
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

    /// Body size in physical px.
    fn body_size(&self, s: f32) -> (u32, u32) {
        (
            (self.w.value * s).ceil().max(1.0) as u32,
            (self.h.value * s).ceil().max(1.0) as u32,
        )
    }

    fn render_body(&mut self, s: f32, now: Instant) -> Option<Pixmap> {
        let (bw, bh) = self.body_size(s);
        let mut pm = Pixmap::new(bw, bh)?;
        let (w, h) = (bw as f32, bh as f32);
        let r = self.radius() * s;
        ui::fill_rounded(&mut pm, 0.0, 0.0, w, h, r, tokens::INK);
        ui::stroke_rounded(&mut pm, 0.0, 0.0, w, h, r, 1.0, tokens::LINE);

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
            ("screenbuddy".to_string(), tokens::MUTE, reveal)
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
            ui::rounded_rect_path(0.0, 0.0, w, h, r),
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
    enabled: bool,
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
        enabled: true,
        fullscreen: false,
        shown: Spring::new(0.0).with(160.0, 26.0),
        activity: now,
        last: now,
        dirty: true,
        shadow_key: None,
        shadow_alpha: -1.0,
        no_peek: false,
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
        if let Some(on) = inbox.visible {
            self.enabled = on;
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
                    self.follow_cursor();
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
        let (hovered, changed, mouse, clicks, fg) = INPUT.with(|i| {
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
        if fg {
            self.follow_cursor();
            let full = fullscreen_on(&self.mon);
            if full != self.fullscreen {
                self.fullscreen = full;
                self.dirty = true;
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
        let awake = !resting
            || hovered
            || self.view.peek
            || now.saturating_duration_since(self.activity) < DIM_AFTER;
        self.view.buddy.set_awake(awake);
        self.view.alpha.set(if awake { 1.0 } else { DIM_ALPHA });
        let hide = !self.enabled || (self.fullscreen && resting);
        self.shown.set(if hide { 0.0 } else { 1.0 });
        self.view.layout(now);

        // ---- animate + draw
        let moving = self.view.tick(dt) | self.shown.tick(dt);
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
            *self.shared.anchor.lock().unwrap() = RECT::default();
            return;
        }
        self.draw(now, opacity);
    }

    fn draw(&mut self, now: Instant, opacity: f32) {
        let s = self.mon.scale;
        let Some(pm) = self.view.render_body(s, now) else {
            return;
        };
        let m = self.mon.rect;
        let (bw, bh) = (pm.width() as i32, pm.height() as i32);
        let x = m.x + (m.w - bw) / 2;
        let y = m.y + (TOP_GAP * s).round() as i32;
        let first = !self.body.visible();
        self.body.present(&pm, x, y, opacity);
        *self.shared.anchor.lock().unwrap() = RECT {
            left: x,
            top: y,
            right: x + bw,
            bottom: y + bh,
        };

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
            || !self.shown.settled()
        {
            return Motion::Fast;
        }
        if !self.body.visible() {
            return Motion::Still(None);
        }
        let mut m = self.view.buddy.motion(now);
        // Wake for the pending peek / unpeek, the dim, and the Done/Error fallback.
        let mut at: Vec<Instant> = Vec::new();
        let (hovered, changed) = INPUT.with(|i| {
            let i = i.borrow();
            (i.hovered, i.changed_at)
        });
        if let Some(c) = changed {
            if hovered && !self.view.peek && !self.no_peek {
                at.push(c + PEEK_DELAY);
            } else if !hovered && self.view.peek {
                at.push(c + UNPEEK_DELAY);
            }
        }
        if self.view.alpha.target == 1.0
            && matches!(
                self.view.buddy.state(),
                BuddyState::Idle | BuddyState::Working(_)
            )
            && !hovered
        {
            at.push(self.activity + DIM_AFTER);
        }
        match self.view.buddy.state() {
            BuddyState::Done => {
                at.push(now + Duration::from_millis(2600).saturating_sub(self.view.buddy.age(now)))
            }
            BuddyState::Error => {
                at.push(now + Duration::from_secs(6).saturating_sub(self.view.buddy.age(now)))
            }
            _ => {}
        }
        if let Some(t) = at.into_iter().min() {
            m = match m {
                Motion::Still(None) => Motion::Still(Some(t)),
                Motion::Still(Some(b)) => Motion::Still(Some(b.min(t))),
                Motion::Slow if t <= now + Duration::from_millis(33) => Motion::Fast,
                other => other,
            };
        }
        m
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
        for s in [&mut v.w, &mut v.h, &mut v.reveal, &mut v.alpha] {
            s.snap(s.target);
        }
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
                    v.buddy.tick(0.01);
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
