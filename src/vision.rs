//! Seeing more than one screenshot: the UI Automation element list sent with
//! the image, the `zoom` tool (re-capture a region at native resolution) and
//! guide mode (`watch`: wait for the user to act, then look again).
//!
//! Coordinates: every `draw` the model makes is resolved here into pixels of
//! the latest full screenshot (the "base" frame) before the overlay maps it to
//! the screen. Shapes may instead name a UI element (`element: "e7"`) or use a
//! zoom image's pixels (`zoom: "z1"`).

use crate::capture::{self, Capture, MonitorInfo, Rect};
use crate::config::Config;
use crate::llm::Out;
use crate::uia::{self, Element};
use base64::Engine;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Model round trips per turn (guide mode needs one or two per step).
pub const MAX_ROUNDS: usize = 40;
/// Elements listed to the model per snapshot.
const MAX_LIST: usize = 120;
/// Smallest zoom area, in screen pixels per side.
const MIN_ZOOM: i32 = 64;
/// Change detection: thumbnail size, per-cell luma delta, cells that must change.
const THUMB_EDGE: u32 = 256;
const CELL_DELTA: u8 = 24;
const MIN_CELLS: usize = 6;
/// Guide mode polling: key check, screen check, settle, give-up.
const TICK: Duration = Duration::from_millis(100);
const SCREEN_EVERY: u32 = 5;
const SETTLE_MAX: Duration = Duration::from_millis(3000);
const WATCH_MAX: Duration = Duration::from_secs(300);

/// Mapping between an image the model saw and the screen.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Frame {
    pub src: Rect,
    pub w: u32,
    pub h: u32,
}

impl Frame {
    pub fn of(cap: &Capture) -> Self {
        Self {
            src: cap.src,
            w: cap.img_w,
            h: cap.img_h,
        }
    }
    /// Screen pixels per image pixel.
    pub fn scale(&self) -> f32 {
        self.src.w as f32 / self.w.max(1) as f32
    }
    fn scale_y(&self) -> f32 {
        self.src.h as f32 / self.h.max(1) as f32
    }
    pub fn to_screen(self, x: f32, y: f32) -> (f32, f32) {
        (
            self.src.x as f32 + x * self.scale(),
            self.src.y as f32 + y * self.scale_y(),
        )
    }
    pub fn image_point(&self, x: f32, y: f32) -> (f32, f32) {
        (
            (x - self.src.x as f32) / self.scale(),
            (y - self.src.y as f32) / self.scale_y(),
        )
    }
    /// A screen rectangle in this image's pixels, clipped to the image.
    pub fn rect_in(&self, r: Rect) -> Option<(f32, f32, f32, f32)> {
        let c = uia::intersect(r, self.src)?;
        let (x0, y0) = self.image_point(c.x as f32, c.y as f32);
        let (x1, y1) = self.image_point((c.x + c.w) as f32, (c.y + c.h) as f32);
        Some((x0, y0, x1 - x0, y1 - y0))
    }
}

pub struct ToolOut {
    pub text: String,
    pub image: Option<Vec<u8>>,
    pub is_error: bool,
}

