//! Agent cards: one small card per agent, hanging below the island (newest
//! on top, at most four, then "+N"). A card shows the agent's plan, live step,
//! result, files and the buttons that matter for its state (cancel during the
//! 5 s window, stop, retry, dismiss, show its desk, open in the palette), a
//! follow-up box with a mic, and approval / question asks, so those surface
//! even with the palette closed. Holding Alt over a card speaks a follow-up to
//! that agent (`keys::set_cursor_target` + `quick`).
//!
//! Runs on its own thread with two layered windows (`screenbuddy.cards` for
//! the cards, click-through `screenbuddy.cards-shadow` under it), rendered with
//! tiny-skia. It draws only while something moves; at rest it sleeps until an
//! agent event, the mouse or the next readout second.
//!
//! The card window is non-activating, except while its follow-up box has
//! focus: a click there turns activation on so keys reach it, and losing focus
//! (Enter, Esc, clicking elsewhere) turns it off again.
//!
//! API: [`start`] (once), [`open`] (expand an agent's card, e.g. from the
//! island), [`target_at`] (the agent whose card is under a screen point).

use crate::agents::drive::DriveEvent;
use crate::agents::{self, Agent, AgentEvent, Answer, AskKind, Question, Status};
use crate::caption::{self, Block, Span};
use crate::ui::tokens::{self, Rgba};
use crate::ui::{self, Face, Layer, MonitorInfo, Spring, TextStyle};
use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, Once};
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
/// Between cards in the stack.
const STACK_GAP: f32 = 8.0;
/// Below the island's pill.
const HANG: f32 = 10.0;
const SLIM_H: f32 = 40.0;
const LINE: f32 = 18.0;
const BTN_H: f32 = 28.0;
const INPUT_H: f32 = 34.0;
const CHIP_H: f32 = 26.0;
const RESULT_MAX_H: f32 = 168.0;
const MORE_H: f32 = 26.0;
const MAX_VISIBLE: usize = 4;
const PLAN_ROWS: usize = 5;
const IDLE_COLLAPSE: Duration = Duration::from_secs(20);
/// A card stays open this long after the cursor leaves it.
const HOVER_LINGER: Duration = Duration::from_millis(700);
const SHADOW_SIDE: f32 = 28.0;
const SHADOW_TOP: f32 = 12.0;
const SHADOW_BELOW: f32 = 40.0;

/// Brighter hairlines (button outlines).
const LINE_2: Rgba = Rgba::rgba(0xFFFFFF, 34);
const LINE_3: Rgba = Rgba::rgba(0xFFFFFF, 52);

