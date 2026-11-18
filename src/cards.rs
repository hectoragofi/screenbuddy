//! Agent cards: live activities hanging from the island. At rest each agent
//! is one calm line (status dot, name, what it is doing, elapsed); hovering a
//! card opens it in place (plan, a follow-up box, its actions) and it closes
//! again shortly after the cursor leaves. Only an approval or a question opens
//! by itself. Finished cards shrink to one line ("Flight prices · Open") and
//! fade after a few seconds; at most three show, the rest fold into "+N". A
//! fact saved to the brain shows as a short "Remembered · …" line with Undo.
//! With `[island] presence = "hotkey"` (the default) running agents keep
//! nothing on screen: only asks, finished agents and the brain line show
//! unsummoned. Holding Alt over a card speaks a follow-up to that agent
//! (`keys::set_cursor_target` + `quick`).
//!
//! Runs on its own thread with two layered windows (`screenbuddy.cards` for
//! the cards, click-through `screenbuddy.cards-shadow` under it), rendered with
//! tiny-skia on the island's glass ([`crate::island::glass`], or its own copy
//! while the island is away), so cards and island read as one material. It
//! draws only while something moves.
//!
//! The card window is non-activating, except while its follow-up box has
//! focus or a card was clicked for the keyboard (Enter = the card's primary
//! action, Esc = close / dismiss); losing focus turns it off again.
//!
//! API: [`start`] (once), [`open`] (expand an agent's card, e.g. from the
//! island), [`target_at`] (the agent whose card is under a screen point),
//! [`brain_saved`] (the "Remembered" line), [`set_presence`].

use crate::agents::drive::DriveEvent;
use crate::agents::{self, Agent, AgentEvent, Answer, AskKind, Question, Status};
use crate::brain::Fact;
use crate::config::Presence;
use crate::caption::{self, Block, Span};
use crate::ui::tokens::{self, Rgba};
use crate::ui::{self, Face, Layer, MonitorInfo, Spring, TextStyle};
use std::cell::RefCell;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tiny_skia::{Pixmap, PixmapPaint, Transform};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::DwmFlush;
use windows::Win32::System::Threading::{GetCurrentThreadId, INFINITE};
use windows::Win32::UI::Controls::WM_MOUSELEAVE;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

// ---------------------------------------------------------------- public API

enum Msg {
    Event(String, AgentEvent),
    Open(String),
    /// The user told the buddy a fact: show who reads it.
    Brain(Fact),
    /// The island moved: re-check where the stack hangs.
    Wake,
    Refresh,
}

static TID: AtomicU32 = AtomicU32::new(0);
static INBOX: Mutex<Vec<Msg>> = Mutex::new(Vec::new());
/// Screen rects (physical px) of the cards drawn now, for `target_at`.
static TARGETS: Mutex<Vec<(RECT, String)>> = Mutex::new(Vec::new());
const WM_WAKE: u32 = WM_APP + 9;

fn post(m: Msg) {
    INBOX.lock().unwrap().push(m);
    let t = TID.load(Ordering::Relaxed);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

/// Starts the cards thread and hooks it up to agents, the Alt-hold cursor
/// target, caption notices and the island (once).
pub fn start() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let spawned = std::thread::Builder::new()
            .name("screenbuddy-cards".into())
            .spawn(move || {
                unsafe {
                    let mut m = MSG::default();
                    let _ = PeekMessageW(&mut m, None, 0, 0, PM_NOREMOVE);
                    TID.store(GetCurrentThreadId(), Ordering::Relaxed);
                }
                let _ = ready_tx.send(());
                if let Err(e) = run() {
                    crate::config::log(&format!("cards: {e:#}"));
                }
            });
        if spawned.is_err() {
            return;
        }
        let _ = ready_rx.recv_timeout(Duration::from_secs(2));
        let rx = agents::subscribe();
        let _ = std::thread::Builder::new()
            .name("screenbuddy-cards-events".into())
            .spawn(move || {
                for (id, ev) in rx {
                    post(Msg::Event(id, ev));
                }
            });
        crate::keys::set_cursor_target(target_at);
        caption::set_notice_sink(|id| open(&id));
        crate::island::on_anchor_change(|| post(Msg::Wake));
    });
}

/// Shows and expands this agent's card (the island's agent rows call this).
pub fn open(id: &str) {
    start();
    post(Msg::Open(id.to_string()));
}

/// Shows "Remembered · <fact>" with Undo for a few seconds; hovering it shows
/// the running agents that read it and See all (Settings → Brain).
pub fn brain_saved(f: &Fact) {
    start();
    post(Msg::Brain(f.clone()));
}

/// The card id of a brain fact (for caption notices that open it).
pub fn brain_card_id(f: &Fact) -> String {
    format!("{BRAIN_PREFIX}{}", f.id)
}

const BRAIN_PREFIX: &str = "brain:";

/// `[island] presence`: 0 = hotkey, 1 = auto-hide, 2 = always.
static PRESENCE: AtomicU8 = AtomicU8::new(0);

/// Which cards show unsummoned (from config `[island] presence`): with
/// "hotkey" only asks, finished agents and the brain line.
pub fn set_presence(p: Presence) {
    let v = match p {
        Presence::Hotkey => 0,
        Presence::AutoHide => 1,
        Presence::Always => 2,
    };
    if PRESENCE.swap(v, Ordering::Relaxed) != v {
        refresh();
    }
}

fn presence() -> Presence {
    match PRESENCE.load(Ordering::Relaxed) {
        1 => Presence::AutoHide,
        2 => Presence::Always,
        _ => Presence::Hotkey,
    }
}

/// Redraws the cards (e.g. a spoken follow-up started or stopped).
pub fn refresh() {
    if TID.load(Ordering::Relaxed) != 0 {
        post(Msg::Refresh);
    }
}

/// The agent whose card is under screen point `p` (physical px), if any.
pub fn target_at(p: (i32, i32)) -> Option<String> {
    TARGETS
        .lock()
        .unwrap()
        .iter()
        .find(|(r, _)| p.0 >= r.left && p.0 < r.right && p.1 >= r.top && p.1 < r.bottom)
        .map(|(_, id)| id.clone())
}


// ---------------------------------------------------------------- layout (logical px)

const CARD_W: f32 = 340.0;
const PAD: f32 = 16.0;
/// The status dot's centre, and where text starts right of it.
const DOT_X: f32 = 22.0;
const TEXT_X: f32 = 38.0;
/// One line: a whole card at rest, the header row of an open one.
const ROW_H: f32 = 44.0;
/// A one-line card is a pill, like the island at rest.
const RADIUS: f32 = 22.0;
/// Between cards in the stack.
const STACK_GAP: f32 = 8.0;
/// Below the island's pill.
const HANG: f32 = 10.0;
const LINE: f32 = 18.0;
const BTN_H: f32 = 32.0;
/// Buttons inside the one-line row (concentric with its corners).
const ROW_BTN_H: f32 = 28.0;
/// Quiet text buttons (Stop, Show desk, Open).
const QUIET_H: f32 = 30.0;
const INPUT_H: f32 = 36.0;
const CHIP_H: f32 = 26.0;
const RESULT_MAX_H: f32 = 132.0;
const MORE_H: f32 = 28.0;
const MAX_VISIBLE: usize = 3;
const PLAN_ROWS: usize = 5;
/// An open card stays open this long after the cursor leaves it.
const HOVER_LINGER: Duration = Duration::from_millis(450);
/// A finished card fades this long after it finished, unless hovered.
const FINISHED_FOR: Duration = Duration::from_secs(8);
/// The "Remembered" line's life.
const NOTICE_FOR: Duration = Duration::from_secs(4);
/// Left alone again after a hover, a timed card waits at least this long.
const AFTER_HOVER: Duration = Duration::from_secs(3);
/// A card opened from the island stays open this long without a hover.
const OPENED_FOR: Duration = Duration::from_secs(5);
const SHADOW_SIDE: f32 = 28.0;
const SHADOW_TOP: f32 = 12.0;
const SHADOW_BELOW: f32 = 40.0;

// Text on the dark glass, and two white tints for controls. Colour means
// status only in the small dot; the accent (SIGNAL) marks live and focus.
const TEXT: Rgba = tokens::PAPER;
const TEXT_2: Rgba = Rgba::rgb(0x9D9A93);
const TEXT_3: Rgba = Rgba::rgb(0x75726C);
const FILL: Rgba = Rgba::rgba(0xFFFFFF, 24);
const FILL_HOVER: Rgba = Rgba::rgba(0xFFFFFF, 40);

// ---------------------------------------------------------------- view (pure data)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Btn {
    Cancel,
    Stop,
    Retry,
    Show,
    /// Open the agent in the palette thread.
    Palette,
    Always,
    Once,
    Deny,
    Send,
    /// Brain line: see all facts (Settings → Brain).
    Brain,
    /// Brain line: take the save back.
    Undo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    /// Anywhere else on the card (a click takes the keyboard, if it can).
    Card,
    Btn(Btn),
    File(usize),
    Input,
    Mic,
    Result,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Pending,
    Running,
    Approval,
    Questions,
    Done,
    Failed,
    Stopped,
}

fn kind(a: &Agent) -> Kind {
    match (&a.ask, a.status) {
        (Some(q), s) if s.is_active() => match q.kind {
            AskKind::Approval => Kind::Approval,
            AskKind::Questions => Kind::Questions,
        },
        (_, Status::Pending) => Kind::Pending,
        (_, Status::Running) => Kind::Running,
        (_, Status::Done) => Kind::Done,
        (_, Status::Failed) => Kind::Failed,
        (_, Status::Stopped) => Kind::Stopped,
    }
}

/// Everything one card shows. Pure data, so it renders headlessly.
#[derive(Clone, Debug, PartialEq)]
struct CardView {
    agent: Agent,
    expanded: bool,
    hover: Option<Hit>,
    /// The follow-up / answer box has the keyboard.
    focused: bool,
    /// The card itself has the keyboard (Enter = its primary action).
    keyed: bool,
    draft: String,
    /// Unix seconds now, for the elapsed readout.
    now: u64,
    /// Seconds left in the cancel window (Pending).
    pending_left: f32,
    scroll: f32,
    mic: bool,
    /// Apeiron is there, so the agent has a desk to show.
    desk: bool,
    drive: Drive,
    /// A "Remembered" line instead of an agent's card.
    brain: Option<BrainView>,
}

/// A fact the user just saved and the agents that read it.
#[derive(Clone, Debug, PartialEq)]
struct BrainView {
    fact: Fact,
    readers: Vec<String>,
    /// Undo was pressed: the line says "Forgotten" on its way out.
    undone: bool,
}

/// Where the agent is with the user's mouse and keyboard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Drive {
    #[default]
    Off,
    /// Asking to start (the Approval ask is about driving).
    Asking,
    Driving,
    /// The user took over; asking to continue.
    Paused,
}

impl Drive {
    fn on(self, ev: &DriveEvent) -> Self {
        match ev {
            DriveEvent::Requested => Drive::Asking,
            DriveEvent::Started | DriveEvent::Resumed => Drive::Driving,
            DriveEvent::Paused => Drive::Paused,
            DriveEvent::Ended => Drive::Off,
        }
    }
}

/// A rendered card: content only (the surface is drawn per frame, so the
/// card can animate its height without re-rendering).
struct Drawn {
    pm: Pixmap,
    /// Natural height, logical px.
    h: f32,
    hits: Vec<(f32, f32, f32, f32, Hit)>,
    max_scroll: f32,
}

/// Names: Geist Medium.
fn title_style(s: f32) -> TextStyle {
    TextStyle::body(s).face(Face::SansMedium).size(13.5 * s)
}

fn body_style(s: f32) -> TextStyle {
    TextStyle::body(s)
}

/// What it's doing, next to the name.
fn sec_style(s: f32) -> TextStyle {
    TextStyle::body(s).color(TEXT_2)
}

fn small_style(s: f32) -> TextStyle {
    TextStyle::body(s).size(12.5 * s).color(TEXT_2)
}

/// Elapsed, counts, commands: small and quiet.
fn meta_style(s: f32) -> TextStyle {
    TextStyle::body(s).size(12.0 * s).color(TEXT_3)
}

fn btn_style(s: f32) -> TextStyle {
    TextStyle::body(s).face(Face::SansMedium)
}

/// Logical width of `t` in `st` (built at scale `s`).
fn tw(t: &str, st: &TextStyle, s: f32) -> f32 {
    ui::text_width(t, st) / s
}

/// The baseline that optically centres `st`'s capitals on `mid` (logical).
fn base(mid: f32, st: &TextStyle, s: f32) -> f32 {
    mid + ui::cap_height(st) / s / 2.0
}

/// Draws in logical px onto an optional pixmap (None = measure only) and
/// records hit rects.
struct Pen<'a> {
    pm: Option<&'a mut Pixmap>,
    s: f32,
    hits: Vec<(f32, f32, f32, f32, Hit)>,
}