impl ToolOut {
    fn text(t: impl Into<String>, is_error: bool) -> Self {
        Self {
            text: t.into(),
            image: None,
            is_error,
        }
    }
    /// Anthropic `tool_result.content`.
    pub fn anthropic(&self) -> Value {
        let mut c = Vec::new();
        if let Some(j) = &self.image {
            c.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64(j)}}));
        }
        c.push(json!({"type": "text", "text": self.text}));
        Value::Array(c)
    }
    /// OpenAI data URL for the image, sent in a user message after the tool results.
    pub fn openai_image(&self) -> Option<Value> {
        self.image.as_ref().map(|j| {
            json!({"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", b64(j))}})
        })
    }
    /// MCP `tools/call` result.
    pub fn mcp(&self) -> Value {
        let mut c = Vec::new();
        if let Some(j) = &self.image {
            c.push(json!({"type": "image", "data": b64(j), "mimeType": "image/jpeg"}));
        }
        c.push(json!({"type": "text", "text": self.text}));
        json!({"content": c, "isError": self.is_error})
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

struct State {
    base: Option<Frame>,
    monitor: Option<MonitorInfo>,
    /// Latest element list (screen px); index + 1 is the element id.
    elements: Vec<Element>,
    pending: Option<Receiver<Vec<Element>>>,
    zooms: Vec<Frame>,
    out: Option<Out>,
    cancel: Option<Arc<AtomicBool>>,
    /// The user pressed Esc during guide mode in this turn.
    stopped: bool,
}

static STATE: Mutex<State> = Mutex::new(State {
    base: None,
    monitor: None,
    elements: Vec::new(),
    pending: None,
    zooms: Vec::new(),
    out: None,
    cancel: None,
    stopped: false,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    STATE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Called whenever a new screenshot becomes the subject of the conversation.
/// Starts the accessibility snapshot in the background.
pub fn set_capture(cap: &Capture, cfg: &Config) {
    let mut s = state();
    s.base = Some(Frame::of(cap));
    s.monitor = Some(cap.monitor);
    s.elements.clear();
    s.zooms.clear();
    s.pending = None;
    if cfg.vision.ui_tree {
        let area = cap.src;
        let roots = uia::target_windows(area, cap.cursor);
        let raw: Vec<isize> = roots.iter().map(|h| h.0 as isize).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let roots = raw
                .into_iter()
                .map(|h| windows::Win32::Foundation::HWND(h as *mut _))
                .collect();
            let _ = tx.send(uia::snapshot(roots, area, uia::budget()));
        });
        s.pending = Some(rx);
    }
}

/// Remembers the active turn so tools served to the Claude CLI (through
/// `screenbuddy --mcp`) reach the palette, and so guide mode can stop.
pub fn turn_started(out: &Out, cancel: &Arc<AtomicBool>) {
    let mut s = state();
    s.out = Some(out.clone());
    s.cancel = Some(cancel.clone());
    s.stopped = false;
}

fn has_image() -> bool {
    state().base.is_some()
}

// ---------------------------------------------------------------- prompt + tools

pub fn prompt(cfg: &Config) -> String {
    let v = &cfg.vision;
    let mut p = String::new();
    if v.ui_tree && cfg.drawing.enabled {
        p.push_str("\n\nA screenshot may come with a list of UI elements read from the app's accessibility tree: id, type, name and bounds in screenshot pixels. When the thing to point at is in the list, pass its id as `element` in the draw shape and the shape snaps to its exact bounds. The list can be incomplete or include hidden items, so trust the image when they disagree and use pixel coordinates.");
    }
    if v.zoom {
        p.push_str("\n\nIf text or icons you need are too small to read reliably, call `zoom` with that region of the screenshot to see it at full resolution before answering. Don't zoom when the screenshot is already clear.");
    }
    if v.guide {
        p.push_str("\n\nWhen the user wants to be walked through doing something on screen, guide one step at a time: point at only the next target (draw with clear: true), say that one step in a short sentence, then call `watch`. It waits until the user acts and returns a fresh screenshot; check it and continue with the next step, or correct them if something went wrong. When the task is done, say so briefly and stop calling `watch`. If watch says the user stopped, stop guiding.");
    }
    p
}

pub const ZOOM_DESC: &str = "See a region of the current screenshot at full native screen resolution (for small text, icons, dense UI). Pass the region in screenshot pixels. Returns an image with a zoom id; to draw on something you located in that image, add `zoom: <id>` to the shape and use that image's pixels.";
pub const WATCH_DESC: &str = "Guide mode: after pointing at the next step, wait for the user to do it. Blocks until the screen or keyboard focus changes (or the user presses Esc), then returns a fresh screenshot of the same area with the same coordinate space and an updated UI element list.";

fn zoom_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "x": {"type": "number"}, "y": {"type": "number"},
            "w": {"type": "number"}, "h": {"type": "number"}
        },
        "required": ["x", "y", "w", "h"]
    })
}

fn watch_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "expect": {"type": "string", "description": "what the user should do next, in a few words"}
        }
    })
}

fn defs(cfg: &Config) -> Vec<(&'static str, &'static str, Value)> {
    let mut v = Vec::new();
    if cfg.vision.zoom {
        v.push(("zoom", ZOOM_DESC, zoom_schema()));
    }
    if cfg.vision.guide {
        v.push(("watch", WATCH_DESC, watch_schema()));
    }
    v
}

pub fn is_tool(name: &str) -> bool {
    matches!(name, "zoom" | "watch")
}

/// Appends the vision tools to an Anthropic `tools` array.
pub fn anthropic_tools(mut tools: Value, cfg: &Config) -> Value {
    if let (Some(a), true) = (tools.as_array_mut(), has_image()) {
        for (name, desc, schema) in defs(cfg) {
            a.push(json!({"name": name, "description": desc, "input_schema": schema}));
        }
    }
    tools
}

