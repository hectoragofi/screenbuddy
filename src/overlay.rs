//! Click-through annotation overlay (layered window + tiny-skia raster) and
//! the drag-to-select region picker.

use crate::capture::{Capture, Rect};
use crate::util::wide;
use serde_json::Value;
use std::sync::OnceLock;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, Transform,
};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{ReleaseCapture, SetCapture, VK_ESCAPE};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

// ---------------------------------------------------------------- shapes

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Arrow,
    Box,
    Circle,
    Highlight,
    Text,
}

/// A shape in overlay-independent virtual-screen pixels.
#[derive(Clone, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub x: f32,
    pub y: f32,
    pub x2: f32,
    pub y2: f32,
    pub w: f32,
    pub h: f32,
    pub r: f32,
    pub label: Option<String>,
    pub color: [u8; 3],
}

#[cfg(test)]
const ACCENT: [u8; 3] = [10, 132, 255];

fn parse_color(s: &str) -> Option<[u8; 3]> {
    let s = s.trim().to_ascii_lowercase();
    let named = match s.as_str() {
        "red" => Some([255, 69, 58]),
        "orange" => Some([255, 159, 10]),
        "yellow" => Some([255, 214, 10]),
        "green" => Some([48, 209, 88]),
        "blue" => Some([10, 132, 255]),
        "purple" => Some([191, 90, 242]),
        "pink" => Some([255, 55, 95]),
        "white" => Some([255, 255, 255]),
        "black" => Some([20, 20, 20]),
        _ => None,
    };
    if named.is_some() {
        return named;
    }
    let hex = s.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

/// Result of parsing one `draw` tool call.
pub struct DrawCall {
    pub clear: bool,
    pub shapes: Vec<Shape>,
}

/// Parses a `draw` tool input (screenshot pixel coords) into screen shapes.
pub fn parse_draw(
    input: &Value,
    cap: &Capture,
    default_color: [u8; 3],
) -> Result<DrawCall, String> {
    let clear = input.get("clear").and_then(Value::as_bool).unwrap_or(false);
    let list = match input.get("shapes") {
        Some(Value::Array(a)) => a.clone(),
        None | Some(Value::Null) => Vec::new(),
        Some(_) => return Err("`shapes` must be an array".into()),
    };
    let ls = cap.length_scale();
    let mut shapes = Vec::new();
    for (i, s) in list.iter().enumerate() {
        let num = |k: &str| s.get(k).and_then(Value::as_f64).map(|v| v as f32);
        let kind = match s.get("kind").and_then(Value::as_str).unwrap_or("") {
            "arrow" => Kind::Arrow,
            "box" | "rect" | "rectangle" => Kind::Box,
            "circle" => Kind::Circle,
            "highlight" => Kind::Highlight,
            "text" | "label" => Kind::Text,
            other => return Err(format!("shape {i}: unknown kind '{other}'")),
        };
        let (Some(x), Some(y)) = (num("x"), num("y")) else {
            return Err(format!("shape {i}: x and y are required"));
        };
        let (sx, sy) = cap.to_screen(x, y);
        let (ex, ey) = match (num("x2"), num("y2")) {
            (Some(a), Some(b)) => cap.to_screen(a, b),
            _ if kind == Kind::Arrow => return Err(format!("shape {i}: arrow needs x2,y2")),
            _ => (sx, sy),
        };
        if matches!(kind, Kind::Box | Kind::Highlight) && (num("w").is_none() || num("h").is_none())
        {
            return Err(format!("shape {i}: {:?} needs w,h", kind));
        }
        shapes.push(Shape {
            kind,
            x: sx,
            y: sy,
            x2: ex,
            y2: ey,
            w: num("w").unwrap_or(0.0).abs() * ls,
            h: num("h").unwrap_or(0.0).abs() * ls,
            r: num("r").unwrap_or(24.0).abs().max(4.0) * ls,
            label: s
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned),
            color: s
                .get("color")
                .and_then(Value::as_str)
                .and_then(parse_color)
                .unwrap_or(default_color),
        });
    }
    Ok(DrawCall { clear, shapes })
}

// ---------------------------------------------------------------- text

