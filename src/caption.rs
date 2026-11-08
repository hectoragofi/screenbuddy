//! The caption: a small card beside the cursor that shows a quick ask while it
//! listens, thinks and streams its answer (markdown-lite), then fades 6 s after
//! it is done. Hovering keeps it; a click continues the conversation in the
//! palette; a right click dismisses it. It also owns the overlay that a quick
//! ask's drawings land on, so they go away with it.
//!
//! A raw layered window on its own thread, rendered with tiny-skia (see
//! overlay.rs for the same technique). No timers run while nothing moves.
//!
//! Colours, sizes and the Geist Mono face come from `ui::tokens` / `ui`.

use crate::capture::{MonitorInfo, Rect};
use crate::overlay::{DrawCall, Overlay};
use crate::ui::tokens;
use crate::util::wide;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::Mutex;
use std::time::Instant;
use tiny_skia::{Color, FillRule, Paint, Path, PathBuilder, Pixmap, Transform};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{TME_LEAVE, TRACKMOUSEEVENT, TrackMouseEvent};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

// ---------------------------------------------------------------- tokens

const fn rgb3(c: crate::ui::Rgba) -> [u8; 3] {
    [c.r, c.g, c.b]
}

pub const INK: [u8; 3] = rgb3(tokens::INK);
pub const INK_2: [u8; 3] = rgb3(tokens::INK_2);
pub const PAPER: [u8; 3] = rgb3(tokens::PAPER);
pub const MUTE: [u8; 3] = rgb3(tokens::MUTE);
/// LINE = white at 8%.
pub const LINE_ALPHA: f32 = tokens::LINE.a as f32 / 255.0;
pub const SIGNAL: [u8; 3] = rgb3(tokens::SIGNAL);
pub const OK: [u8; 3] = rgb3(tokens::OK);
pub const ERR: [u8; 3] = rgb3(tokens::ERR);
pub const ASK: [u8; 3] = rgb3(tokens::ASK);

/// Logical px.
const MAX_W: f32 = 380.0;
const MIN_W: f32 = 196.0;
const PAD: f32 = 14.0;
const RADIUS: f32 = 16.0;
const BODY_PX: f32 = tokens::BODY_PX;
const LABEL_PX: f32 = tokens::LABEL_PX;
const LABEL_TRACK: f32 = tokens::LABEL_TRACKING;
const LINE_H: f32 = BODY_PX * 1.5;
const SHADOW_BLUR: f32 = 24.0;
const SHADOW_Y: f32 = 8.0;
const SHADOW_ALPHA: f32 = 0.35;
/// Room around the card for its shadow.
const MARGIN: f32 = SHADOW_BLUR + SHADOW_Y;
/// Distance of the card from the cursor.
const GAP: f32 = 18.0;
/// Seconds.
const LINGER: u32 = 6000;
const FADE_IN: f32 = 0.18;
const FADE_OUT: f32 = 0.35;

/// Not exported by the windows crate features we enable.
const WM_MOUSELEAVE_: u32 = 0x02A3;

const CLASS: &str = "screenbuddy.caption";

// ---------------------------------------------------------------- view + render

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Phase {
    Listening,
    Thinking,
    Streaming,
    Done,
    Error,
    /// A walkthrough step waiting for the user (calm, no animation).
    Step,
    /// A walkthrough question waiting for an answer.
    Ask,
}

/// What the card's footer offers.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Chrome {
    /// "Open in palette" once done (quick ask).
    #[default]
    Answer,
    /// Walkthrough: "Stop" left, `next` label right.
    Step { next: &'static str },
    /// No footer.
    Plain,
}

/// Everything the card shows. Pure data, so it renders headlessly.
#[derive(Debug, Clone)]
pub struct View {
    pub phase: Phase,
    /// One line in small caps above the body (the question, or a status).
    pub label: String,
    /// Markdown-lite answer.
    pub body: String,
    /// Mic level 0..=1 while listening.
    pub level: f32,
    /// Seconds since shown, for the idle animations.
    pub t: f32,
    pub hover: bool,
    /// Tallest the card may get, logical px.
    pub max_h: f32,
    pub chrome: Chrome,
    /// A notice from an agent: the footer opens its card, not the palette.
    pub notice: bool,
}

impl Default for View {
    fn default() -> Self {
        Self {
            phase: Phase::Listening,
            label: String::new(),
            body: String::new(),
            level: 0.0,
            t: 0.0,
            hover: false,
            max_h: 420.0,
            chrome: Chrome::Answer,
            notice: false,
        }
    }
}

/// Geist Mono (embedded by `ui`); Medium stands in for bold.
fn face(bold: bool) -> Option<&'static fontdue::Font> {
    Some(crate::ui::font(if bold {
        crate::ui::Face::MonoMedium
    } else {
        crate::ui::Face::Mono
    }))
}

type Glyph = (fontdue::Metrics, Vec<u8>);

thread_local! {
    static GLYPHS: RefCell<HashMap<(char, u32, bool), std::rc::Rc<Glyph>>> = RefCell::new(HashMap::new());
}

fn glyph(c: char, px: f32, bold: bool) -> Option<std::rc::Rc<Glyph>> {
    let f = face(bold)?;
    GLYPHS.with(|g| {
        let mut g = g.borrow_mut();
        if g.len() > 4000 {
            g.clear();
        }
        Some(
            g.entry((c, px.to_bits(), bold))
                .or_insert_with(|| std::rc::Rc::new(f.rasterize(c, px)))
                .clone(),
        )
    })
}

fn advance(c: char, px: f32, bold: bool) -> f32 {
    match face(bold) {
        Some(f) => f.metrics(c, px).advance_width,
        None => px * 0.55,
    }
}

fn text_w(s: &str, px: f32, bold: bool, track: f32) -> f32 {
    s.chars().map(|c| advance(c, px, bold) + track).sum()
}

fn ascent(px: f32) -> f32 {
    face(false)
        .and_then(|f| f.horizontal_line_metrics(px))
        .map_or(px * 0.8, |m| m.ascent)
}

/// Blends text into a premultiplied pixmap; `y` is the baseline.
fn draw_text(
    pm: &mut Pixmap,
    s: &str,
    x: f32,
    y: f32,
    px: f32,
    bold: bool,
    track: f32,
    rgb: [u8; 3],
    alpha: f32,
) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let mut pen = x;
    for c in s.chars() {
        let Some(g) = glyph(c, px, bold) else { return };
        let (m, bitmap) = (&g.0, &g.1);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (y - m.height as f32 - m.ymin as f32).round() as i32;
        let pixels = pm.pixels_mut();
        for row in 0..m.height as i32 {
            for col in 0..m.width as i32 {
                let (tx, ty) = (gx + col, gy + row);
                if tx < 0 || ty < 0 || tx >= pw || ty >= ph {
                    continue;
                }
                let a = (bitmap[(row * m.width as i32 + col) as usize] as f32 * alpha) as u32;
                if a == 0 {
                    continue;
                }
                let p = &mut pixels[(ty * pw + tx) as usize];
                let inv = 255 - a;
                let blend = |src: u8, d: u8| ((a * src as u32 + d as u32 * inv) / 255) as u8;
                *p = tiny_skia::PremultipliedColorU8::from_rgba(
                    blend(rgb[0], p.red()),
                    blend(rgb[1], p.green()),
                    blend(rgb[2], p.blue()),
                    blend(255, p.alpha()),
                )
                .unwrap_or(*p);
            }
        }
        pen += m.advance_width + track;
    }
}