pub fn openai_tools(cfg: &Config) -> Vec<Value> {
    if !has_image() {
        return Vec::new();
    }
    defs(cfg)
        .into_iter()
        .map(|(name, desc, schema)| json!({"type": "function", "function": {"name": name, "description": desc, "parameters": schema}}))
        .collect()
}

/// Tools listed by `screenbuddy --mcp` (only when the parent passed a port).
pub fn mcp_tools() -> Vec<Value> {
    if std::env::var_os(PORT_ENV).is_none() {
        return Vec::new();
    }
    defs(&crate::config::load().0)
        .into_iter()
        .map(|(name, desc, schema)| json!({"name": name, "description": desc, "inputSchema": schema}))
        .collect()
}

/// `--allowedTools` entries for the Claude CLI.
pub fn cli_allowed(cfg: &Config) -> Vec<&'static str> {
    let mut v = Vec::new();
    if cfg.vision.zoom {
        v.push("mcp__screenbuddy__zoom");
    }
    if cfg.vision.guide {
        v.push("mcp__screenbuddy__watch");
    }
    v
}

// ---------------------------------------------------------------- UIA context

/// Element list to append to the question when a screenshot is attached.
pub fn context(cfg: &Config) -> String {
    if !cfg.vision.ui_tree {
        return String::new();
    }
    let mut s = state();
    if let Some(rx) = s.pending.take() {
        drop(s);
        let got = rx.recv_timeout(uia::budget() * 2).unwrap_or_default();
        s = state();
        s.elements = select(got, MAX_LIST);
    }
    match s.base {
        Some(base) => format_elements(&s.elements, base),
        None => String::new(),
    }
}

/// Keeps at most `max` elements, preferring named ones, in tree order.
fn select(els: Vec<Element>, max: usize) -> Vec<Element> {
    if els.len() <= max {
        return els;
    }
    let mut idx: Vec<usize> = (0..els.len()).collect();
    idx.sort_by_key(|&i| els[i].name.is_empty());
    idx.truncate(max);
    idx.sort_unstable();
    idx.into_iter().map(|i| els[i].clone()).collect()
}

fn format_elements(els: &[Element], base: Frame) -> String {
    let mut lines = Vec::new();
    for (i, e) in els.iter().enumerate() {
        let Some((x, y, w, h)) = base.rect_in(e.rect) else {
            continue;
        };
        if w < 2.0 || h < 2.0 {
            continue;
        }
        lines.push(format!(
            "e{} {} \"{}\" {},{},{},{}{}",
            i + 1,
            e.kind,
            e.name,
            x.round(),
            y.round(),
            w.round(),
            h.round(),
            if e.focused { " *" } else { "" }
        ));
    }
    if lines.is_empty() {
        return String::new();
    }
    format!(
        "\n[UI elements: id type \"name\" x,y,w,h in screenshot px; * = keyboard focus]\n{}",
        lines.join("\n")
    )
}

// ---------------------------------------------------------------- draw resolution

/// Rewrites `element` / `zoom` shapes into full-screenshot pixels.
pub fn resolve_draw(input: Value) -> Result<Value, String> {
    let s = state();
    let Some(base) = s.base else { return Ok(input) };
    resolve(input, base, &s.zooms, &s.elements)
}

fn resolve(
    mut input: Value,
    base: Frame,
    zooms: &[Frame],
    els: &[Element],
) -> Result<Value, String> {
    let Some(shapes) = input.get_mut("shapes").and_then(Value::as_array_mut) else {
        return Ok(input);
    };
    for (i, s) in shapes.iter_mut().enumerate() {
        let Some(obj) = s.as_object_mut() else {
            continue;
        };
        let num = |o: &serde_json::Map<String, Value>, k: &str| {
            o.get(k).and_then(Value::as_f64).map(|v| v as f32)
        };
        if let Some(id) = obj.remove("zoom").filter(|v| !v.is_null()) {
            let id = id
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| id.to_string());
            let z = id
                .trim_start_matches(['z', 'Z'])
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| zooms.get(n))
                .ok_or_else(|| format!("shape {i}: unknown zoom id '{id}'"))?;
            for (kx, ky) in [("x", "y"), ("x2", "y2")] {
                if let (Some(x), Some(y)) = (num(obj, kx), num(obj, ky)) {
                    let (sx, sy) = z.to_screen(x, y);
                    let (bx, by) = base.image_point(sx, sy);
                    obj.insert(kx.into(), round(bx));
                    obj.insert(ky.into(), round(by));
                }
            }
            let k = z.scale() / base.scale();
            for key in ["w", "h", "r"] {
                if let Some(v) = num(obj, key) {
                    obj.insert(key.into(), round(v * k));
                }
            }
        }
        if let Some(id) = obj.remove("element").filter(|v| !v.is_null()) {
            let id = id
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| id.to_string());
            let el = id
                .trim_start_matches(['e', 'E'])
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| els.get(n))
                .ok_or_else(|| format!("shape {i}: unknown element id '{id}'; use an id from the latest UI element list or pixel coordinates"))?;
            let (x, y, w, h) = base
                .rect_in(el.rect)
                .ok_or_else(|| format!("shape {i}: element '{id}' is outside the screenshot"))?;
            snap(obj, (x, y, w, h), (base.w as f32, base.h as f32));
        }
    }
    Ok(input)
}