fn font() -> Option<&'static fontdue::Font> {
    static FONT: OnceLock<Option<fontdue::Font>> = OnceLock::new();
    FONT.get_or_init(|| {
        let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
        for name in [
            "seguisb.ttf",
            "segoeuib.ttf",
            "segoeui.ttf",
            "arialbd.ttf",
            "arial.ttf",
        ] {
            if let Ok(bytes) = std::fs::read(format!(r"{windir}\Fonts\{name}")) {
                if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    return Some(f);
                }
            }
        }
        None
    })
    .as_ref()
}

fn text_width(text: &str, px: f32) -> f32 {
    match font() {
        Some(f) => text.chars().map(|c| f.metrics(c, px).advance_width).sum(),
        None => text.chars().count() as f32 * px * 0.55,
    }
}

/// Blends glyph coverage in `rgb` into a premultiplied pixmap; (x, y) is top-left.
fn draw_text(pm: &mut Pixmap, text: &str, x: f32, y: f32, px: f32, rgb: [u8; 3]) {
    let Some(f) = font() else { return };
    let ascent = f
        .horizontal_line_metrics(px)
        .map(|m| m.ascent)
        .unwrap_or(px * 0.8);
    let baseline = y + ascent;
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let pixels = pm.pixels_mut();
    let mut pen = x;
    for c in text.chars() {
        let (m, bitmap) = f.rasterize(c, px);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
        for row in 0..m.height as i32 {
            for col in 0..m.width as i32 {
                let (tx, ty) = (gx + col, gy + row);
                if tx < 0 || ty < 0 || tx >= pw || ty >= ph {
                    continue;
                }
                let a = bitmap[(row * m.width as i32 + col) as usize] as u32;
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
        pen += m.advance_width;
    }
}

// ---------------------------------------------------------------- raster

struct Label {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    line: Line,
    color: [u8; 3],
}

/// A label's text and formulas laid out on one baseline.
struct Line {
    runs: Vec<Run>,
    width: f32,
    above: f32,
    below: f32,
    math: bool,
}

enum Run {
    Text(String, f32),
    Math(std::sync::Arc<katex_native::raster::Raster>),
}

fn ink_for(c: [u8; 3]) -> [u8; 3] {
    let luma = 0.299 * c[0] as f32 + 0.587 * c[1] as f32 + 0.114 * c[2] as f32;
    if luma > 170.0 {
        [20, 20, 20]
    } else {
        [255, 255, 255]
    }
}

fn lay_line(text: &str, px: f32, ink: [u8; 3]) -> Line {
    let ascent = font()
        .and_then(|f| f.horizontal_line_metrics(px))
        .map(|m| m.ascent)
        .unwrap_or(px * 0.8);
    let mut line = Line {
        runs: Vec::new(),
        width: 0.0,
        above: ascent,
        below: px * 1.35 - ascent,
        math: false,
    };
    for piece in crate::math::split(text) {
        match piece {
            // Labels are tiny; display style keeps fractions and limits legible.
            crate::math::Piece::Math { tex, .. } => {
                if let Some(r) = crate::math::ink(&tex, true, px * 1.05, ink) {
                    line.above = line.above.max(r.baseline as f32);
                    line.below = line.below.max(r.height as f32 - r.baseline as f32);
                    line.width += r.width as f32 + px * 0.15;
                    line.math = true;
                    line.runs.push(Run::Math(r));
                } else {
                    let w = text_width(&tex, px);
                    line.width += w;
                    line.runs.push(Run::Text(tex, w));
                }
            }
            crate::math::Piece::Text(t) => {
                let w = text_width(&t, px);
                line.width += w;
                line.runs.push(Run::Text(t, w));
            }
        }
    }
    line
}

/// Blends a straight-RGBA bitmap into a premultiplied pixmap at (x, y).
fn blit(pm: &mut Pixmap, r: &katex_native::raster::Raster, x: i32, y: i32) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let pixels = pm.pixels_mut();
    for row in 0..r.height as i32 {
        for col in 0..r.width as i32 {
            let (tx, ty) = (x + col, y + row);
            if tx < 0 || ty < 0 || tx >= pw || ty >= ph {
                continue;
            }
            let i = ((row * r.width as i32 + col) * 4) as usize;
            let a = r.rgba[i + 3] as u32;
            if a == 0 {
                continue;
            }
            let p = &mut pixels[(ty * pw + tx) as usize];
            let inv = 255 - a;
            let blend = |src: u8, d: u8| ((a * src as u32 + d as u32 * inv) / 255) as u8;
            *p = tiny_skia::PremultipliedColorU8::from_rgba(
                blend(r.rgba[i], p.red()),
                blend(r.rgba[i + 1], p.green()),
                blend(r.rgba[i + 2], p.blue()),
                blend(255, p.alpha()),
            )
            .unwrap_or(*p);
        }
    }
}

fn label_box(s: &Shape, scale: f32, m: Rect) -> Option<Label> {
    let text = s.label.as_deref()?;
    let px = 15.0 * scale;
    let pad = 8.0 * scale;
    let line = lay_line(text, px, ink_for(s.color));
    let w = line.width + pad * 2.0;
    let h = line.above + line.below + pad;
    let gap = 6.0 * scale;
    let (x, y) = match s.kind {
        // Label at the arrow's tail, pushed away from the head.
        Kind::Arrow => {
            let dx = s.x - s.x2;
            let dy = s.y - s.y2;
            let lx = if dx >= 0.0 { s.x + gap } else { s.x - w - gap };
            let ly = if dy >= 0.0 { s.y + gap } else { s.y - h - gap };
            (lx, ly)
        }
        Kind::Box | Kind::Highlight => (s.x, s.y - h - gap),
        Kind::Circle => (s.x + s.r + gap, s.y - h / 2.0),
        Kind::Text => (s.x, s.y),
    };
    // Keep labels on the monitor: flip below boxes that touch the top edge.
    let (mx0, my0, mx1, my1) = (
        m.x as f32,
        m.y as f32,
        (m.x + m.w) as f32,
        (m.y + m.h) as f32,
    );
    let y = if y < my0 && matches!(s.kind, Kind::Box | Kind::Highlight) {
        s.y + s.h + gap
    } else {
        y
    };
    let x = x.clamp(mx0, (mx1 - w).max(mx0));
    let y = y.clamp(my0, (my1 - h).max(my0));
    Some(Label {
        x,
        y,
        w,
        h,
        line,
        color: s.color,
    })
}

fn bounds(s: &Shape, scale: f32, m: Rect) -> (f32, f32, f32, f32) {
    let pad = 12.0 * scale;
    let (mut x0, mut y0, mut x1, mut y1) = match s.kind {
        Kind::Arrow => (s.x.min(s.x2), s.y.min(s.y2), s.x.max(s.x2), s.y.max(s.y2)),
        Kind::Box | Kind::Highlight => (s.x, s.y, s.x + s.w, s.y + s.h),
        Kind::Circle => (s.x - s.r, s.y - s.r, s.x + s.r, s.y + s.r),
        Kind::Text => (s.x, s.y, s.x, s.y),
    };
    if let Some(l) = label_box(s, scale, m) {
        x0 = x0.min(l.x);
        y0 = y0.min(l.y);
        x1 = x1.max(l.x + l.w);
        y1 = y1.max(l.y + l.h);
    }
    (x0 - pad, y0 - pad, x1 + pad, y1 + pad)
}

fn rgba(c: [u8; 3], a: f32) -> Color {
    Color::from_rgba8(c[0], c[1], c[2], (a * 255.0) as u8)
}

fn paint(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c);
    p.anti_alias = true;
    p
}

