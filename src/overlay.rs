//! Click-through annotation overlay (layered window + tiny-skia raster) and
//! the drag-to-select region picker.

use crate::capture::{Capture, Rect};
use crate::util::wide;
use serde_json::Value;
use std::sync::{Arc, OnceLock};
use std::time::Instant;
use tiny_skia::{
    Color, FillRule, LineCap, LineJoin, Paint, PathBuilder, Pixmap, Stroke, StrokeDash, Transform,
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
    /// A rendered Mermaid diagram on a card; x,y is its top-left, w,h the card.
    Diagram,
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
    pub diagram: Option<Arc<crate::diagram::Img>>,
    /// Not drawn yet: a droplet is still on its way here.
    pub hidden: bool,
    /// The Apeiron window this was drawn on (None without Apeiron).
    pub anchor: Option<Anchor>,
    /// Not drawn while Apeiron's camera moves (see `Overlay::follow`).
    pub away: bool,
}

/// The window a shape points at, so the shape can follow Apeiron's camera.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Anchor {
    pub hwnd: isize,
    /// The window's screen rect that the shape's coordinates belong to.
    pub at: Rect,
}

/// The point that decides which window a shape belongs to.
fn anchor_point(s: &Shape) -> (f32, f32) {
    match s.kind {
        Kind::Arrow => (s.x2, s.y2),
        Kind::Box | Kind::Highlight => (s.x + s.w / 2.0, s.y + s.h / 2.0),
        _ => (s.x, s.y),
    }
}

/// Moves a shape drawn on a window at `from` to the same spot on it at `to`
/// (a pan translates, a zoom also scales). Diagram cards only move.
pub fn reproject(s: &mut Shape, from: Rect, to: Rect) {
    let sx = to.w as f32 / from.w.max(1) as f32;
    let sy = to.h as f32 / from.h.max(1) as f32;
    let map = |x: f32, y: f32| (to.x as f32 + (x - from.x as f32) * sx, to.y as f32 + (y - from.y as f32) * sy);
    (s.x, s.y) = map(s.x, s.y);
    (s.x2, s.y2) = map(s.x2, s.y2);
    if s.kind != Kind::Diagram {
        s.w *= sx;
        s.h *= sy;
        s.r *= (sx + sy) / 2.0;
    }
}

/// Inner padding of a diagram card, in points.
const CARD_PAD: f32 = 14.0;
const CARD_BG: [u8; 3] = [30, 30, 32];