fn round(v: f32) -> Value {
    json!(((v * 10.0).round() / 10.0) as f64)
}

/// Places a shape on an element's bounds (all in screenshot px).
fn snap(
    obj: &mut serde_json::Map<String, Value>,
    (x, y, w, h): (f32, f32, f32, f32),
    img: (f32, f32),
) {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let kind = obj
        .get("kind")
        .and_then(Value::as_str)
        .unwrap_or("box")
        .to_string();
    let get = |o: &serde_json::Map<String, Value>, k: &str| {
        o.get(k).and_then(Value::as_f64).map(|v| v as f32)
    };
    let put = |o: &mut serde_json::Map<String, Value>, k: &str, v: f32| {
        o.insert(k.into(), round(v));
    };
    match kind.as_str() {
        "circle" => {
            put(obj, "x", cx);
            put(obj, "y", cy);
            put(obj, "r", w.max(h) / 2.0 + 4.0);
        }
        "arrow" => {
            let tail = match (get(obj, "x"), get(obj, "y")) {
                (Some(tx), Some(ty)) => (tx, ty),
                _ => {
                    // Come in from the side facing the image center so the arrow stays visible.
                    let (dx, dy) = (img.0 / 2.0 - cx, img.1 / 2.0 - cy);
                    let len = (dx * dx + dy * dy).sqrt();
                    let (ux, uy) = if len < 1.0 {
                        (
                            -std::f32::consts::FRAC_1_SQRT_2,
                            std::f32::consts::FRAC_1_SQRT_2,
                        )
                    } else {
                        (dx / len, dy / len)
                    };
                    let reach = w.max(h) / 2.0 + 90.0;
                    (cx + ux * reach, cy + uy * reach)
                }
            };
            let (hx, hy) = edge_point((x, y, w, h), tail);
            put(obj, "x", tail.0);
            put(obj, "y", tail.1);
            put(obj, "x2", hx);
            put(obj, "y2", hy);
        }
        "text" | "label" => {
            if get(obj, "x").is_none() || get(obj, "y").is_none() {
                put(obj, "x", x);
                put(obj, "y", y + h + 4.0);
            }
        }
        _ => {
            put(obj, "x", x);
            put(obj, "y", y);
            put(obj, "w", w);
            put(obj, "h", h);
        }
    }
}

/// Where the segment from `from` to the rect's center crosses the rect's edge.
fn edge_point((x, y, w, h): (f32, f32, f32, f32), from: (f32, f32)) -> (f32, f32) {
    let (cx, cy) = (x + w / 2.0, y + h / 2.0);
    let (vx, vy) = (from.0 - cx, from.1 - cy);
    if vx.abs() < 1e-3 && vy.abs() < 1e-3 {
        return (cx, cy);
    }
    let tx = if vx.abs() > 1e-3 {
        (w / 2.0) / vx.abs()
    } else {
        f32::INFINITY
    };
    let ty = if vy.abs() > 1e-3 {
        (h / 2.0) / vy.abs()
    } else {
        f32::INFINITY
    };
    let t = tx.min(ty).min(1.0);
    (cx + vx * t, cy + vy * t)
}

// ---------------------------------------------------------------- tools

/// Runs a vision tool for the active turn.
pub fn handle(name: &str, input: &Value, out: &Out) -> ToolOut {
    match name {
        "zoom" => zoom(input, out),
        "watch" => watch(out),
        _ => ToolOut::text("error: unknown tool", true),
    }
}