fn stroke(width: f32) -> Stroke {
    Stroke {
        width,
        line_cap: LineCap::Round,
        line_join: LineJoin::Round,
        ..Default::default()
    }
}

/// Strokes `path` twice: a soft dark halo for contrast, then the color.
fn halo_stroke(pm: &mut Pixmap, path: &tiny_skia::Path, color: [u8; 3], w: f32, t: Transform) {
    pm.stroke_path(
        path,
        &paint(Color::from_rgba8(0, 0, 0, 90)),
        &stroke(w + 3.0),
        t,
        None,
    );
    pm.stroke_path(path, &paint(rgba(color, 1.0)), &stroke(w), t, None);
}

fn draw_shape(pm: &mut Pixmap, s: &Shape, scale: f32, m: Rect, t: Transform) {
    let w = 3.0 * scale;
    match s.kind {
        Kind::Arrow => {
            let (dx, dy) = (s.x2 - s.x, s.y2 - s.y);
            let len = (dx * dx + dy * dy).sqrt().max(1.0);
            let (ux, uy) = (dx / len, dy / len);
            let head = (18.0 * scale).min(len * 0.6);
            let (bx, by) = (s.x2 - ux * head, s.y2 - uy * head);
            let mut pb = PathBuilder::new();
            pb.move_to(s.x, s.y);
            pb.line_to(bx + ux * 2.0, by + uy * 2.0);
            if let Some(p) = pb.finish() {
                halo_stroke(pm, &p, s.color, w, t);
            }
            let (px, py) = (-uy * head * 0.55, ux * head * 0.55);
            let mut hb = PathBuilder::new();
            hb.move_to(s.x2, s.y2);
            hb.line_to(bx + px, by + py);
            hb.line_to(bx - px, by - py);
            hb.close();
            if let Some(p) = hb.finish() {
                pm.stroke_path(
                    &p,
                    &paint(Color::from_rgba8(0, 0, 0, 90)),
                    &stroke(3.0),
                    t,
                    None,
                );
                pm.fill_path(&p, &paint(rgba(s.color, 1.0)), FillRule::Winding, t, None);
            }
        }
        Kind::Box | Kind::Highlight => {
            let Some(r) = tiny_skia::Rect::from_xywh(s.x, s.y, s.w.max(1.0), s.h.max(1.0)) else {
                return;
            };
            let p = rounded_rect(r, 8.0 * scale);
            if s.kind == Kind::Highlight {
                pm.fill_path(&p, &paint(rgba(s.color, 0.22)), FillRule::Winding, t, None);
                pm.stroke_path(
                    &p,
                    &paint(rgba(s.color, 0.9)),
                    &stroke(2.0 * scale),
                    t,
                    None,
                );
            } else {
                halo_stroke(pm, &p, s.color, w, t);
            }
        }
        Kind::Circle => {
            if let Some(p) = PathBuilder::from_circle(s.x, s.y, s.r) {
                halo_stroke(pm, &p, s.color, w, t);
            }
        }
        Kind::Text => {}
    }
    if let Some(l) = label_box(s, scale, m) {
        let Some(r) = tiny_skia::Rect::from_xywh(l.x, l.y, l.w, l.h) else {
            return;
        };
        let radius = if l.line.math { 10.0 * scale } else { l.h / 2.0 };
        let p = rounded_rect(r, radius);
        pm.fill_path(&p, &paint(rgba(l.color, 0.95)), FillRule::Winding, t, None);
        let px = 15.0 * scale;
        let (tx, ty) = (t.tx, t.ty);
        let ink = ink_for(l.color);
        let line_h = l.line.above + l.line.below;
        let base = l.y + ty + (l.h - line_h) / 2.0 + l.line.above;
        let ascent = font()
            .and_then(|f| f.horizontal_line_metrics(px))
            .map(|m| m.ascent)
            .unwrap_or(px * 0.8);
        let mut pen = l.x + tx + 8.0 * scale;
        for run in &l.line.runs {
            match run {
                Run::Text(text, w) => {
                    draw_text(pm, text, pen, base - ascent, px, ink);
                    pen += w;
                }
                Run::Math(r) => {
                    let x = (pen + px * 0.075).round() as i32;
                    blit(pm, r, x, (base - r.baseline as f32).round() as i32);
                    pen += r.width as f32 + px * 0.15;
                }
            }
        }
    }
}