impl Pen<'_> {
    /// Text with its line box's top at `y`; returns the logical width.
    fn text(&mut self, t: &str, x: f32, y: f32, st: &TextStyle) -> f32 {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::draw_text(pm, t, (x * s).round(), y * s, st);
        }
        tw(t, st, s)
    }

    /// Text optically centred on `mid` (cap height).
    fn text_mid(&mut self, t: &str, x: f32, mid: f32, st: &TextStyle) -> f32 {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            let base = (mid * s + ui::cap_height(st) / 2.0).round();
            ui::draw_text_baseline(pm, t, (x * s).round(), base, st);
        }
        tw(t, st, s)
    }

    /// Text on the baseline `base`; returns the logical width.
    fn text_base(&mut self, t: &str, x: f32, base: f32, st: &TextStyle) -> f32 {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::draw_text_baseline(pm, t, (x * s).round(), (base * s).round(), st);
        }
        tw(t, st, s)
    }

    /// A rounded outline `width` logical px thick, inside the shape.
    #[allow(clippy::too_many_arguments)]
    fn outline(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, width: f32, c: Rgba) {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::stroke_rounded(pm, x * s, y * s, w * s, h * s, r * s, width * s, c);
        }
    }

    /// One body line in a `LINE`-tall row starting at `y`.
    fn line(&mut self, t: &str, x: f32, y: f32, st: &TextStyle) -> f32 {
        let lh = ui::line_height(st) / self.s;
        self.text(t, x, y + (LINE - lh) / 2.0, st)
    }

    fn rounded(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::fill_rounded(pm, x * s, y * s, w * s, h * s, r * s, c);
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgba) {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::fill_circle(pm, cx * s, cy * s, r * s, c);
        }
    }

    fn rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::fill_rect(pm, x * s, y * s, w * s, h * s, c);
        }
    }

    /// A polyline stroke (logical points).
    fn poly(&mut self, pts: &[(f32, f32)], width: f32, c: Rgba) {
        let s = self.s;
        let Some(pm) = self.pm.as_deref_mut() else {
            return;
        };
        let mut pb = tiny_skia::PathBuilder::new();
        for (i, (x, y)) in pts.iter().enumerate() {
            if i == 0 {
                pb.move_to(x * s, y * s);
            } else {
                pb.line_to(x * s, y * s);
            }
        }
        if let Some(p) = pb.finish() {
            let mut paint = tiny_skia::Paint::default();
            paint.set_color(c.skia());
            paint.anti_alias = true;
            let st = tiny_skia::Stroke {
                width: width * s,
                line_cap: tiny_skia::LineCap::Round,
                line_join: tiny_skia::LineJoin::Round,
                ..Default::default()
            };
            pm.stroke_path(&p, &paint, &st, Transform::identity(), None);
        }
    }

    /// An arc from 12 o'clock, clockwise over `frac` of the circle.
    fn arc(&mut self, cx: f32, cy: f32, r: f32, frac: f32, width: f32, c: Rgba) {
        let n = ((48.0 * frac).ceil() as usize).max(2);
        let pts: Vec<(f32, f32)> = (0..=n)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2
                    + std::f32::consts::TAU * frac.clamp(0.0, 1.0) * i as f32 / n as f32;
                (cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        self.poly(&pts, width, c);
    }

    fn hit(&mut self, x: f32, y: f32, w: f32, h: f32, hit: Hit) {
        self.hits.push((x, y, w, h, hit));
    }
}

fn elapsed_label(secs: u64) -> String {
    if secs >= 3600 {
        format!("{}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
    } else {
        format!("{}:{:02}", secs / 60, secs % 60)
    }
}

/// Words of a line for loose matching: lower-case, 4+ letters, first 4 chars
/// (so "searching" meets "search", "listing" meets "list").
fn stems(s: &str) -> HashSet<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(|w| w.to_lowercase().chars().take(4).collect())
        .collect()
}

/// Which plan item the agent is on: for each step line, the plan item (at or
/// after the current one) that it covers best, if it covers at least half of
/// that item's words. Never moves backwards.
fn plan_progress(plan: &[String], steps: &[String]) -> usize {
    let items: Vec<HashSet<String>> = plan.iter().map(|p| stems(p)).collect();
    let mut cur = 0;
    for st in steps {
        let t = stems(st);
        let mut best = (0.5, None);
        for (i, p) in items.iter().enumerate().skip(cur) {
            if p.is_empty() {
                continue;
            }
            let score = p.intersection(&t).count() as f32 / p.len() as f32;
            if score >= best.0 {
                best = (score, Some(i));
            }
        }
        if let Some(i) = best.1 {
            cur = i;
        }
    }
    cur.min(plan.len().saturating_sub(1))
}

// ---- markdown-lite

#[derive(Clone, Copy, PartialEq)]
enum Run {
    Plain,
    Bold,
    Code,
}

#[derive(Default)]
struct MdLine {
    indent: f32,
    marker: Option<String>,
    runs: Vec<(String, Run)>,
    code_block: bool,
    gap: bool,
}

impl MdLine {
    fn h(&self) -> f32 {
        if self.gap { 7.0 } else { LINE }
    }
}

fn run_style(r: Run, s: f32) -> TextStyle {
    match r {
        Run::Plain => body_style(s),
        Run::Bold => body_style(s).face(Face::SansMedium),
        Run::Code => TextStyle::mono(s).size(12.0 * s).color(TEXT.mix(TEXT_2, 0.3)),
    }
}

fn wrap_runs(spans: &[Span], width: f32, indent: f32, marker: Option<String>) -> Vec<MdLine> {
    let mut lines = vec![MdLine {
        indent,
        marker,
        ..Default::default()
    }];
    let avail = width - indent;
    let mut x = 0.0;
    let push = |l: &mut MdLine, t: &str, r: Run| match l.runs.last_mut() {
        Some((s, rr)) if *rr == r => s.push_str(t),
        _ => l.runs.push((t.to_string(), r)),
    };
    for sp in spans {
        let r = if sp.code {
            Run::Code
        } else if sp.bold {
            Run::Bold
        } else {
            Run::Plain
        };
        let st = run_style(r, 1.0);
        // Words with their trailing space.
        let mut words: Vec<String> = Vec::new();
        let mut w = String::new();
        for c in sp.text.chars() {
            w.push(c);
            if c == ' ' {
                words.push(std::mem::take(&mut w));
            }
        }
        if !w.is_empty() {
            words.push(w);
        }
        for word in words {
            let ww = ui::text_width(word.trim_end(), &st);
            if x > 0.0 && x + ww > avail {
                lines.push(MdLine {
                    indent,
                    ..Default::default()
                });
                x = 0.0;
            }
            let word = if x == 0.0 {
                word.trim_start().to_string()
            } else {
                word
            };
            if ui::text_width(word.trim_end(), &st) <= avail - x {
                x += ui::text_width(&word, &st);
                push(lines.last_mut().unwrap(), &word, r);
                continue;
            }
            // Wider than a line: break by characters.
            for c in word.chars() {
                let cw = ui::text_width(&c.to_string(), &st);
                if x + cw > avail && x > 0.0 {
                    lines.push(MdLine {
                        indent,
                        ..Default::default()
                    });
                    x = 0.0;
                }
                push(lines.last_mut().unwrap(), &c.to_string(), r);
                x += cw;
            }
        }
    }
    lines
}

fn md_lines(md: &str, width: f32) -> Vec<MdLine> {
    let mut out = Vec::new();
    for b in caption::parse(md) {
        match b {
            Block::Para(sp) => out.extend(wrap_runs(&sp, width, 0.0, None)),
            Block::Item(m, sp) => {
                let ind = ui::text_width(&m, &body_style(1.0)) + 8.0;
                out.extend(wrap_runs(&sp, width, ind, Some(m)));
            }
            Block::Code(t) => {
                for l in t.lines() {
                    let sp = [Span {
                        text: l.to_string(),
                        bold: false,
                        code: true,
                    }];
                    for mut ml in wrap_runs(&sp, width - 8.0, 8.0, None) {
                        ml.code_block = true;
                        out.push(ml);
                    }
                }
            }
            Block::Gap => out.push(MdLine {
                gap: true,
                ..Default::default()
            }),
        }
    }
    out
}

fn draw_md(pen: &mut Pen, lines: &[MdLine], x: f32, mut y: f32, width: f32) {
    let s = pen.s;
    for l in lines {
        if l.gap {
            y += l.h();
            continue;
        }
        if l.code_block {
            pen.rect(x, y, width, LINE, FILL.fade(0.6));
            pen.rect(x, y, 2.0, LINE, FILL_HOVER);
        }
        if let Some(m) = &l.marker {
            pen.line(m, x, y, &body_style(s).color(TEXT_2));
        }
        let mut lx = x + l.indent;
        for (t, r) in &l.runs {
            lx += pen.line(t, lx, y, &run_style(*r, s));
        }
        y += l.h();
    }
}


// ---- pieces

#[derive(Clone, Copy, PartialEq)]
enum Look {
    /// The one thing to do: a light pill.
    Primary,
    /// A tinted pill.
    Secondary,
    /// Text until hovered.
    Quiet,
}

fn button_w(label: &str, ring: bool, s: f32) -> f32 {
    tw(label, &btn_style(s), s) + if ring { 18.0 } else { 0.0 } + 28.0
}

/// A pill button at (x, y); `w` None = fit the label. `ring` = seconds left
/// on a countdown ring, `focus` = Enter presses it. Returns its width.
#[allow(clippy::too_many_arguments)]
fn button(
    pen: &mut Pen,
    x: f32,
    y: f32,
    w: Option<f32>,
    h: f32,
    label: &str,
    look: Look,
    btn: Btn,
    v: &CardView,
    ring: Option<f32>,
    focus: bool,
) -> f32 {
    let s = pen.s;
    let hovered = v.hover == Some(Hit::Btn(btn));
    let ls = btn_style(s);
    let lw = tw(label, &ls, s);
    let ring_w = if ring.is_some() { 18.0 } else { 0.0 };
    let w = w.unwrap_or(lw + ring_w + 28.0);
    let r = h / 2.0;
    let fg = match look {
        Look::Primary => {
            let c = if hovered { Rgba::rgb(0xFFFFFF) } else { TEXT };
            pen.rounded(x, y, w, h, r, c);
            tokens::INK
        }
        Look::Secondary => {
            pen.rounded(x, y, w, h, r, if hovered { FILL_HOVER } else { FILL });
            TEXT
        }
        Look::Quiet => {
            if hovered {
                pen.rounded(x, y, w, h, r, FILL);
                TEXT
            } else {
                TEXT_2
            }
        }
    };
    if focus {
        pen.outline(x - 3.0, y - 3.0, w + 6.0, h + 6.0, r + 3.0, 1.5, tokens::SIGNAL.fade(0.55));
    }
    let mut cx = x + (w - lw - ring_w) / 2.0;
    let mid = y + h / 2.0;
    if let Some(left) = ring {
        let rc = cx + 6.0;
        let whole = agents::PENDING_WINDOW.as_secs_f32().max(1.0);
        pen.arc(rc, mid, 5.5, 1.0, 1.4, FILL_HOVER);
        pen.arc(rc, mid, 5.5, (left / whole).clamp(0.0, 1.0), 1.6, tokens::SIGNAL);
        cx += ring_w;
    }
    pen.text_mid(label, cx, mid, &ls.color(fg));
    pen.hit(x, y, w, h, Hit::Btn(btn));
    w
}

/// The follow-up / answer box with its mic (or send, once there is text).
fn input_box(pen: &mut Pen, v: &CardView, y: f32, placeholder: &str) {
    let s = pen.s;
    let (x, w) = (PAD, CARD_W - 2.0 * PAD);
    let r = INPUT_H / 2.0;
    let hovered = v.hover == Some(Hit::Input);
    pen.rounded(x, y, w, INPUT_H, r, if v.focused || hovered { FILL_HOVER.fade(0.75) } else { FILL.fade(0.75) });
    if v.focused {
        pen.outline(x, y, w, INPUT_H, r, 1.5, tokens::SIGNAL.fade(0.45));
    }
    let btn_d = 26.0;
    let bx = x + w - 5.0 - btn_d;
    let by = y + (INPUT_H - btn_d) / 2.0;
    let text_x = x + 15.0;
    let room = bx - 8.0 - text_x;
    let bs = body_style(s);
    let mid = y + INPUT_H / 2.0;
    let b = base(mid, &bs, s);
    if v.draft.is_empty() {
        let ph = if v.mic { "Listening…" } else { placeholder };
        let c = if v.mic { tokens::SIGNAL } else { TEXT_3 };
        pen.text_base(ph, text_x, b, &bs.color(c));
        if v.focused {
            pen.rect(text_x - 1.5, mid - 8.0, 1.5, 16.0, tokens::SIGNAL);
        }
    } else {
        // Keep the end in view while typing.
        let mut t: Vec<char> = v.draft.chars().collect();
        let mut shown: String = t.iter().collect();
        let mut cut = false;
        while tw(&shown, &bs, s) > room - 10.0 && !t.is_empty() {
            t.remove(0);
            shown = t.iter().collect();
            cut = true;
        }
        let shown = if cut { format!("…{shown}") } else { shown };
        let w = pen.text_base(&shown, text_x, b, &bs);
        if v.focused {
            pen.rect(text_x + w + 1.5, mid - 8.0, 1.5, 16.0, tokens::SIGNAL);
        }
    }
    pen.hit(x, y, bx - x - 2.0, INPUT_H, Hit::Input);

    // Mic, or send when there's something to send.
    let (cx, cy) = (bx + btn_d / 2.0, by + btn_d / 2.0);
    if !v.draft.trim().is_empty() {
        let hov = v.hover == Some(Hit::Btn(Btn::Send));
        pen.circle(cx, cy, btn_d / 2.0, if hov { Rgba::rgb(0xFFFFFF) } else { TEXT });
        pen.poly(&[(cx, cy + 5.0), (cx, cy - 5.0)], 1.7, tokens::INK);
        pen.poly(&[(cx - 4.0, cy - 1.0), (cx, cy - 5.0), (cx + 4.0, cy - 1.0)], 1.7, tokens::INK);
        pen.hit(bx - 2.0, y, btn_d + 7.0, INPUT_H, Hit::Btn(Btn::Send));
    } else {
        let hov = v.hover == Some(Hit::Mic);
        let (bg, fg) = if v.mic {
            (tokens::SIGNAL, tokens::INK)
        } else if hov {
            (FILL, TEXT)
        } else {
            (Rgba::rgba(0, 0), TEXT_2)
        };
        pen.circle(cx, cy, btn_d / 2.0, bg);
        // Capsule + cradle + stem.
        pen.rounded(cx - 2.6, cy - 6.0, 5.2, 8.6, 2.6, fg);
        let cradle: Vec<(f32, f32)> = (0..=12)
            .map(|i| {
                let a = std::f32::consts::PI * i as f32 / 12.0;
                (cx + 4.6 * a.cos(), cy - 0.6 + 4.2 * a.sin())
            })
            .collect();
        pen.poly(&cradle, 1.3, fg);
        pen.poly(&[(cx, cy + 3.6), (cx, cy + 6.2)], 1.3, fg);
        pen.hit(bx - 2.0, y, btn_d + 7.0, INPUT_H, Hit::Mic);
    }
}