fn zoom(input: &Value, out: &Out) -> ToolOut {
    let (base, monitor) = {
        let s = state();
        match (s.base, s.monitor) {
            (Some(b), Some(m)) => (b, m),
            _ => return ToolOut::text("error: no screenshot to zoom into", true),
        }
    };
    let n = |k: &str| input.get(k).and_then(Value::as_f64).map(|v| v as f32);
    let (Some(x), Some(y), Some(w), Some(h)) = (n("x"), n("y"), n("w"), n("h")) else {
        return ToolOut::text("error: zoom needs x, y, w, h in screenshot pixels", true);
    };
    let Some(rect) = zoom_rect(base, monitor.rect, (x, y, w, h)) else {
        return ToolOut::text("error: that region is outside the screenshot", true);
    };
    out.status("zooming…");
    let cap = match capture::grab_rect(rect).and_then(|s| capture::encode(&s, None)) {
        Ok(c) => c,
        Err(e) => return ToolOut::text(format!("error: capture failed ({e})"), true),
    };
    let frame = Frame::of(&cap);
    let id = {
        let mut s = state();
        s.zooms.push(frame);
        format!("z{}", s.zooms.len())
    };
    let (bx, by, bw, bh) = base.rect_in(rect).unwrap_or((x, y, w, h));
    let native = if frame.scale() <= 1.0 + 1e-3 {
        "native resolution"
    } else {
        "downscaled to fit image limits"
    };
    crate::config::log(&format!(
        "zoom {id}: screen {rect:?} -> {}x{}",
        frame.w, frame.h
    ));
    out.status("thinking…");
    ToolOut {
        text: format!(
            "Zoom {id}: screenshot region {},{},{},{} is shown {:.1}x larger ({}x{} px, {native}). To draw on something you see here, add \"zoom\": \"{id}\" to the shape and use this image's pixels; shapes without it use full-screenshot pixels.",
            bx.round(),
            by.round(),
            bw.round(),
            bh.round(),
            base.scale() / frame.scale(),
            frame.w,
            frame.h
        ),
        image: Some(cap.jpeg),
        is_error: false,
    }
}

/// The screen rectangle for a zoom request (screenshot px), at least
/// MIN_ZOOM screen px per side and clipped to the monitor.
fn zoom_rect(base: Frame, monitor: Rect, (x, y, w, h): (f32, f32, f32, f32)) -> Option<Rect> {
    let (x0, y0) = base.to_screen(x.min(x + w), y.min(y + h));
    let (x1, y1) = base.to_screen(x.max(x + w), y.max(y + h));
    let (mut x0, mut y0, mut x1, mut y1) = (
        x0.floor() as i32,
        y0.floor() as i32,
        x1.ceil() as i32,
        y1.ceil() as i32,
    );
    for (a, b) in [(&mut x0, &mut x1), (&mut y0, &mut y1)] {
        let short = MIN_ZOOM - (*b - *a);
        if short > 0 {
            *a -= short / 2;
            *b += short - short / 2;
        }
    }
    let r = uia::intersect(
        Rect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        },
        monitor,
    )?;
    uia::intersect(r, base.src)
}

fn esc_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};
    unsafe { (GetAsyncKeyState(VK_ESCAPE.0 as i32) as u16 & 0x8000) != 0 }
}

/// Cells whose luma changed by more than CELL_DELTA between two thumbnails.
pub fn changed_cells(a: &[u8], b: &[u8]) -> usize {
    if a.len() != b.len() {
        return usize::MAX;
    }
    a.iter()
        .zip(b)
        .filter(|(x, y)| x.abs_diff(**y) > CELL_DELTA)
        .count()
}

pub fn is_change(cells: usize) -> bool {
    cells >= MIN_CELLS
}

enum Wake {
    Changed,
    Stopped,
    Cancelled,
    Timeout,
}