fn rounded_rect(r: tiny_skia::Rect, radius: f32) -> tiny_skia::Path {
    let rad = radius.min(r.width() / 2.0).min(r.height() / 2.0);
    let (x0, y0, x1, y1) = (r.left(), r.top(), r.right(), r.bottom());
    let k = 0.552_284_8 * rad;
    let mut pb = PathBuilder::new();
    pb.move_to(x0 + rad, y0);
    pb.line_to(x1 - rad, y0);
    pb.cubic_to(x1 - rad + k, y0, x1, y0 + rad - k, x1, y0 + rad);
    pb.line_to(x1, y1 - rad);
    pb.cubic_to(x1, y1 - rad + k, x1 - rad + k, y1, x1 - rad, y1);
    pb.line_to(x0 + rad, y1);
    pb.cubic_to(x0 + rad - k, y1, x0, y1 - rad + k, x0, y1 - rad);
    pb.line_to(x0, y0 + rad);
    pb.cubic_to(x0, y0 + rad - k, x0 + rad - k, y0, x0 + rad, y0);
    pb.close();
    pb.finish().unwrap_or_else(|| PathBuilder::from_rect(r))
}

// ---------------------------------------------------------------- window

const OVERLAY_CLASS: &str = "screenbuddy_overlay";
const REGION_CLASS: &str = "screenbuddy_region";

