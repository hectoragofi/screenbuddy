//! Shared drawing kit for screenbuddy's always-on surfaces (island, captions,
//! cards): embedded fonts, text layout on fontdue, rounded shapes + soft
//! shadows on tiny-skia, and a layered-window helper that presents a pixmap
//! with `UpdateLayeredWindow`.
//!
//! Everything here works in **physical** pixels; multiply the logical sizes in
//! [`tokens`] by the monitor scale first. Text and shapes blend source-over
//! into premultiplied pixmaps, so draw back to front.
//!
//! ```ignore
//! let s = mon.scale;
//! let mut pm = Pixmap::new(w, h).unwrap();
//! ui::shadow(&mut pm, x, y, bw, bh, 16.0 * s, s);
//! ui::fill_rounded(&mut pm, x, y, bw, bh, 16.0 * s, tokens::INK);
//! ui::draw_text(&mut pm, "hello", x + 12.0 * s, y + 10.0 * s, &TextStyle::body(s));
//! layer.present(&pm, screen_x, screen_y, 1.0);
//! ```
#![allow(dead_code)] // shared API; captions, cards and agents use the rest

pub mod tokens;

pub use tokens::{Rgba, Spring};

use crate::util::wide;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::OnceLock;
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, PixmapPaint, Stroke, Transform};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

// ---------------------------------------------------------------- fonts

/// The embedded typefaces (all SIL OFL, licences in `assets/fonts/`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Face {
    /// Geist Mono Regular: body text.
    Mono,
    /// Geist Mono Medium: labels, emphasis.
    MonoMedium,
    /// Doto (static wght 700, round dots): dot-matrix readouts and numbers.
    Doto,
}

static GEIST: &[u8] = include_bytes!("../../assets/fonts/GeistMono-Regular.ttf");
static GEIST_MEDIUM: &[u8] = include_bytes!("../../assets/fonts/GeistMono-Medium.ttf");
static DOTO: &[u8] = include_bytes!("../../assets/fonts/Doto-Bold-Round.ttf");

pub(crate) fn font(face: Face) -> &'static fontdue::Font {
    static FONTS: OnceLock<[fontdue::Font; 3]> = OnceLock::new();
    let fonts = FONTS.get_or_init(|| {
        let load = |b: &'static [u8]| {
            fontdue::Font::from_bytes(b, fontdue::FontSettings::default()).expect("embedded font")
        };
        [load(GEIST), load(GEIST_MEDIUM), load(DOTO)]
    });
    &fonts[face as usize]
}

/// How a run of text looks. `px` is the physical font size.
#[derive(Clone, Copy, Debug)]
pub struct TextStyle {
    pub face: Face,
    pub px: f32,
    /// Extra letter spacing as a fraction of `px` (0.06 = +6 %).
    pub tracking: f32,
    pub color: Rgba,
    /// Upper-case the text before drawing (labels).
    pub caps: bool,
}

impl TextStyle {
    /// 10.5 px Geist Mono Medium, ALL CAPS, +6 % tracking, MUTE.
    pub fn label(scale: f32) -> Self {
        Self {
            face: Face::MonoMedium,
            px: tokens::LABEL_PX * scale,
            tracking: tokens::LABEL_TRACKING,
            color: tokens::MUTE,
            caps: true,
        }
    }

    /// 12.5 px Geist Mono, PAPER.
    pub fn body(scale: f32) -> Self {
        Self {
            face: Face::Mono,
            px: tokens::BODY_PX * scale,
            tracking: 0.0,
            color: tokens::PAPER,
            caps: false,
        }
    }

    /// Doto dot-matrix readout at `READOUT_PX`, PAPER.
    pub fn readout(scale: f32) -> Self {
        Self {
            face: Face::Doto,
            px: tokens::READOUT_PX * scale,
            tracking: 0.04,
            color: tokens::PAPER,
            caps: true,
        }
    }

    pub fn color(mut self, c: Rgba) -> Self {
        self.color = c;
        self
    }

    pub fn size(mut self, px: f32) -> Self {
        self.px = px;
        self
    }

    fn prepare<'a>(&self, text: &'a str) -> std::borrow::Cow<'a, str> {
        if self.caps {
            text.to_uppercase().into()
        } else {
            text.into()
        }
    }
}

type Glyph = Rc<(fontdue::Metrics, Vec<u8>)>;

thread_local! {
    static GLYPHS: RefCell<HashMap<(Face, char, u32), Glyph>> = RefCell::new(HashMap::new());
}