fn watch(out: &Out) -> ToolOut {
    let (base, monitor, cancel, stopped) = {
        let s = state();
        match (s.base, s.monitor) {
            (Some(b), Some(m)) => (b, m, s.cancel.clone().unwrap_or_default(), s.stopped),
            _ => return ToolOut::text("error: no screenshot to watch", true),
        }
    };
    if stopped {
        return ToolOut::text(
            "The user stopped guide mode. Stop guiding; acknowledge in a few words.",
            false,
        );
    }
    // Only the work area: the taskbar clock and tray would count as changes.
    let area = uia::intersect(base.src, monitor.work).unwrap_or(base.src);
    out.status("watching…");
    let wake = wait_for_change(area, &cancel);
    match wake {
        Wake::Cancelled => return ToolOut::text("Cancelled.", true),
        Wake::Stopped => {
            state().stopped = true;
            out.status("thinking…");
            return ToolOut::text(
                "The user pressed Esc to stop guide mode. Stop guiding; acknowledge in a few words.",
                false,
            );
        }
        Wake::Timeout => {
            out.status("thinking…");
            return ToolOut::text(
                format!(
                    "Nothing changed for {} minutes. Ask whether they want to continue instead of calling watch again.",
                    WATCH_MAX.as_secs() / 60
                ),
                false,
            );
        }
        Wake::Changed => {}
    }
    // The old pointer refers to the old screen.
    let _ = out.draw(json!({"clear": true, "shapes": []}));
    out.status("looking…");
    let region = (base.src != monitor.rect).then_some(base.src);
    let cap = match capture::grab_rect(monitor.rect).and_then(|s| capture::encode(&s, region)) {
        Ok(c) => c,
        Err(e) => return ToolOut::text(format!("error: capture failed ({e})"), true),
    };
    let cfg = crate::config::load().0;
    let ctx = {
        let mut s = state();
        s.base = Some(Frame::of(&cap));
        s.elements.clear();
        s.pending = None;
        drop(s);
        if cfg.vision.ui_tree {
            let roots = uia::target_windows(cap.src, capture::cursor_pos());
            let got = select(uia::snapshot(roots, cap.src, uia::budget()), MAX_LIST);
            let mut s = state();
            s.elements = got;
            format_elements(&s.elements, Frame::of(&cap))
        } else {
            String::new()
        }
    };
    out.status("thinking…");
    ToolOut {
        text: format!(
            "The screen changed. Fresh screenshot attached ({}x{} px, same coordinates as before; earlier element ids no longer apply).{ctx}",
            cap.img_w, cap.img_h
        ),
        image: Some(cap.jpeg),
        is_error: false,
    }
}

/// Sleeps in short ticks until the screen (or focus) changes and settles.
/// Checks Esc every tick and the screen every few ticks; no busy work.
fn wait_for_change(area: Rect, cancel: &AtomicBool) -> Wake {
    let probe = uia::FocusProbe::new();
    let focus = |p: &Option<uia::FocusProbe>| p.as_ref().and_then(|p| p.signature());
    let shot = || {
        capture::thumb(area, THUMB_EDGE)
            .map(|t| t.2)
            .unwrap_or_default()
    };
    let focus0 = focus(&probe);
    let mut base = shot();
    let start = Instant::now();
    let mut tick = 0u32;
    loop {
        std::thread::sleep(TICK);
        tick += 1;
        if cancel.load(Ordering::Relaxed) {
            return Wake::Cancelled;
        }
        if esc_down() {
            return Wake::Stopped;
        }
        if start.elapsed() > WATCH_MAX {
            return Wake::Timeout;
        }
        if !tick.is_multiple_of(SCREEN_EVERY) {
            continue;
        }
        let now = shot();
        let focus_moved = focus0.is_some() && focus(&probe) != focus0;
        if !focus_moved && !is_change(changed_cells(&base, &now)) {
            continue;
        }
        // Debounce: wait for the screen to stop moving (animations, typing).
        let mut prev = now;
        let settle = Instant::now();
        while settle.elapsed() < SETTLE_MAX {
            std::thread::sleep(TICK * 3);
            if cancel.load(Ordering::Relaxed) {
                return Wake::Cancelled;
            }
            if esc_down() {
                return Wake::Stopped;
            }
            let next = shot();
            let still = !is_change(changed_cells(&prev, &next));
            prev = next;
            if still {
                break;
            }
        }
        // A hover that came and went is not a step.
        if focus_moved || is_change(changed_cells(&base, &prev)) {
            return Wake::Changed;
        }
        base = prev;
    }
}

// ---------------------------------------------------------------- Claude CLI bridge

const PORT_ENV: &str = "SCREENBUDDY_VISION_PORT";
const TOKEN_ENV: &str = "SCREENBUDDY_VISION_TOKEN";