fn hinstance() -> HINSTANCE {
    unsafe {
        GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

fn register_class(name: &str, proc_: WNDPROC, cursor: PCWSTR) {
    let cls = wide(name);
    unsafe {
        let wc = WNDCLASSW {
            lpfnWndProc: proc_,
            hInstance: hinstance(),
            lpszClassName: PCWSTR(cls.as_ptr()),
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            ..Default::default()
        };
        // Re-registering on later calls fails harmlessly.
        RegisterClassW(&wc);
    }
}

const FADE_TIMER: usize = 1;

unsafe extern "system" fn overlay_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if msg == WM_NCHITTEST {
            return LRESULT(HTTRANSPARENT as isize);
        }
        if msg == WM_TIMER && wp.0 == FADE_TIMER {
            // Fade in: ease the layer's constant alpha up to opaque.
            let a = (GetWindowLongPtrW(hwnd, GWLP_USERDATA) + 36).min(255);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, a);
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: a as u8,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let _ = UpdateLayeredWindow(
                hwnd,
                None,
                None,
                None,
                None,
                None,
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
            if a >= 255 {
                let _ = KillTimer(Some(hwnd), FADE_TIMER);
            }
            return LRESULT(0);
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

/// Annotation overlay for one monitor. The window is sized to the bounding box
/// of the shapes only, so memory stays proportional to what is drawn.
pub struct Overlay {
    hwnd: HWND,
    monitor: Rect,
    scale: f32,
    shapes: Vec<Shape>,
}

impl Overlay {
    pub fn new(monitor: Rect, scale: f32) -> anyhow::Result<Self> {
        register_class(OVERLAY_CLASS, Some(overlay_proc), IDC_ARROW);
        let cls = wide(OVERLAY_CLASS);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(cls.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                monitor.x,
                monitor.y,
                1,
                1,
                None,
                None,
                Some(hinstance()),
                None,
            )?
        };
        unsafe {
            // Keep annotations out of our own later re-captures.
            if crate::win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
        }
        Ok(Self {
            hwnd,
            monitor,
            scale,
            shapes: Vec::new(),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    pub fn apply(&mut self, call: DrawCall) {
        if call.clear {
            self.shapes.clear();
        }
        self.shapes.extend(call.shapes);
        self.render();
    }

    /// Screen-space bounding box of everything drawn.
    pub fn drawn_bounds(&self) -> Option<Rect> {
        if self.shapes.is_empty() {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for s in &self.shapes {
            let b = bounds(s, self.scale, self.monitor);
            x0 = x0.min(b.0);
            y0 = y0.min(b.1);
            x1 = x1.max(b.2);
            y1 = y1.max(b.3);
        }
        Some(Rect {
            x: x0 as i32,
            y: y0 as i32,
            w: (x1 - x0) as i32,
            h: (y1 - y0) as i32,
        })
    }

    fn render(&mut self) {
        unsafe {
            if self.shapes.is_empty() {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                return;
            }
        }
        let m = self.monitor;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for s in &self.shapes {
            let b = bounds(s, self.scale, m);
            x0 = x0.min(b.0);
            y0 = y0.min(b.1);
            x1 = x1.max(b.2);
            y1 = y1.max(b.3);
        }
        let x0 = (x0.floor() as i32).clamp(m.x, m.x + m.w - 1);
        let y0 = (y0.floor() as i32).clamp(m.y, m.y + m.h - 1);
        let x1 = (x1.ceil() as i32).clamp(x0 + 1, m.x + m.w);
        let y1 = (y1.ceil() as i32).clamp(y0 + 1, m.y + m.h);
        let (w, h) = (x1 - x0, y1 - y0);
        let Some(mut pm) = Pixmap::new(w as u32, h as u32) else {
            return;
        };
        let t = Transform::from_translate(-x0 as f32, -y0 as f32);
        for s in &self.shapes {
            draw_shape(&mut pm, s, self.scale, m, t);
        }
        unsafe { present(self.hwnd, &pm, x0, y0) };
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Uploads a premultiplied RGBA pixmap to a layered window at (x, y).
unsafe fn present(hwnd: HWND, pm: &Pixmap, x: i32, y: i32) {
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
                SourceConstantAlpha: 0,
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
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let _ = SetTimer(Some(hwnd), FADE_TIMER, 15, None);
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
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
    }
}

// ---------------------------------------------------------------- region picker

#[derive(Default)]
struct RegionState {
    origin: Rect,
    start: Option<(i32, i32)>,
    cur: (i32, i32),
    done: bool,
    result: Option<Rect>,
}

thread_local! {
    static REGION: std::cell::RefCell<RegionState> = std::cell::RefCell::new(RegionState::default());
}

const KEY: COLORREF = COLORREF(0x00FF00FF); // magenta = see-through hole

fn sel_rect(s: &RegionState) -> Option<RECT> {
    let (sx, sy) = s.start?;
    Some(RECT {
        left: sx.min(s.cur.0),
        top: sy.min(s.cur.1),
        right: sx.max(s.cur.0),
        bottom: sy.max(s.cur.1),
    })
}

unsafe extern "system" fn region_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let pt = || {
        (
            (lp.0 & 0xFFFF) as i16 as i32,
            ((lp.0 >> 16) & 0xFFFF) as i16 as i32,
        )
    };
    unsafe {
        match msg {
            WM_LBUTTONDOWN => {
                REGION.with_borrow_mut(|s| {
                    s.start = Some(pt());
                    s.cur = pt();
                });
                SetCapture(hwnd);
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let dragging = REGION.with_borrow_mut(|s| {
                    s.cur = pt();
                    s.start.is_some()
                });
                if dragging {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                REGION.with_borrow_mut(|s| {
                    s.cur = pt();
                    if let Some(r) = sel_rect(s) {
                        if r.right - r.left >= 8 && r.bottom - r.top >= 8 {
                            s.result = Some(Rect {
                                x: s.origin.x + r.left,
                                y: s.origin.y + r.top,
                                w: r.right - r.left,
                                h: r.bottom - r.top,
                            });
                        }
                    }
                    s.done = true;
                });
                LRESULT(0)
            }
            WM_RBUTTONUP => {
                REGION.with_borrow_mut(|s| s.done = true);
                LRESULT(0)
            }
            WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
                REGION.with_borrow_mut(|s| s.done = true);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let hdc = BeginPaint(hwnd, &mut ps);
                let mut client = RECT::default();
                let _ = GetClientRect(hwnd, &mut client);
                let dim = CreateSolidBrush(COLORREF(0x00101010));
                let hole = CreateSolidBrush(KEY);
                let edge = CreateSolidBrush(COLORREF(0x00FFFFFF));
                let sel = REGION.with_borrow(sel_rect);
                // Paint around the selection to avoid flicker from overdraw.
                match sel {
                    Some(r) => {
                        let parts = [
                            RECT {
                                left: 0,
                                top: 0,
                                right: client.right,
                                bottom: r.top,
                            },
                            RECT {
                                left: 0,
                                top: r.bottom,
                                right: client.right,
                                bottom: client.bottom,
                            },
                            RECT {
                                left: 0,
                                top: r.top,
                                right: r.left,
                                bottom: r.bottom,
                            },
                            RECT {
                                left: r.right,
                                top: r.top,
                                right: client.right,
                                bottom: r.bottom,
                            },
                        ];
                        for p in parts {
                            FillRect(hdc, &p, dim);
                        }
                        FillRect(hdc, &r, hole);
                        let frame = RECT {
                            left: r.left - 2,
                            top: r.top - 2,
                            right: r.right + 2,
                            bottom: r.bottom + 2,
                        };
                        FrameRect(hdc, &frame, edge);
                    }
                    None => {
                        FillRect(hdc, &client, dim);
                    }
                }
                let _ = DeleteObject(dim.into());
                let _ = DeleteObject(hole.into());
                let _ = DeleteObject(edge.into());
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

/// Runs a modal drag-to-select over `monitor`. Returns the selection in
/// virtual-screen pixels, or None if cancelled (Esc / right-click).
pub fn pick_region(monitor: Rect) -> Option<Rect> {
    register_class(REGION_CLASS, Some(region_proc), IDC_CROSS);
    REGION.with_borrow_mut(|s| {
        *s = RegionState {
            origin: monitor,
            ..Default::default()
        }
    });
    let cls = wide(REGION_CLASS);
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            monitor.x,
            monitor.y,
            monitor.w,
            monitor.h,
            None,
            None,
            Some(hinstance()),
            None,
        )
        .ok()?;
        let _ = SetLayeredWindowAttributes(hwnd, KEY, 110, LWA_ALPHA | LWA_COLORKEY);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(hwnd));
        let mut msg = MSG::default();
        while !REGION.with_borrow(|s| s.done) {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = DestroyWindow(hwnd);
    }
    REGION.with_borrow_mut(|s| s.result.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::MonitorInfo;
    use serde_json::json;

    fn cap() -> Capture {
        let r = Rect {
            x: 1920,
            y: 0,
            w: 2560,
            h: 1440,
        };
        Capture {
            jpeg: vec![],
            img_w: 1280,
            img_h: 720,
            src: r,
            monitor: MonitorInfo {
                rect: r,
                work: r,
                scale: 1.0,
            },
            cursor: (0, 0),
        }
    }

    #[test]
    fn maps_to_screen() {
        let c = parse_draw(&json!({"shapes":[{"kind":"box","x":10,"y":20,"w":100,"h":50,"label":"here","color":"blue"}]}), &cap(), ACCENT).unwrap();
        let s = &c.shapes[0];
        assert_eq!((s.x, s.y, s.w, s.h), (1940.0, 40.0, 200.0, 100.0));
        assert_eq!(s.color, [10, 132, 255]);
    }

    #[test]
    fn rejects_bad_shapes() {
        assert!(
            parse_draw(
                &json!({"shapes":[{"kind":"arrow","x":1,"y":1}]}),
                &cap(),
                ACCENT
            )
            .is_err()
        );
        assert!(
            parse_draw(
                &json!({"shapes":[{"kind":"blob","x":1,"y":1}]}),
                &cap(),
                ACCENT
            )
            .is_err()
        );
    }

    #[test]
    fn rasterizes() {
        let c = parse_draw(
            &json!({"shapes":[{"kind":"arrow","x":10,"y":10,"x2":200,"y2":120,"label":"Click"}]}),
            &cap(),
            ACCENT,
        )
        .unwrap();
        let mut pm = Pixmap::new(800, 400).unwrap();
        for s in &c.shapes {
            draw_shape(
                &mut pm,
                s,
                1.0,
                cap().monitor.rect,
                Transform::from_translate(-1920.0, 0.0),
            );
        }
        assert!(pm.data().chunks_exact(4).any(|p| p[3] > 0));
    }

    #[test]
    fn math_labels_render() {
        let m = Rect {
            x: 0,
            y: 0,
            w: 900,
            h: 300,
        };
        let mk = |kind, x, y, label: &str| Shape {
            kind,
            x,
            y,
            w: 200.0,
            h: 60.0,
            x2: 0.0,
            y2: 0.0,
            r: 0.0,
            label: Some(label.into()),
            color: ACCENT,
        };
        let shapes = [
            mk(Kind::Box, 40.0, 120.0, r"$f'(x) = \frac{3e^{3x}}{3x}$"),
            mk(
                Kind::Text,
                420.0,
                40.0,
                r"so $x = \frac{-b \pm \sqrt{b^2-4ac}}{2a}$ here",
            ),
            mk(Kind::Text, 420.0, 200.0, "plain label"),
        ];
        let mut pm = Pixmap::new(900, 300).unwrap();
        pm.fill(Color::from_rgba8(40, 40, 44, 255));
        for sh in &shapes {
            let l = label_box(sh, 1.5, m).unwrap();
            assert!(l.line.math || sh.label.as_deref() == Some("plain label"));
            draw_shape(&mut pm, sh, 1.5, m, Transform::identity());
        }
        if let Ok(out) = std::env::var("SCREENBUDDY_MATH_PNG") {
            pm.save_png(out).unwrap();
        }
    }
}