fn rgba(c: [u8; 3], a: f32) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], (a.clamp(0.0, 1.0) * 255.0) as u8)
}

fn paint(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

fn rounded(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn circle(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Color) {
    if let Some(p) = PathBuilder::from_circle(cx, cy, r) {
        pm.fill_path(
            &p,
            &paint(c),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// Three box blurs each way ≈ a gaussian; one channel (the shadow is black).
fn blur_alpha(a: &mut [f32], w: usize, h: usize, radius: usize) {
    fn pass(from: &[f32], to: &mut [f32], n: usize, m: usize, si: usize, sj: usize, r: usize) {
        let win = (2 * r + 1) as f32;
        for i in 0..n {
            let at = |j: usize| from[i * si + j * sj];
            let mut sum: f32 = (0..r.min(m)).map(at).sum();
            for j in 0..m {
                if j + r < m {
                    sum += at(j + r);
                }
                if j > r {
                    sum -= at(j - r - 1);
                }
                to[i * si + j * sj] = sum / win;
            }
        }
    }
    let mut tmp = vec![0.0f32; a.len()];
    for _ in 0..3 {
        pass(a, &mut tmp, h, w, w, 1, radius);
        pass(&tmp, a, w, h, 1, w, radius);
    }
}

thread_local! {
    static SHADOW: RefCell<Option<((u32, u32, u32), Pixmap)>> = const { RefCell::new(None) };
}

/// The card's drop shadow, cached by size.
fn shadow(pw: u32, ph: u32, scale: f32) -> Option<Pixmap> {
    let key = (pw, ph, scale.to_bits());
    if let Some(p) = SHADOW.with(|s| {
        s.borrow()
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, p)| p.clone())
    }) {
        return Some(p);
    }
    let (w, h) = (pw as usize, ph as usize);
    let m = MARGIN * scale;
    let (cw, ch) = (pw as f32 - 2.0 * m, ph as f32 - 2.0 * m);
    let mut mask = Pixmap::new(pw, ph)?;
    let path = rounded(m, m + SHADOW_Y * scale, cw, ch, RADIUS * scale)?;
    mask.fill_path(
        &path,
        &paint(Color::BLACK),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut a: Vec<f32> = mask
        .pixels()
        .iter()
        .map(|p| p.alpha() as f32 / 255.0)
        .collect();
    blur_alpha(
        &mut a,
        w,
        h,
        ((SHADOW_BLUR * scale) / 3.0).round().max(1.0) as usize,
    );
    let mut out = Pixmap::new(pw, ph)?;
    for (p, v) in out.pixels_mut().iter_mut().zip(a) {
        let al = (v * SHADOW_ALPHA * 255.0).round().clamp(0.0, 255.0) as u8;
        *p = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, al).unwrap_or(*p);
    }
    SHADOW.with(|s| *s.borrow_mut() = Some((key, out.clone())));
    Some(out)
}

// ------------------------------------------------ markdown-lite

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub text: String,
    pub bold: bool,
    pub code: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Block {
    Para(Vec<Span>),
    /// Marker ("•", "1.") and its text.
    Item(String, Vec<Span>),
    Code(String),
    Gap,
}

/// Inline: **bold**, __bold__, `code`, *em*/_em_ (shown plain), [text](url) → text.
pub fn inline(s: &str) -> Vec<Span> {
    let mut out: Vec<Span> = Vec::new();
    let mut cur = String::new();
    let (mut bold, mut code) = (false, false);
    let cs: Vec<char> = s.chars().collect();
    let flush = |cur: &mut String, out: &mut Vec<Span>, bold: bool, code: bool| {
        if !cur.is_empty() {
            match out.last_mut() {
                Some(l) if l.bold == bold && l.code == code => l.text.push_str(cur),
                _ => out.push(Span {
                    text: std::mem::take(cur),
                    bold,
                    code,
                }),
            }
            cur.clear();
        }
    };
    let has_closing =
        |from: usize, pat: &[char]| (from..cs.len()).any(|j| cs[j..].starts_with(pat));
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c == '`' {
            flush(&mut cur, &mut out, bold, code);
            if code || has_closing(i + 1, &['`']) {
                code = !code;
            } else {
                cur.push(c);
            }
            i += 1;
            continue;
        }
        if code {
            cur.push(c);
            i += 1;
            continue;
        }
        let two = |a: char| i + 1 < cs.len() && cs[i] == a && cs[i + 1] == a;
        if two('*') || two('_') {
            let pat = [c, c];
            if bold || has_closing(i + 2, &pat) {
                flush(&mut cur, &mut out, bold, code);
                bold = !bold;
                i += 2;
                continue;
            }
        }
        if (c == '*' || c == '_')
            && i + 1 < cs.len()
            && cs[i + 1] != c
            && !cs[i + 1].is_whitespace()
            && has_closing(i + 1, &[c])
        {
            // Emphasis: drop the markers, keep the words (word_chars_like_this stay).
            let inner_word = c == '_' && i > 0 && cs[i - 1].is_alphanumeric();
            if !inner_word {
                if let Some(end) = (i + 1..cs.len()).find(|&j| cs[j] == c) {
                    cur.extend(&cs[i + 1..end]);
                    i = end + 1;
                    continue;
                }
            }
        }
        if c == '[' {
            if let Some(close) = (i + 1..cs.len()).find(|&j| cs[j] == ']') {
                if cs.get(close + 1) == Some(&'(') {
                    if let Some(end) = (close + 2..cs.len()).find(|&j| cs[j] == ')') {
                        cur.extend(&cs[i + 1..close]);
                        i = end + 1;
                        continue;
                    }
                }
            }
        }
        cur.push(c);
        i += 1;
    }
    flush(&mut cur, &mut out, bold, code);
    out
}

pub fn parse(md: &str) -> Vec<Block> {
    let mut out = Vec::new();
    let mut fence = false;
    let mut para = String::new();
    let end_para = |para: &mut String, out: &mut Vec<Block>| {
        if !para.trim().is_empty() {
            out.push(Block::Para(inline(para.trim())));
        }
        para.clear();
    };
    for line in md.lines() {
        let t = line.trim_end();
        if t.trim_start().starts_with("```") {
            end_para(&mut para, &mut out);
            fence = !fence;
            continue;
        }
        if fence {
            out.push(Block::Code(t.to_string()));
            continue;
        }
        let s = t.trim_start();
        if s.is_empty() {
            end_para(&mut para, &mut out);
            if !matches!(out.last(), None | Some(Block::Gap)) {
                out.push(Block::Gap);
            }
            continue;
        }
        if let Some(h) = s.strip_prefix('#') {
            end_para(&mut para, &mut out);
            let text = h.trim_start_matches('#').trim();
            out.push(Block::Para(vec![Span {
                text: inline(text).into_iter().map(|s| s.text).collect(),
                bold: true,
                code: false,
            }]));
            continue;
        }
        if let Some(rest) = s
            .strip_prefix("- ")
            .or_else(|| s.strip_prefix("* "))
            .or_else(|| s.strip_prefix("+ "))
        {
            end_para(&mut para, &mut out);
            out.push(Block::Item("•".into(), inline(rest)));
            continue;
        }
        let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 && digits < 4 && s[digits..].starts_with(". ") {
            end_para(&mut para, &mut out);
            out.push(Block::Item(
                s[..digits + 1].to_string(),
                inline(&s[digits + 2..]),
            ));
            continue;
        }
        if s.chars().all(|c| matches!(c, '-' | '*' | '_' | ' ')) && s.len() >= 3 {
            end_para(&mut para, &mut out);
            continue; // horizontal rule
        }
        if !para.is_empty() {
            para.push(' ');
        }
        para.push_str(s);
    }
    end_para(&mut para, &mut out);
    while matches!(out.last(), Some(Block::Gap)) {
        out.pop();
    }
    out
}

/// One laid-out line: runs of (text, bold, code) starting at `indent`.
#[derive(Debug, Clone, Default)]
struct VLine {
    indent: f32,
    marker: Option<String>,
    runs: Vec<(String, bool, bool)>,
    code_block: bool,
    /// Half-height spacer between paragraphs.
    gap: bool,
}

fn wrap_spans(
    spans: &[Span],
    width: f32,
    px: f32,
    indent: f32,
    marker: Option<String>,
) -> Vec<VLine> {
    let mut lines = vec![VLine {
        indent,
        marker,
        ..Default::default()
    }];
    let mut x = 0.0;
    // Tokens: words and the spaces after them, keeping each span's style.
    for sp in spans {
        let mut word = String::new();
        let push_word = |word: &mut String, lines: &mut Vec<VLine>, x: &mut f32| {
            if word.is_empty() {
                return;
            }
            let ww = text_w(word.trim_end(), px, sp.bold, 0.0);
            if *x > 0.0 && *x + ww > width {
                lines.push(VLine {
                    indent,
                    ..Default::default()
                });
                *x = 0.0;
                let t = word.trim_start().to_string();
                *word = t;
            }
            // A word wider than the line breaks by characters.
            let mut chunk = String::new();
            for c in word.chars() {
                let cw = advance(c, px, sp.bold);
                if *x + cw > width && *x > 0.0 && !c.is_whitespace() {
                    push_run(lines.last_mut().unwrap(), std::mem::take(&mut chunk), sp);
                    lines.push(VLine {
                        indent,
                        ..Default::default()
                    });
                    *x = 0.0;
                }
                chunk.push(c);
                *x += cw;
            }
            push_run(lines.last_mut().unwrap(), chunk, sp);
            word.clear();
        };
        for c in sp.text.chars() {
            word.push(c);
            if c == ' ' {
                push_word(&mut word, &mut lines, &mut x);
            }
        }
        push_word(&mut word, &mut lines, &mut x);
    }
    lines
}

fn push_run(l: &mut VLine, text: String, sp: &Span) {
    if text.is_empty() {
        return;
    }
    match l.runs.last_mut() {
        Some(r) if r.1 == sp.bold && r.2 == sp.code => r.0.push_str(&text),
        _ => l.runs.push((text, sp.bold, sp.code)),
    }
}

fn layout(blocks: &[Block], width: f32, px: f32) -> Vec<VLine> {
    let mut out = Vec::new();
    for b in blocks {
        match b {
            Block::Para(sp) => out.extend(wrap_spans(sp, width, px, 0.0, None)),
            Block::Item(m, sp) => {
                let ind = text_w(m, px, false, 0.0) + px * 0.6;
                out.extend(wrap_spans(sp, width - ind, px, ind, Some(m.clone())));
            }
            Block::Code(t) => {
                let sp = [Span {
                    text: t.clone(),
                    bold: false,
                    code: true,
                }];
                for mut l in wrap_spans(&sp, width - px, px, px * 0.5, None) {
                    l.code_block = true;
                    out.push(l);
                }
            }
            Block::Gap => out.push(VLine {
                gap: true,
                ..Default::default()
            }),
        }
    }
    out
}

fn line_h(l: &VLine) -> f32 {
    if l.gap { LINE_H * 0.5 } else { LINE_H }
}

/// Natural (unwrapped) width of the body, logical px.
fn natural_w(blocks: &[Block], px: f32) -> f32 {
    blocks
        .iter()
        .map(|b| match b {
            Block::Para(sp) => sp.iter().map(|s| text_w(&s.text, px, s.bold, 0.0)).sum(),
            Block::Item(m, sp) => {
                text_w(m, px, false, 0.0)
                    + px * 0.6
                    + sp.iter()
                        .map(|s| text_w(&s.text, px, s.bold, 0.0))
                        .sum::<f32>()
            }
            Block::Code(t) => text_w(t, px, false, 0.0) + px,
            Block::Gap => 0.0,
        })
        .fold(0.0, f32::max)
}

/// Uppercases and cuts a label to fit `w` logical px.
fn fit_label(s: &str, w: f32) -> String {
    let up: String = s
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase();
    let track = LABEL_PX * LABEL_TRACK;
    if text_w(&up, LABEL_PX, false, track) <= w {
        return up;
    }
    let mut out = String::new();
    for c in up.chars() {
        if text_w(&out, LABEL_PX, false, track)
            + advance(c, LABEL_PX, false)
            + track
            + text_w("…", LABEL_PX, false, track)
            > w
        {
            break;
        }
        out.push(c);
    }
    out.trim_end().to_string() + "…"
}

/// Renders the card (with its shadow margin) at `scale` physical px per logical px.
pub fn render(v: &View, scale: f32) -> Option<Pixmap> {
    let s = scale;
    let blocks = parse(&v.body);
    let has_body = matches!(
        v.phase,
        Phase::Streaming | Phase::Done | Phase::Error | Phase::Step | Phase::Ask
    ) && !blocks.is_empty();
    let label_w_nat = text_w(
        &v.label.to_uppercase(),
        LABEL_PX,
        false,
        LABEL_PX * LABEL_TRACK,
    ) + 14.0;
    let w = match v.phase {
        Phase::Listening => MIN_W,
        Phase::Thinking => (label_w_nat + PAD * 2.0).clamp(MIN_W, MAX_W),
        _ => (natural_w(&blocks, BODY_PX).max(label_w_nat) + PAD * 2.0 + 1.0).clamp(MIN_W, MAX_W),
    };
    let inner = w - PAD * 2.0;
    let mut lines = if has_body {
        layout(&blocks, inner, BODY_PX)
    } else {
        Vec::new()
    };
    let footer = match v.chrome {
        Chrome::Answer => v.phase == Phase::Done,
        Chrome::Step { .. } => true,
        Chrome::Plain => false,
    };
    let label_h = LABEL_PX + 6.0;
    let body_top = PAD + label_h + 8.0;
    let meter_h = 22.0;
    let footer_h = if footer { LABEL_PX + 12.0 } else { 0.0 };
    let body_h = |ls: &[VLine]| ls.iter().map(line_h).sum::<f32>();
    let max_body = (v.max_h - body_top - PAD - footer_h).max(LINE_H * 3.0);
    let mut cut_top = false;
    let mut cut_bottom = false;
    if body_h(&lines) > max_body {
        if v.phase == Phase::Streaming {
            // Show the newest lines while streaming.
            while body_h(&lines) > max_body {
                lines.remove(0);
            }
            cut_top = true;
        } else {
            while body_h(&lines) > max_body {
                lines.pop();
            }
            cut_bottom = true;
        }
    }
    let content_h = match v.phase {
        Phase::Listening | Phase::Thinking => meter_h,
        _ => body_h(&lines),
    };
    let h = body_top + content_h + footer_h + PAD;

    let m = MARGIN * s;
    let (pw, ph) = (
        ((w * s) + 2.0 * m).ceil() as u32,
        ((h * s) + 2.0 * m).ceil() as u32,
    );
    let mut pm = Pixmap::new(pw, ph)?;
    if let Some(sh) = shadow(pw, ph, s) {
        pm.draw_pixmap(
            0,
            0,
            sh.as_ref(),
            &tiny_skia::PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    let card = rounded(m, m, w * s, h * s, RADIUS * s)?;
    pm.fill_path(
        &card,
        &paint(rgba(INK, 0.97)),
        FillRule::Winding,
        Transform::identity(),
        None,
    );
    let mut hair = tiny_skia::Stroke::default();
    hair.width = 1.0;
    let edge = rounded(m + 0.5, m + 0.5, w * s - 1.0, h * s - 1.0, RADIUS * s)?;
    pm.stroke_path(
        &edge,
        &paint(
            Color::from_rgba(1.0, 1.0, 1.0, LINE_ALPHA * if v.hover { 2.0 } else { 1.0 }).unwrap(),
        ),
        &hair,
        Transform::identity(),
        None,
    );

    let x0 = m + PAD * s;
    let y0 = m + PAD * s;
    // Status dot + label.
    let (dot, pulse) = match v.phase {
        Phase::Listening => (SIGNAL, 1.0),
        Phase::Thinking => (SIGNAL, 0.55 + 0.45 * (v.t * 5.0).sin().abs()),
        Phase::Streaming => (SIGNAL, 1.0),
        Phase::Done => (OK, 1.0),
        Phase::Error => (ERR, 1.0),
        Phase::Step => (SIGNAL, 1.0),
        Phase::Ask => (ASK, 1.0),
    };
    let label_mid = y0 + label_h * s * 0.5;
    circle(&mut pm, x0 + 3.0 * s, label_mid, 3.0 * s, rgba(dot, pulse));
    let lx = x0 + 12.0 * s;
    let label = fit_label(&v.label, inner - 12.0);
    let track = LABEL_PX * LABEL_TRACK * s;
    let base = label_mid + ascent(LABEL_PX * s) * 0.5 - 1.0 * s;
    draw_text(
        &mut pm,
        &label,
        lx,
        base,
        LABEL_PX * s,
        false,
        track,
        if v.phase == Phase::Error { ERR } else { MUTE },
        1.0,
    );

    let by = m + body_top * s;
    match v.phase {
        Phase::Listening => {
            // Dot-matrix level meter: columns of dots rising with the voice.
            let cols = ((inner / 7.0) as usize).max(8);
            let rows = 3usize;
            let pitch = inner / cols as f32;
            for c in 0..cols {
                let wobble = 0.5
                    + 0.5
                        * ((c as f32 * 1.7 + v.t * 9.0).sin() * (c as f32 * 0.6 - v.t * 4.0).cos());
                let lit = (v.level.clamp(0.0, 1.0) * (0.4 + 0.6 * wobble) * rows as f32 + 0.15)
                    .round() as usize;
                for r in 0..rows {
                    let on = r < lit.max(if c % 4 == 0 { 1 } else { 0 });
                    let cx = x0 + (c as f32 + 0.5) * pitch * s;
                    let cy = by + (meter_h - 4.0 - r as f32 * 7.0) * s;
                    let col = if on {
                        rgba(SIGNAL, 1.0)
                    } else {
                        rgba(MUTE, 0.25)
                    };
                    circle(&mut pm, cx, cy, 1.6 * s, col);
                }
            }
        }
        Phase::Thinking => {
            // A dot orbiting three idle dots.
            for i in 0..3 {
                let phase =
                    ((v.t * 2.2 - i as f32 * 0.33).rem_euclid(1.0) * std::f32::consts::TAU).sin();
                let a = 0.3 + 0.7 * phase.max(0.0);
                circle(
                    &mut pm,
                    x0 + (4.0 + i as f32 * 10.0) * s,
                    by + meter_h * 0.5 * s,
                    2.2 * s,
                    rgba(PAPER, a),
                );
            }
        }
        _ => {
            let px = BODY_PX * s;
            let asc = ascent(px);
            let mut y = by;
            for l in &lines {
                let lh = line_h(l) * s;
                if l.gap {
                    y += lh;
                    continue;
                }
                let base = y + (lh - px) * 0.5 + asc * 0.92;
                if l.code_block {
                    if let Some(p) = rounded(x0 - 2.0 * s, y, inner * s + 4.0 * s, lh, 2.0 * s) {
                        pm.fill_path(
                            &p,
                            &paint(rgba(INK_2, 1.0)),
                            FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    }
                }
                if let Some(mk) = &l.marker {
                    draw_text(&mut pm, mk, x0, base, px, false, 0.0, MUTE, 1.0);
                }
                let mut x = x0 + l.indent * s;
                for (t, bold, code) in &l.runs {
                    let tw = text_w(t, px, *bold, 0.0);
                    if *code && !l.code_block {
                        if let Some(p) = rounded(
                            x - 2.0 * s,
                            y + 2.0 * s,
                            tw + 4.0 * s,
                            lh - 4.0 * s,
                            4.0 * s,
                        ) {
                            pm.fill_path(
                                &p,
                                &paint(rgba(INK_2, 1.0)),
                                FillRule::Winding,
                                Transform::identity(),
                                None,
                            );
                        }
                    }
                    let col = if *code { [0xE6, 0xC8, 0xB4] } else { PAPER };
                    draw_text(
                        &mut pm,
                        t,
                        x,
                        base,
                        px,
                        *bold,
                        0.0,
                        col,
                        if *bold || *code { 1.0 } else { 0.92 },
                    );
                    x += tw;
                }
                y += lh;
            }
            if v.phase == Phase::Streaming {
                // Live caret after the last glyph.
                if let Some(l) = lines.last() {
                    let lw: f32 = l.runs.iter().map(|r| text_w(&r.0, px, r.1, 0.0)).sum();
                    let cx = x0 + l.indent * s + lw + 2.0 * s;
                    let cy = y - LINE_H * s * 0.5;
                    circle(&mut pm, cx + 3.0 * s, cy, 2.5 * s, rgba(SIGNAL, 1.0));
                }
            }
            let fade = |pm: &mut Pixmap, top: bool| {
                // Soften the cut edge into the card.
                let span = (LINE_H * s) as i32;
                let (pw, card_l, card_r) = (pm.width() as i32, m as i32, (m + w * s) as i32);
                let start = if top {
                    by as i32
                } else {
                    (y - LINE_H * s) as i32
                };
                let pixels = pm.pixels_mut();
                for dy in 0..span {
                    let yy = start + dy;
                    let k = if top {
                        1.0 - dy as f32 / span as f32
                    } else {
                        dy as f32 / span as f32
                    };
                    for xx in card_l..card_r {
                        if let Some(p) = pixels.get_mut((yy * pw + xx) as usize) {
                            let mix = |c: u8, ink: u8| {
                                (c as f32 * (1.0 - k) + ink as f32 * k * (p.alpha() as f32 / 255.0))
                                    as u8
                            };
                            *p = tiny_skia::PremultipliedColorU8::from_rgba(
                                mix(p.red(), INK[0]),
                                mix(p.green(), INK[1]),
                                mix(p.blue(), INK[2]),
                                p.alpha(),
                            )
                            .unwrap_or(*p);
                        }
                    }
                }
            };
            if cut_top {
                fade(&mut pm, true);
            }
            if cut_bottom {
                fade(&mut pm, false);
            }
        }
    }
    if let (true, Chrome::Step { next }) = (footer, v.chrome) {
        // Two big targets: Stop on the left half, Next on the right.
        let fy = m + (h - PAD - 2.0) * s;
        let col = if v.hover { PAPER } else { MUTE };
        let a = if v.hover { 1.0 } else { 0.8 };
        draw_text(&mut pm, "STOP", x0, fy, LABEL_PX * s, false, track, col, a);
        let nw = text_w(next, LABEL_PX * s, false, track);
        draw_text(&mut pm, next, m + (w - PAD) * s - nw, fy, LABEL_PX * s, false, track, SIGNAL, a);
    } else if footer {
        let text = if v.notice {
            "SHOW CARD  ›"
        } else if cut_bottom {
            "MORE IN PALETTE  ›"
        } else {
            "OPEN IN PALETTE  ›"
        };
        let fy = m + (h - PAD - 2.0) * s;
        let tw = text_w(text, LABEL_PX * s, false, track);
        let col = if v.hover { PAPER } else { MUTE };
        draw_text(
            &mut pm,
            text,
            m + (w - PAD) * s - tw,
            fy,
            LABEL_PX * s,
            false,
            track,
            col,
            if v.hover { 1.0 } else { 0.8 },
        );
    }
    Some(pm)
}

// ---------------------------------------------------------------- thread

/// What the quick-ask flow tells the caption.
pub enum Cmd {
    /// Start a new caption at the cursor (physical px); clears old drawings.
    Listen {
        at: (i32, i32),
    },
    Level(f32),
    /// The question is in; waiting for the first words.
    Think {
        question: String,
    },
    Append(String),
    Done,
    Error(String),
    /// Drawings for the overlay on `monitor`.
    Draw(DrawCall, Rect, f32),
    Hide,
    /// A short notice beside the cursor ("‹name› is done") for agent `id`.
    /// Never replaces a quick ask in progress; a click goes to the notice sink.
    Notice {
        text: String,
        id: String,
    },
    /// A walkthrough card beside `near` (or the cursor), with an optional
    /// pointer drawn on the overlay. Replaces whatever the caption showed.
    Walk(Card),
}

/// One walkthrough card (see `walk.rs`).
#[derive(Debug, Clone)]
pub struct Card {
    pub phase: Phase,
    pub label: String,
    pub body: String,
    pub chrome: Chrome,
    /// Screen rect to sit beside (physical px); None = beside the cursor.
    pub near: Option<Rect>,
    /// The ring/pointer at the target (screen px).
    pub pointer: Option<crate::overlay::Shape>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum CaptionEvent {
    /// Left click: continue in the palette.
    Clicked,
    /// Right click, or it faded away.
    Dismissed,
    /// Walkthrough card: "Next" (or "Skip").
    Next,
    /// Walkthrough card: "Stop", or a right click.
    Stop,
}

static TX: Mutex<Option<Sender<Cmd>>> = Mutex::new(None);
static THREAD: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
static EVENTS: Mutex<Vec<Sender<CaptionEvent>>> = Mutex::new(Vec::new());
const WM_WAKE: u32 = WM_APP + 3;

type NoticeSink = Box<dyn Fn(String) + Send + Sync>;
static NOTICE_SINK: Mutex<Option<NoticeSink>> = Mutex::new(None);

/// Where clicks on a [`Cmd::Notice`] go (with its id).
pub fn set_notice_sink(f: impl Fn(String) + Send + Sync + 'static) {
    *NOTICE_SINK.lock().unwrap() = Some(Box::new(f));
}

/// Where clicks and dismissals go (every subscriber gets every event;
/// dropped receivers are forgotten).
pub fn subscribe(tx: Sender<CaptionEvent>) {
    EVENTS.lock().unwrap().push(tx);
}

/// Sends a command, starting the caption thread on first use.
pub fn send(cmd: Cmd) {
    let mut g = TX.lock().unwrap();
    if g.is_none() {
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        if std::thread::Builder::new()
            .name("screenbuddy-caption".into())
            .spawn(move || thread_main(rx, ready_tx))
            .is_err()
        {
            return;
        }
        let _ = ready_rx.recv_timeout(std::time::Duration::from_secs(2));
        *g = Some(tx);
    }
    if let Some(tx) = g.as_ref() {
        let _ = tx.send(cmd);
    }
    let t = THREAD.load(std::sync::atomic::Ordering::Relaxed);
    if t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

fn emit(ev: CaptionEvent) {
    EVENTS.lock().unwrap().retain(|tx| tx.send(ev).is_ok());
}

struct St {
    hwnd: HWND,
    view: View,
    visible: bool,
    anchor: (i32, i32),
    monitor: Option<MonitorInfo>,
    shown: Instant,
    fade_out: Option<Instant>,
    frame_timer: usize,
    linger_timer: usize,
    overlay: Option<Overlay>,
    dirty: bool,
    /// Walkthrough: the target rect the card sits beside.
    near: Option<Rect>,
    /// The agent whose notice is showing.
    notice: Option<String>,
}

#[derive(Clone, Copy)]
enum Mouse {
    Enter,
    Leave,
    /// Left button up at window coords.
    Click(i32, i32),
    Dismiss,
}

thread_local! {
    /// Card rect in window coords, for hit testing without borrowing the state.
    static CARD: Cell<(i32, i32, i32, i32)> = const { Cell::new((0, 0, 0, 0)) };
    static TRACKING: Cell<bool> = const { Cell::new(false) };
    static MOUSE: RefCell<Vec<Mouse>> = const { RefCell::new(Vec::new()) };
}

fn hinstance() -> HINSTANCE {
    unsafe {
        GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        let push = |m: Mouse| MOUSE.with(|q| q.borrow_mut().push(m));
        match msg {
            WM_NCHITTEST => {
                let mut r = RECT::default();
                let _ = GetWindowRect(hwnd, &mut r);
                let (x, y) = (
                    (lp.0 & 0xFFFF) as i16 as i32 - r.left,
                    ((lp.0 >> 16) & 0xFFFF) as i16 as i32 - r.top,
                );
                let (l, t, rr, b) = CARD.with(|c| c.get());
                if x >= l && x < rr && y >= t && y < b {
                    LRESULT(HTCLIENT as isize)
                } else {
                    LRESULT(HTTRANSPARENT as isize)
                }
            }
            WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
            WM_SETCURSOR => {
                if let Ok(c) = LoadCursorW(None, IDC_HAND) {
                    SetCursor(Some(c));
                }
                LRESULT(1)
            }
            WM_MOUSEMOVE => {
                if !TRACKING.with(|t| t.replace(true)) {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    let _ = TrackMouseEvent(&mut tme);
                    push(Mouse::Enter);
                }
                LRESULT(0)
            }
            WM_MOUSELEAVE_ => {
                TRACKING.with(|t| t.set(false));
                push(Mouse::Leave);
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let (x, y) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                push(Mouse::Click(x, y));
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                push(Mouse::Dismiss);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

fn thread_main(rx: Receiver<Cmd>, ready: Sender<()>) {
    unsafe {
        THREAD.store(
            windows::Win32::System::Threading::GetCurrentThreadId(),
            std::sync::atomic::Ordering::Relaxed,
        );
        // Make sure the thread has a queue before anyone posts to it.
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let cls = wide(CLASS);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            hCursor: LoadCursorW(None, IDC_HAND).unwrap_or_default(),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            Some(hinstance()),
            None,
        );
        let _ = ready.send(());
        let Ok(hwnd) = hwnd else {
            crate::config::log("caption window failed");
            return;
        };
        if crate::win::exclude_from_capture() {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        let mut st = St {
            hwnd,
            view: View::default(),
            visible: false,
            anchor: (0, 0),
            monitor: None,
            shown: Instant::now(),
            fade_out: None,
            frame_timer: 0,
            linger_timer: 0,
            overlay: None,
            dirty: false,
            near: None,
            notice: None,
        };
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.0.is_null() && msg.message == WM_TIMER {
                if msg.wParam.0 == st.linger_timer && st.linger_timer != 0 {
                    let _ = KillTimer(None, st.linger_timer);
                    st.linger_timer = 0;
                    if !st.view.hover {
                        st.fade_out = Some(Instant::now());
                    }
                }
                st.dirty = true;
            } else if msg.hwnd.0.is_null() && msg.message == WM_WAKE {
                while let Ok(cmd) = rx.try_recv() {
                    st.apply(cmd);
                }
            } else {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            for m in MOUSE.with(|q| std::mem::take(&mut *q.borrow_mut())) {
                st.mouse(m);
            }
            st.frame();
        }
    }
}

impl St {
    fn apply(&mut self, cmd: Cmd) {
        if !matches!(cmd, Cmd::Notice { .. } | Cmd::Level(_)) {
            self.notice = None;
            self.view.notice = false;
        }
        match cmd {
            Cmd::Listen { at } => {
                self.overlay = None;
                self.near = None;
                self.anchor = at;
                self.monitor = Some(crate::capture::monitor_at(at.0, at.1));
                self.view = View {
                    phase: Phase::Listening,
                    label: "Listening".into(),
                    ..Default::default()
                };
                self.show();
            }
            Cmd::Level(l) => {
                // Fast attack, slow release, so the meter reads as speech.
                let cur = self.view.level;
                self.view.level = if l > cur { l } else { cur * 0.8 + l * 0.2 };
            }
            Cmd::Think { question } => {
                if !self.visible {
                    let at = crate::capture::cursor_pos();
                    self.anchor = at;
                    self.monitor = Some(crate::capture::monitor_at(at.0, at.1));
                    self.show();
                }
                self.view.chrome = Chrome::Answer;
                self.view.phase = Phase::Thinking;
                self.view.label = question;
                self.view.body.clear();
            }
            Cmd::Append(t) => {
                self.view.phase = Phase::Streaming;
                self.view.body.push_str(&t);
            }
            Cmd::Done => {
                if self.view.body.trim().is_empty() && self.overlay.is_none() {
                    self.view.body = "Done.".into();
                }
                self.view.phase = Phase::Done;
                self.linger();
            }
            Cmd::Error(e) => {
                if !self.visible {
                    let at = crate::capture::cursor_pos();
                    self.anchor = at;
                    self.monitor = Some(crate::capture::monitor_at(at.0, at.1));
                    self.show();
                }
                self.view.phase = Phase::Error;
                self.view.chrome = Chrome::Answer;
                self.view.label = "Couldn't answer".into();
                self.view.body = e;
                self.linger();
            }
            Cmd::Draw(call, monitor, scale) => {
                if self.overlay.is_none() {
                    self.overlay = Overlay::new(monitor, scale).ok();
                }
                if let Some(o) = self.overlay.as_mut() {
                    o.apply(call, false);
                }
                // Stay above the drawings.
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        Some(HWND_TOPMOST),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
            Cmd::Hide => self.hide(),
            Cmd::Walk(card) => self.walk(card),
            Cmd::Notice { text, id } => {
                let busy = self.visible
                    && self.notice.is_none()
                    && matches!(
                        self.view.phase,
                        Phase::Listening
                            | Phase::Thinking
                            | Phase::Streaming
                            | Phase::Step
                            | Phase::Ask
                    );
                if busy {
                    return;
                }
                self.overlay = None;
                self.near = None;
                let at = crate::capture::cursor_pos();
                self.anchor = at;
                self.monitor = Some(crate::capture::monitor_at(at.0, at.1));
                self.view = View {
                    phase: Phase::Done,
                    label: "Agent".into(),
                    body: text,
                    notice: true,
                    ..Default::default()
                };
                self.notice = Some(id);
                self.show();
                self.linger();
            }
        }
        self.dirty = true;
    }

    fn walk(&mut self, c: Card) {
        let at = c
            .near
            .map_or_else(crate::capture::cursor_pos, |r| (r.x + r.w / 2, r.y + r.h / 2));
        let moved = self.near != c.near || !self.visible;
        if c.near.is_some() || !self.visible || self.near.is_some() {
            self.anchor = at;
        }
        self.near = c.near;
        self.monitor = Some(crate::capture::monitor_at(self.anchor.0, self.anchor.1));
        self.overlay = None;
        if let (Some(p), Some(mon)) = (c.pointer, self.monitor) {
            self.overlay = Overlay::new(mon.rect, mon.scale).ok();
            if let Some(o) = self.overlay.as_mut() {
                o.apply(DrawCall { clear: true, shapes: vec![p] }, false);
            }
            // Stay above the pointer.
            unsafe {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        let hover = self.view.hover;
        self.view = View {
            phase: c.phase,
            label: c.label,
            body: c.body,
            chrome: c.chrome,
            hover,
            ..Default::default()
        };
        if moved {
            self.show();
        } else {
            self.fade_out = None;
            self.kill_linger();
        }
        if matches!(c.phase, Phase::Done | Phase::Error) {
            self.linger();
        }
    }

    fn show(&mut self) {
        self.visible = true;
        self.shown = Instant::now();
        self.fade_out = None;
        self.kill_linger();
    }

    fn hide(&mut self) {
        self.visible = false;
        self.near = None;
        self.fade_out = None;
        self.overlay = None;
        self.kill_linger();
        self.view.hover = false;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn linger(&mut self) {
        self.kill_linger();
        if !self.view.hover {
            self.linger_timer = unsafe { SetTimer(None, 0, LINGER, None) };
        }
    }

    fn kill_linger(&mut self) {
        if self.linger_timer != 0 {
            unsafe {
                let _ = KillTimer(None, self.linger_timer);
            }
            self.linger_timer = 0;
        }
    }

    fn mouse(&mut self, m: Mouse) {
        match m {
            Mouse::Enter => {
                self.view.hover = true;
                self.kill_linger();
                if self.fade_out.take().is_some() {
                    self.shown = Instant::now() - std::time::Duration::from_secs(1);
                }
            }
            Mouse::Leave => {
                self.view.hover = false;
                if matches!(self.view.phase, Phase::Done | Phase::Error) {
                    self.linger();
                }
            }
            Mouse::Click(x, y) => {
                if matches!(self.view.phase, Phase::Listening) {
                    return;
                }
                match self.view.chrome {
                    Chrome::Step { .. } => {
                        // Only the footer is a button; the card stays up
                        // until the walkthrough moves on.
                        let (l, _, r, b) = CARD.with(|c| c.get());
                        let s = self.monitor.map_or(1.0, |m| m.scale.max(0.5));
                        let footer_top = b - ((LABEL_PX + 12.0 + PAD) * s) as i32;
                        if y >= footer_top && y < b {
                            emit(if x < (l + r) / 2 {
                                CaptionEvent::Stop
                            } else {
                                CaptionEvent::Next
                            });
                        }
                    }
                    Chrome::Plain => {
                        self.hide();
                        emit(CaptionEvent::Dismissed);
                    }
                    Chrome::Answer => {
                        self.hide();
                        if let Some(id) = self.notice.take() {
                            if let Some(f) = NOTICE_SINK.lock().unwrap().as_ref() {
                                f(id);
                            }
                            return;
                        }
                        emit(CaptionEvent::Clicked);
                    }
                }
            }
            Mouse::Dismiss => {
                if let Chrome::Step { .. } = self.view.chrome {
                    emit(CaptionEvent::Stop);
                    return;
                }
                self.hide();
                if self.notice.take().is_some() {
                    return;
                }
                emit(CaptionEvent::Dismissed);
            }
        }
        self.dirty = true;
    }

    fn animating(&self) -> bool {
        self.visible
            && (matches!(self.view.phase, Phase::Listening | Phase::Thinking)
                || self.fade_out.is_some()
                || self.shown.elapsed().as_secs_f32() < FADE_IN)
    }

    /// Renders if needed, and keeps a ~60 Hz timer only while something moves.
    fn frame(&mut self) {
        let anim = self.animating();
        if self.visible && (self.dirty || anim) {
            self.dirty = false;
            self.paint();
        }
        let want = self.visible && self.animating();
        unsafe {
            if want && self.frame_timer == 0 {
                self.frame_timer = SetTimer(None, 0, 16, None);
            } else if !want && self.frame_timer != 0 {
                let _ = KillTimer(None, self.frame_timer);
                self.frame_timer = 0;
            }
        }
    }

    fn paint(&mut self) {
        let Some(mon) = self.monitor else { return };
        let s = mon.scale.max(0.5);
        let age = self.shown.elapsed().as_secs_f32();
        let mut alpha = ease_out((age / FADE_IN).min(1.0));
        let mut rise = (1.0 - alpha) * 6.0;
        if let Some(f) = self.fade_out {
            let k = f.elapsed().as_secs_f32() / FADE_OUT;
            if k >= 1.0 {
                self.hide();
                if self.notice.take().is_none() {
                    emit(CaptionEvent::Dismissed);
                }
                return;
            }
            alpha *= 1.0 - ease_out(k);
            rise = 0.0;
        }
        self.view.t = age;
        self.view.max_h = (mon.work.h as f32 / s * 0.55).max(160.0);
        let Some(pm) = render(&self.view, s) else {
            return;
        };
        let m = (MARGIN * s) as i32;
        let (cw, ch) = (pm.width() as i32 - 2 * m, pm.height() as i32 - 2 * m);
        let gap = (GAP * s) as i32;
        let work = mon.work;
        let (x, y) = match self.near {
            Some(r) => beside((cw, ch), work, r, gap),
            None => {
                let mut x = self.anchor.0 + gap;
                if x + cw > work.x + work.w - 8 {
                    x = self.anchor.0 - gap - cw;
                }
                x = x.clamp(work.x + 8, (work.x + work.w - cw - 8).max(work.x + 8));
                let mut y = self.anchor.1 + gap;
                if y + ch > work.y + work.h - 8 {
                    y = work.y + work.h - 8 - ch;
                }
                (x, y.max(work.y + 8))
            }
        };
        let y = y + (rise * s) as i32;
        CARD.with(|c| c.set((m, m, m + cw, m + ch)));
        unsafe { present(self.hwnd, &pm, x - m, y - m, (alpha * 255.0) as u8) };
    }
}

/// Where a `w`x`h` card goes so it sits next to `target` without covering
/// it: right of it, else left, else below, else above; always inside `work`.
pub fn beside((w, h): (i32, i32), work: Rect, target: Rect, gap: i32) -> (i32, i32) {
    let (l, t, r, b) = (work.x + 8, work.y + 8, work.x + work.w - 8, work.y + work.h - 8);
    let clamp_x = |x: i32| x.clamp(l, (r - w).max(l));
    let clamp_y = |y: i32| y.clamp(t, (b - h).max(t));
    let mid = clamp_y(target.y + target.h / 2 - h / 2);
    if target.x + target.w + gap + w <= r {
        return (target.x + target.w + gap, mid);
    }
    if target.x - gap - w >= l {
        return (target.x - gap - w, mid);
    }
    let left = clamp_x(target.x);
    if target.y + target.h + gap + h <= b {
        return (left, target.y + target.h + gap);
    }
    if target.y - gap - h >= t {
        return (left, target.y - gap - h);
    }
    (clamp_x(target.x + target.w + gap), mid)
}

fn ease_out(t: f32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(3)
}

/// Uploads a premultiplied pixmap to the layered window at (x, y) with a constant alpha.
unsafe fn present(hwnd: HWND, pm: &Pixmap, x: i32, y: i32, alpha: u8) {
    unsafe {
        let (w, h) = (pm.width() as i32, pm.height() as i32);
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        if let Ok(dib) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
            let dst = std::slice::from_raw_parts_mut(bits as *mut u8, (w * h * 4) as usize);
            for (d, s) in dst.chunks_exact_mut(4).zip(pm.data().chunks_exact(4)) {
                d[0] = s[2];
                d[1] = s[1];
                d[2] = s[0];
                d[3] = s[3];
            }
            let old = SelectObject(mem, dib.into());
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: alpha,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let pos = POINT { x, y };
            let size = SIZE { cx: w, cy: h };
            let src = POINT { x: 0, y: 0 };
            let _ = UpdateLayeredWindow(
                hwnd,
                Some(screen),
                Some(&pos),
                Some(&size),
                Some(mem),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            SelectObject(mem, old);
            let _ = DeleteObject(dib.into());
            if !IsWindowVisible(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = SetWindowPos(
                    hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(s: &str) -> Vec<Span> {
        vec![Span {
            text: s.into(),
            bold: false,
            code: false,
        }]
    }

    #[test]
    fn inline_markup() {
        assert_eq!(inline("just text"), plain("just text"));
        let v = inline("press **Save** then `Ctrl+S`");
        assert_eq!(v.len(), 4);
        assert!(v[1].bold && v[1].text == "Save");
        assert!(v[3].code && v[3].text == "Ctrl+S");
        assert_eq!(inline("an *emphasised* word"), plain("an emphasised word"));
        assert_eq!(
            inline("see [the docs](https://x.y) now"),
            plain("see the docs now")
        );
        assert_eq!(
            inline("snake_case_name stays"),
            plain("snake_case_name stays")
        );
        // Unclosed markers are text (mid-stream).
        assert_eq!(inline("a **half"), plain("a **half"));
        assert_eq!(inline("2 * 3 = 6"), plain("2 * 3 = 6"));
    }

    #[test]
    fn blocks() {
        let b = parse("# Title\nline one\nline two\n\n- a\n2. b\n```\ncode\n```\n");
        assert_eq!(
            b[0],
            Block::Para(vec![Span {
                text: "Title".into(),
                bold: true,
                code: false
            }])
        );
        assert_eq!(b[1], Block::Para(plain("line one line two")));
        assert_eq!(b[2], Block::Gap);
        assert_eq!(b[3], Block::Item("•".into(), plain("a")));
        assert_eq!(b[4], Block::Item("2.".into(), plain("b")));
        assert_eq!(b[5], Block::Code("code".into()));
    }

    #[test]
    fn wraps_within_width() {
        let lines = layout(&parse("word ".repeat(80).trim()), 300.0, BODY_PX);
        assert!(lines.len() > 3);
        for l in &lines {
            let w: f32 = l
                .runs
                .iter()
                .map(|r| text_w(r.0.trim_end(), BODY_PX, r.1, 0.0))
                .sum();
            assert!(w <= 300.5, "{w}");
        }
        // A long unbroken token breaks too.
        let lines = layout(&parse(&"x".repeat(200)), 200.0, BODY_PX);
        assert!(lines.len() > 1);
    }

    /// `SCREENBUDDY_CAPTION_PNG=dir` writes the states as PNGs to look at.
    #[test]
    fn renders() {
        let body = "Click **File › Export** in the top bar, then pick `PNG`.\n\n- Keeps transparency\n- Up to 4× scale\n\nIf the menu is greyed out, select a layer first.";
        let views = [
            (
                "listening",
                View {
                    phase: Phase::Listening,
                    label: "Listening".into(),
                    level: 0.7,
                    t: 0.4,
                    ..Default::default()
                },
            ),
            (
                "thinking",
                View {
                    phase: Phase::Thinking,
                    label: "how do I export this as a png".into(),
                    t: 0.3,
                    ..Default::default()
                },
            ),
            (
                "streaming",
                View {
                    phase: Phase::Streaming,
                    label: "how do I export this as a png".into(),
                    body: body[..60].into(),
                    ..Default::default()
                },
            ),
            (
                "done",
                View {
                    phase: Phase::Done,
                    label: "how do I export this as a png".into(),
                    body: body.into(),
                    ..Default::default()
                },
            ),
            (
                "long",
                View {
                    phase: Phase::Done,
                    label: "explain".into(),
                    body: "A sentence that goes on. ".repeat(80),
                    max_h: 260.0,
                    hover: true,
                    ..Default::default()
                },
            ),
            (
                "error",
                View {
                    phase: Phase::Error,
                    label: "Couldn't answer".into(),
                    body: "Turn on voice in Settings › Voice to ask by holding Alt.".into(),
                    ..Default::default()
                },
            ),
        ];
        let dir = std::env::var_os("SCREENBUDDY_CAPTION_PNG").map(std::path::PathBuf::from);
        for (name, v) in views {
            let pm = render(&v, 1.5).expect(name);
            let max = ((MAX_W + 2.0 * MARGIN) * 1.5).ceil() as u32;
            assert!(pm.width() <= max, "{name} {}", pm.width());
            if v.max_h < 400.0 {
                assert!(
                    pm.height() as f32 <= (v.max_h + 2.0 * MARGIN) * 1.5 + 2.0,
                    "{name} {}",
                    pm.height()
                );
            }
            if let Some(d) = &dir {
                let _ = std::fs::create_dir_all(d);
                pm.save_png(d.join(format!("caption-{name}.png"))).unwrap();
            }
        }
    }
}