/// Loopback server through which `screenbuddy --mcp` runs zoom/watch in
/// this process. Started once; the accept thread sleeps in `accept`.
fn server() -> Option<&'static (u16, String)> {
    static SERVER: std::sync::OnceLock<Option<(u16, String)>> = std::sync::OnceLock::new();
    SERVER
        .get_or_init(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").ok()?;
            let port = listener.local_addr().ok()?.port();
            let token = random_token();
            let t = token.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming().flatten() {
                    let t = t.clone();
                    std::thread::spawn(move || serve(stream, &t));
                }
            });
            Some((port, token))
        })
        .as_ref()
}

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut h = std::collections::hash_map::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    let a = h.finish();
    let b = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    format!("{a:016x}{b:016x}")
}

fn serve(stream: std::net::TcpStream, token: &str) {
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let req: Value = serde_json::from_str(&line).unwrap_or_default();
    let res = if req["token"].as_str() != Some(token) {
        ToolOut::text("error: unauthorized", true)
    } else {
        let out = state().out.clone();
        match out {
            Some(out) => handle(req["tool"].as_str().unwrap_or(""), &req["input"], &out),
            None => ToolOut::text("error: no active question", true),
        }
    };
    let reply =
        json!({"text": res.text, "is_error": res.is_error, "image": res.image.as_deref().map(b64)});
    let mut w = &stream;
    let _ = writeln!(w, "{reply}");
}

/// Environment for the MCP server spawned by the Claude CLI.
pub fn cli_env(env: &mut serde_json::Map<String, Value>) {
    if let Some((port, token)) = server() {
        env.insert(PORT_ENV.into(), json!(port.to_string()));
        env.insert(TOKEN_ENV.into(), json!(token));
    }
}