/// The agent's files as chips from `x0`; the rest fold into "+N".
fn file_chips(pen: &mut Pen, v: &CardView, x0: f32, mut y: f32) -> f32 {
    let s = pen.s;
    let files = &v.agent.files;
    if files.is_empty() {
        return y;
    }
    let right = CARD_W - PAD;
    let es = meta_style(s).size(10.0 * s);
    let ns = body_style(s).size(12.5 * s);
    let mut x = x0;
    let mut rows = 1;
    for (i, f) in files.iter().enumerate() {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| f.display().to_string());
        let (stem, ext) = match name.rsplit_once('.') {
            Some((a, b)) if !a.is_empty() && b.len() <= 4 => (a.to_string(), b.to_lowercase()),
            _ => (name.clone(), if f.is_dir() { "dir".into() } else { String::new() }),
        };
        let ew = if ext.is_empty() { 0.0 } else { tw(&ext, &es, s) + 6.0 };
        let stem = ui::ellipsize(&stem, &ns, 140.0 * s);
        let w = 11.0 + ew + tw(&stem, &ns, s) + 11.0;
        let rest = files.len() - i;
        let more_w = 40.0;
        if x > x0 && x + w > right - if rest > 1 && rows == 2 { more_w } else { 0.0 } {
            if rows == 2 {
                // Out of room: the rest go into "+N" (the palette has them all).
                let label = format!("+{rest}");
                let hov = v.hover == Some(Hit::Btn(Btn::Palette));
                pen.rounded(x, y, more_w - 6.0, CHIP_H, CHIP_H / 2.0, if hov { FILL_HOVER } else { FILL });
                let lw = tw(&label, &ns, s);
                pen.text_mid(&label, x + (more_w - 6.0 - lw) / 2.0, y + CHIP_H / 2.0, &ns.color(TEXT_2));
                pen.hit(x, y, more_w - 6.0, CHIP_H, Hit::Btn(Btn::Palette));
                break;
            }
            rows += 1;
            x = x0;
            y += CHIP_H + 6.0;
        }
        let hov = v.hover == Some(Hit::File(i));
        pen.rounded(x, y, w, CHIP_H, CHIP_H / 2.0, if hov { FILL_HOVER } else { FILL });
        let mid = y + CHIP_H / 2.0;
        let b = base(mid, &ns, s);
        let mut tx = x + 11.0;
        if !ext.is_empty() {
            tx += pen.text_base(&ext, tx, b, &es) + 6.0;
        }
        pen.text_base(&stem, tx, b, &ns.color(TEXT));
        pen.hit(x, y, w, CHIP_H, Hit::File(i));
        x += w + 6.0;
    }
    y + CHIP_H
}

/// Lines of `text` wrapped to `w`, at most `max` (last one ellipsized).
fn wrap_max(text: &str, st: &TextStyle, w: f32, s: f32, max: usize) -> Vec<String> {
    let mut lines = ui::wrap(text.trim(), st, w * s);
    lines.retain(|l| !l.trim().is_empty());
    if lines.len() > max {
        let rest = lines[max - 1..].join(" ");
        lines.truncate(max - 1);
        lines.push(ui::ellipsize(&rest, st, w * s));
    }
    lines
}

/// The status, as the only colour on the card: a small dot (a check when
/// done). Live states get a soft halo.
fn status_mark(pen: &mut Pen, k: Kind, cy: f32) {
    let halo = |pen: &mut Pen, c: Rgba| pen.circle(DOT_X, cy, 3.5, c);
    match k {
        Kind::Running => halo(pen, tokens::SIGNAL),
        Kind::Approval | Kind::Questions => halo(pen, tokens::ASK),
        Kind::Done => pen.poly(
            &[(DOT_X - 4.2, cy + 0.2), (DOT_X - 1.4, cy + 3.0), (DOT_X + 4.4, cy - 3.4)],
            1.8,
            tokens::OK,
        ),
        Kind::Failed => pen.circle(DOT_X, cy, 3.5, tokens::ERR),
        Kind::Pending => pen.arc(DOT_X, cy, 3.4, 1.0, 1.3, TEXT_2),
        Kind::Stopped => pen.circle(DOT_X, cy, 3.5, TEXT_3),
    }
}

/// What the agent is doing, said next to its name.
fn subtitle(v: &CardView, k: Kind) -> String {
    let a = &v.agent;
    match k {
        Kind::Pending => "Starting…".into(),
        Kind::Running if v.drive == Drive::Driving => "Using your mouse and keyboard".into(),
        Kind::Running if v.drive == Drive::Paused => "Paused while you work".into(),
        Kind::Running => a.steps.last().cloned().unwrap_or_else(|| "Working…".into()),
        Kind::Approval => "Needs your OK".into(),
        Kind::Questions => match a.ask.as_ref().map_or(0, |q| q.questions.len()) {
            0 | 1 => "Has a question".into(),
            n => format!("Has {n} questions"),
        },
        Kind::Done => String::new(),
        Kind::Failed => "Didn't finish".into(),
        Kind::Stopped if a.counted => "Stopped".into(),
        Kind::Stopped => "Stopped · didn't count".into(),
    }
}

fn elapsed(v: &CardView) -> String {
    let a = &v.agent;
    let start = a.started.unwrap_or(a.created);
    let end = if a.status.is_active() { v.now } else { a.ended.unwrap_or(v.now) };
    elapsed_label(end.saturating_sub(start))
}

/// The one-line row (the whole card at rest, the header when open): status,
/// name, what it's doing, then elapsed or the row's one action. Returns
/// whether the "doing" text had to be cut.
fn header(pen: &mut Pen, v: &CardView, k: Kind) -> bool {
    let s = pen.s;
    let mid = ROW_H / 2.0;
    let by = (ROW_H - ROW_BTN_H) / 2.0;
    // Pills sit concentric with the card's corners.
    let edge = CARD_W - by;
    let pill = |pen: &mut Pen, label: &str, btn: Btn, ring: Option<f32>| -> f32 {
        let w = button_w(label, ring.is_some(), s);
        button(pen, edge - w, by, Some(w), ROW_BTN_H, label, Look::Secondary, btn, v, ring, v.keyed);
        edge - w - 10.0
    };
    let ts = title_style(s);
    let (title, sub, right) = if let Some(b) = &v.brain {
        pen.circle(DOT_X, mid, 3.5, tokens::SIGNAL);
        let right = if b.undone {
            CARD_W - PAD
        } else {
            pill(pen, "Undo", Btn::Undo, None)
        };
        let t = if b.undone { "Forgotten" } else { "Remembered" };
        // Open, the fact is said once, in full, below.
        let sub = if v.expanded { String::new() } else { b.fact.text.clone() };
        (t.to_string(), sub, right)
    } else {
        status_mark(pen, k, mid);
        let right = match k {
            Kind::Pending => pill(pen, "Cancel", Btn::Cancel, Some(v.pending_left)),
            Kind::Done | Kind::Stopped => pill(pen, "Open", Btn::Palette, None),
            Kind::Failed => pill(pen, "Retry", Btn::Retry, None),
            _ => {
                let ms = meta_style(s);
                let t = elapsed(v);
                let x = CARD_W - PAD - 2.0 - tw(&t, &ms, s);
                pen.text_base(&t, x, base(mid, &ts, s), &ms);
                x - 12.0
            }
        };
        // Open, an ask says it in full below and the plan highlights the
        // current step, so the row keeps just the name.
        let quiet = matches!(k, Kind::Approval | Kind::Questions)
            || (matches!(k, Kind::Running | Kind::Pending) && !v.agent.plan.is_empty());
        let sub = if v.expanded && quiet {
            String::new()
        } else {
            subtitle(v, k)
        };
        (v.agent.name.clone(), sub, right)
    };
    let ss = sec_style(s);
    let b = base(mid, &ts, s);
    let avail = right - TEXT_X;
    let gap = 8.0;
    let nw = tw(&title, &ts, s);
    let sw = if sub.is_empty() { 0.0 } else { tw(&sub, &ss, s) };
    let name_w = if sub.is_empty() || nw + gap + sw <= avail {
        nw.min(avail)
    } else {
        nw.min((avail - sw - gap).max(avail * 0.58))
    };
    let name = ui::ellipsize(&title, &ts, name_w * s);
    let drawn = pen.text_base(&name, TEXT_X, b, &ts);
    if sub.is_empty() {
        return false;
    }
    let room = avail - drawn - gap;
    if room < 36.0 {
        return true;
    }
    let t = ui::ellipsize(&sub, &ss, room * s);
    pen.text_base(&t, TEXT_X + drawn + gap, b, &ss);
    sw > room
}

/// The plan, one row per item under the status dot: done items checked,
/// the current one live, the rest waiting.
fn plan_list(pen: &mut Pen, v: &CardView, k: Kind, mut y: f32) -> f32 {
    let s = pen.s;
    let a = &v.agent;
    let cur = if k == Kind::Pending { 0 } else { plan_progress(&a.plan, &a.steps) };
    let first = if a.plan.len() > PLAN_ROWS {
        cur.saturating_sub(1).min(a.plan.len() - PLAN_ROWS)
    } else {
        0
    };
    let ps = body_style(s).size(12.5 * s);
    let row = 22.0;
    for (i, item) in a.plan.iter().enumerate().skip(first).take(PLAN_ROWS) {
        let mid = y + row / 2.0;
        let c = if i < cur {
            pen.poly(
                &[(DOT_X - 3.2, mid + 0.2), (DOT_X - 1.0, mid + 2.4), (DOT_X + 3.4, mid - 2.6)],
                1.4,
                TEXT_3,
            );
            TEXT_3
        } else if i == cur && k != Kind::Pending {
            pen.circle(DOT_X, mid, 3.0, tokens::SIGNAL);
            TEXT
        } else {
            pen.arc(DOT_X, mid, 2.6, 1.0, 1.1, TEXT_3);
            TEXT_2
        };
        let t = ui::ellipsize(item, &ps, (CARD_W - TEXT_X - PAD) * s);
        pen.text_base(&t, TEXT_X, base(mid, &ps, s), &ps.color(c));
        y += row;
    }
    let more = a.plan.len().saturating_sub(first + PLAN_ROWS);
    if more > 0 {
        let st = small_style(s).color(TEXT_3);
        pen.text_base(&format!("+{more} more"), TEXT_X, base(y + 9.0, &st, s), &st);
        y += 18.0;
    }
    y
}

/// Stop / Show desk on the left, Open on the right, all quiet.
fn actions(pen: &mut Pen, v: &CardView, y: f32) -> f32 {
    let s = pen.s;
    let mut x = PAD;
    x += button(pen, x, y, None, QUIET_H, "Stop", Look::Quiet, Btn::Stop, v, None, false) + 2.0;
    if v.desk {
        button(pen, x, y, None, QUIET_H, "Show desk", Look::Quiet, Btn::Show, v, None, false);
    }
    let w = button_w("Open", false, s);
    button(pen, CARD_W - PAD - w, y, Some(w), QUIET_H, "Open", Look::Quiet, Btn::Palette, v, None, false);
    y + QUIET_H
}

/// Files, the follow-up box and Show desk, under a finished card.
fn after(pen: &mut Pen, v: &CardView, mut y: f32) -> f32 {
    if !v.agent.files.is_empty() {
        y = file_chips(pen, v, TEXT_X, y) + 12.0;
    }
    input_box(pen, v, y, "Follow up…");
    y += INPUT_H;
    if v.desk {
        y += 6.0;
        button(pen, PAD, y, None, QUIET_H, "Show desk", Look::Quiet, Btn::Show, v, None, false);
        y += QUIET_H;
    }
    y
}

/// Lays out (and with a pixmap, draws) one card's content. Returns its
/// height and how far its result scrolls.
fn compose(v: &CardView, pen: &mut Pen) -> (f32, f32) {
    let k = kind(&v.agent);
    let tall = if v.expanded { 10_000.0 } else { ROW_H };
    pen.hit(0.0, 0.0, CARD_W, tall, Hit::Card);
    let cut = header(pen, v, k);
    if !v.expanded {
        return (ROW_H, 0.0);
    }
    if let Some(b) = &v.brain {
        return (brain_body(pen, v, b) + 12.0, 0.0);
    }
    let s = pen.s;
    let a = &v.agent;
    let text_w = CARD_W - TEXT_X - PAD;
    let top = ROW_H - 2.0;
    let mut y = top;
    let mut max_scroll = 0.0;
    // What it's doing, in full, when the row had to cut it.
    if cut && matches!(k, Kind::Running | Kind::Pending) && a.plan.is_empty() {
        let st = small_style(s);
        for l in wrap_max(&subtitle(v, k), &st, text_w, s, 3) {
            pen.line(&l, TEXT_X, y, &st);
            y += LINE;
        }
        y += 6.0;
    }
    match k {
        Kind::Pending | Kind::Running => {
            if !a.plan.is_empty() {
                y = plan_list(pen, v, k, y) + 6.0;
            }
            if k == Kind::Running {
                y += 2.0;
                input_box(pen, v, y, "Add to it…");
                y += INPUT_H + 6.0;
                y = actions(pen, v, y);
            } else if y == top {
                return (ROW_H, 0.0);
            }
        }
        Kind::Approval => {
            if let Some(q) = &a.ask {
                y = approval(pen, v, q, y);
            }
        }
        Kind::Questions => {
            if let Some(q) = &a.ask {
                y = questions(pen, v, q, y);
            }
        }
        Kind::Done => {
            let (ny, ms) = result(pen, v, y);
            max_scroll = ms;
            y = after(pen, v, ny);
        }
        Kind::Failed | Kind::Stopped => {
            if k == Kind::Failed {
                let es = small_style(s);
                let err = a.error.as_deref().unwrap_or("Something went wrong.");
                for l in wrap_max(err, &es, text_w, s, 4) {
                    pen.line(&l, TEXT_X, y, &es);
                    y += LINE;
                }
                y += 10.0;
            }
            y = after(pen, v, y);
        }
    }
    (y + 12.0, max_scroll)
}