/// Card colors for diagrams drawn on screen; `accent` is the shape color.
pub fn card_theme(accent: [u8; 3]) -> crate::diagram::Theme {
    crate::diagram::Theme {
        bg: egui::Color32::from_rgb(CARD_BG[0], CARD_BG[1], CARD_BG[2]),
        fg: egui::Color32::from_rgb(236, 236, 240),
        muted: egui::Color32::from_rgb(152, 152, 160),
        accent: egui::Color32::from_rgb(accent[0], accent[1], accent[2]),
    }
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
pub fn parse_draw(input: &Value, cap: &Capture, default_color: [u8; 3]) -> Result<DrawCall, String> {
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
            "diagram" | "mermaid" => Kind::Diagram,
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
        let color = s
            .get("color")
            .and_then(Value::as_str)
            .and_then(parse_color)
            .unwrap_or(default_color);
        let (mut sx, mut sy) = (sx, sy);
        let mut w = num("w").unwrap_or(0.0).abs() * ls;
        let mut h = num("h").unwrap_or(0.0).abs() * ls;
        let mut diagram = None;
        if kind == Kind::Diagram {
            let Some(src) = s.get("mermaid").and_then(Value::as_str).filter(|m| !m.trim().is_empty()) else {
                return Err(format!("shape {i}: diagram needs `mermaid` source"));
            };
            let m = cap.monitor.rect;
            let scale = cap.monitor.scale.max(0.5);
            let pad = CARD_PAD * scale;
            let margin = 16.0 * scale;
            // Card box: `w` if given, else 60% of the monitor; never taller
            // than the monitor, so tall diagrams shrink (or turn sideways).
            let max_w = if w > 0.0 { w } else { m.w as f32 * 0.6 }.min(m.w as f32 - margin * 2.0);
            let max_h = m.h as f32 - margin * 2.0;
            let (bw, bh) = ((max_w - pad * 2.0) / scale, (max_h - pad * 2.0) / scale);
            let img = crate::diagram::render_blocking(src, card_theme(color), bw, bh, scale)
                .map_err(|e| format!("shape {i}: the Mermaid diagram didn't parse ({e}); fix the syntax and draw again"))?;
            w = img.size[0] as f32 + pad * 2.0;
            h = img.size[1] as f32 + pad * 2.0;
            // Keep the card on the monitor.
            sx = sx.min((m.x + m.w) as f32 - w - margin).max(m.x as f32);
            sy = sy.min((m.y + m.h) as f32 - h - margin).max(m.y as f32);
            diagram = Some(img);
        }
        let mut shape = Shape {
            kind,
            x: sx,
            y: sy,
            x2: ex,
            y2: ey,
            w,
            h,
            r: num("r").unwrap_or(24.0).abs().max(4.0) * ls,
            label: s
                .get("label")
                .and_then(Value::as_str)
                .filter(|l| !l.trim().is_empty())
                .map(str::to_owned),
            color,
            diagram,
            hidden: false,
            anchor: None,
            away: false,
        };
        if shape.kind != Kind::Diagram {
            let (ax, ay) = anchor_point(&shape);
            shape.anchor = crate::apeiron::window_at(&cap.windows, ax, ay).map(|p| Anchor { hwnd: p.hwnd, at: p.rect });
        }
        shapes.push(shape);
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
    if luma > 170.0 { [20, 20, 20] } else { [255, 255, 255] }
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

/// Composites a premultiplied-RGBA diagram onto the pixmap at (x, y).
fn blit_premul(pm: &mut Pixmap, img: &crate::diagram::Img, x: i32, y: i32) {
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let (w, h) = (img.size[0] as i32, img.size[1] as i32);
    let pixels = pm.pixels_mut();
    for row in 0..h {
        let ty = y + row;
        if ty < 0 || ty >= ph {
            continue;
        }
        for col in 0..w {
            let tx = x + col;
            if tx < 0 || tx >= pw {
                continue;
            }
            let i = ((row * w + col) * 4) as usize;
            let a = img.rgba[i + 3] as u32;
            if a == 0 {
                continue;
            }
            let p = &mut pixels[(ty * pw + tx) as usize];
            let inv = 255 - a;
            let over = |src: u8, d: u8| (src as u32 + d as u32 * inv / 255).min(255) as u8;
            *p = tiny_skia::PremultipliedColorU8::from_rgba(
                over(img.rgba[i], p.red()),
                over(img.rgba[i + 1], p.green()),
                over(img.rgba[i + 2], p.blue()),
                over(img.rgba[i + 3], p.alpha()),
            )
            .unwrap_or(*p);
        }
    }
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
        Kind::Box | Kind::Highlight | Kind::Diagram => (s.x, s.y - h - gap),
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
    let y = if y < my0 && matches!(s.kind, Kind::Box | Kind::Highlight | Kind::Diagram) {
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
        Kind::Box | Kind::Highlight | Kind::Diagram => (s.x, s.y, s.x + s.w, s.y + s.h),
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

/// Where a droplet lands on a shape.
fn landing(s: &Shape) -> (f32, f32) {
    match s.kind {
        Kind::Arrow => (s.x2, s.y2),
        Kind::Circle => {
            let k = std::f32::consts::FRAC_1_SQRT_2 * s.r;
            (s.x - k, s.y - k)
        }
        _ => (s.x, s.y),
    }
}

/// The shape a ring draws around, if it has one.
fn ring_path(s: &Shape, scale: f32) -> Option<tiny_skia::Path> {
    match s.kind {
        Kind::Box | Kind::Highlight => {
            let r = tiny_skia::Rect::from_xywh(s.x, s.y, s.w.max(1.0), s.h.max(1.0))?;
            Some(rounded_rect(r, 8.0 * scale))
        }
        Kind::Circle => PathBuilder::from_circle(s.x, s.y, s.r),
        _ => None,
    }
}

/// Small filled dot where a droplet came to rest.
fn draw_pin(pm: &mut Pixmap, s: &Shape, scale: f32, grow: f32, alpha: f32, t: Transform) {
    let (x, y) = landing(s);
    let r = 6.0 * scale * grow;
    if let Some(p) = PathBuilder::from_circle(x, y, r) {
        pm.stroke_path(&p, &paint(Color::from_rgba8(0, 0, 0, (90.0 * alpha) as u8)), &stroke(3.0), t, None);
        pm.fill_path(&p, &paint(rgba(s.color, alpha)), FillRule::Winding, t, None);
    }
}

fn draw_shape(pm: &mut Pixmap, s: &Shape, scale: f32, m: Rect, t: Transform) {
    let w = 3.0 * scale;
    if ring_path(s, scale).is_some() {
        draw_pin(pm, s, scale, 1.0, 1.0, t);
    }
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
        Kind::Diagram => {
            let Some(r) = tiny_skia::Rect::from_xywh(s.x, s.y, s.w.max(1.0), s.h.max(1.0)) else {
                return;
            };
            let p = rounded_rect(r, 12.0 * scale);
            pm.fill_path(&p, &paint(rgba(CARD_BG, 0.97)), FillRule::Winding, t, None);
            pm.stroke_path(&p, &paint(Color::from_rgba8(255, 255, 255, 30)), &stroke(scale), t, None);
            if let Some(img) = &s.diagram {
                let pad = CARD_PAD * scale;
                blit_premul(pm, img, (s.x + pad + t.tx).round() as i32, (s.y + pad + t.ty).round() as i32);
            }
        }
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
    flights: Vec<Flight>,
    /// The layer is on screen, so changes to it shouldn't fade it in again.
    shown: bool,
    /// Apeiron layout generation last followed.
    seen: u64,
    waker: Option<u64>,
}

/// How long a droplet takes to reach a target, by distance in pixels.
fn flight_time(dist: f32, scale: f32) -> f32 {
    (dist / scale.max(0.5) / 1400.0).clamp(0.22, 0.42)
}

const RING_TIME: f32 = 0.24;
const PIN_TIME: f32 = 0.16;
/// The ring starts just before the droplet lands, so the two read as one move.
const RING_LEAD: f32 = 0.04;
/// How long the bud takes to leave the palette before its droplet takes over.
pub const BUD_TIME: f32 = 0.09;
const MAX_FLIGHTS: usize = 6;

/// A droplet travelling to one shape, then the ring it turns into. Both are tiny
/// layered windows, so the frames cost a small bitmap instead of the whole layer.
struct Flight {
    shape: usize,
    /// Set by `launch`, once the palette has settled where it's going.
    from: Option<(f32, f32)>,
    start: Instant,
    delay: f32,
    time: f32,
    drop: Option<HWND>,
    ring: Option<HWND>,
}

impl Drop for Flight {
    fn drop(&mut self) {
        for hwnd in [self.drop.take(), self.ring.take()].into_iter().flatten() {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }
}

/// A click-through window for one layer, kept out of our own captures.
fn layer_window(at: Rect) -> Option<HWND> {
    let cls = wide(OVERLAY_CLASS);
    unsafe {
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            at.x,
            at.y,
            1,
            1,
            None,
            None,
            Some(hinstance()),
            None,
        )
        .ok()?;
        if crate::win::exclude_from_capture() {
            let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        }
        Some(hwnd)
    }
}

fn draw_droplet(hwnd: HWND, cx: f32, cy: f32, radius: f32) {
    let side = (radius * 2.0).ceil() as u32 + 6;
    let Some(mut pm) = Pixmap::new(side, side) else { return };
    let c = side as f32 / 2.0;
    if let Some(p) = PathBuilder::from_circle(c, c, radius) {
        let s = crate::surface::SURFACE;
        pm.fill_path(&p, &paint(Color::from_rgba8(s.r(), s.g(), s.b(), 255)), FillRule::Winding, Transform::identity(), None);
        pm.stroke_path(&p, &paint(Color::from_rgba8(255, 255, 255, 60)), &stroke(1.0), Transform::identity(), None);
    }
    unsafe { present(hwnd, &pm, (cx - c).round() as i32, (cy - c).round() as i32, false) };
}

fn draw_ring(hwnd: HWND, s: &Shape, scale: f32, age: f32) {
    let Some(path) = ring_path(s, scale) else { return };
    let drawn = crate::surface::bezier(crate::surface::EASE_OUT, age / RING_TIME);
    let pad = 16.0 * scale;
    let b = path.bounds();
    let (x0, y0) = ((b.left() - pad).floor(), (b.top() - pad).floor());
    let (w, h) = ((b.width() + pad * 2.0).ceil() as u32, (b.height() + pad * 2.0).ceil() as u32);
    let Some(mut pm) = Pixmap::new(w.max(1), h.max(1)) else { return };
    let t = Transform::from_translate(-x0, -y0);
    // Generous on purpose: the dash just has to outrun the true perimeter.
    let around = (b.width() + b.height()) * 2.0;
    let dash = StrokeDash::new(vec![(around * drawn).max(0.1), around + 1.0], 0.0);
    let line = 3.0 * scale;
    for (color, width) in [(Color::from_rgba8(0, 0, 0, 90), line + 3.0), (rgba(s.color, 1.0), line)] {
        let mut st = stroke(width);
        st.dash = dash.clone();
        pm.stroke_path(&path, &paint(color), &st, t, None);
    }
    let pin = crate::surface::bezier(crate::surface::EASE_OUT, age / PIN_TIME);
    draw_pin(&mut pm, s, scale, 0.8 + 0.2 * pin, pin, t);
    unsafe { present(hwnd, &pm, x0 as i32, y0 as i32, false) };
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
            flights: Vec::new(),
            shown: false,
            seen: 0,
            // Without Apeiron nothing is anchored, so nothing needs waking.
            waker: crate::apeiron::connected().then(|| crate::apeiron::add_waker(crate::win::request_repaint)),
        })
    }

    pub fn is_empty(&self) -> bool {
        self.shapes.is_empty()
    }

    /// Adds shapes. With `animate`, the first few wait for a droplet (see `launch`).
    pub fn apply(&mut self, call: DrawCall, animate: bool) {
        if call.clear {
            self.flights.clear();
            self.shapes.clear();
        }
        let mut shapes = call.shapes;
        bring_into_view(&mut shapes);
        for mut shape in shapes {
            let flies = matches!(shape.kind, Kind::Arrow | Kind::Box | Kind::Highlight | Kind::Circle);
            if animate && flies && self.flights.len() < MAX_FLIGHTS {
                shape.hidden = true;
                self.flights.push(Flight {
                    shape: self.shapes.len(),
                    from: None,
                    start: Instant::now(),
                    delay: 0.0,
                    time: 0.0,
                    drop: None,
                    ring: None,
                });
            }
            self.shapes.push(shape);
        }
        self.render(true);
        self.seen = 0;
        self.follow();
    }

    /// Keeps anchored shapes on their windows as Apeiron's camera moves: hidden
    /// while it moves (or the overview is open), re-projected from the
    /// window's new screen rect once it settles, dropped when the window left
    /// the screen. A no-op without Apeiron or anchors.
    fn follow(&mut self) {
        if !self.shapes.iter().any(|s| s.anchor.is_some()) {
            return;
        }
        let Some(l) = crate::apeiron::get() else {
            // Apeiron went away: show what we have where it was.
            if self.shapes.iter().any(|s| s.away) {
                self.shapes.iter_mut().for_each(|s| s.away = false);
                self.render(false);
            }
            return;
        };
        if l.generation == self.seen {
            return;
        }
        self.seen = l.generation;
        let (next, changed) = follow_shapes(std::mem::take(&mut self.shapes), &l, self.monitor);
        self.shapes = next;
        if changed {
            // Flights index into `shapes` and would land in stale spots.
            self.flights.clear();
            self.shapes.iter_mut().for_each(|s| s.hidden = false);
            self.render(false);
        }
    }

    /// Sends waiting droplets from the palette, whose surface is `(x0, y0, x1,
    /// y1)` on screen. Returns where each leaves that surface and how long after
    /// now, so the palette can draw the bud that it breaks away from.
    pub fn launch(&mut self, surface: (f32, f32, f32, f32)) -> Vec<((f32, f32), (f32, f32), f32)> {
        let (x0, y0, x1, y1) = surface;
        let now = Instant::now();
        let mut buds = Vec::new();
        let mut order = 0;
        for f in self.flights.iter_mut().filter(|f| f.from.is_none()) {
            let to = landing(&self.shapes[f.shape]);
            let edge = (to.0.clamp(x0, x1), to.1.clamp(y0, y1));
            let wait = crate::surface::stagger(order);
            order += 1;
            f.from = Some(edge);
            f.start = now;
            f.delay = wait + BUD_TIME;
            f.time = flight_time((to.0 - edge.0).hypot(to.1 - edge.1), self.scale);
            buds.push((edge, to, wait));
        }
        buds
    }

    /// Advances the flights one frame. True while any are still going.
    pub fn tick(&mut self) -> bool {
        self.follow();
        if self.flights.is_empty() {
            return false;
        }
        let now = Instant::now();
        let scale = self.scale;
        let mut landed = false;
        let shapes = &mut self.shapes;
        let monitor = self.monitor;
        self.flights.retain_mut(|f| {
            let Some(from) = f.from else { return true };
            let age = now.duration_since(f.start).as_secs_f32() - f.delay;
            if age < 0.0 {
                return true;
            }
            let shape = &shapes[f.shape];
            let has_ring = ring_path(shape, scale).is_some();
            if age < f.time {
                let hwnd = f.drop.get_or_insert_with(|| {
                    layer_window(monitor).unwrap_or_default()
                });
                let along = crate::surface::bezier(crate::surface::EASE_IN_OUT, age / f.time);
                let to = landing(shape);
                let grow = (age / (f.time * 0.25)).min(1.0);
                let shrink = ((f.time - age) / (f.time * 0.2)).min(1.0);
                let size = 11.0 * scale * (0.75 + 0.25 * grow) * (0.55 + 0.45 * shrink);
                draw_droplet(*hwnd, from.0 + (to.0 - from.0) * along, from.1 + (to.1 - from.1) * along, size);
            } else if let Some(hwnd) = f.drop.take() {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
            if !has_ring {
                if age >= f.time {
                    shapes[f.shape].hidden = false;
                    landed = true;
                    return false;
                }
                return true;
            }
            let ring_age = age - (f.time - RING_LEAD);
            if ring_age >= RING_TIME {
                shapes[f.shape].hidden = false;
                landed = true;
                return false;
            }
            if ring_age >= 0.0 {
                let hwnd = f.ring.get_or_insert_with(|| {
                    layer_window(monitor).unwrap_or_default()
                });
                draw_ring(*hwnd, shape, scale, ring_age);
            }
            true
        });
        if landed {
            // The finished shape lands on the same pixels the ring just drew.
            self.render(false);
        }
        !self.flights.is_empty()
    }

    /// Screen-space bounding box of everything drawn.
    pub fn drawn_bounds(&self) -> Option<Rect> {
        if self.shapes.is_empty() {
            return None;
        }
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for s in self.shapes.iter().filter(|s| !s.away) {
            let b = bounds(s, self.scale, self.monitor);
            x0 = x0.min(b.0);
            y0 = y0.min(b.1);
            x1 = x1.max(b.2);
            y1 = y1.max(b.3);
        }
        if x0 > x1 {
            return None;
        }
        Some(Rect {
            x: x0 as i32,
            y: y0 as i32,
            w: (x1 - x0) as i32,
            h: (y1 - y0) as i32,
        })
    }

    fn render(&mut self, fade: bool) {
        unsafe {
            if self.shapes.iter().all(|s| s.hidden || s.away) {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
                self.shown = false;
                return;
            }
        }
        let m = self.monitor;
        let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for s in self.shapes.iter().filter(|s| !s.hidden && !s.away) {
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
        for s in self.shapes.iter().filter(|s| !s.hidden && !s.away) {
            draw_shape(&mut pm, s, self.scale, m, t);
        }
        unsafe { present(self.hwnd, &pm, x0, y0, fade && !self.shown) };
        self.shown = true;
    }
}

/// A draw aimed at a window that has since left the screen (panned away,
/// hidden tab) flies Apeiron there first; then shapes drawn on windows that
/// moved since the screenshot are moved along with them.
fn bring_into_view(shapes: &mut [Shape]) {
    if !shapes.iter().any(|s| s.anchor.is_some()) {
        return;
    }
    let Some(mut live) = crate::apeiron::get() else { return };
    let gone = shapes
        .iter()
        .filter_map(|s| s.anchor)
        .find(|a| live.window(a.hwnd).is_some_and(|w| !w.visible()));
    if let Some(a) = gone
        && crate::apeiron::focus_window(a.hwnd).is_ok()
        && let Some(l) = crate::apeiron::refresh()
    {
        live = l;
    }
    for s in shapes.iter_mut() {
        let Some(a) = s.anchor else { continue };
        if let Some(r) = live.window(a.hwnd).filter(|w| w.visible()).and_then(|w| w.screen_rect)
            && r != a.at
        {
            reproject(s, a.at, r);
            s.anchor = Some(Anchor { at: r, ..a });
        }
    }
}

/// One follow step over `shapes` for layout `l` (see `Overlay::follow`).
/// Returns the shapes to keep and whether anything visibly changed.
fn follow_shapes(shapes: Vec<Shape>, l: &crate::apeiron::Layout, monitor: Rect) -> (Vec<Shape>, bool) {
    let hold = l.unsettled();
    let mut changed = false;
    let mut keep = Vec::with_capacity(shapes.len());
    for mut s in shapes {
        let Some(a) = s.anchor else {
            keep.push(s);
            continue;
        };
        if hold {
            changed |= !s.away;
            s.away = true;
            keep.push(s);
            continue;
        }
        let Some(r) = l.window(a.hwnd).filter(|w| w.visible()).and_then(|w| w.screen_rect) else {
            changed = true; // the window closed or left the screen
            continue;
        };
        if r != a.at {
            reproject(&mut s, a.at, r);
            s.anchor = Some(Anchor { at: r, ..a });
            changed = true;
        }
        let (x, y) = anchor_point(&s);
        let m = monitor;
        if x < m.x as f32 || y < m.y as f32 || x >= (m.x + m.w) as f32 || y >= (m.y + m.h) as f32 {
            changed = true; // on a window, but not on this overlay's monitor anymore
            continue;
        }
        changed |= s.away;
        s.away = false;
        keep.push(s);
    }
    (keep, changed)
}

impl Drop for Overlay {
    fn drop(&mut self) {
        if let Some(id) = self.waker.take() {
            crate::apeiron::remove_waker(id);
        }
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Uploads a premultiplied RGBA pixmap to a layered window at (x, y), fading it
/// in if `fade`, otherwise showing it at full strength straight away.
unsafe fn present(hwnd: HWND, pm: &Pixmap, x: i32, y: i32, fade: bool) {
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
                SourceConstantAlpha: if fade { 0 } else { 255 },
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
            if fade {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let _ = SetTimer(Some(hwnd), FADE_TIMER, 15, None);
            } else {
                let _ = KillTimer(Some(hwnd), FADE_TIMER);
            }
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
            windows: Vec::new(),
        }
    }

    fn win(hwnd: isize, rect: Rect, visible: bool) -> crate::apeiron::Window {
        crate::apeiron::Window {
            hwnd,
            title: String::new(),
            exe: String::new(),
            canvas_rect: [0.0; 4],
            screen_rect: visible.then_some(rect),
            on_screen: visible,
            tab_hidden: false,
            away: false,
        }
    }

    fn rect(x: i32, y: i32, w: i32, h: i32) -> Rect {
        Rect { x, y, w, h }
    }

    #[test]
    fn anchors_to_topmost_window_under_shape() {
        let mut c = cap();
        c.windows = vec![
            crate::apeiron::Placed { hwnd: 7, rect: rect(2000, 100, 800, 600) },
            crate::apeiron::Placed { hwnd: 8, rect: rect(1920, 0, 2560, 1440) },
        ];
        // Screenshot (100,100) = screen (2120, 200): on window 7.
        let call = parse_draw(&json!({"shapes":[{"kind":"circle","x":100,"y":100},{"kind":"box","x":1000,"y":600,"w":40,"h":40}]}), &c, ACCENT).unwrap();
        assert_eq!(call.shapes[0].anchor.unwrap().hwnd, 7);
        assert_eq!(call.shapes[1].anchor.unwrap().hwnd, 8);
        // Without Apeiron nothing is anchored.
        let call = parse_draw(&json!({"shapes":[{"kind":"circle","x":100,"y":100}]}), &cap(), ACCENT).unwrap();
        assert!(call.shapes[0].anchor.is_none());
    }

    #[test]
    fn reprojects_pan_and_zoom() {
        let c = cap();
        let mut s = parse_draw(&json!({"shapes":[{"kind":"box","x":100,"y":50,"w":20,"h":10}]}), &c, ACCENT).unwrap().shapes.remove(0);
        // Screen box (2120,100) 40x20 on a window at (2000,0) 800x600.
        let from = rect(2000, 0, 800, 600);
        // Panned 300 left at the same zoom.
        let mut p = s.clone();
        reproject(&mut p, from, rect(1700, 0, 800, 600));
        assert_eq!((p.x, p.y, p.w, p.h), (1820.0, 100.0, 40.0, 20.0));
        // Zoomed to half size around a new origin.
        reproject(&mut s, from, rect(1000, 200, 400, 300));
        assert_eq!((s.x, s.y, s.w, s.h), (1060.0, 250.0, 20.0, 10.0));
        // And back: the round trip lands where it started.
        reproject(&mut s, rect(1000, 200, 400, 300), from);
        assert_eq!((s.x, s.y, s.w, s.h), (2120.0, 100.0, 40.0, 20.0));
    }

    #[test]
    fn follows_camera() {
        use crate::apeiron::Layout;
        let m = rect(1920, 0, 2560, 1440);
        let at = rect(2000, 0, 800, 600);
        let mut a = parse_draw(&json!({"shapes":[{"kind":"circle","x":100,"y":100}]}), &cap(), ACCENT).unwrap().shapes.remove(0);
        a.anchor = Some(Anchor { hwnd: 1, at });
        let mut b = a.clone();
        b.anchor = Some(Anchor { hwnd: 2, at });
        let free = parse_draw(&json!({"shapes":[{"kind":"text","x":10,"y":10,"label":"hi"}]}), &cap(), ACCENT).unwrap().shapes.remove(0);
        let shapes = vec![a, b, free];
        // Camera moving: anchored shapes hide, the free one stays.
        let mut l = Layout { moving: true, windows: vec![win(1, at, true), win(2, at, true)], ..Default::default() };
        let (v, changed) = follow_shapes(shapes, &l, m);
        assert!(changed);
        assert_eq!(v.iter().map(|s| s.away).collect::<Vec<_>>(), [true, true, false]);
        // Settled: window 1 moved right by 500, window 2 panned off-screen.
        l.moving = false;
        l.windows = vec![win(1, rect(2500, 0, 800, 600), true), win(2, at, false)];
        let (v, changed) = follow_shapes(v, &l, m);
        assert!(changed);
        assert_eq!(v.len(), 2);
        assert!(!v[0].away);
        assert_eq!(v[0].x, 2120.0 + 500.0);
        assert_eq!(v[0].anchor.unwrap().at.x, 2500);
        // Nothing new: nothing changes.
        let (v, changed) = follow_shapes(v, &l, m);
        assert!(!changed && v.len() == 2);
        // Window moved onto another monitor: dropped from this overlay.
        l.windows = vec![win(1, rect(-1500, 0, 800, 600), true)];
        let (v, _) = follow_shapes(v, &l, m);
        assert_eq!(v.len(), 1);
        assert!(v[0].anchor.is_none());
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
        assert!(parse_draw(&json!({"shapes":[{"kind":"arrow","x":1,"y":1}]}), &cap(), ACCENT).is_err());
        assert!(parse_draw(&json!({"shapes":[{"kind":"blob","x":1,"y":1}]}), &cap(), ACCENT).is_err());
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
        let m = Rect { x: 0, y: 0, w: 900, h: 300 };
        let mk = |kind, x, y, label: &str| Shape {
            hidden: false,
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
            diagram: None,
            anchor: None,
            away: false,
        };
        let shapes = [
            mk(Kind::Box, 40.0, 120.0, r"$f'(x) = \frac{3e^{3x}}{3x}$"),
            mk(Kind::Text, 420.0, 40.0, r"so $x = \frac{-b \pm \sqrt{b^2-4ac}}{2a}$ here"),
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

    #[test]
    fn diagram_cards_render_and_report_errors() {
        let c = cap();
        let src = "flowchart LR\n A[Hotkey] --> B{Open?}\n B -->|No| C[Capture]";
        // Placed off the right edge: the card is pulled back onto the monitor.
        let call = parse_draw(&json!({"shapes":[{"kind":"diagram","mermaid":src,"x":1270,"y":10}]}), &c, ACCENT).unwrap();
        let s = &call.shapes[0];
        assert_eq!(s.kind, Kind::Diagram);
        assert!(s.diagram.is_some() && s.w > 100.0 && s.h > 40.0);
        assert!(s.x + s.w <= (c.monitor.rect.x + c.monitor.rect.w) as f32 + 0.5, "{} {}", s.x, s.w);

        // A tall top-down chart (what a model drew for "git rebase") stays on screen.
        let tall = "flowchart TD\n A[git rebase main] --> B[Find merge base]\n B --> C[Save commits as patches]\n C --> D[Move HEAD to main]\n D --> E[Apply next commit]\n E --> F{Conflict?}\n F -->|No| G[New commit]\n F -->|Yes| H[Fix files, git add]\n H --> I{What next?}\n I -->|continue| G\n I -->|skip| J[Drop commit]\n I -->|abort| K[Back to start]\n G --> L{More commits?}\n J --> L\n L -->|Yes| E\n L -->|No| M[Point branch at last commit]\n M --> N[Done]";
        let call2 = parse_draw(&json!({"shapes":[{"kind":"diagram","mermaid":tall,"x":900,"y":40}]}), &c, ACCENT).unwrap();
        let t = &call2.shapes[0];
        let mr = c.monitor.rect;
        assert!(t.y >= mr.y as f32 && t.y + t.h <= (mr.y + mr.h) as f32, "{} {}", t.y, t.h);
        assert!(t.x >= mr.x as f32 && t.x + t.w <= (mr.x + mr.w) as f32, "{} {}", t.x, t.w);
        if let Ok(out) = std::env::var("SCREENBUDDY_DIAGRAM_PNG") {
            let mut pm = Pixmap::new(mr.w as u32, mr.h as u32).unwrap();
            pm.fill(Color::from_rgba8(235, 235, 240, 255));
            draw_shape(&mut pm, t, 1.0, mr, Transform::from_translate(-mr.x as f32, -mr.y as f32));
            pm.save_png(out.replace(".png", "-tall.png")).unwrap();
        }

        let err = parse_draw(&json!({"shapes":[{"kind":"diagram","mermaid":"flowchart LR\n A -->","x":0,"y":0}]}), &c, ACCENT)
            .err()
            .unwrap();
        assert!(err.contains("didn't parse"), "{err}");
        assert!(parse_draw(&json!({"shapes":[{"kind":"diagram","x":0,"y":0}]}), &c, ACCENT).is_err());

        let m = c.monitor.rect;
        let mut pm = Pixmap::new(m.w as u32, 400).unwrap();
        pm.fill(Color::from_rgba8(235, 235, 240, 255));
        let t = Transform::from_translate(-m.x as f32, 0.0);
        draw_shape(&mut pm, s, 1.0, m, t);
        assert!(pm.pixels().iter().any(|p| p.red() < 60));
        if let Ok(out) = std::env::var("SCREENBUDDY_DIAGRAM_PNG") {
            pm.save_png(out).unwrap();
        }
    }
}