fn glyph(face: Face, c: char, px: f32) -> Glyph {
    GLYPHS.with(|g| {
        let mut g = g.borrow_mut();
        if g.len() > 4096 {
            g.clear();
        }
        g.entry((face, c, px.to_bits()))
            .or_insert_with(|| Rc::new(font(face).rasterize(c, px)))
            .clone()
    })
}

/// Distance from the top of the line box to the baseline.
pub fn ascent(style: &TextStyle) -> f32 {
    font(style.face)
        .horizontal_line_metrics(style.px)
        .map(|m| m.ascent)
        .unwrap_or(style.px * 0.8)
}

/// Height of one line box (ascent + descent, no extra leading).
pub fn line_height(style: &TextStyle) -> f32 {
    font(style.face)
        .horizontal_line_metrics(style.px)
        .map(|m| m.ascent - m.descent)
        .unwrap_or(style.px * 1.2)
}

/// Height of capital letters (for optically centring caps labels).
pub fn cap_height(style: &TextStyle) -> f32 {
    font(style.face).metrics('H', style.px).height as f32
}

fn advance(style: &TextStyle, c: char) -> f32 {
    font(style.face).metrics(c, style.px).advance_width + style.tracking * style.px
}

/// Width of `text` (trailing tracking excluded).
pub fn text_width(text: &str, style: &TextStyle) -> f32 {
    let t = style.prepare(text);
    let w: f32 = t.chars().map(|c| advance(style, c)).sum();
    if t.is_empty() {
        0.0
    } else {
        w - style.tracking * style.px
    }
}

/// Draws one line with its line box's top-left at (x, y); returns its width.
pub fn draw_text(pm: &mut Pixmap, text: &str, x: f32, y: f32, style: &TextStyle) -> f32 {
    let baseline = (y + ascent(style)).round();
    draw_text_baseline(pm, text, x, baseline, style)
}

/// Draws one line on the given baseline; returns its width.
pub fn draw_text_baseline(
    pm: &mut Pixmap,
    text: &str,
    x: f32,
    baseline: f32,
    style: &TextStyle,
) -> f32 {
    let t = style.prepare(text);
    let (pw, ph) = (pm.width() as i32, pm.height() as i32);
    let ca = style.color.a as u32;
    let [cr, cg, cb] = [
        style.color.r as u32,
        style.color.g as u32,
        style.color.b as u32,
    ];
    let pixels = pm.pixels_mut();
    let mut pen = x;
    for c in t.chars() {
        let g = glyph(style.face, c, style.px);
        let (m, bitmap) = (&g.0, &g.1);
        let gx = (pen + m.xmin as f32).round() as i32;
        let gy = (baseline - m.height as f32 - m.ymin as f32).round() as i32;
        for row in 0..m.height as i32 {
            let ty = gy + row;
            if ty < 0 || ty >= ph {
                continue;
            }
            for col in 0..m.width as i32 {
                let tx = gx + col;
                if tx < 0 || tx >= pw {
                    continue;
                }
                let cov = bitmap[(row * m.width as i32 + col) as usize] as u32;
                if cov == 0 {
                    continue;
                }
                // Source-over with premultiplied destination; sa in 0..=255.
                let sa = cov * ca / 255;
                let inv = 255 - sa;
                let p = &mut pixels[(ty * pw + tx) as usize];
                let mix = |s: u32, d: u8| ((s * sa + d as u32 * inv + 127) / 255).min(255) as u8;
                let a = ((sa * 255 + p.alpha() as u32 * inv + 127) / 255).min(255) as u8;
                let (r, g, b) = (mix(cr, p.red()), mix(cg, p.green()), mix(cb, p.blue()));
                *p = tiny_skia::PremultipliedColorU8::from_rgba(r.min(a), g.min(a), b.min(a), a)
                    .unwrap_or(*p);
            }
        }
        pen += advance(style, c);
    }
    pen - x
}

/// `text` cut to fit `max_w`, ending in "…" when shortened.
pub fn ellipsize(text: &str, style: &TextStyle, max_w: f32) -> String {
    if text_width(text, style) <= max_w {
        return text.to_string();
    }
    let ell = text_width("…", style);
    let mut out = String::new();
    let mut w = 0.0;
    for c in text.chars() {
        let a = advance(style, c);
        if w + a + ell > max_w {
            break;
        }
        w += a;
        out.push(c);
    }
    format!("{}…", out.trim_end())
}