/// The result as markdown-lite, in a scrolling window.
fn result(pen: &mut Pen, v: &CardView, mut y: f32) -> (f32, f32) {
    let s = pen.s;
    let a = &v.agent;
    let text_w = CARD_W - TEXT_X - PAD;
    let body = if a.result.trim().is_empty() { "Done." } else { a.result.trim() };
    let lines = md_lines(body, text_w);
    let total: f32 = lines.iter().map(|l| l.h()).sum();
    let vh = total.min(RESULT_MAX_H);
    let max_scroll = (total - vh).max(0.0);
    let scroll = v.scroll.clamp(0.0, max_scroll);
    if pen.pm.is_some() {
        if let Some(mut sub) = Pixmap::new(((CARD_W - PAD) * s).ceil() as u32, (vh * s).ceil().max(1.0) as u32) {
            let mut sp = Pen {
                pm: Some(&mut sub),
                s,
                hits: Vec::new(),
            };
            draw_md(&mut sp, &lines, TEXT_X, -scroll, text_w);
            if scroll > 0.5 {
                fade_edge(&mut sub, true, 18.0 * s);
            }
            if scroll < max_scroll - 0.5 {
                fade_edge(&mut sub, false, 22.0 * s);
            }
            if let Some(pm) = pen.pm.as_deref_mut() {
                pm.draw_pixmap(
                    0,
                    (y * s).round() as i32,
                    sub.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
            }
        }
    }
    if max_scroll > 0.0 {
        // A slim scroll thumb.
        let th = (vh * vh / total).max(18.0);
        let ty = y + (vh - th) * (scroll / max_scroll);
        pen.rounded(CARD_W - 8.0, ty, 2.5, th, 1.25, TEXT_3.fade(0.6));
    }
    pen.hit(0.0, y, CARD_W, vh, Hit::Result);
    y += vh + 12.0;
    (y, max_scroll)
}

/// The approval ask: what it wants to do, then Don't Allow / Allow, and a
/// quiet "Always allow for this agent".
fn approval(pen: &mut Pen, v: &CardView, q: &Question, mut y: f32) -> f32 {
    let s = pen.s;
    let text_w = CARD_W - TEXT_X - PAD;
    let is = body_style(s).size(13.5 * s);
    let row = 19.0;
    let lh = ui::line_height(&is) / s;
    for l in wrap_max(&q.text, &is, text_w, s, 3) {
        pen.text(&l, TEXT_X, y + (row - lh) / 2.0, &is);
        y += row;
    }
    if !q.detail.trim().is_empty() {
        let ds = meta_style(s);
        let dl = ui::line_height(&ds) / s;
        y += 3.0;
        for l in wrap_max(&q.detail.replace('\n', " "), &ds, text_w, s, 2) {
            pen.text(&l, TEXT_X, y + (15.0 - dl) / 2.0, &ds);
            y += 15.0;
        }
    }
    y += 12.0;
    // One quiet row: Always on the left; Don't Allow and Allow on the right.
    let h = 30.0;
    let aw = button_w("Allow", false, s) + 6.0;
    let ax = CARD_W - PAD - aw;
    button(pen, ax, y, Some(aw), h, "Allow", Look::Primary, Btn::Once, v, None, v.keyed);
    let dw = button_w("Don't Allow", false, s);
    button(pen, ax - 4.0 - dw, y, Some(dw), h, "Don't Allow", Look::Quiet, Btn::Deny, v, None, false);
    if !matches!(v.drive, Drive::Asking | Drive::Paused) {
        // The drive machine remembers its own yes for the conversation;
        // agents.always_approve doesn't cover driving.
        button(pen, PAD, y, None, h, "Always allow", Look::Quiet, Btn::Always, v, None, false);
    }
    y + h
}

/// Questions, numbered when there are several, then the answer box.
fn questions(pen: &mut Pen, v: &CardView, q: &Question, mut y: f32) -> f32 {
    let s = pen.s;
    let text_w = CARD_W - TEXT_X - PAD;
    let qs = body_style(s).size(13.5 * s);
    let num = meta_style(s).size(11.5 * s);
    let row = 19.0;
    let list: Vec<&String> = if q.questions.is_empty() {
        vec![&q.text]
    } else {
        q.questions.iter().collect()
    };
    let many = list.len() > 1;
    for (i, item) in list.iter().enumerate() {
        for (j, l) in wrap_max(item, &qs, text_w, s, 4).iter().enumerate() {
            let b = base(y + row / 2.0, &qs, s);
            if many && j == 0 {
                let n = format!("{}", i + 1);
                pen.text_base(&n, DOT_X - tw(&n, &num, s) / 2.0, b, &num);
            }
            pen.text_base(l, TEXT_X, b, &qs);
            y += row;
        }
        y += 5.0;
    }
    y += 5.0;
    input_box(pen, v, y, "Your answer…");
    y += INPUT_H + 6.0;
    actions(pen, v, y)
}

/// Under the "Remembered" line: the fact in full, the agents
/// that read it, and See all.
fn brain_body(pen: &mut Pen, v: &CardView, b: &BrainView) -> f32 {
    let s = pen.s;
    let text_w = CARD_W - TEXT_X - PAD;
    let mut y = ROW_H - 2.0;
    {
        let fs = body_style(s);
        for l in wrap_max(&b.fact.text, &fs, text_w, s, 4) {
            pen.line(&l, TEXT_X, y, &fs);
            y += LINE;
        }
        y += 8.0;
    }
    let ss = small_style(s);
    if b.readers.is_empty() {
        pen.line("Agents read it when they start.", TEXT_X, y, &ss);
        y += LINE + 4.0;
    } else {
        const ROWS: usize = 3;
        let row = 22.0;
        for name in b.readers.iter().take(ROWS) {
            let mid = y + row / 2.0;
            pen.circle(DOT_X, mid, 2.5, tokens::SIGNAL);
            let ns = ss.color(TEXT);
            let tail = "reads it";
            let tail_w = tw(tail, &ss, s);
            let n = ui::ellipsize(name, &ns, (text_w - tail_w - 8.0) * s);
            let w = pen.text_base(&n, TEXT_X, base(mid, &ns, s), &ns);
            pen.text_base(tail, TEXT_X + w + 6.0, base(mid, &ns, s), &ss.color(TEXT_3));
            y += row;
        }
        if b.readers.len() > ROWS {
            let st = ss.color(TEXT_3);
            let t = format!("+{} more", b.readers.len() - ROWS);
            pen.text_base(&t, TEXT_X, base(y + 9.0, &st, s), &st);
            y += 18.0;
        }
        y += 4.0;
    }
    // Footer: where it lives, and the way to all of it.
    let st = small_style(s).color(TEXT_3);
    let w = button_w("See all", false, s);
    let mid = y + QUIET_H / 2.0;
    let meta = format!("{} \u{00b7} {}", b.fact.topic, b.fact.from_label());
    let meta = ui::ellipsize(&meta, &st, (CARD_W - PAD - w - TEXT_X - 6.0) * s);
    pen.text_base(&meta, TEXT_X, base(mid, &st, s), &st);
    button(pen, CARD_W - PAD - w, y, Some(w), QUIET_H, "See all", Look::Quiet, Btn::Brain, v, None, false);
    y + QUIET_H
}

/// Fades the top or bottom `px` rows of a premultiplied pixmap to clear.
fn fade_edge(pm: &mut Pixmap, top: bool, px: f32) {
    let (w, h) = (pm.width() as usize, pm.height() as usize);
    let n = (px as usize).min(h);
    let data = pm.data_mut();
    for i in 0..n {
        let k = (i as f32 + 0.5) / n as f32;
        let k = k * k * (3.0 - 2.0 * k);
        let row = if top { i } else { h - 1 - i };
        for b in &mut data[row * w * 4..(row + 1) * w * 4] {
            *b = (*b as f32 * k).round() as u8;
        }
    }
}

fn render_card(v: &CardView, s: f32) -> Option<Drawn> {
    let mut measure = Pen {
        pm: None,
        s,
        hits: Vec::new(),
    };
    let (h, _) = compose(v, &mut measure);
    let mut pm = Pixmap::new((CARD_W * s).ceil() as u32, (h * s).ceil().max(1.0) as u32)?;
    let mut pen = Pen {
        pm: Some(&mut pm),
        s,
        hits: Vec::new(),
    };
    let (_, max_scroll) = compose(v, &mut pen);
    let hits = std::mem::take(&mut pen.hits);
    Some(Drawn {
        pm,
        h,
        hits,
        max_scroll,
    })
}

/// What a card's surface is filled with: the island's glass (a texture in
/// the card window's pixels) or, without one, the island's solid ink.
enum Fill<'a> {
    Ink,
    Glass(&'a Pixmap),
}

fn fill_surface(pm: &mut Pixmap, path: &tiny_skia::Path, fill: &Fill, alpha: f32) {
    let paint = match fill {
        Fill::Ink => {
            let mut p = tiny_skia::Paint::default();
            p.set_color(tokens::INK.fade(alpha).skia());
            p.anti_alias = true;
            p
        }
        Fill::Glass(tex) => tiny_skia::Paint {
            shader: tiny_skia::Pattern::new(
                tex.as_ref(),
                tiny_skia::SpreadMode::Pad,
                tiny_skia::FilterQuality::Nearest,
                alpha.clamp(0.0, 1.0),
                Transform::identity(),
            ),
            anti_alias: true,
            ..Default::default()
        },
    };
    pm.fill_path(path, &paint, tiny_skia::FillRule::Winding, Transform::identity(), None);
}

/// The island's glass under the card window, one texel per window pixel
/// (`x0`, `top`: the window's screen position).
fn glass_texture(
    b: &crate::surface::Backdrop,
    opacity: f32,
    x0: i32,
    top: i32,
    w: u32,
    h: u32,
) -> Option<Pixmap> {
    let mut tex = Pixmap::new(w, h)?;
    let ink = [tokens::INK.r, tokens::INK.g, tokens::INK.b];
    let px = tex.pixels_mut();
    for y in 0..h {
        let sy = top as f32 + y as f32 + 0.5;
        for x in 0..w {
            let c = b.sample_screen(x0 as f32 + x as f32 + 0.5, sy);
            let [r, g, bl] = crate::surface::glass_pixel(c, ink, opacity);
            if let Some(p) = tiny_skia::PremultipliedColorU8::from_rgba(r, g, bl, 255) {
                px[(y * w + x) as usize] = p;
            }
        }
    }
    Some(tex)
}

/// Draws one card's surface at (x, y) with height `h` (logical; may be
/// shorter than the content while it animates) and its content clipped to
/// it, cross-fading from `prev` (with the fade's progress) when it morphs.
#[allow(clippy::too_many_arguments)]
fn paint_card(
    pm: &mut Pixmap,
    d: &Drawn,
    prev: Option<(&Drawn, f32)>,
    x: f32,
    y: f32,
    h: f32,
    alpha: f32,
    s: f32,
    fill: &Fill,
) {
    if alpha <= 0.002 || h <= 0.5 {
        return;
    }
    let (px, py, pw, ph) = (x * s, y * s, CARD_W * s, h * s);
    let r = RADIUS.min(h / 2.0) * s;
    let Some(path) = ui::rounded_rect_path(px, py, pw, ph, r) else {
        return;
    };
    fill_surface(pm, &path, fill, alpha);
    if let Some(mut mask) = tiny_skia::Mask::new(pm.width(), pm.height()) {
        mask.fill_path(&path, tiny_skia::FillRule::Winding, true, Transform::identity());
        let mut layer = |src: &Drawn, a: f32| {
            if a > 0.002 {
                let paint = PixmapPaint {
                    opacity: a.clamp(0.0, 1.0),
                    ..Default::default()
                };
                pm.draw_pixmap(
                    px.round() as i32,
                    py.round() as i32,
                    src.pm.as_ref(),
                    &paint,
                    Transform::identity(),
                    Some(&mask),
                );
            }
        };
        match prev {
            Some((p, t)) => {
                let t = t.clamp(0.0, 1.0);
                layer(p, alpha * (1.0 - t));
                layer(d, alpha * t);
            }
            None => layer(d, alpha),
        }
    }
    ui::stroke_rounded(pm, px, py, pw, ph, r, s.max(1.0), tokens::LINE.fade(alpha));
}

/// The "+N" pill under the stack, centred in a `CARD_W` column at `ox`.
/// Returns its x and width (logical, relative to `ox`).
#[allow(clippy::too_many_arguments)]
fn paint_more(
    pm: &mut Pixmap,
    n: usize,
    open: bool,
    hover: bool,
    ox: f32,
    y: f32,
    s: f32,
    fill: &Fill,
) -> (f32, f32) {
    let label = if open { "Show fewer".to_string() } else { format!("+{n}") };
    let st = btn_style(s).size(12.5 * s).color(if hover { TEXT } else { TEXT_2 });
    let w = tw(&label, &st, s) + 28.0;
    let x = (CARD_W - w) / 2.0;
    let (px, py, pw, ph, r) = ((ox + x) * s, y * s, w * s, MORE_H * s, MORE_H / 2.0 * s);
    if let Some(p) = ui::rounded_rect_path(px, py, pw, ph, r) {
        fill_surface(pm, &p, fill, 1.0);
        if hover {
            ui::fill_rounded(pm, px, py, pw, ph, r, FILL);
        }
    }
    ui::stroke_rounded(pm, px, py, pw, ph, r, s.max(1.0), tokens::LINE);
    let b = ((y + MORE_H / 2.0) * s + ui::cap_height(&st) / 2.0).round();
    ui::draw_text_baseline(pm, &label, ((ox + x + 14.0) * s).round(), b, &st);
    (x, w)
}

// ---------------------------------------------------------------- window thread

#[derive(Default)]
struct Input {
    hovered: bool,
    mouse: (i32, i32),
    down: bool,
    tracking: bool,
    /// (x, y, right button), window px.
    clicks: Vec<(i32, i32, bool)>,
    /// (screen x, screen y, delta).
    wheel: Vec<(i32, i32, i16)>,
    chars: Vec<u16>,
    blurred: bool,
    /// The foreground window before a click activated us (to hand it back).
    prev_fg: Option<isize>,
    /// Window-px rects of text inputs (WM_MOUSEACTIVATE decides on these).
    inputs: Vec<(i32, i32, i32, i32)>,
    /// Window-px rects of clickable things (hand cursor).
    buttons: Vec<(i32, i32, i32, i32)>,
    /// Window-px rects of cards a click gives the keyboard to.
    keyable: Vec<(i32, i32, i32, i32)>,
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

fn inside(r: &(i32, i32, i32, i32), p: (i32, i32)) -> bool {
    p.0 >= r.0 && p.0 < r.0 + r.2 && p.1 >= r.1 && p.1 < r.1 + r.3
}

fn window_point(hwnd: HWND) -> (i32, i32) {
    let mut p = POINT::default();
    let mut r = RECT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
        let _ = GetWindowRect(hwnd, &mut r);
    }
    (p.x - r.left, p.y - r.top)
}

fn set_activatable(hwnd: HWND, on: bool) {
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let na = WS_EX_NOACTIVATE.0 as isize;
        let new = if on { ex & !na } else { ex | na };
        if new != ex {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new);
        }
    }
}