/// `tools/call` for zoom/watch inside `screenbuddy --mcp`: forwarded to the parent.
pub fn mcp_call(name: &str, args: &Value) -> Value {
    let port = std::env::var(PORT_ENV)
        .ok()
        .and_then(|p| p.parse::<u16>().ok());
    let token = std::env::var(TOKEN_ENV).unwrap_or_default();
    let res = port
        .and_then(|p| std::net::TcpStream::connect(("127.0.0.1", p)).ok())
        .and_then(|mut s| {
            writeln!(
                s,
                "{}",
                json!({"token": token, "tool": name, "input": args})
            )
            .ok()?;
            let mut line = String::new();
            BufReader::new(&s).read_line(&mut line).ok()?;
            serde_json::from_str::<Value>(&line).ok()
        });
    match res {
        Some(v) => ToolOut {
            text: v["text"].as_str().unwrap_or("").to_string(),
            image: v["image"]
                .as_str()
                .and_then(|d| base64::engine::general_purpose::STANDARD.decode(d).ok()),
            is_error: v["is_error"].as_bool().unwrap_or(false),
        }
        .mcp(),
        None => ToolOut::text("error: screenbuddy is not reachable", true).mcp(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Frame {
        // 3840x2160 monitor at x=1920 downscaled to 1430x804.
        Frame {
            src: Rect {
                x: 1920,
                y: 0,
                w: 3840,
                h: 2160,
            },
            w: 1430,
            h: 804,
        }
    }

    #[test]
    fn frame_roundtrip() {
        let b = base();
        let (sx, sy) = b.to_screen(100.0, 50.0);
        let (x, y) = b.image_point(sx, sy);
        assert!((x - 100.0).abs() < 1e-3 && (y - 50.0).abs() < 1e-3);
        let (rx, ry, rw, rh) = b
            .rect_in(Rect {
                x: 1920 + 384,
                y: 216,
                w: 384,
                h: 216,
            })
            .unwrap();
        assert!(
            (rx - 143.0).abs() < 0.01 && (ry - 80.4).abs() < 0.01,
            "{rx},{ry}"
        );
        assert!((rw - 143.0).abs() < 0.01 && (rh - 80.4).abs() < 0.01);
        assert!(
            b.rect_in(Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100
            })
            .is_none()
        );
    }

    #[test]
    fn zoom_maps_back_to_screenshot() {
        let b = base();
        let r = zoom_rect(b, b.src, (100.0, 100.0, 200.0, 100.0)).unwrap();
        let (x0, y0) = b.to_screen(100.0, 100.0);
        assert_eq!((r.x, r.y), (x0.floor() as i32, y0.floor() as i32));
        // Native-resolution zoom of that area.
        let z = Frame {
            src: r,
            w: r.w as u32,
            h: r.h as u32,
        };
        let input = json!({"shapes": [{"kind": "box", "zoom": "z1", "x": 0, "y": 0, "w": r.w, "h": r.h},
                                      {"kind": "arrow", "zoom": "z1", "x": 10, "y": 10, "x2": 20, "y2": 20}]});
        let out = resolve(input, b, &[z], &[]).unwrap();
        let s = &out["shapes"][0];
        assert!((s["x"].as_f64().unwrap() - 100.0).abs() < 0.6, "{s}");
        assert!((s["y"].as_f64().unwrap() - 100.0).abs() < 0.6);
        assert!((s["w"].as_f64().unwrap() - 200.0).abs() < 1.0, "{s}");
        assert!(s.get("zoom").is_none());
        let a = &out["shapes"][1];
        assert!(a["x2"].as_f64().unwrap() > a["x"].as_f64().unwrap());
        assert!(
            resolve(
                json!({"shapes": [{"kind": "box", "zoom": "z9", "x": 0, "y": 0}]}),
                b,
                &[z],
                &[]
            )
            .is_err()
        );
    }

    #[test]
    fn tiny_zoom_grows_and_clips() {
        let b = Frame {
            src: Rect {
                x: 0,
                y: 0,
                w: 1000,
                h: 800,
            },
            w: 1000,
            h: 800,
        };
        let r = zoom_rect(b, b.src, (500.0, 400.0, 2.0, 2.0)).unwrap();
        assert_eq!((r.w, r.h), (MIN_ZOOM, MIN_ZOOM));
        let r = zoom_rect(b, b.src, (990.0, 790.0, 50.0, 50.0)).unwrap();
        assert_eq!((r.x + r.w, r.y + r.h), (1000, 800));
        assert!(zoom_rect(b, b.src, (2000.0, 2000.0, 10.0, 10.0)).is_none());
    }

    #[test]
    fn element_snaps() {
        let b = base();
        let el = Element {
            kind: "button",
            name: "Save".into(),
            rect: Rect {
                x: 1920 + 384,
                y: 216,
                w: 384,
                h: 216,
            },
            focused: false,
        };
        let out = resolve(json!({"shapes": [{"kind": "box", "element": "e1"}, {"kind": "circle", "element": "e1"}, {"kind": "arrow", "element": "e1"}]}), b, &[], &[el.clone()]).unwrap();
        let bx = &out["shapes"][0];
        assert!(
            (bx["x"].as_f64().unwrap() - 143.0).abs() < 0.1
                && (bx["w"].as_f64().unwrap() - 143.0).abs() < 0.1,
            "{bx}"
        );
        let c = &out["shapes"][1];
        assert!((c["x"].as_f64().unwrap() - 214.5).abs() < 0.1);
        let a = &out["shapes"][2];
        // Head lands on the element's edge, tail outside it.
        let (hx, hy) = (a["x2"].as_f64().unwrap(), a["y2"].as_f64().unwrap());
        assert!(
            (143.0..=286.1).contains(&hx) && (80.3..=160.9).contains(&hy),
            "{a}"
        );
        assert!(a["x"].as_f64().unwrap() > 286.0);
        assert!(
            resolve(
                json!({"shapes": [{"kind": "box", "element": "e2"}]}),
                b,
                &[],
                &[el]
            )
            .is_err()
        );
        let txt = format_elements(
            &[Element {
                kind: "button",
                name: "Save".into(),
                rect: Rect {
                    x: 1920 + 384,
                    y: 216,
                    w: 384,
                    h: 216,
                },
                focused: true,
            }],
            b,
        );
        assert!(txt.contains("e1 button \"Save\" 143,80,143,80 *"), "{txt}");
    }

    #[test]
    fn change_detection() {
        let a = vec![100u8; 256 * 144];
        let mut b = a.clone();
        assert!(!is_change(changed_cells(&a, &b)));
        // Small noise (a blinking caret, compression jitter) is ignored.
        b[10] = 160;
        b[20] = 110;
        assert!(!is_change(changed_cells(&a, &b)));
        // A dialog appearing changes a block of cells.
        for v in b.iter_mut().skip(1000).take(400) {
            *v = 230;
        }
        assert!(is_change(changed_cells(&a, &b)));
        assert!(is_change(changed_cells(&a, &a[..10])));
    }

    #[test]
    fn select_prefers_named() {
        let e = |n: &str| Element {
            kind: "button",
            name: n.into(),
            rect: Rect::default(),
            focused: false,
        };
        let got = select(vec![e(""), e("a"), e(""), e("b")], 2);
        assert_eq!(
            got.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(),
            ["a", "b"]
        );
    }
}