// ---------------------------------------------------------------- view (pure data)

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Btn {
    Cancel,
    Stop,
    Retry,
    Dismiss,
    Show,
    Palette,
    Always,
    Once,
    Deny,
    Send,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hit {
    /// Anywhere else on the card (a slim row expands on click).
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
    focused: bool,
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
    edge: Rgba,
}

fn label_style(s: f32) -> TextStyle {
    TextStyle::label(s)
}

fn body_style(s: f32) -> TextStyle {
    TextStyle::body(s)
}

fn small_style(s: f32) -> TextStyle {
    TextStyle::body(s).size(11.5 * s).color(tokens::MUTE)
}

fn name_style(s: f32) -> TextStyle {
    TextStyle {
        face: Face::MonoMedium,
        ..TextStyle::body(s).size(13.5 * s)
    }
}

/// Logical width of `t` in `st` (built at scale `s`).
fn tw(t: &str, st: &TextStyle, s: f32) -> f32 {
    ui::text_width(t, st) / s
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

    fn stroke(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
        let s = self.s;
        if let Some(pm) = self.pm.as_deref_mut() {
            ui::stroke_rounded(pm, x * s, y * s, w * s, h * s, r * s, s.max(1.0), c);
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
        format!("{:02}:{:02}", secs / 60, secs % 60)
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
        Run::Bold => TextStyle {
            face: Face::MonoMedium,
            ..body_style(s)
        },
        Run::Code => body_style(s).color(tokens::PAPER.mix(tokens::MUTE, 0.35)),
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

fn draw_md(pen: &mut Pen, lines: &[MdLine], x: f32, mut y: f32) {
    let s = pen.s;
    for l in lines {
        if l.gap {
            y += l.h();
            continue;
        }
        if l.code_block {
            pen.rect(x, y, CARD_W - 2.0 * PAD, LINE, tokens::INK_2);
            pen.rect(x, y, 2.0, LINE, tokens::LINE);
        }
        if let Some(m) = &l.marker {
            pen.line(m, x, y, &body_style(s).color(tokens::MUTE));
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
enum BtnLook {
    Primary,
    Accent,
    Ghost,
    Quiet,
}

/// A pill button at (x, y); `w` None = fit the label. Returns its width.
#[allow(clippy::too_many_arguments)]
fn button(
    pen: &mut Pen,
    x: f32,
    y: f32,
    w: Option<f32>,
    label: &str,
    look: BtnLook,
    btn: Btn,
    hover: Option<Hit>,
    ring: Option<f32>,
) -> f32 {
    let s = pen.s;
    let hovered = hover == Some(Hit::Btn(btn));
    let ls = label_style(s);
    let lw = tw(label, &ls, s);
    let ring_w = if ring.is_some() { 18.0 } else { 0.0 };
    let w = w.unwrap_or(lw + ring_w + 26.0);
    let r = BTN_H / 2.0;
    let fg = match look {
        BtnLook::Primary => {
            let c = if hovered {
                tokens::PAPER
            } else {
                tokens::PAPER.mix(tokens::MUTE, 0.08)
            };
            pen.rounded(x, y, w, BTN_H, r, c);
            tokens::INK
        }
        BtnLook::Accent => {
            let c = if hovered {
                tokens::ASK.mix(tokens::PAPER, 0.12)
            } else {
                tokens::ASK
            };
            pen.rounded(x, y, w, BTN_H, r, c);
            Rgba::rgb(0xFFFFFF)
        }
        BtnLook::Ghost => {
            if hovered {
                pen.rounded(x, y, w, BTN_H, r, tokens::INK_2);
            }
            pen.stroke(x, y, w, BTN_H, r, if hovered { LINE_3 } else { LINE_2 });
            tokens::PAPER
        }
        BtnLook::Quiet => {
            if hovered {
                pen.rounded(x, y, w, BTN_H, r, tokens::INK_2);
            }
            if hovered { tokens::PAPER } else { tokens::MUTE }
        }
    };
    let content = lw + ring_w;
    let mut cx = x + (w - content) / 2.0;
    let mid = y + BTN_H / 2.0;
    if let Some(left) = ring {
        let rc = cx + 6.0;
        pen.arc(rc, mid, 5.5, 1.0, 1.4, LINE_2);
        pen.arc(
            rc,
            mid,
            5.5,
            (left / 5.0).clamp(0.0, 1.0),
            1.6,
            tokens::SIGNAL,
        );
        cx += ring_w;
    }
    pen.text_mid(label, cx, mid, &ls.color(fg));
    pen.hit(x, y, w, BTN_H, Hit::Btn(btn));
    w
}

/// The follow-up / answer box with its mic (or send, once there is text).
fn input_box(pen: &mut Pen, v: &CardView, y: f32, placeholder: &str) {
    let s = pen.s;
    let (x, w) = (PAD, CARD_W - 2.0 * PAD);
    let hovered = v.hover == Some(Hit::Input);
    let fill = if v.focused || hovered {
        tokens::INK_2.mix(tokens::PAPER, 0.03)
    } else {
        tokens::INK_2
    };
    pen.rounded(x, y, w, INPUT_H, 10.0, fill);
    if v.focused {
        pen.stroke(x, y, w, INPUT_H, 10.0, tokens::PAPER.fade(0.22));
    }
    let btn_d = 24.0;
    let bx = x + w - 5.0 - btn_d;
    let by = y + (INPUT_H - btn_d) / 2.0;
    let text_x = x + 12.0;
    let room = bx - 8.0 - text_x;
    let bs = body_style(s);
    let mid = y + INPUT_H / 2.0;
    if v.draft.is_empty() {
        let ph = if v.mic { "Listening…" } else { placeholder };
        let c = if v.mic { tokens::SIGNAL } else { tokens::MUTE };
        pen.text_mid(
            ph,
            text_x,
            mid,
            &bs.color(c.fade(if v.focused { 0.6 } else { 1.0 })),
        );
        if v.focused {
            pen.rect(text_x - 1.0, mid - 7.5, 1.5, 15.0, tokens::SIGNAL);
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
        let w = pen.text_mid(&shown, text_x, mid, &bs);
        if v.focused {
            pen.rect(text_x + w + 1.5, mid - 7.5, 1.5, 15.0, tokens::SIGNAL);
        }
    }
    pen.hit(x, y, bx - x - 2.0, INPUT_H, Hit::Input);

    // Mic, or send when there's something to send.
    let (cx, cy) = (bx + btn_d / 2.0, by + btn_d / 2.0);
    if !v.draft.trim().is_empty() {
        let hov = v.hover == Some(Hit::Btn(Btn::Send));
        let c = if hov {
            tokens::PAPER
        } else {
            tokens::PAPER.mix(tokens::MUTE, 0.1)
        };
        pen.circle(cx, cy, btn_d / 2.0, c);
        pen.poly(&[(cx, cy + 5.0), (cx, cy - 5.0)], 1.7, tokens::INK);
        pen.poly(
            &[(cx - 4.0, cy - 1.0), (cx, cy - 5.0), (cx + 4.0, cy - 1.0)],
            1.7,
            tokens::INK,
        );
        pen.hit(bx - 2.0, y, btn_d + 7.0, INPUT_H, Hit::Btn(Btn::Send));
    } else {
        let hov = v.hover == Some(Hit::Mic);
        let (bg, fg) = if v.mic {
            (tokens::SIGNAL, tokens::INK)
        } else if hov {
            (tokens::PAPER.fade(0.12), tokens::PAPER)
        } else {
            (Rgba::rgba(0, 0), tokens::MUTE)
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

fn file_chips(pen: &mut Pen, v: &CardView, mut y: f32) -> f32 {
    let s = pen.s;
    let files = &v.agent.files;
    if files.is_empty() {
        return y;
    }
    let (x0, right) = (PAD, CARD_W - PAD);
    let es = label_style(s).size(9.0 * s);
    let ns = body_style(s).size(12.0 * s);
    let mut x = x0;
    let mut rows = 1;
    for (i, f) in files.iter().enumerate() {
        let name = f
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| f.display().to_string());
        let (stem, ext) = match name.rsplit_once('.') {
            Some((a, b)) if !a.is_empty() && b.len() <= 4 => (a.to_string(), b.to_string()),
            _ => (
                name.clone(),
                if f.is_dir() {
                    "dir".into()
                } else {
                    String::new()
                },
            ),
        };
        let ew = if ext.is_empty() {
            0.0
        } else {
            tw(&ext, &es, s) + 7.0
        };
        let stem = ui::ellipsize(&stem, &ns, 150.0 * s);
        let w = 10.0 + ew + tw(&stem, &ns, s) + 10.0;
        let rest = files.len() - i;
        let more_w = 44.0;
        if x > x0 && x + w > right - if rest > 1 && rows == 2 { more_w } else { 0.0 } {
            if rows == 2 {
                // Out of room: the rest go into "+N".
                let label = format!("+{rest}");
                let hov = v.hover == Some(Hit::Btn(Btn::Palette));
                let c = if hov { tokens::PAPER } else { tokens::MUTE };
                pen.rounded(
                    x,
                    y,
                    more_w - 6.0,
                    CHIP_H,
                    tokens::RADIUS_CHIP,
                    tokens::INK_2,
                );
                let lw = tw(&label, &ns, s);
                pen.text_mid(
                    &label,
                    x + (more_w - 6.0 - lw) / 2.0,
                    y + CHIP_H / 2.0,
                    &ns.color(c),
                );
                pen.hit(x, y, more_w - 6.0, CHIP_H, Hit::Btn(Btn::Palette));
                break;
            }
            rows += 1;
            x = x0;
            y += CHIP_H + 6.0;
        }
        let hov = v.hover == Some(Hit::File(i));
        let bg = if hov {
            tokens::INK_2.mix(tokens::PAPER, 0.07)
        } else {
            tokens::INK_2
        };
        pen.rounded(x, y, w, CHIP_H, tokens::RADIUS_CHIP, bg);
        let mid = y + CHIP_H / 2.0;
        let mut tx = x + 10.0;
        if !ext.is_empty() {
            tx += pen.text_mid(&ext, tx, mid, &es.color(tokens::MUTE)) + 7.0;
        }
        pen.text_mid(&stem, tx, mid, &ns.color(tokens::PAPER));
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

/// Header colour + label for the state.
fn status_label(v: &CardView, k: Kind) -> (String, Rgba) {
    let a = &v.agent;
    match k {
        Kind::Pending => ("Starting".into(), tokens::MUTE),
        Kind::Running if v.drive == Drive::Driving => ("Driving…".into(), tokens::SIGNAL),
        Kind::Running if v.drive == Drive::Paused => ("Driving · paused".into(), tokens::ASK),
        Kind::Running => {
            if a.plan.len() > 1 {
                let i = plan_progress(&a.plan, &a.steps) + 1;
                (format!("Working · {i}/{}", a.plan.len()), tokens::SIGNAL)
            } else {
                ("Working".into(), tokens::SIGNAL)
            }
        }
        Kind::Approval => ("Needs your OK".into(), tokens::ASK),
        Kind::Questions => {
            let n = a.ask.as_ref().map_or(0, |q| q.questions.len());
            let l = if n == 1 { "A question" } else { "Questions" };
            (l.into(), tokens::ASK)
        }
        Kind::Done => ("Done".into(), tokens::OK),
        Kind::Failed => ("Failed".into(), tokens::ERR),
        Kind::Stopped => {
            let l = if a.counted {
                "Stopped"
            } else {
                "Stopped · didn't count"
            };
            (l.into(), tokens::MUTE)
        }
    }
}

fn dot_color(k: Kind) -> Rgba {
    match k {
        Kind::Running => tokens::SIGNAL,
        Kind::Approval | Kind::Questions => tokens::ASK,
        Kind::Done => tokens::OK,
        Kind::Failed => tokens::ERR,
        Kind::Pending | Kind::Stopped => tokens::MUTE,
    }
}

/// The Doto readout: elapsed (counting down while pending).
fn readout(v: &CardView, k: Kind) -> (String, Rgba) {
    let a = &v.agent;
    if k == Kind::Pending {
        let left = v.pending_left.ceil().max(0.0) as u64;
        return (elapsed_label(left), tokens::SIGNAL);
    }
    let start = a.started.unwrap_or(a.created);
    let end = if a.status.is_active() {
        v.now
    } else {
        a.ended.unwrap_or(v.now)
    };
    let c = if a.status.is_active() {
        tokens::PAPER
    } else {
        tokens::MUTE.mix(tokens::PAPER, 0.25)
    };
    (elapsed_label(end.saturating_sub(start)), c)
}

/// Lays out (and with a pixmap, draws) one card's content.
fn compose(v: &CardView, pen: &mut Pen) -> (f32, f32) {
    let s = pen.s;
    let a = &v.agent;
    let k = kind(a);
    let inner = CARD_W - 2.0 * PAD;
    let dot = dot_color(k);
    let (rtext, rcolor) = readout(v, k);
    let rs = TextStyle::readout(s).size(16.0 * s).color(rcolor);
    let rw = tw(&rtext, &rs, s);

    if !v.expanded {
        // Slim row: dot · name ······ readout.
        let mid = SLIM_H / 2.0;
        if k == Kind::Running {
            pen.circle(PAD + 3.0, mid, 6.0, dot.fade(0.18));
        }
        pen.circle(PAD + 3.0, mid, 3.0, dot);
        let right = CARD_W - PAD;
        pen.text_mid(&rtext, right - rw, mid, &rs);
        let mut name_right = right - rw - 12.0;
        if matches!(k, Kind::Approval | Kind::Questions) {
            let ls = label_style(s).color(tokens::ASK);
            let t = "Ask";
            let w = tw(t, &ls, s);
            pen.text_mid(t, name_right - w, mid, &ls);
            name_right -= w + 12.0;
        }
        let ns = body_style(s);
        let name = ui::ellipsize(&a.name, &ns, (name_right - PAD - 16.0) * s);
        pen.text_mid(&name, PAD + 16.0, mid, &ns);
        pen.hit(0.0, 0.0, CARD_W, SLIM_H, Hit::Card);
        return (SLIM_H, 0.0);
    }

    pen.hit(0.0, 0.0, CARD_W, 10_000.0, Hit::Card);
    let mut y = 16.0;
    // ---- header: ● LABEL ········ 00:41
    let head_mid = y + 7.0;
    if k == Kind::Running {
        pen.circle(PAD + 3.0, head_mid, 6.0, dot.fade(0.18));
    }
    pen.circle(PAD + 3.0, head_mid, 3.0, dot);
    let (label, lcolor) = status_label(v, k);
    let ls = label_style(s).color(lcolor);
    let label = ui::ellipsize(&label, &ls, (inner - 16.0 - rw - 12.0) * s);
    pen.text_mid(&label, PAD + 14.0, head_mid, &ls);
    pen.text_mid(&rtext, CARD_W - PAD - rw, head_mid, &rs);
    y += 22.0;
    // ---- name
    let ns = name_style(s);
    for l in wrap_max(&a.name, &ns, inner, s, 2) {
        pen.line(&l, PAD, y, &ns);
        y += LINE + 1.0;
    }
    y += 9.0;

    let mut max_scroll = 0.0;
    let section = |pen: &mut Pen, y: &mut f32| {
        pen.rect(PAD, *y, inner, 1.0 / s, tokens::LINE);
        *y += 12.0;
    };

    // ---- plan
    let show_plan = !a.plan.is_empty()
        && matches!(
            k,
            Kind::Running | Kind::Approval | Kind::Questions | Kind::Pending
        );
    if show_plan {
        section(pen, &mut y);
        let cur = if k == Kind::Pending {
            0
        } else {
            plan_progress(&a.plan, &a.steps)
        };
        let first = if a.plan.len() > PLAN_ROWS {
            cur.saturating_sub(1).min(a.plan.len() - PLAN_ROWS)
        } else {
            0
        };
        let ps = body_style(s).size(12.0 * s);
        for (i, item) in a.plan.iter().enumerate().skip(first).take(PLAN_ROWS) {
            let mid = y + LINE / 2.0;
            let mx = PAD + 3.0;
            let (tc, text) = if i < cur {
                pen.poly(
                    &[
                        (mx - 3.2, mid + 0.2),
                        (mx - 0.9, mid + 2.5),
                        (mx + 3.4, mid - 2.6),
                    ],
                    1.4,
                    tokens::MUTE,
                );
                (tokens::MUTE, item.as_str())
            } else if i == cur && k != Kind::Pending {
                // Waiting on the user: the step holds in the ask colour.
                let c = if k == Kind::Running {
                    tokens::SIGNAL
                } else {
                    tokens::ASK
                };
                pen.circle(mx, mid, 6.0, c.fade(0.18));
                pen.circle(mx, mid, 3.0, c);
                (tokens::PAPER, item.as_str())
            } else {
                let ring = |pen: &mut Pen| pen.arc(mx, mid, 2.8, 1.0, 1.1, tokens::MUTE.fade(0.7));
                ring(pen);
                (tokens::MUTE.fade(0.75), item.as_str())
            };
            let t = ui::ellipsize(text, &ps, (inner - 16.0) * s);
            pen.line(&t, PAD + 16.0, y, &ps.color(tc));
            y += LINE + 2.0;
        }
        if a.plan.len() > PLAN_ROWS {
            let more = a.plan.len() - first - PLAN_ROWS.min(a.plan.len() - first);
            if more > 0 {
                let st = small_style(s).color(tokens::MUTE.fade(0.7));
                pen.line(&format!("+{more} more"), PAD + 16.0, y - 2.0, &st);
                y += LINE - 2.0;
            }
        }
        y += 4.0;
    }

    // ---- live step
    if k == Kind::Running {
        if let Some(step) = a.steps.last() {
            if !show_plan {
                section(pen, &mut y);
            }
            let st = body_style(s)
                .size(12.0 * s)
                .color(tokens::PAPER.mix(tokens::MUTE, 0.4));
            let lines = wrap_max(step, &st, inner - 16.0, s, 2);
            pen.line("›", PAD + 1.0, y, &st.color(tokens::SIGNAL));
            for l in lines {
                pen.line(&l, PAD + 16.0, y, &st);
                y += LINE;
            }
            y += 8.0;
        }
    }

    // ---- asks
    if let (Some(q), Kind::Approval) = (&a.ask, k) {
        y = approval(pen, v, q, y);
    }
    if let (Some(q), Kind::Questions) = (&a.ask, k) {
        section(pen, &mut y);
        let qs = body_style(s);
        let num = TextStyle::readout(s).size(15.0 * s).color(tokens::ASK);
        let list: Vec<&String> = if q.questions.is_empty() {
            vec![&q.text]
        } else {
            q.questions.iter().collect()
        };
        let many = list.len() > 1;
        for (i, item) in list.iter().enumerate() {
            let x = if many {
                pen.line(&format!("{}", i + 1), PAD, y, &num);
                PAD + 16.0
            } else {
                PAD
            };
            for l in wrap_max(item, &qs, CARD_W - PAD - x, s, 4) {
                pen.line(&l, x, y, &qs);
                y += LINE;
            }
            y += 6.0;
        }
        y += 4.0;
        input_box(pen, v, y, "Your answer…");
        y += INPUT_H + 10.0;
    }

    // ---- result
    if k == Kind::Done {
        section(pen, &mut y);
        let body = if a.result.trim().is_empty() {
            "Done."
        } else {
            a.result.trim()
        };
        let lines = md_lines(body, inner);
        let total: f32 = lines.iter().map(|l| l.h()).sum();
        let vh = total.min(RESULT_MAX_H);
        max_scroll = (total - vh).max(0.0);
        let scroll = v.scroll.clamp(0.0, max_scroll);
        if pen.pm.is_some() {
            if let Some(mut sub) = Pixmap::new(
                ((CARD_W - PAD) * s).ceil() as u32,
                (vh * s).ceil().max(1.0) as u32,
            ) {
                let mut sp = Pen {
                    pm: Some(&mut sub),
                    s,
                    hits: Vec::new(),
                };
                draw_md(&mut sp, &lines, PAD, -scroll);
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
            pen.rounded(CARD_W - 7.0, ty, 2.5, th, 1.25, tokens::MUTE.fade(0.45));
        }
        pen.hit(0.0, y, CARD_W, vh, Hit::Result);
        y += vh + 12.0;
    }

    // ---- error
    if k == Kind::Failed {
        section(pen, &mut y);
        let es = body_style(s).color(tokens::ERR.mix(tokens::PAPER, 0.45));
        let err = a.error.as_deref().unwrap_or("Something went wrong.");
        for l in wrap_max(err, &es, inner, s, 4) {
            pen.line(&l, PAD, y, &es);
            y += LINE;
        }
        y += 10.0;
    }

    // ---- files
    if !a.files.is_empty() && matches!(k, Kind::Done | Kind::Running | Kind::Stopped | Kind::Failed)
    {
        y = file_chips(pen, v, y) + 12.0;
    }

    // ---- follow-up
    if matches!(k, Kind::Running | Kind::Done | Kind::Stopped | Kind::Failed) {
        let ph = if k == Kind::Running {
            "Add to it…"
        } else {
            "Follow up…"
        };
        input_box(pen, v, y, ph);
        y += INPUT_H + 10.0;
    }

    // ---- buttons
    if k != Kind::Approval {
        let h = v.hover;
        let mut x = PAD;
        let gap = 6.0;
        let mut right = CARD_W - PAD;
        let mut push_right = |pen: &mut Pen, label: &str, look: BtnLook, btn: Btn| {
            let st = label_style(s);
            let w = tw(label, &st, s) + 26.0;
            right -= w;
            button(pen, right, y, Some(w), label, look, btn, h, None);
            right -= gap;
        };
        match k {
            Kind::Pending => {
                x += button(
                    pen,
                    x,
                    y,
                    None,
                    "Cancel",
                    BtnLook::Ghost,
                    Btn::Cancel,
                    h,
                    Some(v.pending_left),
                ) + gap;
            }
            Kind::Running | Kind::Questions => {
                x += button(pen, x, y, None, "Stop", BtnLook::Ghost, Btn::Stop, h, None) + gap;
                if v.desk {
                    x += button(pen, x, y, None, "Show", BtnLook::Quiet, Btn::Show, h, None) + gap;
                }
                push_right(pen, "Palette ›", BtnLook::Quiet, Btn::Palette);
            }
            Kind::Done => {
                if v.desk {
                    x += button(pen, x, y, None, "Show", BtnLook::Ghost, Btn::Show, h, None) + gap;
                }
                x += button(
                    pen,
                    x,
                    y,
                    None,
                    "Palette ›",
                    BtnLook::Quiet,
                    Btn::Palette,
                    h,
                    None,
                ) + gap;
                push_right(pen, "Dismiss", BtnLook::Quiet, Btn::Dismiss);
            }
            Kind::Failed | Kind::Stopped => {
                let look = if k == Kind::Failed {
                    BtnLook::Primary
                } else {
                    BtnLook::Ghost
                };
                x += button(pen, x, y, None, "Retry", look, Btn::Retry, h, None) + gap;
                x += button(
                    pen,
                    x,
                    y,
                    None,
                    "Palette ›",
                    BtnLook::Quiet,
                    Btn::Palette,
                    h,
                    None,
                ) + gap;
                push_right(pen, "Dismiss", BtnLook::Quiet, Btn::Dismiss);
            }
            Kind::Approval => {}
        }
        let _ = x;
        y += BTN_H;
    }
    (y + 14.0, max_scroll)
}

/// The approval ask: what it wants to do, then Always / Once / Don't.
fn approval(pen: &mut Pen, v: &CardView, q: &Question, mut y: f32) -> f32 {
    let s = pen.s;
    let inner = CARD_W - 2.0 * PAD;
    let ip = 12.0;
    let tw_ = inner - 2.0 * ip;
    let is = body_style(s);
    let ds = TextStyle::body(s).size(11.0 * s).color(tokens::MUTE);
    let intent = wrap_max(&q.text, &is, tw_, s, 3);
    let detail = if q.detail.trim().is_empty() {
        Vec::new()
    } else {
        wrap_max(&q.detail.replace('\n', " "), &ds, tw_, s, 2)
    };
    let dl = 15.0;
    let ph = ip
        + intent.len() as f32 * LINE
        + if detail.is_empty() {
            0.0
        } else {
            6.0 + detail.len() as f32 * dl
        }
        + ip;
    pen.rounded(PAD, y, inner, ph, 12.0, tokens::ASK.fade(0.11));
    pen.stroke(PAD, y, inner, ph, 12.0, tokens::ASK.fade(0.28));
    let mut ty = y + ip;
    for l in &intent {
        pen.line(l, PAD + ip, ty, &is);
        ty += LINE;
    }
    if !detail.is_empty() {
        ty += 6.0;
        for l in &detail {
            let lh = ui::line_height(&ds) / s;
            pen.text(l, PAD + ip, ty + (dl - lh) / 2.0, &ds);
            ty += dl;
        }
    }
    y += ph + 12.0;
    let h = v.hover;
    if matches!(v.drive, Drive::Asking | Drive::Paused) {
        // The drive machine remembers its own yes for the conversation;
        // agents.always_approve doesn't cover driving.
        let half = (inner - 6.0) / 2.0;
        button(
            pen,
            PAD,
            y,
            Some(half),
            "Allow",
            BtnLook::Accent,
            Btn::Once,
            h,
            None,
        );
        button(
            pen,
            PAD + half + 6.0,
            y,
            Some(half),
            "Don't allow",
            BtnLook::Quiet,
            Btn::Deny,
            h,
            None,
        );
        return y + BTN_H;
    }
    button(
        pen,
        PAD,
        y,
        Some(inner),
        "Always approve",
        BtnLook::Accent,
        Btn::Always,
        h,
        None,
    );
    y += BTN_H + 6.0;
    let half = (inner - 6.0) / 2.0;
    button(
        pen,
        PAD,
        y,
        Some(half),
        "Approve once",
        BtnLook::Ghost,
        Btn::Once,
        h,
        None,
    );
    button(
        pen,
        PAD + half + 6.0,
        y,
        Some(half),
        "Don't allow",
        BtnLook::Quiet,
        Btn::Deny,
        h,
        None,
    );
    y + BTN_H
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
    let edge = match kind(&v.agent) {
        Kind::Approval | Kind::Questions => tokens::ASK.fade(0.45),
        _ => tokens::LINE,
    };
    Some(Drawn {
        pm,
        h,
        hits,
        max_scroll,
        edge,
    })
}

/// Draws one card's surface at (x, y) with height `h` (logical; may be
/// shorter than the content while it animates) and its content clipped to it.
#[allow(clippy::too_many_arguments)]
fn paint_card(pm: &mut Pixmap, d: &Drawn, x: f32, y: f32, h: f32, alpha: f32, s: f32) {
    if alpha <= 0.002 || h <= 0.5 {
        return;
    }
    let (px, py, pw, ph) = (x * s, y * s, CARD_W * s, h * s);
    let r = tokens::RADIUS_CARD.min(h / 2.0) * s;
    ui::fill_rounded(pm, px, py, pw, ph, r, tokens::INK.fade(alpha));
    if let (Some(mut mask), Some(path)) = (
        tiny_skia::Mask::new(pm.width(), pm.height()),
        ui::rounded_rect_path(px, py, pw, ph, r),
    ) {
        mask.fill_path(
            &path,
            tiny_skia::FillRule::Winding,
            true,
            Transform::identity(),
        );
        let paint = PixmapPaint {
            opacity: alpha.clamp(0.0, 1.0),
            ..Default::default()
        };
        pm.draw_pixmap(
            px.round() as i32,
            py.round() as i32,
            d.pm.as_ref(),
            &paint,
            Transform::identity(),
            Some(&mask),
        );
    }
    ui::stroke_rounded(pm, px, py, pw, ph, r, s.max(1.0), d.edge.fade(alpha));
}

/// The "+N" pill under the stack.
fn render_more(n: usize, open: bool, hover: bool, s: f32) -> Option<Pixmap> {
    let label = if open {
        "Show fewer".to_string()
    } else {
        format!("+{n} more")
    };
    let ls = label_style(s).color(if hover { tokens::PAPER } else { tokens::MUTE });
    let w = tw(&label, &ls, s) + 28.0;
    let mut pm = Pixmap::new((CARD_W * s).ceil() as u32, (MORE_H * s).ceil() as u32)?;
    let x = (CARD_W - w) / 2.0;
    ui::fill_rounded(
        &mut pm,
        x * s,
        0.0,
        w * s,
        MORE_H * s,
        MORE_H / 2.0 * s,
        tokens::INK,
    );
    ui::stroke_rounded(
        &mut pm,
        x * s,
        0.0,
        w * s,
        MORE_H * s,
        MORE_H / 2.0 * s,
        s.max(1.0),
        tokens::LINE,
    );
    let base = (MORE_H / 2.0 * s + ui::cap_height(&ls) / 2.0).round();
    ui::draw_text_baseline(&mut pm, &label, ((x + 14.0) * s).round(), base, &ls);
    Some(pm)
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
                let on_input = INPUT.with(|i| i.borrow().inputs.iter().any(|r| inside(r, p)));
                if on_input {
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

struct Card {
    id: String,
    agent: Agent,
    stale: bool,
    pending_at: Instant,
    activity: Instant,
    hover_until: Option<Instant>,
    y: Spring,
    h: Spring,
    appear: Spring,
    leaving: bool,
    fresh: bool,
    draft: String,
    scroll: f32,
    drive: Drive,
    cache: Option<(CardView, Drawn)>,
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
        Self {
            id: agent.id.clone(),
            agent,
            stale: false,
            pending_at,
            activity: now,
            hover_until: None,
            y: Spring::new(0.0),
            h: Spring::new(SLIM_H),
            appear: Spring::new(0.0).with(300.0, 26.0),
            leaving: false,
            fresh: true,
            draft: String::new(),
            scroll: 0.0,
            drive: Drive::Off,
            cache: None,
        }
    }

    fn kind(&self) -> Kind {
        kind(&self.agent)
    }

    fn pending_left(&self, now: Instant) -> f32 {
        (agents::PENDING_WINDOW.as_secs_f32()
            - now.saturating_duration_since(self.pending_at).as_secs_f32())
        .max(0.0)
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

struct Cards {
    cards: Vec<Card>,
    body: Layer,
    shadow: Layer,
    mon: MonitorInfo,
    /// Card id with the focused follow-up box.
    focus: Option<String>,
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
        card.activity = now;
        match &ev {
            AgentEvent::Status(Status::Pending) => card.pending_at = now,
            AgentEvent::Status(Status::Running) => card.scroll = 0.0,
            AgentEvent::Status(s) if !s.is_active() => card.drive = Drive::Off,
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

    /// The user opened a card (island row, notice, click).
    fn open(&mut self, id: &str, now: Instant) {
        if let Some(c) = self.ensure(id, now) {
            c.activity = now;
            c.stale = true;
            if !c.agent.status.is_active() {
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

    /// Indices of live cards in display order: asks first, then newest first.
    fn order(&self) -> Vec<usize> {
        let mut idx: Vec<usize> = (0..self.cards.len())
            .filter(|&i| !self.cards[i].leaving)
            .collect();
        idx.sort_by_key(|&i| {
            let c = &self.cards[i];
            let asking = matches!(c.kind(), Kind::Approval | Kind::Questions);
            (
                !asking,
                std::cmp::Reverse(c.agent.created),
                std::cmp::Reverse(c.id.clone()),
            )
        });
        idx
    }

    fn expanded(&self, c: &Card, now: Instant) -> bool {
        let forced = matches!(c.kind(), Kind::Pending | Kind::Approval | Kind::Questions)
            || self.focus.as_deref() == Some(c.id.as_str())
            || self.listening.as_deref() == Some(c.id.as_str())
            || c.hover_until.is_some_and(|t| now < t)
            || self.hovered.as_ref().is_some_and(|(h, _)| *h == c.id);
        forced || now.saturating_duration_since(c.activity) < IDLE_COLLAPSE
    }

    fn frame(&mut self, now: Instant) {
        let dt = now
            .saturating_duration_since(self.last)
            .as_secs_f32()
            .min(0.05);
        self.last = now;

        // ---- inbox
        let msgs = std::mem::take(&mut *INBOX.lock().unwrap());
        for m in msgs {
            match m {
                Msg::Event(id, ev) => self.on_event(id, ev, now),
                Msg::Open(id) => self.open(&id, now),
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
        if blurred && self.focus.is_some() {
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
            // Leaving a card: it stays open a moment (gaps, small moves).
            if let Some((old, _)) = &self.hovered {
                if hover.as_ref().is_none_or(|(n, _)| n != old) {
                    if let Some(c) = self.cards.iter_mut().find(|c| &c.id == old) {
                        c.hover_until = Some(now + HOVER_LINGER);
                    }
                }
            }
            if let Some((id, _)) = &hover {
                let id = id.clone();
                if let Some(c) = self.find(&id) {
                    c.hover_until = None;
                    if !c.agent.status.is_active() {
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
                    c.activity = now;
                    self.dirty = true;
                }
            }
        }
        if !chars.is_empty() {
            self.typed(&chars, now);
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
                draft: c.draft.clone(),
                now: unix,
                pending_left: c.pending_left(now),
                scroll: c.scroll,
                mic: self.listening.as_deref() == Some(c.id.as_str()),
                desk,
                drive: c.drive,
            };
            if c.cache.as_ref().is_none_or(|(v, _)| *v != view) {
                if let Some(d) = render_card(&view, s) {
                    let c = &mut self.cards[i];
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
            let h = c.cache.as_ref().map_or(SLIM_H, |(_, d)| d.h);
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

        // ---- animate
        let mut moving = false;
        for c in &mut self.cards {
            moving |= c.y.tick(dt) | c.h.tick(dt) | c.appear.tick(dt);
        }
        self.cards
            .retain(|c| !(c.leaving && c.appear.value <= 0.01 && c.appear.target == 0.0));
        if !(moving || self.dirty) {
            return;
        }
        self.dirty = false;
        self.draw(order.len().saturating_sub(MAX_VISIBLE));
    }

    fn click(&mut self, id: &str, hit: Hit, right: bool, now: Instant) {
        let Some(c) = self.find(id) else { return };
        c.activity = now;
        c.stale = true;
        let finished = !c.agent.status.is_active();
        let ask = c.agent.ask.clone();
        if finished {
            crate::wire::mark_agent_seen(id);
        }
        if hit != Hit::Input && self.focus.as_deref() == Some(id) && hit != Hit::Btn(Btn::Send) {
            self.blur(true);
        }
        let ok = match hit {
            Hit::Card | Hit::Result => true,
            Hit::Input => {
                self.focus = Some(id.to_string());
                unsafe {
                    let h = self.body.hwnd();
                    set_activatable(h, true);
                    let _ = SetForegroundWindow(h);
                    let _ = SetFocus(Some(h));
                }
                true
            }
            Hit::Mic => {
                crate::quick::mic_follow_up(id);
                true
            }
            Hit::File(i) => {
                let file = self.find(id).and_then(|c| c.agent.files.get(i).cloned());
                if let Some(f) = file {
                    if right {
                        reveal(&f);
                    } else {
                        open_file(&f);
                    }
                }
                true
            }
            Hit::Btn(b) => {
                if right {
                    return;
                }
                self.button(id, b, ask)
            }
        };
        let _ = ok;
    }

    fn button(&mut self, id: &str, b: Btn, ask: Option<Question>) -> bool {
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
            }
            Btn::Dismiss => self.dismiss(id),
            Btn::Show => {
                agents::desk::show(id);
            }
            Btn::Palette => {
                crate::wire::mark_agent_seen(id);
                crate::win::push(crate::win::AppEvent::AgentPalette(id.to_string()));
            }
            Btn::Always => {
                let (mut cfg, _) = crate::config::load();
                if !cfg.agents.always_approve {
                    cfg.agents.always_approve = true;
                    if let Err(e) = crate::config::save(&cfg) {
                        crate::config::log(&format!("cards: saving always_approve: {e}"));
                    }
                }
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::Allow);
                }
            }
            Btn::Once => {
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::AllowOnce);
                }
            }
            Btn::Deny => {
                if let Some(q) = ask {
                    agents::answer(id, q.id, Answer::Deny);
                }
            }
            Btn::Send => self.submit(id),
        }
        true
    }

    fn dismiss(&mut self, id: &str) {
        if self.focus.as_deref() == Some(id) {
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

    fn typed(&mut self, units: &[u16], now: Instant) {
        let Some(id) = self.focus.clone() else { return };
        for ch in char::decode_utf16(units.iter().copied()).flatten() {
            match ch as u32 {
                0x0D => {
                    self.submit(&id);
                    return;
                }
                0x1B => {
                    self.blur(true);
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
        if let Some(c) = self.find(&id) {
            c.activity = now;
        }
        self.dirty = true;
    }

    /// Drops text focus; `restore` hands the foreground back to where it was.
    fn blur(&mut self, restore: bool) {
        if self.focus.take().is_none() {
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

    /// Where the stack hangs: below the island's pill, centred on it.
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
            let m = self.mon.rect;
            let s = self.mon.scale;
            let top = m.y + ((8.0 + crate::island::PILL_H + HANG) * s).round() as i32;
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
            TARGETS.lock().unwrap().clear();
            INPUT.with(|i| {
                let mut i = i.borrow_mut();
                i.inputs.clear();
                i.buttons.clear();
            });
            return;
        }
        let (cx, top) = self.spot;
        let s = self.mon.scale;
        let drop = 14.0;
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
        let mut hits = Vec::new();
        let mut targets = Vec::new();
        let mut inputs = Vec::new();
        let mut buttons = Vec::new();
        let x0 = cx - pw as i32 / 2;
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
            paint_card(&mut pm, d, 0.0, y, h, ease, s);
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
        // Hits were pushed bottom card first; later (on top) wins in `rev()` lookup
        // only within one card, so keep cards' hits grouped: card rect first.
        self.more_rect = None;
        if more {
            if let Some(mp) = render_more(overflow, self.show_all, self.more_hover, s) {
                pm.draw_pixmap(
                    0,
                    (more_y * s).round() as i32,
                    mp.as_ref(),
                    &PixmapPaint::default(),
                    Transform::identity(),
                    None,
                );
                self.more_rect = Some((CARD_W / 2.0 - 70.0, more_y, 140.0, MORE_H));
                let r = (
                    ((CARD_W / 2.0 - 70.0) * s) as i32,
                    (more_y * s) as i32,
                    (140.0 * s) as i32,
                    (MORE_H * s) as i32,
                );
                buttons.push(r);
                rects.push((more_y, MORE_H, 0.6));
            }
        }
        self.hits = hits;
        self.origin = (x0, top);
        *TARGETS.lock().unwrap() = targets;
        INPUT.with(|i| {
            let mut i = i.borrow_mut();
            i.inputs = inputs;
            i.buttons = buttons;
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
                    let r = tokens::RADIUS_CARD.min(h / 2.0) * s;
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
        let moving = self
            .cards
            .iter()
            .any(|c| !c.y.settled() || !c.h.settled() || !c.appear.settled());
        if moving {
            return None;
        }
        let shown: Vec<&Card> = self
            .cards
            .iter()
            .filter(|c| c.appear.target > 0.0 && !c.leaving)
            .collect();
        if shown.iter().any(|c| c.kind() == Kind::Pending) {
            return None; // the countdown ring
        }
        let mut at: Vec<Instant> = Vec::new();
        for c in &shown {
            if c.agent.status.is_active() {
                // The elapsed readout ticks once a second.
                let ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.subsec_millis())
                    .unwrap_or(0);
                at.push(now + Duration::from_millis(1000 - ms as u64 + 5));
            }
            if self.expanded(c, now) {
                at.push(c.activity + IDLE_COLLAPSE);
                if let Some(t) = c.hover_until {
                    at.push(t);
                }
            }
        }
        Some(at.into_iter().filter(|t| *t > now).min())
    }
}

fn open_file(p: &Path) {
    let file = crate::util::wide(&p.to_string_lossy());
    let verb = crate::util::wide("open");
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

fn reveal(p: &Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", p.display()))
        .spawn();
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
            draft: String::new(),
            now: now_unix(),
            pending_left: 3.4,
            scroll: 0.0,
            mic: false,
            desk: true,
            drive: Drive::Off,
        }
    }

    /// Composites shadows + cards over a backdrop, like the desktop would.
    fn composite(cards: &[CardView], s: f32, light: bool) -> Pixmap {
        let drawn: Vec<Drawn> = cards.iter().map(|v| render_card(v, s).unwrap()).collect();
        let pad = 40.0;
        let total: f32 = drawn.iter().map(|d| d.h + STACK_GAP).sum::<f32>() - STACK_GAP;
        let (w, h) = (CARD_W + 2.0 * pad, total + 2.0 * pad + 16.0);
        let mut pm = Pixmap::new((w * s) as u32, (h * s) as u32).unwrap();
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
        let mut y = pad;
        for d in &drawn {
            ui::shadow(
                &mut pm,
                pad * s,
                y * s,
                CARD_W * s,
                d.h * s,
                tokens::RADIUS_CARD * s,
                s,
            );
            y += d.h + STACK_GAP;
        }
        let mut y = pad;
        for d in &drawn {
            // paint_card works in logical px from the pixmap origin.
            let mut layer = Pixmap::new(pm.width(), pm.height()).unwrap();
            paint_card(&mut layer, d, pad, y, d.h, 1.0, s);
            pm.draw_pixmap(
                0,
                0,
                layer.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            y += d.h + STACK_GAP;
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
    fn cards_measure_and_hit() {
        let v = view(agent(Status::Running));
        let d = render_card(&v, 1.0).unwrap();
        assert!(d.h > 150.0 && d.h < 400.0, "{}", d.h);
        assert!(d.hits.iter().any(|h| h.4 == Hit::Btn(Btn::Stop)));
        assert!(d.hits.iter().any(|h| h.4 == Hit::Input));
        let mut slim = v.clone();
        slim.expanded = false;
        assert_eq!(render_card(&slim, 1.0).unwrap().h, SLIM_H);
        let lines = md_lines("**Cheapest**: KLM at €89\n\n- one\n- two", 300.0);
        assert!(lines.len() >= 4);
    }

    fn previews() -> Vec<(&'static str, Vec<CardView>)> {
        let mut out = Vec::new();

        let mut p = view(agent(Status::Pending));
        p.agent.plan.clear();
        p.agent.steps.clear();
        p.agent.started = None;
        p.agent.name = "Tidy the Downloads folder".into();
        out.push(("pending", vec![p.clone()]));

        let mut r = view(agent(Status::Running));
        r.hover = Some(Hit::Btn(Btn::Stop));
        out.push(("running", vec![r.clone()]));

        let mut ap = view(agent(Status::Running));
        ap.agent.name = "Tidy the Downloads folder".into();
        ap.agent.plan = vec![
            "List old installers and archives".into(),
            "Delete what's older than a year".into(),
            "Report what was freed".into(),
        ];
        ap.agent.steps = vec![
            "Listing old installers".into(),
            "Delete 212 old files".into(),
        ];
        ap.agent.ask = Some(Question {
            id: 7,
            kind: AskKind::Approval,
            text: "Delete 212 files older than a year from Downloads (3.4 GB)".into(),
            detail: "desktop delete C:\\Users\\hecto\\Downloads\\*.msi --older-than 365d".into(),
            questions: Vec::new(),
        });
        ap.hover = Some(Hit::Btn(Btn::Always));
        out.push(("asking-approval", vec![ap.clone()]));

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
        out.push(("questions", vec![q.clone()]));

        let mut d = view(agent(Status::Done));
        d.agent.ended = Some(d.agent.started.unwrap() + 133);
        d.agent.result = "**Cheapest:** KLM, Sat 07:05 → 09:40, **€89** return with a cabin bag.\n\n- TAP is €96 but leaves at 06:00\n- Ryanair looks €54 until you add a bag (€118)\n\nI put the comparison in `fares.md` and a calendar hold in `trip.ics`.".into();
        d.agent.files = vec![
            PathBuf::from(r"C:\agents\x\fares.md"),
            PathBuf::from(r"C:\agents\x\trip.ics"),
            PathBuf::from(r"C:\agents\x\screenshots-of-every-fare.zip"),
        ];
        d.hover = Some(Hit::File(0));
        out.push(("done-with-files", vec![d.clone()]));

        let mut long = d.clone();
        long.agent.result = (1..=14)
            .map(|i| format!("- Option {i}: a fare with some detail about times and bags"))
            .collect::<Vec<_>>()
            .join("\n");
        long.scroll = 40.0;
        long.hover = None;
        out.push(("done-scrolling", vec![long]));

        let mut f = view(agent(Status::Failed));
        f.agent.ended = Some(f.agent.started.unwrap() + 19);
        f.agent.error = Some("The Claude CLI exited: rate limited. Try again in a minute.".into());
        f.agent.files.clear();
        f.hover = Some(Hit::Btn(Btn::Retry));
        out.push(("failed", vec![f.clone()]));

        let mut c1 = r.clone();
        c1.expanded = false;
        c1.hover = None;
        let mut c2 = d.clone();
        c2.expanded = false;
        c2.agent.name = "Summarise the Q3 deck".into();
        let mut c3 = ap.clone();
        c3.expanded = false;
        c3.hover = None;
        out.push(("collapsed", vec![c3, c1, c2]));

        let mut stack_r = r.clone();
        stack_r.hover = None;
        let mut slim_d = d.clone();
        slim_d.expanded = false;
        out.push(("stack", vec![ap, stack_r, slim_d]));
        out
    }

    /// `SCREENBUDDY_CARDS_PNG=<dir> cargo test cards` writes previews of every
    /// card state.
    #[test]
    fn cards_render_previews() {
        let dir = std::env::var("SCREENBUDDY_CARDS_PNG").ok();
        let s = 2.0;
        for (name, cards) in previews() {
            let pm = composite(&cards, s, true);
            let dark = composite(&cards, s, false);
            if let Some(dir) = &dir {
                let _ = std::fs::create_dir_all(dir);
                let p = std::path::Path::new(dir);
                pm.save_png(p.join(format!("{name}.png"))).unwrap();
                if matches!(name, "running" | "stack") {
                    dark.save_png(p.join(format!("{name}-dark.png"))).unwrap();
                }
            }
        }
    }
}