unsafe extern "system" fn body_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_MOUSEACTIVATE => {
                let p = window_point(hwnd);
                let take = INPUT.with(|i| {
                    let i = i.borrow();
                    i.inputs.iter().any(|r| inside(r, p))
                        || (i.keyable.iter().any(|r| inside(r, p))
                            && !i.buttons.iter().any(|r| inside(r, p)))
                });
                if take {
                    let fg = GetForegroundWindow();
                    if fg != hwnd {
                        INPUT.with(|i| i.borrow_mut().prev_fg = Some(fg.0 as isize));
                    }
                    set_activatable(hwnd, true);
                    return LRESULT(MA_ACTIVATE as isize);
                }
                return LRESULT(MA_NOACTIVATE as isize);
            }
            WM_MOUSEMOVE => {
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    i.mouse = lparam_xy(lp);
                    i.hovered = true;
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
                });
                return LRESULT(0);
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN => {
                INPUT.with(|i| i.borrow_mut().down = true);
                return LRESULT(0);
            }
            WM_LBUTTONUP | WM_RBUTTONUP => {
                INPUT.with(|i| {
                    let mut i = i.borrow_mut();
                    if i.down {
                        let (x, y) = lparam_xy(lp);
                        i.clicks.push((x, y, msg == WM_RBUTTONUP));
                    }
                    i.down = false;
                });
                return LRESULT(0);
            }
            WM_MOUSEWHEEL => {
                let (x, y) = lparam_xy(lp);
                let d = ((wp.0 >> 16) & 0xFFFF) as i16;
                INPUT.with(|i| i.borrow_mut().wheel.push((x, y, d)));
                return LRESULT(0);
            }
            WM_CHAR => {
                INPUT.with(|i| i.borrow_mut().chars.push(wp.0 as u16));
                return LRESULT(0);
            }
            WM_KILLFOCUS => {
                INPUT.with(|i| i.borrow_mut().blurred = true);
            }
            WM_ACTIVATE => {
                if (wp.0 & 0xFFFF) as u32 == WA_INACTIVE {
                    INPUT.with(|i| i.borrow_mut().blurred = true);
                }
            }
            WM_SETCURSOR => {
                let p = window_point(hwnd);
                let (on_input, on_btn) = INPUT.with(|i| {
                    let i = i.borrow();
                    (
                        i.inputs.iter().any(|r| inside(r, p)),
                        i.buttons.iter().any(|r| inside(r, p)),
                    )
                });
                let c = if on_input {
                    IDC_IBEAM
                } else if on_btn {
                    IDC_HAND
                } else {
                    IDC_ARROW
                };
                if let Ok(c) = LoadCursorW(None, c) {
                    SetCursor(Some(c));
                }
                return LRESULT(1);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// Size and position springs: quick and just short of bouncy (~220 ms,
/// no visible overshoot).
const MORPH: (f32, f32) = (420.0, 38.0);

struct Card {
    id: String,
    agent: Agent,
    stale: bool,
    pending_at: Instant,
    hover_until: Option<Instant>,
    /// When a finished card (or the "Remembered" line) goes on its own.
    expires: Option<Instant>,
    /// The user asked for this card (island row, notice): it shows even
    /// while running with presence "hotkey".
    opened: bool,
    y: Spring,
    h: Spring,
    appear: Spring,
    /// 0 → 1: cross-fade from `prev` to the current content.
    xfade: Spring,
    leaving: bool,
    fresh: bool,
    draft: String,
    scroll: f32,
    drive: Drive,
    cache: Option<(CardView, Drawn)>,
    prev: Option<Drawn>,
    brain: Option<BrainView>,
}

impl Card {
    fn new(agent: Agent, now: Instant) -> Self {
        let pending_at = if agent.status == Status::Pending {
            // Re-derive the window from the record when we start late.
            let age = now_unix().saturating_sub(agent.created);
            now.checked_sub(Duration::from_secs(age.min(5)))
                .unwrap_or(now)
        } else {
            now
        };
        let expires = (!agent.status.is_active()).then(|| now + FINISHED_FOR);
        Self {
            id: agent.id.clone(),
            agent,
            stale: false,
            pending_at,
            hover_until: None,
            expires,
            opened: false,
            y: Spring::new(0.0).with(MORPH.0, MORPH.1),
            h: Spring::new(ROW_H).with(MORPH.0, MORPH.1),
            appear: Spring::new(0.0).with(300.0, 30.0),
            xfade: Spring::new(1.0).with(520.0, 46.0),
            leaving: false,
            fresh: true,
            draft: String::new(),
            scroll: 0.0,
            drive: Drive::Off,
            cache: None,
            prev: None,
            brain: None,
        }
    }

    /// The "Remembered" line for a fact (it leaves on its own).
    fn brain(fact: Fact, now: Instant) -> Self {
        let readers = agents::list()
            .into_iter()
            .filter(|a| a.status.is_active())
            .map(|a| a.name)
            .collect();
        let agent = Agent {
            id: brain_card_id(&fact),
            name: fact.topic.clone(),
            status: Status::Done,
            created: now_unix(),
            ..Default::default()
        };
        Self {
            brain: Some(BrainView {
                fact,
                readers,
                undone: false,
            }),
            expires: Some(now + NOTICE_FOR),
            ..Self::new(agent, now)
        }
    }

    fn kind(&self) -> Kind {
        kind(&self.agent)
    }

    /// Waiting on the user: opens by itself and stays until handled.
    fn asking(&self) -> bool {
        self.brain.is_none() && matches!(self.kind(), Kind::Approval | Kind::Questions)
    }

    /// A click gives this card the keyboard (Enter has something to do).
    fn keyable(&self) -> bool {
        self.brain.is_none()
            && matches!(
                self.kind(),
                Kind::Approval | Kind::Questions | Kind::Done | Kind::Failed | Kind::Stopped
            )
    }

    /// Shows without being called: asks, finished agents, the brain line,
    /// and any card the user opened. Running agents only with presence
    /// "auto_hide" / "always" (with "hotkey" the palette shows them).
    fn shows(&self, presence: Presence) -> bool {
        self.brain.is_some()
            || self.asking()
            || !self.agent.status.is_active()
            || self.opened
            || presence != Presence::Hotkey
    }

    fn pending_left(&self, now: Instant) -> f32 {
        (agents::PENDING_WINDOW.as_secs_f32()
            - now.saturating_duration_since(self.pending_at).as_secs_f32())
        .max(0.0)
    }
}

/// A new rendering cross-fades (rather than swaps) when the card opens or
/// closes, changes state or its line of text changes.
fn morphs(old: &CardView, new: &CardView) -> bool {
    old.expanded != new.expanded
        || kind(&old.agent) != kind(&new.agent)
        || old.agent.steps.last() != new.agent.steps.last()
        || old.brain.as_ref().map(|b| b.undone) != new.brain.as_ref().map(|b| b.undone)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The glass texture under the card window, and what it was made for:
/// (backdrop, x0, top, width, opacity bits).
type GlassKey = (usize, i32, i32, u32, u32);

struct Cards {
    cards: Vec<Card>,
    body: Layer,
    shadow: Layer,
    mon: MonitorInfo,
    /// Card id with the focused follow-up box.
    focus: Option<String>,
    /// Card id that has the keyboard itself (Enter = its primary action).
    kb: Option<String>,
    hovered: Option<(String, Hit)>,
    show_all: bool,
    more_hover: bool,
    /// Hit rects of the last frame (window logical px).
    hits: Vec<(f32, f32, f32, f32, String, Hit)>,
    more_rect: Option<(f32, f32, f32, f32)>,
    origin: (i32, i32),
    last: Instant,
    dirty: bool,
    shadow_key: Vec<(i32, i32, i32)>,
    placed: bool,
    /// Where the stack hangs, and the scale its cards were rendered at.
    spot: (i32, i32),
    cache_scale: f32,
    /// The agent `quick` is recording a spoken follow-up for.
    listening: Option<String>,
    /// Windows' "Animation effects" is off: no motion, no fades.
    reduce: bool,
    reduce_at: Instant,
    presence: Presence,
    glass: Option<(GlassKey, Pixmap)>,
    /// Our own blurred backdrop while the island (and its glass) is away.
    own_glass: Option<(Arc<crate::surface::Backdrop>, Instant)>,
}

fn run() -> anyhow::Result<()> {
    let now = Instant::now();
    let body = Layer::new("cards", Some(body_proc), false)?;
    let shadow = Layer::new("cards-shadow", None, true)?;
    let mut c = Cards {
        cards: Vec::new(),
        body,
        shadow,
        mon: ui::monitor_under_cursor(),
        focus: None,
        kb: None,
        hovered: None,
        show_all: false,
        more_hover: false,
        hits: Vec::new(),
        more_rect: None,
        origin: (0, 0),
        last: now,
        dirty: true,
        shadow_key: Vec::new(),
        placed: false,
        spot: (0, 0),
        cache_scale: 0.0,
        listening: None,
        reduce: crate::surface::reduced_motion(),
        reduce_at: now,
        presence: presence(),
        glass: None,
        own_glass: None,
    };
    // Agents already waiting on the user (or still running) get their cards.
    for a in agents::list() {
        if a.status.is_active() {
            c.cards.push(Card::new(a, now));
        }
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
        c.frame(now);
        let wait = |ms: u32| unsafe {
            MsgWaitForMultipleObjects(None, false, ms, QS_ALLINPUT);
        };
        match c.pace(now) {
            None => unsafe {
                if DwmFlush().is_err() {
                    wait(8);
                }
            },
            Some(Some(t)) => wait(
                t.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(60_000) as u32
                    + 1,
            ),
            Some(None) => wait(INFINITE),
        }
    }
}

impl Cards {
    fn find(&mut self, id: &str) -> Option<&mut Card> {
        self.cards.iter_mut().find(|c| c.id == id && !c.leaving)
    }

    fn ensure(&mut self, id: &str, now: Instant) -> Option<&mut Card> {
        if self.find(id).is_none() {
            let a = agents::get(id)?;
            // A dismissed card that is still fading out makes room.
            self.cards.retain(|c| c.id != id);
            self.cards.push(Card::new(a, now));
        }
        self.find(id)
    }

    fn on_event(&mut self, id: String, ev: AgentEvent, now: Instant) {
        let shows = matches!(
            ev,
            AgentEvent::Status(Status::Pending | Status::Running)
                | AgentEvent::Ask(_)
                | AgentEvent::Done
                | AgentEvent::Failed(_)
        );
        let card = if shows {
            self.ensure(&id, now)
        } else {
            self.find(&id)
        };
        let Some(card) = card else { return };
        card.stale = true;
        match &ev {
            AgentEvent::Status(Status::Pending) => {
                card.pending_at = now;
                card.expires = None;
            }
            AgentEvent::Status(Status::Running) => {
                card.scroll = 0.0;
                card.expires = None;
            }
            AgentEvent::Status(s) if !s.is_active() => {
                card.drive = Drive::Off;
                card.expires = Some(now + FINISHED_FOR);
            }
            AgentEvent::Drive(d) => card.drive = card.drive.on(d),
            _ => {}
        }
        if ev == AgentEvent::Done {
            let busy = self.focus.as_deref() == Some(id.as_str())
                || self.hovered.as_ref().is_some_and(|(h, _)| *h == id);
            let name = agents::get(&id).map(|a| a.name).unwrap_or_default();
            if !busy && !name.is_empty() {
                caption::send(caption::Cmd::Notice {
                    text: format!("\u{2039}{name}\u{203a} is done"),
                    id: id.clone(),
                });
            }
        }
        self.dirty = true;
    }

    /// The user opened a card (island row, notice).
    fn open(&mut self, id: &str, now: Instant) {
        if let Some(c) = self.ensure(id, now) {
            c.stale = true;
            c.opened = true;
            c.hover_until = Some(now + OPENED_FOR);
            if !c.agent.status.is_active() {
                c.expires = Some(now + OPENED_FOR + AFTER_HOVER);
                crate::wire::mark_agent_seen(id);
            }
        }
        // Make sure it is in the visible few.
        if let Some(i) = self.order().iter().position(|&i| self.cards[i].id == id) {
            if i >= MAX_VISIBLE {
                self.show_all = true;
            }
        }
        self.dirty = true;
    }

    /// Indices of cards to show, in display order: the brain line, asks,
    /// running, finished; newest first within each.
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.cards.len())
            .filter(|&i| !self.cards[i].leaving && self.cards[i].shows(self.presence))
            .collect();
        idx.sort_by_key(|&i| {
            let c = &self.cards[i];
            let rank = if c.brain.is_some() {
                0
            } else if c.asking() {
                1
            } else if c.agent.status.is_active() {
                2
            } else {
                3
            };
            (
                rank,
                std::cmp::Reverse(c.agent.created),
                std::cmp::Reverse(c.id.clone()),
            )
        });
        idx
    }

    /// The user is with this card: hovering, typing, talking, or it was just
    /// opened.
    fn engaged(&self, c: &Card, now: Instant) -> bool {
        let id = Some(c.id.as_str());
        self.focus.as_deref() == id
            || self.kb.as_deref() == id
            || self.listening.as_deref() == id
            || c.hover_until.is_some_and(|t| now < t)
            || self.hovered.as_ref().is_some_and(|(h, _)| *h == c.id)
    }

    fn expanded(&self, c: &Card, now: Instant) -> bool {
        c.asking() || self.engaged(c, now)
    }

    fn frame(&mut self, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last)
            .as_secs_f32()
            .min(0.05);
        self.last = now;
        if now.saturating_duration_since(self.reduce_at) >= Duration::from_secs(3) {
            self.reduce_at = now;
            self.reduce = crate::surface::reduced_motion();
        }
        let p = presence();
        if p != self.presence {
            self.presence = p;
            self.dirty = true;
        }

        // ---- inbox
        let msgs = std::mem::take(&mut *INBOX.lock().unwrap());
        for m in msgs {
            match m {
                Msg::Event(id, ev) => self.on_event(id, ev, now),
                Msg::Open(id) => self.open(&id, now),
                Msg::Brain(f) => {
                    let id = brain_card_id(&f);
                    self.cards.retain(|c| c.id != id);
                    self.cards.push(Card::brain(f, now));
                    self.dirty = true;
                }
                Msg::Refresh => self.dirty = true,
                Msg::Wake => {}
            }
        }
        // Follow the island (cheap; only a move or a new monitor redraws).
        let spot = self.place();
        if spot != self.spot {
            self.spot = spot;
            self.dirty = true;
        }
        if self.mon.scale != self.cache_scale {
            self.cache_scale = self.mon.scale;
            for c in &mut self.cards {
                c.cache = None;
            }
        }
        let listening = crate::quick::listening_for();
        if listening != self.listening {
            self.listening = listening;
            self.dirty = true;
        }
        for c in self.cards.iter_mut().filter(|c| c.stale) {
            c.stale = false;
            if let Some(a) = agents::get(&c.id) {
                c.agent = a;
            }
        }

        // ---- input
        let (hovered, mouse, clicks, wheel, chars, blurred) = INPUT.with(|i| {
            let mut i = i.borrow_mut();
            (
                i.hovered,
                i.mouse,
                std::mem::take(&mut i.clicks),
                std::mem::take(&mut i.wheel),
                std::mem::take(&mut i.chars),
                std::mem::take(&mut i.blurred),
            )
        });
        if blurred && (self.focus.is_some() || self.kb.is_some()) {
            self.blur(false);
        }
        let s = self.mon.scale;
        let hit_at = |me: &Self, x: i32, y: i32| {
            let (lx, ly) = (x as f32 / s, y as f32 / s);
            // Most specific hit wins: buttons are recorded after the card.
            me.hits
                .iter()
                .rev()
                .find(|(hx, hy, hw, hh, _, _)| {
                    lx >= *hx && lx < hx + hw && ly >= *hy && ly < hy + hh
                })
                .map(|(.., id, h)| (id.clone(), *h))
        };
        let more_at = |me: &Self, x: i32, y: i32| {
            let (lx, ly) = (x as f32 / s, y as f32 / s);
            me.more_rect.is_some_and(|(mx, my, mw, mh)| {
                lx >= mx && lx < mx + mw && ly >= my && ly < my + mh
            })
        };
        let hover = if hovered {
            hit_at(self, mouse.0, mouse.1)
        } else {
            None
        };
        let more_hover = hovered && more_at(self, mouse.0, mouse.1);
        if more_hover != self.more_hover {
            self.more_hover = more_hover;
            self.dirty = true;
        }
        if hover != self.hovered {
            // Leaving a card: it stays open a moment (gaps, small moves), and
            // a timed one waits a little before it goes.
            if let Some((old, _)) = &self.hovered {
                if hover.as_ref().is_none_or(|(n, _)| n != old) {
                    if let Some(c) = self.cards.iter_mut().find(|c| &c.id == old) {
                        c.hover_until = Some(now + HOVER_LINGER);
                        c.opened = false;
                        if let Some(t) = c.expires {
                            c.expires = Some(t.max(now + AFTER_HOVER));
                        }
                    }
                }
            }
            if let Some((id, _)) = &hover {
                let id = id.clone();
                if let Some(c) = self.find(&id) {
                    c.hover_until = None;
                    if !c.agent.status.is_active() && c.brain.is_none() {
                        crate::wire::mark_agent_seen(&id);
                    }
                }
            }
            self.hovered = hover;
            self.dirty = true;
        }
        for (x, y, right) in clicks {
            if more_at(self, x, y) {
                self.show_all = !self.show_all;
                self.dirty = true;
                continue;
            }
            if let Some((id, hit)) = hit_at(self, x, y) {
                self.click(&id, hit, right, now);
            }
            self.dirty = true;
        }
        for (sx, sy, d) in wheel {
            let p = (sx - self.origin.0, sy - self.origin.1);
            if let Some((id, _)) = hit_at(self, p.0, p.1) {
                if let Some(c) = self.find(&id) {
                    let max = c.cache.as_ref().map_or(0.0, |(_, d)| d.max_scroll);
                    c.scroll = (c.scroll - d as f32 / 120.0 * 54.0).clamp(0.0, max);
                    self.dirty = true;
                }
            }
        }
        if !chars.is_empty() {
            self.typed(&chars, now);
        }

        // ---- timed cards leave on their own, unless the user is with them
        let due: Vec<String> = self
            .cards
            .iter()
            .filter(|c| {
                !c.leaving
                    && !c.asking()
                    && (c.brain.is_some() || !c.agent.status.is_active())
                    && c.expires.is_some_and(|t| now >= t)
                    && !self.engaged(c, now)
            })
            .map(|c| c.id.clone())
            .collect();
        for id in due {
            self.dismiss(&id);
        }

        // ---- content
        let desk = crate::apeiron::connected();
        let unix = now_unix();
        let order = self.order();
        let visible: Vec<usize> = if self.show_all {
            order.clone()
        } else {
            order.iter().copied().take(MAX_VISIBLE).collect()
        };
        for i in 0..self.cards.len() {
            let expanded = self.expanded(&self.cards[i], now);
            let c = &self.cards[i];
            let view = CardView {
                agent: c.agent.clone(),
                expanded,
                hover: self
                    .hovered
                    .as_ref()
                    .filter(|(h, _)| *h == c.id)
                    .map(|(_, h)| *h),
                focused: self.focus.as_deref() == Some(c.id.as_str()),
                keyed: self.kb.as_deref() == Some(c.id.as_str()),
                draft: c.draft.clone(),
                now: unix,
                pending_left: c.pending_left(now),
                scroll: c.scroll,
                mic: self.listening.as_deref() == Some(c.id.as_str()),
                desk,
                drive: c.drive,
                brain: c.brain.clone(),
            };
            if c.cache.as_ref().is_none_or(|(v, _)| *v != view) {
                if let Some(d) = render_card(&view, s) {
                    let reduce = self.reduce;
                    let c = &mut self.cards[i];
                    let morph = c.cache.as_ref().is_some_and(|(old, _)| morphs(old, &view));
                    if morph && !reduce {
                        c.prev = c.cache.take().map(|(_, d)| d);
                        c.xfade.snap(0.0);
                        c.xfade.set(1.0);
                    }
                    c.scroll = c.scroll.min(d.max_scroll);
                    c.cache = Some((view, d));
                    self.dirty = true;
                }
            }
        }

        // ---- targets
        let mut y = 0.0;
        for &i in &visible {
            let c = &mut self.cards[i];
            let h = c.cache.as_ref().map_or(ROW_H, |(_, d)| d.h);
            if c.fresh {
                c.fresh = false;
                c.y.snap(y);
                c.h.snap(h);
            }
            c.y.set(y);
            c.h.set(h);
            c.appear.set(1.0);
            y += h + STACK_GAP;
        }
        for (i, c) in self.cards.iter_mut().enumerate() {
            if !visible.contains(&i) {
                c.appear.set(0.0);
                if c.fresh {
                    c.fresh = false;
                    c.y.snap(y);
                }
            }
        }

        // ---- animate (or, with animations off, jump)
        let reduce = self.reduce;
        let mut moving = false;
        let mut jumped = false;
        for c in &mut self.cards {
            if reduce {
                for sp in [&mut c.y, &mut c.h, &mut c.appear, &mut c.xfade] {
                    if !sp.settled() {
                        sp.snap(sp.target);
                        jumped = true;
                    }
                }
            }
            moving |= c.y.tick(dt) | c.h.tick(dt) | c.appear.tick(dt) | c.xfade.tick(dt);
            if c.prev.is_some() && c.xfade.settled() {
                c.prev = None;
            }
        }
        self.cards
            .retain(|c| !(c.leaving && c.appear.value <= 0.01 && c.appear.target == 0.0));
        if !(moving || jumped || self.dirty) {
            return;
        }
        self.dirty = false;
        self.draw(order.len().saturating_sub(MAX_VISIBLE));
    }

    /// Gives the card window the keyboard (it is non-activating otherwise).
    fn take_keys(&self) {
        unsafe {
            let h = self.body.hwnd();
            set_activatable(h, true);
            let _ = SetForegroundWindow(h);
            let _ = SetFocus(Some(h));
        }
    }

    fn click(&mut self, id: &str, hit: Hit, right: bool, now: Instant) {
        let Some(c) = self.find(id) else { return };
        c.stale = true;
        let finished = !c.agent.status.is_active() && c.brain.is_none();
        let ask = c.agent.ask.clone();
        let keyable = c.keyable();
        if finished {
            crate::wire::mark_agent_seen(id);
        }
        if hit != Hit::Input && self.focus.as_deref() == Some(id) && hit != Hit::Btn(Btn::Send) {
            self.blur(true);
        }
        match hit {
            Hit::Card => {
                if keyable && !right {
                    self.focus = None;
                    self.kb = Some(id.to_string());
                    self.take_keys();
                }
            }
            Hit::Result => {}
            Hit::Input => {
                self.kb = None;
                self.focus = Some(id.to_string());
                self.take_keys();
            }
            Hit::Mic => crate::quick::mic_follow_up(id),
            Hit::File(i) => {
                let file = self.find(id).and_then(|c| c.agent.files.get(i).cloned());
                if let Some(f) = file {
                    if right {
                        crate::win::reveal(&f);
                    } else {
                        crate::win::open_agent_file(&f);
                    }
                }
            }
            Hit::Btn(b) => {
                if !right {
                    self.button(id, b, ask, now);
                }
            }
        }
    }

    fn button(&mut self, id: &str, b: Btn, ask: Option<Question>, now: Instant) {
        match b {
            Btn::Cancel => {
                agents::cancel(id);
                self.dismiss(id);
            }
            Btn::Stop => {
                agents::stop(id);
            }
            Btn::Retry => {
                agents::retry(id);
                self.blur(true);
            }
            Btn::Show => {
                agents::desk::show(id);
            }
            Btn::Palette => {
                crate::wire::mark_agent_seen(id);
                // The palette takes the foreground itself.
                self.blur(false);
                crate::win::push(crate::win::AppEvent::AgentPalette(id.to_string()));
                if let Some(c) = self.find(id) {
                    if !c.agent.status.is_active() {
                        // Its result is in the thread now; the line can go.
                        c.expires = Some(now + Duration::from_millis(600));
                        c.hover_until = None;
                    }
                }
            }
            Btn::Always => {
                // Never save over a config.toml that did not load (that would
                // write defaults); the click still allows this conversation.
                let saved = match crate::config::load() {
                    (_, Some(e)) => Err(e),
                    (cfg, None) if cfg.agents.always_approve => Ok(()),
                    (mut cfg, None) => {
                        cfg.agents.always_approve = true;
                        crate::config::save(&cfg).map_err(|e| e.to_string())
                    }
                };
                if let Err(e) = saved {
                    crate::config::log(&format!("cards: saving always_approve: {e}"));
                    caption::send(caption::Cmd::Notice {
                        text: format!("Allowed for this agent, but couldn't save \"always\" ({e})."),
                        id: id.to_string(),
                    });
                }
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::Allow);
                }
                self.blur(true);
            }
            Btn::Once => {
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::AllowOnce);
                }
                self.blur(true);
            }
            Btn::Deny => {
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::Deny);
                }
                self.blur(true);
            }
            Btn::Send => self.submit(id),
            Btn::Brain => {
                crate::settings::open_brain();
                self.dismiss(id);
            }
            Btn::Undo => {
                if let Some(c) = self.find(id) {
                    if let Some(b) = &mut c.brain {
                        if !b.undone {
                            if crate::brain::undo(&b.fact.id) {
                                b.undone = true;
                                c.expires = Some(now + Duration::from_millis(1400));
                                c.hover_until = None;
                            } else {
                                crate::config::log("cards: brain undo found nothing to undo");
                            }
                        }
                    }
                }
            }
        }
        self.dirty = true;
    }

    fn dismiss(&mut self, id: &str) {
        let id_owned = Some(id);
        if self.focus.as_deref() == id_owned || self.kb.as_deref() == id_owned {
            self.blur(true);
        }
        if let Some(c) = self.find(id) {
            c.leaving = true;
            c.appear.set(0.0);
        }
        self.dirty = true;
    }

    fn submit(&mut self, id: &str) {
        let Some(c) = self.find(id) else { return };
        let text = c.draft.trim().to_string();
        if text.is_empty() {
            return;
        }
        c.draft.clear();
        let q = c.agent.ask.clone().filter(|q| q.kind == AskKind::Questions);
        match q {
            Some(q) => {
                agents::answer(id, q.id, Answer::Text(text));
            }
            None => {
                agents::follow_up(id, &text);
            }
        }
        self.blur(true);
        self.dirty = true;
    }

    /// Keys on a card that has the keyboard itself (not its box): Enter =
    /// the primary action, Esc = close / dismiss, typing starts the box.
    fn keyed(&mut self, id: &str, units: &[u16], now: Instant) {
        let chars: Vec<char> = char::decode_utf16(units.iter().copied()).flatten().collect();
        for (i, &ch) in chars.iter().enumerate() {
            match ch as u32 {
                0x0D => {
                    self.primary(id, now);
                    return;
                }
                0x1B => {
                    let timed = self.find(id).is_some_and(|c| !c.agent.status.is_active());
                    if timed {
                        self.dismiss(id);
                    } else {
                        self.blur(true);
                        if let Some(c) = self.find(id) {
                            c.hover_until = None;
                        }
                    }
                    return;
                }
                u if u < 0x20 => {}
                _ => {
                    // Typing on a card with a box starts a follow-up or answer.
                    let has_box = self.find(id).is_some_and(|c| {
                        matches!(c.kind(), Kind::Questions | Kind::Done | Kind::Failed | Kind::Stopped)
                    });
                    if has_box {
                        self.kb = None;
                        self.focus = Some(id.to_string());
                        let rest: Vec<u16> = chars[i..].iter().collect::<String>().encode_utf16().collect();
                        self.typed(&rest, now);
                    }
                    return;
                }
            }
        }
    }

    /// Enter on a card: Allow, answer, open the result, or retry.
    fn primary(&mut self, id: &str, now: Instant) {
        let Some(c) = self.find(id) else { return };
        let ask = c.agent.ask.clone();
        match c.kind() {
            Kind::Approval => self.button(id, Btn::Once, ask, now),
            Kind::Questions => {
                self.kb = None;
                self.focus = Some(id.to_string());
            }
            Kind::Done | Kind::Stopped => self.button(id, Btn::Palette, ask, now),
            Kind::Failed => self.button(id, Btn::Retry, ask, now),
            Kind::Pending | Kind::Running => {}
        }
        self.dirty = true;
    }

    fn typed(&mut self, units: &[u16], now: Instant) {
        let Some(id) = self.focus.clone() else {
            if let Some(id) = self.kb.clone() {
                self.keyed(&id, units, now);
            }
            return;
        };
        for ch in char::decode_utf16(units.iter().copied()).flatten() {
            match ch as u32 {
                0x0D => {
                    self.submit(&id);
                    return;
                }
                0x1B => {
                    self.blur(true);
                    if let Some(c) = self.find(&id) {
                        c.hover_until = None;
                    }
                    return;
                }
                0x16 => {
                    let paste = clipboard_text().unwrap_or_default();
                    if let Some(c) = self.find(&id) {
                        c.draft
                            .push_str(&paste.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                }
                0x08 => {
                    if let Some(c) = self.find(&id) {
                        c.draft.pop();
                    }
                }
                0x7F => {
                    // Ctrl+Backspace: the last word.
                    if let Some(c) = self.find(&id) {
                        let t = c.draft.trim_end().to_string();
                        let cut = t.rfind(' ').map_or(0, |i| i + 1);
                        c.draft.truncate(cut);
                    }
                }
                u if u < 0x20 => {}
                _ => {
                    if let Some(c) = self.find(&id) {
                        if c.draft.chars().count() < 2000 {
                            c.draft.push(ch);
                        }
                    }
                }
            }
        }
        self.dirty = true;
    }

    /// Drops the keyboard; `restore` hands the foreground back to where it was.
    fn blur(&mut self, restore: bool) {
        let had = self.focus.take().is_some() | self.kb.take().is_some();
        if !had {
            return;
        }
        let h = self.body.hwnd();
        set_activatable(h, false);
        let prev = INPUT.with(|i| i.borrow_mut().prev_fg.take());
        unsafe {
            if restore && GetForegroundWindow() == h {
                if let Some(p) = prev {
                    let _ = SetForegroundWindow(HWND(p as *mut _));
                }
            }
        }
        self.dirty = true;
    }

    /// Where the stack hangs: below the island's pill, centred on it; with
    /// the island away, at the top of the monitor the user is on.
    fn place(&mut self) -> (i32, i32) {
        let a = crate::island::handle()
            .map(|h| h.anchor())
            .unwrap_or_default();
        if a.right > a.left {
            let cx = (a.left + a.right) / 2;
            self.mon = crate::capture::monitor_at(cx, a.top + 1);
            let s = self.mon.scale;
            let top = a.top + ((crate::island::PILL_H + HANG) * s).round() as i32;
            (cx, top)
        } else {
            if !self.body.visible() {
                self.mon = ui::monitor_under_cursor();
            }
            let m = self.mon.rect;
            let s = self.mon.scale;
            let top = m.y + (10.0 * s).round() as i32;
            (m.x + m.w / 2, top)
        }
    }

    fn draw(&mut self, overflow: usize) {
        let live = self
            .cards
            .iter()
            .any(|c| c.appear.value > 0.002 || c.appear.target > 0.0);
        if !live {
            self.body.hide();
            self.shadow.hide();
            self.body.release_memory();
            self.shadow.release_memory();
            self.shadow_key.clear();
            self.hits.clear();
            self.placed = false;
            self.glass = None;
            self.own_glass = None;
            TARGETS.lock().unwrap().clear();
            INPUT.with(|i| {
                let mut i = i.borrow_mut();
                i.inputs.clear();
                i.buttons.clear();
                i.keyable.clear();
            });
            return;
        }
        let (cx, top) = self.spot;
        let s = self.mon.scale;
        let drop = 10.0;
        let mut bottom: f32 = 0.0;
        for c in &self.cards {
            if c.appear.value > 0.002 {
                bottom = bottom.max(c.y.value + c.h.value);
            }
        }
        let more = overflow > 0;
        let more_y = bottom + STACK_GAP;
        let total = if more { more_y + MORE_H } else { bottom } + 2.0;
        let (pw, ph) = (
            (CARD_W * s).ceil() as u32,
            (total * s).ceil().max(1.0) as u32,
        );
        let Some(mut pm) = Pixmap::new(pw, ph) else {
            return;
        };
        let x0 = cx - pw as i32 / 2;

        // ---- the material: the island's glass, or our own while it's away
        let strength = crate::island::glass_strength();
        let glass = crate::island::glass().or_else(|| {
            if strength <= 0.0 {
                return None;
            }
            let stale = self
                .own_glass
                .as_ref()
                .is_none_or(|(_, t)| t.elapsed() >= Duration::from_secs(2));
            if stale {
                let pad = (30.0 * s) as i32;
                let rect = crate::capture::Rect {
                    x: x0 - pad,
                    y: top - pad,
                    w: pw as i32 + 2 * pad,
                    h: (640.0 * s) as i32 + 2 * pad,
                };
                self.own_glass = crate::capture::grab_rect(rect)
                    .ok()
                    .and_then(|shot| crate::surface::Backdrop::new(&shot, strength))
                    .map(|b| (b, Instant::now()));
            }
            self.own_glass
                .as_ref()
                .map(|(b, _)| (b.clone(), crate::surface::glass_opacity(strength)))
        });
        match &glass {
            Some((b, op)) => {
                let key = (Arc::as_ptr(b) as usize, x0, top, pw, op.to_bits());
                let fresh = self
                    .glass
                    .as_ref()
                    .is_some_and(|(k, t)| *k == key && t.height() >= ph);
                if !fresh {
                    let h = ph.max((640.0 * s) as u32);
                    self.glass = glass_texture(b, *op, x0, top, pw, h).map(|t| (key, t));
                }
            }
            None => self.glass = None,
        }
        let fill = match &self.glass {
            Some((_, t)) => Fill::Glass(t),
            None => Fill::Ink,
        };

        let mut hits = Vec::new();
        let mut targets = Vec::new();
        let mut inputs = Vec::new();
        let mut buttons = Vec::new();
        let mut keyable = Vec::new();
        let mut rects = Vec::new();
        // Bottom cards first so a dropping card slides over the one below.
        for c in self.cards.iter().rev() {
            let Some((_, d)) = &c.cache else { continue };
            let a = c.appear.value.clamp(0.0, 1.0);
            if a <= 0.002 {
                continue;
            }
            let ease = 1.0 - (1.0 - a).powi(3);
            let y = c.y.value - (1.0 - ease) * drop;
            let h = c.h.value.max(1.0);
            let prev = c.prev.as_ref().map(|p| (p, c.xfade.value));
            paint_card(&mut pm, d, prev, 0.0, y, h, ease, s, &fill);
            rects.push((y, h, ease));
            if c.leaving || a < 0.5 {
                continue;
            }
            for (hx, hy, hw, hh, hit) in &d.hits {
                let hh = hh.min(h - hy).max(0.0);
                if hh <= 0.0 {
                    continue;
                }
                hits.push((*hx, y + hy, *hw, hh, c.id.clone(), *hit));
                let r = (
                    (hx * s) as i32,
                    ((y + hy) * s) as i32,
                    (hw * s) as i32,
                    (hh * s) as i32,
                );
                match hit {
                    Hit::Input => inputs.push(r),
                    Hit::Card | Hit::Result => {}
                    _ => buttons.push(r),
                }
            }
            if c.keyable() {
                keyable.push((0, (y * s) as i32, pw as i32, (h * s) as i32));
            }
            // Alt-hold over a card speaks to its agent; the brain line has none.
            if c.brain.is_some() {
                continue;
            }
            targets.push((
                RECT {
                    left: x0,
                    top: top + (y * s) as i32,
                    right: x0 + pw as i32,
                    bottom: top + ((y + h) * s) as i32,
                },
                c.id.clone(),
            ));
        }
        self.more_rect = None;
        if more {
            let (mx, mw) = paint_more(&mut pm, overflow, self.show_all, self.more_hover, 0.0, more_y, s, &fill);
            self.more_rect = Some((mx, more_y, mw, MORE_H));
            buttons.push((
                (mx * s) as i32,
                (more_y * s) as i32,
                (mw * s) as i32,
                (MORE_H * s) as i32,
            ));
            rects.push((more_y, MORE_H, 0.6));
        }
        self.hits = hits;
        self.origin = (x0, top);
        *TARGETS.lock().unwrap() = targets;
        INPUT.with(|i| {
            let mut i = i.borrow_mut();
            i.inputs = inputs;
            i.buttons = buttons;
            i.keyable = keyable;
        });
        let first = !self.body.visible();
        self.body.present(&pm, x0, top, 1.0);

        // Shadow layer, re-rendered only when the geometry changes.
        let key: Vec<(i32, i32, i32)> = rects
            .iter()
            .map(|(y, h, a)| ((y * s) as i32, (h * s) as i32, (a * 50.0) as i32))
            .collect();
        if key != self.shadow_key || first {
            let side = (SHADOW_SIDE * s).ceil();
            let st = (SHADOW_TOP * s).ceil();
            let below = (SHADOW_BELOW * s).ceil();
            if let Some(mut sp) = Pixmap::new(pw + 2 * side as u32, ph + st as u32 + below as u32) {
                for (y, h, a) in &rects {
                    let r = RADIUS.min(h / 2.0) * s;
                    ui::soft_shadow(
                        &mut sp,
                        side,
                        st + y * s + tokens::SHADOW_Y * s,
                        pw as f32,
                        h * s,
                        r,
                        tokens::SHADOW_BLUR * s,
                        tokens::SHADOW_ALPHA * a,
                    );
                }
                self.shadow
                    .present(&sp, x0 - side as i32, top - st as i32, 1.0);
            }
            self.shadow_key = key;
        }
        if first || !self.placed {
            self.placed = true;
            unsafe {
                // Hang under the island in z-order, shadow under the cards.
                let cls = crate::util::wide("screenbuddy.island");
                if let Ok(isl) = FindWindowW(PCWSTR(cls.as_ptr()), PCWSTR::null()) {
                    self.body.place_below(isl);
                }
            }
            self.shadow.place_below(self.body.hwnd());
        }
    }

    /// None = animate at display rate; Some(t) = sleep until t (None = forever).
    fn pace(&self, now: Instant) -> Option<Option<Instant>> {
        let moving = self.cards.iter().any(|c| {
            !c.y.settled() || !c.h.settled() || !c.appear.settled() || !c.xfade.settled()
        });
        if moving {
            return None;
        }
        let shown: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| c.appear.target > 0.0 && !c.leaving)
            .collect();
        if shown.iter().any(|c| c.kind() == Kind::Pending && c.brain.is_none()) {
            return None; // the countdown ring
        }
        // Wake for the elapsed tick, hover lingering and timed cards leaving.
        let mut at: Vec<Instant> = Vec::new();
        for c in &shown {
            if c.agent.status.is_active() && c.brain.is_none() {
                let ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_millis())
                    .unwrap_or(0);
                at.push(now + Duration::from_millis(1000 - ms as u64 + 5));
            }
            if let Some(t) = c.hover_until {
                at.push(t);
            }
            if let Some(t) = c.expires {
                at.push(t);
            }
        }
        Some(at.into_iter().filter(|t| *t > now).min())
    }
}