/// Greedy word wrap into lines no wider than `max_w`; long words are split.
pub fn wrap(text: &str, style: &TextStyle, max_w: f32) -> Vec<String> {
    let mut lines = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let cand = if line.is_empty() {
                word.to_string()
            } else {
                format!("{line} {word}")
            };
            if text_width(&cand, style) <= max_w {
                line = cand;
                continue;
            }
            if !line.is_empty() {
                lines.push(std::mem::take(&mut line));
            }
            // Split a word that is wider than the whole line.
            for c in word.chars() {
                let mut next = line.clone();
                next.push(c);
                if !line.is_empty() && text_width(&next, style) > max_w {
                    lines.push(std::mem::take(&mut line));
                    next = c.to_string();
                }
                line = next;
            }
        }
        lines.push(line);
    }
    lines
}

// ---------------------------------------------------------------- shapes

fn paint(c: Rgba) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(c.skia());
    p.anti_alias = true;
    p
}

/// A rounded rectangle path; `r` is clamped to half the shorter side (pill).
pub fn rounded_rect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<tiny_skia::Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let (x1, y1) = (x + w, y + h);
    let k = 0.552_284_8 * r;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x1 - r, y);
    pb.cubic_to(x1 - r + k, y, x1, y + r - k, x1, y + r);
    pb.line_to(x1, y1 - r);
    pb.cubic_to(x1, y1 - r + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x + r, y1);
    pb.cubic_to(x + r - k, y1, x, y1 - r + k, x, y1 - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

pub fn fill_rounded(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, c: Rgba) {
    if let Some(p) = rounded_rect_path(x, y, w, h, r) {
        pm.fill_path(
            &p,
            &paint(c),
            FillRule::Winding,
            Transform::identity(),
            None,
        );
    }
}

/// A hairline drawn inside the shape's edge (so it never grows the shape).
pub fn stroke_rounded(
    pm: &mut Pixmap,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    r: f32,
    width: f32,
    c: Rgba,
) {
    let i = width / 2.0;
    if let Some(p) = rounded_rect_path(x + i, y + i, w - width, h - width, (r - i).max(0.0)) {
        let s = Stroke {
            width,
            ..Default::default()
        };
        pm.stroke_path(&p, &paint(c), &s, Transform::identity(), None);
    }
}

pub fn fill_circle(pm: &mut Pixmap, cx: f32, cy: f32, r: f32, c: Rgba) {
    if r <= 0.0 || c.a == 0 {
        return;
    }
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

pub fn fill_rect(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: Rgba) {
    if let Some(r) = tiny_skia::Rect::from_xywh(x, y, w, h) {
        pm.fill_rect(r, &paint(c), Transform::identity(), None);
    }
}

/// The token drop shadow (0 8 24 rgba(0,0,0,.35)) of a rounded rect, at
/// `scale`. Leave ~`SHADOW_BLUR` px of margin around the shape, more below.
pub fn shadow(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, scale: f32) {
    soft_shadow(
        pm,
        x,
        y + tokens::SHADOW_Y * scale,
        w,
        h,
        r,
        tokens::SHADOW_BLUR * scale,
        tokens::SHADOW_ALPHA,
    );
}

/// A gaussian-ish blurred black rounded rect. `blur` is the CSS blur radius
/// (sigma = blur / 2). Rendered at reduced resolution, so it is cheap enough
/// for every animation frame.
pub fn soft_shadow(pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, r: f32, blur: f32, alpha: f32) {
    let sigma = (blur / 2.0).max(0.5);
    let d = (sigma / 3.0).clamp(1.0, 6.0).floor();
    let (sw, sh) = (
        (pm.width() as f32 / d).ceil() as u32 + 2,
        (pm.height() as f32 / d).ceil() as u32 + 2,
    );
    let Some(mut small) = Pixmap::new(sw, sh) else {
        return;
    };
    fill_rounded(&mut small, x / d, y / d, w / d, h / d, r / d, Rgba::rgb(0));
    let mut a: Vec<u8> = small.pixels().iter().map(|p| p.alpha()).collect();
    blur_alpha(&mut a, sw as usize, sh as usize, sigma / d);
    for (p, v) in small.pixels_mut().iter_mut().zip(a) {
        *p = tiny_skia::PremultipliedColorU8::from_rgba(0, 0, 0, v).unwrap_or(*p);
    }
    let paint = PixmapPaint {
        opacity: alpha.clamp(0.0, 1.0),
        quality: tiny_skia::FilterQuality::Bilinear,
        ..Default::default()
    };
    pm.draw_pixmap(
        0,
        0,
        small.as_ref(),
        &paint,
        Transform::from_scale(d, d),
        None,
    );
}

/// Three box-blur passes per axis approximate a gaussian of `sigma`.
fn blur_alpha(a: &mut [u8], w: usize, h: usize, sigma: f32) {
    let box_w = (4.0 * sigma * sigma + 1.0).sqrt();
    let rad = ((box_w - 1.0) / 2.0).round().max(1.0) as usize;
    let mut tmp = vec![0u8; a.len()];
    for _ in 0..3 {
        box_pass(a, &mut tmp, w, h, rad, true);
        box_pass(&tmp, a, w, h, rad, false);
    }
}

fn box_pass(src: &[u8], dst: &mut [u8], w: usize, h: usize, rad: usize, horizontal: bool) {
    let (lines, len) = if horizontal { (h, w) } else { (w, h) };
    let at = |line: usize, i: usize| {
        if horizontal {
            line * w + i
        } else {
            i * w + line
        }
    };
    let div = (2 * rad + 1) as u32;
    for line in 0..lines {
        let mut sum: u32 = 0;
        // Edges clamp to zero (outside the pixmap is transparent).
        for i in 0..=rad.min(len - 1) {
            sum += src[at(line, i)] as u32;
        }
        for i in 0..len {
            dst[at(line, i)] = (sum / div) as u8;
            if i + rad + 1 < len {
                sum += src[at(line, i + rad + 1)] as u32;
            }
            if i >= rad {
                sum -= src[at(line, i - rad)] as u32;
            }
        }
    }
}

// ---------------------------------------------------------------- monitors

pub use crate::capture::MonitorInfo;

/// Geometry + DPI scale of the monitor under the cursor.
pub fn monitor_under_cursor() -> MonitorInfo {
    let (x, y) = crate::capture::cursor_pos();
    crate::capture::monitor_at(x, y)
}

// ---------------------------------------------------------------- layered windows

fn hinstance() -> HINSTANCE {
    unsafe {
        GetModuleHandleW(None)
            .map(|m| HINSTANCE(m.0))
            .unwrap_or_default()
    }
}

/// A top-most, non-activating, per-pixel-alpha window (`screenbuddy.<name>`),
/// excluded from screen capture unless `SCREENBUDDY_ALLOW_CAPTURE` is set.
/// Owned by the thread that created it; draw it with [`Layer::present`].
pub struct Layer {
    hwnd: HWND,
    shown: bool,
    dib: Option<Dib>,
}

struct Dib {
    dc: HDC,
    bmp: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    w: i32,
    h: i32,
}

impl Layer {
    /// Creates the (hidden) window. `proc_` handles its messages (None =
    /// `DefWindowProcW`); `click_through` adds `WS_EX_TRANSPARENT`.
    pub fn new(name: &str, proc_: WNDPROC, click_through: bool) -> anyhow::Result<Self> {
        let cls = wide(&format!("screenbuddy.{name}"));
        unsafe {
            let wc = WNDCLASSW {
                lpfnWndProc: proc_.or(Some(def_proc)),
                hInstance: hinstance(),
                lpszClassName: PCWSTR(cls.as_ptr()),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                ..Default::default()
            };
            // Registering again (a second layer of the same name) fails harmlessly.
            RegisterClassW(&wc);
            let mut ex = WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE;
            if click_through {
                ex |= WS_EX_TRANSPARENT;
            }
            let hwnd = CreateWindowExW(
                ex,
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
            )?;
            if crate::win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            Ok(Self {
                hwnd,
                shown: false,
                dib: None,
            })
        }
    }

    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    pub fn visible(&self) -> bool {
        self.shown
    }

    /// Shows `pm` (premultiplied RGBA) with its top-left at screen (x, y),
    /// multiplied by `opacity`. Resizes the window to the pixmap.
    pub fn present(&mut self, pm: &Pixmap, x: i32, y: i32, opacity: f32) {
        let (w, h) = (pm.width() as i32, pm.height() as i32);
        unsafe {
            let screen = GetDC(None);
            if self.dib.as_ref().is_none_or(|d| d.w < w || d.h < h) {
                // Grow-only backing store: animating sizes reuse one bitmap.
                let (cw, ch) = self
                    .dib
                    .as_ref()
                    .map_or((w, h), |d| (d.w.max(w), d.h.max(h)));
                self.dib = None;
                self.dib = Dib::new(screen, cw, ch);
            }
            if let Some(d) = &self.dib {
                let dst = std::slice::from_raw_parts_mut(d.bits, (d.w * d.h * 4) as usize);
                let src = pm.data();
                for row in 0..h as usize {
                    let s = &src[row * w as usize * 4..(row + 1) * w as usize * 4];
                    let o = row * d.w as usize * 4;
                    for (dp, sp) in dst[o..o + w as usize * 4]
                        .chunks_exact_mut(4)
                        .zip(s.chunks_exact(4))
                    {
                        dp[0] = sp[2];
                        dp[1] = sp[1];
                        dp[2] = sp[0];
                        dp[3] = sp[3];
                    }
                }
                let blend = blend(opacity);
                let pos = POINT { x, y };
                let size = SIZE { cx: w, cy: h };
                let src = POINT { x: 0, y: 0 };
                let _ = UpdateLayeredWindow(
                    self.hwnd,
                    Some(screen),
                    Some(&pos),
                    Some(&size),
                    Some(d.dc),
                    Some(&src),
                    COLORREF(0),
                    Some(&blend),
                    ULW_ALPHA,
                );
            }
            ReleaseDC(None, screen);
            if !self.shown {
                self.shown = true;
                let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
                self.raise();
            }
        }
    }

    /// Changes only the window's overall opacity (no re-render).
    pub fn set_opacity(&self, opacity: f32) {
        let blend = blend(opacity);
        unsafe {
            let _ = UpdateLayeredWindow(
                self.hwnd,
                None,
                None,
                None,
                None,
                None,
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            );
        }
    }

    /// Re-asserts top-most (e.g. after another top-most window appeared).
    pub fn raise(&self) {
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

    /// Puts this window directly below `above` in z-order (e.g. a shadow layer).
    pub fn place_below(&self, above: HWND) {
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(above),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
    }

    pub fn hide(&mut self) {
        if self.shown {
            self.shown = false;
            unsafe {
                let _ = ShowWindow(self.hwnd, SW_HIDE);
            }
        }
    }

    /// Frees the backing bitmap (call when hidden for a while).
    pub fn release_memory(&mut self) {
        self.dib = None;
    }
}

unsafe extern "system" fn def_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
}

fn blend(opacity: f32) -> BLENDFUNCTION {
    BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: (opacity.clamp(0.0, 1.0) * 255.0).round() as u8,
        AlphaFormat: AC_SRC_ALPHA as u8,
    }
}

impl Dib {
    unsafe fn new(screen: HDC, w: i32, h: i32) -> Option<Self> {
        unsafe {
            let dc = CreateCompatibleDC(Some(screen));
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
            match CreateDIBSection(Some(dc), &info, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(bmp) if !bits.is_null() => {
                    let old = SelectObject(dc, bmp.into());
                    Some(Self {
                        dc,
                        bmp,
                        old,
                        bits: bits as *mut u8,
                        w,
                        h,
                    })
                }
                _ => {
                    let _ = DeleteDC(dc);
                    None
                }
            }
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bmp.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

impl Drop for Layer {
    fn drop(&mut self) {
        self.dib = None;
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_measures_and_wraps() {
        let s = TextStyle::body(1.0);
        let w = text_width("abcd", &s);
        assert!(w > 20.0 && w < 40.0, "{w}");
        // Monospace: every glyph has the same advance.
        assert!((text_width("iiii", &s) - text_width("MMMM", &s)).abs() < 0.01);
        let lines = wrap("the quick brown fox jumps over the lazy dog", &s, 80.0);
        assert!(lines.len() > 2 && lines.iter().all(|l| text_width(l, &s) <= 80.0));
        let e = ellipsize("a very long agent status line indeed", &s, 100.0);
        assert!(e.ends_with('…') && text_width(&e, &s) <= 100.0);
        assert!(text_width("03", &TextStyle::readout(1.0)) > 0.0);
    }

    #[test]
    fn shadow_and_text_draw() {
        let mut pm = Pixmap::new(200, 100).unwrap();
        shadow(&mut pm, 40.0, 20.0, 120.0, 36.0, 18.0, 1.0);
        let under = pm.pixel(100, 70).unwrap().alpha();
        assert!(under > 10, "shadow below the shape: {under}");
        assert_eq!(pm.pixel(2, 2).unwrap().alpha(), 0);
        fill_rounded(&mut pm, 40.0, 20.0, 120.0, 36.0, 18.0, tokens::INK);
        draw_text(&mut pm, "HI", 60.0, 28.0, &TextStyle::body(1.0));
        assert!(pm.pixels().iter().any(|p| p.red() > 200));
    }
}