fn clipboard_text() -> Option<String> {
    use windows::Win32::System::DataExchange::{CloseClipboard, GetClipboardData, OpenClipboard};
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
    const CF_UNICODETEXT: u32 = 13;
    unsafe {
        OpenClipboard(None).ok()?;
        let out = (|| {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let mut n = 0;
            while *p.add(n) != 0 && n < 100_000 {
                n += 1;
            }
            let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
            let _ = GlobalUnlock(g);
            Some(s)
        })();
        let _ = CloseClipboard();
        out
    }
}


// ---------------------------------------------------------------- headless previews

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn agent(status: Status) -> Agent {
        let now = now_unix();
        Agent {
            id: "20261108-101500-a1b2".into(),
            name: "Flight prices to Lisbon".into(),
            brief: "Find the cheapest flight to Lisbon next weekend".into(),
            status,
            created: now - 47,
            started: Some(now - 41),
            plan: vec![
                "Search fares on three sites".into(),
                "Compare prices and times".into(),
                "Check baggage rules".into(),
                "Write a short summary".into(),
            ],
            steps: vec![
                "Searching fares on Skyscanner".into(),
                "Comparing prices and times for 14 fares".into(),
            ],
            counted: true,
            ..Default::default()
        }
    }

    fn view(a: Agent) -> CardView {
        CardView {
            agent: a,
            expanded: true,
            hover: None,
            focused: false,
            keyed: false,
            draft: String::new(),
            now: now_unix(),
            pending_left: 3.4,
            scroll: 0.0,
            mic: false,
            desk: true,
            drive: Drive::Off,
            brain: None,
        }
    }

    fn approval_view() -> CardView {
        let mut ap = view(agent(Status::Running));
        ap.agent.name = "Tidy the Downloads folder".into();
        ap.agent.plan = vec![
            "List old installers and archives".into(),
            "Delete what's older than a year".into(),
            "Report what was freed".into(),
        ];
        ap.agent.steps = vec!["Listing old installers".into(), "Delete 212 old files".into()];
        ap.agent.ask = Some(Question {
            id: 7,
            kind: AskKind::Approval,
            text: "Delete 212 files older than a year from Downloads (3.4 GB)".into(),
            detail: "desktop delete C:\\Users\\hecto\\Downloads\\*.msi --older-than 365d".into(),
            questions: Vec::new(),
        });
        ap
    }

    fn fact() -> Fact {
        Fact {
            id: "f1".into(),
            topic: "Relay pricing".into(),
            text: "Pro is $10 a seat, was $12 until the March change".into(),
            source: crate::brain::YOU.into(),
            created: now_unix() * 1000,
            updated: now_unix() * 1000,
            ..Default::default()
        }
    }

    fn brain_view(readers: &[&str]) -> CardView {
        let mut b = view(agent(Status::Done));
        b.expanded = false;
        b.brain = Some(BrainView {
            fact: fact(),
            readers: readers.iter().map(|r| r.to_string()).collect(),
            undone: false,
        });
        b
    }

    fn has(d: &Drawn, b: Btn) -> bool {
        d.hits.iter().any(|h| h.4 == Hit::Btn(b))
    }

    /// Composites the island, shadows and cards over a backdrop, like the
    /// desktop would, with the glass made from that backdrop.
    fn composite(cards: &[CardView], more: usize, s: f32, light: bool) -> Pixmap {
        let drawn: Vec<Drawn> = cards.iter().map(|v| render_card(v, s).unwrap()).collect();
        let pad = 40.0;
        let island_h = crate::island::PILL_H;
        let top = pad + island_h + HANG;
        let mut total: f32 = drawn.iter().map(|d| d.h + STACK_GAP).sum::<f32>() - STACK_GAP;
        if more > 0 {
            total += STACK_GAP + MORE_H;
        }
        let (w, h) = (CARD_W + 2.0 * pad, top + total + pad + 16.0);
        let mut pm = Pixmap::new((w * s) as u32, (h * s) as u32).unwrap();
        let (a, b, c) = if light {
            (0xECEAE5, 0xC9D3E3, 0xE9D9C4)
        } else {
            (0x2B3140, 0x14161B, 0x3A2A33)
        };
        let (a, b, c) = (Rgba::rgb(a), Rgba::rgb(b), Rgba::rgb(c));
        let (pw, ph) = (pm.width(), pm.height());
        for (i, p) in pm.pixels_mut().iter_mut().enumerate() {
            let (x, y) = (i as u32 % pw, i as u32 / pw);
            let col = a.mix(b, y as f32 / ph as f32).mix(c, x as f32 / pw as f32 * 0.6);
            *p = tiny_skia::PremultipliedColorU8::from_rgba(col.r, col.g, col.b, 255).unwrap();
        }
        // The glass, as the island makes it from what's behind.
        let mut tex = pm.clone();
        let ink = [tokens::INK.r, tokens::INK.g, tokens::INK.b];
        let op = crate::surface::glass_opacity(60.0);
        for p in tex.pixels_mut() {
            let c = [p.red() as f32 / 255.0, p.green() as f32 / 255.0, p.blue() as f32 / 255.0];
            let [r, g, b] = crate::surface::glass_pixel(c, ink, op);
            *p = tiny_skia::PremultipliedColorU8::from_rgba(r, g, b, 255).unwrap();
        }
        let fill = Fill::Glass(&tex);
        // The island pill they hang from.
        let iw = 176.0;
        let ix = (w - iw) / 2.0;
        let r = island_h / 2.0;
        ui::shadow(&mut pm, ix * s, pad * s, iw * s, island_h * s, r * s, s);
        if let Some(p) = ui::rounded_rect_path(ix * s, pad * s, iw * s, island_h * s, r * s) {
            fill_surface(&mut pm, &p, &fill, 1.0);
        }
        ui::stroke_rounded(&mut pm, ix * s, pad * s, iw * s, island_h * s, r * s, s, tokens::LINE);
        ui::fill_circle(&mut pm, (ix + 22.0) * s, (pad + r) * s, 5.0 * s, tokens::PAPER);
        let mut y = top;
        for d in &drawn {
            ui::shadow(&mut pm, pad * s, y * s, CARD_W * s, d.h * s, RADIUS.min(d.h / 2.0) * s, s);
            y += d.h + STACK_GAP;
        }
        let mut y = top;
        for d in &drawn {
            paint_card(&mut pm, d, None, pad, y, d.h, 1.0, s, &fill);
            y += d.h + STACK_GAP;
        }
        if more > 0 {
            paint_more(&mut pm, more, false, false, pad, y, s, &fill);
        }
        pm
    }

    #[test]
    fn plan_progress_follows_steps() {
        let a = agent(Status::Running);
        assert_eq!(plan_progress(&a.plan, &a.steps), 1);
        assert_eq!(plan_progress(&a.plan, &[]), 0);
        let back = vec!["Comparing prices".into(), "Searching fares again".into()];
        assert_eq!(plan_progress(&a.plan, &back), 1, "never moves backwards");
    }

    #[test]
    fn running_is_one_line_until_opened() {
        let v = view(agent(Status::Running));
        let d = render_card(&v, 1.0).unwrap();
        assert!(d.h > 150.0 && d.h < 400.0, "{}", d.h);
        assert!(has(&d, Btn::Stop) && has(&d, Btn::Palette));
        assert!(d.hits.iter().any(|h| h.4 == Hit::Input));
        let mut slim = v.clone();
        slim.expanded = false;
        let d = render_card(&slim, 1.0).unwrap();
        assert_eq!(d.h, ROW_H);
        assert!(!has(&d, Btn::Stop), "calm by default: no buttons at rest");
        let lines = md_lines("**Cheapest**: KLM at €89\n\n- one\n- two", 300.0);
        assert!(lines.len() >= 4);
    }

    #[test]
    fn approval_offers_allow_and_dont_allow() {
        let v = approval_view();
        let d = render_card(&v, 1.0).unwrap();
        assert!(has(&d, Btn::Once) && has(&d, Btn::Deny) && has(&d, Btn::Always));
        let mut drive = v.clone();
        drive.drive = Drive::Asking;
        let d = render_card(&drive, 1.0).unwrap();
        assert!(has(&d, Btn::Once) && has(&d, Btn::Deny) && !has(&d, Btn::Always));
    }

    #[test]
    fn finished_cards_are_one_line_with_one_action() {
        let mut v = view(agent(Status::Done));
        v.expanded = false;
        let d = render_card(&v, 1.0).unwrap();
        assert_eq!(d.h, ROW_H);
        assert!(has(&d, Btn::Palette));
        let mut f = view(agent(Status::Failed));
        f.expanded = false;
        let d = render_card(&f, 1.0).unwrap();
        assert_eq!(d.h, ROW_H);
        assert!(has(&d, Btn::Retry));
    }

    #[test]
    fn brain_line_has_undo_then_see_all() {
        let v = brain_view(&["Writer"]);
        let d = render_card(&v, 1.0).unwrap();
        assert_eq!(d.h, ROW_H);
        assert!(has(&d, Btn::Undo));
        let mut open = v.clone();
        open.expanded = true;
        let d = render_card(&open, 1.0).unwrap();
        assert!(has(&d, Btn::Brain) && has(&d, Btn::Undo));
        assert!(!d.hits.iter().any(|h| h.4 == Hit::Input));
        let mut undone = v;
        undone.brain.as_mut().unwrap().undone = true;
        assert!(!has(&render_card(&undone, 1.0).unwrap(), Btn::Undo));
    }

    #[test]
    fn hotkey_presence_shows_only_notices() {
        let now = Instant::now();
        let running = Card::new(agent(Status::Running), now);
        assert!(!running.shows(Presence::Hotkey));
        assert!(running.shows(Presence::AutoHide) && running.shows(Presence::Always));
        let mut asking = Card::new(approval_view().agent, now);
        assert!(asking.shows(Presence::Hotkey));
        asking.agent.ask = None;
        asking.opened = true;
        assert!(asking.shows(Presence::Hotkey), "opened from the island");
        let done = Card::new(agent(Status::Done), now);
        assert!(done.shows(Presence::Hotkey));
        assert!(done.expires.is_some(), "a finished card fades by itself");
    }

    fn previews() -> Vec<(&'static str, Vec<CardView>, usize)> {
        let mut out = Vec::new();

        let mut p = view(agent(Status::Pending));
        p.agent.plan.clear();
        p.agent.steps.clear();
        p.agent.started = None;
        p.agent.name = "Tidy the Downloads folder".into();
        p.expanded = false;
        out.push(("pending", vec![p.clone()], 0));

        let mut r = view(agent(Status::Running));
        r.expanded = false;
        out.push(("running", vec![r.clone()], 0));
        let mut rh = view(agent(Status::Running));
        rh.hover = Some(Hit::Btn(Btn::Stop));
        out.push(("running-hover", vec![rh.clone()], 0));

        let mut drv = r.clone();
        drv.drive = Drive::Driving;
        drv.agent.name = "Fill in the expense form".into();
        out.push(("running-driving", vec![drv.clone()], 0));

        let ap = approval_view();
        out.push(("asking-approval", vec![ap.clone()], 0));
        let mut apk = ap.clone();
        apk.keyed = true;
        apk.hover = Some(Hit::Btn(Btn::Always));
        out.push(("asking-approval-keyed", vec![apk], 0));

        let mut q = view(agent(Status::Running));
        q.agent.name = "Book a table for Friday".into();
        q.agent.plan.clear();
        q.agent.ask = Some(Question {
            id: 8,
            kind: AskKind::Questions,
            text: "A few questions".into(),
            detail: String::new(),
            questions: vec![
                "How many people, and is 19:30 OK?".into(),
                "Any part of town you'd rather avoid?".into(),
            ],
        });
        q.focused = true;
        q.draft = "Four of us, 19:30 is perfect".into();
        out.push(("questions", vec![q.clone()], 0));

        let mut d = view(agent(Status::Done));
        d.agent.ended = Some(d.agent.started.unwrap() + 133);
        d.agent.result = "**Cheapest:** KLM, Sat 07:05 → 09:40, **€89** return with a cabin bag.\n\n- TAP is €96 but leaves at 06:00\n- Ryanair looks €54 until you add a bag (€118)\n\nI put the comparison in `fares.md` and a calendar hold in `trip.ics`.".into();
        d.agent.files = vec![
            PathBuf::from(r"C:\agents\x\fares.md"),
            PathBuf::from(r"C:\agents\x\trip.ics"),
            PathBuf::from(r"C:\agents\x\screenshots-of-every-fare.zip"),
        ];
        let mut dc = d.clone();
        dc.expanded = false;
        out.push(("done", vec![dc.clone()], 0));
        d.hover = Some(Hit::File(0));
        out.push(("done-hover", vec![d.clone()], 0));

        let mut long = d.clone();
        long.agent.result = (1..=14)
            .map(|i| format!("- Option {i}: a fare with some detail about times and bags"))
            .collect::<Vec<_>>()
            .join("\n");
        long.scroll = 40.0;
        long.hover = None;
        out.push(("done-scrolling", vec![long], 0));

        let mut f = view(agent(Status::Failed));
        f.agent.ended = Some(f.agent.started.unwrap() + 19);
        f.agent.error = Some("The Claude CLI exited: rate limited. Try again in a minute.".into());
        f.agent.files.clear();
        let mut fc = f.clone();
        fc.expanded = false;
        fc.hover = Some(Hit::Btn(Btn::Retry));
        out.push(("failed", vec![fc], 0));
        out.push(("failed-hover", vec![f], 0));

        out.push(("brain", vec![brain_view(&["Launch post", "Pricing page copy"])], 0));
        let mut bh = brain_view(&["Launch post", "Pricing page copy", "Investor update email", "Weekly notes"]);
        bh.expanded = true;
        bh.hover = Some(Hit::Btn(Btn::Undo));
        out.push(("brain-hover", vec![bh], 0));
        let mut none = brain_view(&[]);
        none.expanded = true;
        out.push(("brain-no-agents", vec![none], 0));

        let mut r2 = r.clone();
        r2.agent.name = "Summarise the Q3 deck".into();
        r2.agent.steps = vec!["Reading slide 12 of 40".into()];
        r2.agent.started = Some(now_unix() - 312);
        let mut d2 = dc.clone();
        d2.agent.name = "Rename the holiday photos".into();
        out.push(("stack", vec![ap.clone(), r.clone(), d2.clone()], 2));
        out.push(("stack-calm", vec![r.clone(), r2, dc], 0));
        out
    }

    /// `SCREENBUDDY_CARDS_PNG=<dir> cargo test cards` writes previews of every
    /// card state, over a light and a dark desktop.
    #[test]
    fn cards_render_previews() {
        let dir = std::env::var("SCREENBUDDY_CARDS_PNG").ok();
        let s = 2.0;
        for (name, cards, more) in previews() {
            let light = composite(&cards, more, s, true);
            let dark = composite(&cards, more, s, false);
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
                let p = std::path::Path::new(dir);
                light.save_png(p.join(format!("{name}.png"))).unwrap();
                dark.save_png(p.join(format!("{name}-dark.png"))).unwrap();
            }
        }
    }
}
