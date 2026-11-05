//! The palette's one continuous surface: rounded boxes merged by a signed
//! distance field in a fragment shader, over a blurred copy of the screen.
//! Also the motion helpers shared by the palette and the on-screen drawings.

use crate::capture::{Rect, RawShot};
use eframe::egui_glow;
use eframe::glow::{self, HasContext};
use egui::Color32;
use std::sync::{Arc, Mutex};

/// Surface color. Opaque at blur 0; see `glass_opacity` otherwise.
pub const SURFACE: Color32 = Color32::from_rgb(14, 22, 38);
pub const TEXT: Color32 = Color32::from_rgb(241, 244, 248);
/// Secondary text. Stays above 4.5:1 at every blur strength (tested below).
pub const TEXT_2: Color32 = Color32::from_rgb(169, 182, 200);

/// Most shapes the shader merges at once.
pub const MAX_BLOBS: usize = 8;

// ---------------------------------------------------------------- motion

/// Strong ease-out for things entering or leaving.
pub const EASE_OUT: [f32; 4] = [0.23, 1.0, 0.32, 1.0];
/// Strong ease-in-out for things travelling across the screen.
pub const EASE_IN_OUT: [f32; 4] = [0.77, 0.0, 0.175, 1.0];

/// Evaluates a CSS-style cubic bezier at `t` in 0..=1.
pub fn bezier([x1, y1, x2, y2]: [f32; 4], t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t <= 0.0 || t >= 1.0 {
        return t;
    }
    let axis = |a: f32, b: f32, u: f32| {
        let v = 1.0 - u;
        3.0 * v * v * u * a + 3.0 * v * u * u * b + u * u * u
    };
    // x(u) is monotonic for these curves, so bisection always lands.
    let (mut lo, mut hi) = (0.0f32, 1.0f32);
    for _ in 0..18 {
        let mid = (lo + hi) / 2.0;
        if axis(x1, x2, mid) < t {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    axis(y1, y2, (lo + hi) / 2.0)
}

/// Normalized progress of a segment starting at `start` and lasting `dur`, eased.
pub fn ease(curve: [f32; 4], now: f64, start: f64, dur: f32) -> f32 {
    bezier(curve, ((now - start) as f32 / dur.max(0.001)).clamp(0.0, 1.0))
}

/// Non-uniform stagger: later items wait less, so a group reads as one wave.
pub fn stagger(i: usize) -> f32 {
    [0.0, 0.05, 0.08, 0.10].get(i).copied().unwrap_or(0.11)
}

/// A critically damped value. Retargeting mid-flight keeps its velocity, and
/// real elapsed time drives it, so it looks the same at any frame rate.
#[derive(Clone, Copy, Debug)]
pub struct Smooth {
    pub value: f32,
    velocity: f32,
    pub target: f32,
}

impl Smooth {
    pub fn new(value: f32) -> Self {
        Self { value, velocity: 0.0, target: value }
    }

    pub fn snap(&mut self, value: f32) {
        *self = Self::new(value);
    }

    pub fn settled(&self) -> bool {
        self.value == self.target
    }

    /// `smooth_time` is roughly a third of the time it takes to arrive.
    pub fn step(&mut self, dt: f32, smooth_time: f32) {
        if self.settled() {
            return;
        }
        let omega = 2.0 / smooth_time.max(0.001);
        let x = omega * dt;
        let decay = 1.0 / (1.0 + x + 0.48 * x * x + 0.235 * x * x * x);
        let delta = self.value - self.target;
        let temp = (self.velocity + omega * delta) * dt;
        self.velocity = (self.velocity - omega * temp) * decay;
        self.value = self.target + (delta + temp) * decay;
        if (self.value - self.target).abs() < 0.25 && self.velocity.abs() < 30.0 {
            self.snap(self.target);
        }
    }
}

/// How quickly a height change should settle: a big change takes the full
/// 280ms or so, a one-line streamed change much less.
pub fn smooth_time_for(delta: f32) -> f32 {
    (delta.abs() / 1600.0).clamp(0.05, 0.09)
}

/// True when Windows' "Animation effects" setting is off.
pub fn reduced_motion() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
    };
    let mut on = windows::core::BOOL(1);
    let read = unsafe {
        SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut on as *mut windows::core::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    read.is_ok() && !on.as_bool()
}

// ---------------------------------------------------------------- glass

/// How much of the surface is its own color at a blur strength of 0..=100; the
/// rest is what's behind it. Readability doesn't depend on this: the shader limits
/// how bright the backdrop can get (see `PEAK` there), so it's safe to go to zero
/// and let a black window behind the surface look black.
pub fn glass_opacity(strength: f32) -> f32 {
    (1.0 - strength / 80.0).clamp(0.0, 1.0)
}

/// Blur radius in points at a blur strength of 0..=100.
fn glass_radius(strength: f32) -> f32 {
    8.0 + 32.0 * (strength / 100.0).clamp(0.0, 1.0)
}

/// A small, blurred copy of the monitor. It's made once per screenshot, so the
/// surface pays for a single texture read per pixel. It doesn't follow changes
/// on screen until the next capture.
pub struct Backdrop {
    w: usize,
    h: usize,
    rgba: Vec<u8>,
    /// The area it shows, in virtual-screen pixels.
    pub area: Rect,
}

impl Backdrop {
    /// None at strength 0, which means a solid surface.
    pub fn new(shot: &RawShot, strength: f32) -> Option<Arc<Self>> {
        if strength <= 0.0 {
            return None;
        }
        let area = shot.monitor.rect;
        let (mw, mh) = (area.w.max(1) as usize, area.h.max(1) as usize);
        if shot.bgra.len() < mw * mh * 4 {
            return None;
        }
        let step = (mw / 480).max(1);
        let (w, h) = ((mw / step).max(1), (mh / step).max(1));
        let mut rgb = vec![0f32; w * h * 3];
        for y in 0..h {
            for x in 0..w {
                // Sparse average: smooth enough once blurred, and fast at 4K.
                let (mut sum, mut n) = ([0u32; 3], 0u32);
                let stride = (step / 4).max(1);
                for sy in (y * step..(y + 1) * step).step_by(stride) {
                    for sx in (x * step..(x + 1) * step).step_by(stride) {
                        let i = (sy * mw + sx) * 4;
                        sum[0] += shot.bgra[i + 2] as u32;
                        sum[1] += shot.bgra[i + 1] as u32;
                        sum[2] += shot.bgra[i] as u32;
                        n += 1;
                    }
                }
                let o = (y * w + x) * 3;
                for c in 0..3 {
                    rgb[o + c] = sum[c] as f32 / n.max(1) as f32;
                }
            }
        }
        let radius = glass_radius(strength) * shot.monitor.scale / step as f32;
        // Three box passes approximate a Gaussian closely.
        let r = ((radius / 1.7).round() as usize).max(1);
        for _ in 0..3 {
            box_blur(&mut rgb, w, h, r);
        }
        let mut out = vec![255u8; w * h * 4];
        for (px, c) in out.chunks_exact_mut(4).zip(rgb.chunks_exact(3)) {
            // A little more saturation keeps color alive under the dark tint.
            let l = 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
            for i in 0..3 {
                px[i] = (l + (c[i] - l) * 1.4).clamp(0.0, 255.0) as u8;
            }
        }
        Some(Arc::new(Self { w, h, rgba: out, area }))
    }
}

/// Separable running-sum box blur over interleaved RGB.
fn box_blur(px: &mut [f32], w: usize, h: usize, r: usize) {
    let mut line = vec![0f32; w.max(h)];
    let mut pass = |px: &mut [f32], n: usize, lines: usize, step: usize, across: usize| {
        for l in 0..lines {
            for c in 0..3 {
                let at = |i: usize| l * across + i * step + c;
                for (i, v) in line.iter_mut().enumerate().take(n) {
                    *v = px[at(i)];
                }
                let get = |i: isize| line[i.clamp(0, n as isize - 1) as usize];
                let mut sum: f32 = (-(r as isize)..=r as isize).map(get).sum();
                let norm = 1.0 / (2 * r + 1) as f32;
                for i in 0..n {
                    px[at(i)] = sum * norm;
                    sum += get(i as isize + r as isize + 1) - get(i as isize - r as isize);
                }
            }
        }
    };
    pass(px, w, h, 3, w * 3);
    pass(px, h, w, w * 3, 3);
}

// ---------------------------------------------------------------- shapes

/// One rounded box of the surface, in points.
#[derive(Clone, Copy, Debug)]
pub struct Blob {
    pub rect: egui::Rect,
    pub radius: f32,
    /// How softly it melts into the shapes before it; 0 is a plain union.
    pub merge: f32,
}

/// What the surface looks like this frame.
#[derive(Clone, Debug, Default)]
pub struct Scene {
    pub blobs: Vec<Blob>,
    /// The window's top-left in virtual-screen pixels, to line up the backdrop.
    pub window_px: (f32, f32),
}

impl Scene {
    fn bounds(&self) -> Option<egui::Rect> {
        self.blobs.iter().map(|b| b.rect).reduce(|a, b| a.union(b))
    }
}

/// Whether the top edge and sides of `rect` (window points) are on `area`. The
/// bottom can run past it; the shader clamps that, and it isn't worth a jump.
fn covers(area: crate::capture::Rect, window_px: (f32, f32), rect: egui::Rect, ppp: f32) -> bool {
    let left = window_px.0 + rect.left() * ppp;
    let right = window_px.0 + rect.right() * ppp;
    let top = window_px.1 + rect.top() * ppp;
    left >= area.x as f32
        && right <= (area.x + area.w) as f32
        && top >= area.y as f32
        && top < (area.y + area.h) as f32
}

/// Plain egui version, for when the shader isn't available: no merging or blur.
pub fn fallback_shapes(scene: &Scene) -> Vec<egui::Shape> {
    scene
        .blobs
        .iter()
        .map(|b| egui::Shape::rect_filled(b.rect, b.radius, SURFACE))
        .collect()
}

// ---------------------------------------------------------------- shader

const VERTEX: &str = r#"
void main() {
    vec2 p = vec2(float((gl_VertexID << 1) & 2), float(gl_VertexID & 2));
    gl_Position = vec4(p * 2.0 - 1.0, 0.0, 1.0);
}
"#;

const FRAGMENT: &str = r#"
uniform vec4 u_box[8];    // center x, y and half size x, y, in pixels from the top-left
uniform vec2 u_shape[8];  // corner radius, merge amount
uniform int u_count;
uniform float u_height;   // framebuffer height
uniform float u_ppp;
uniform vec3 u_tint;
uniform float u_opacity;
uniform vec4 u_map;       // window offset on the backdrop in pixels, then 1 / its size
uniform int u_has_backdrop;
uniform sampler2D u_backdrop;
out vec4 o_color;

// The backdrop's brightest channel can't go above this, which keeps secondary text
// at 4.5:1 over anything. Colors keep their hue. Same value as the test's `PEAK`.
const float PEAK = 70.0 / 255.0;

float box_at(vec2 p, int i) {
    vec2 q = abs(p - u_box[i].xy) - u_box[i].zw + u_shape[i].x;
    return length(max(q, vec2(0.0))) + min(max(q.x, q.y), 0.0) - u_shape[i].x;
}

float field(vec2 p) {
    float d = box_at(p, 0);
    for (int i = 1; i < u_count; i++) {
        float next = box_at(p, i);
        float k = u_shape[i].y;
        if (k > 0.0) {
            float h = max(k - abs(d - next), 0.0) / k;
            d = min(d, next) - h * h * k * 0.25;
        } else {
            d = min(d, next);
        }
    }
    return d;
}

void main() {
    vec2 p = vec2(gl_FragCoord.x, u_height - gl_FragCoord.y);
    float d = field(p);
    float cover = clamp(0.5 - d, 0.0, 1.0);

    vec3 rgb = u_tint;
    if (u_has_backdrop != 0 && cover > 0.0) {
        vec3 behind = texture(u_backdrop, (p + u_map.xy) * u_map.zw).rgb;
        behind *= min(1.0, PEAK / max(max(max(behind.r, behind.g), behind.b), 0.001));
        // Dither, so the very smooth gradient doesn't band in 8 bits.
        float noise = fract(sin(dot(p, vec2(12.9898, 78.233))) * 43758.5453) - 0.5;
        rgb = mix(behind, u_tint, u_opacity) + noise / 255.0;
    }
    float rim = clamp(1.0 - abs(d + 0.75 * u_ppp) / u_ppp, 0.0, 1.0) * 0.10;
    rgb += (vec3(1.0) - rgb) * rim;

    o_color = vec4(rgb * cover, cover);
}
"#;

struct Uniforms {
    boxes: glow::UniformLocation,
    shapes: glow::UniformLocation,
    count: glow::UniformLocation,
    height: glow::UniformLocation,
    ppp: glow::UniformLocation,
    tint: glow::UniformLocation,
    opacity: glow::UniformLocation,
    map: glow::UniformLocation,
    has_backdrop: glow::UniformLocation,
    backdrop: glow::UniformLocation,
}

struct Texture {
    id: glow::Texture,
    upload: Option<Arc<Backdrop>>,
}

/// The compiled shader. Lives as long as the palette's GL context.
pub struct Renderer {
    program: glow::Program,
    vao: glow::VertexArray,
    uniforms: Uniforms,
    /// Holds a new backdrop until the next draw, which has the GL context.
    texture: Mutex<Texture>,
    current: Mutex<Option<Arc<Backdrop>>>,
    opacity: f32,
}

// SAFETY: glow's handles are plain integers on desktop GL, and they're only
// used inside paint callbacks, on the thread that owns the context.
unsafe impl Send for Renderer {}
unsafe impl Sync for Renderer {}

impl Renderer {
    /// None if the driver can't build the shader; the palette then falls back
    /// to `fallback_shapes`.
    pub fn new(gl: &glow::Context, backdrop: Option<Arc<Backdrop>>, opacity: f32) -> Option<Arc<Self>> {
        unsafe {
            let program = ["#version 330 core\n", "#version 140\n"]
                .iter()
                .find_map(|head| link(gl, head))?;
            let u = |name: &str| gl.get_uniform_location(program, name);
            let uniforms = Uniforms {
                boxes: u("u_box")?,
                shapes: u("u_shape")?,
                count: u("u_count")?,
                height: u("u_height")?,
                ppp: u("u_ppp")?,
                tint: u("u_tint")?,
                opacity: u("u_opacity")?,
                map: u("u_map")?,
                has_backdrop: u("u_has_backdrop")?,
                backdrop: u("u_backdrop")?,
            };
            let vao = gl.create_vertex_array().ok()?;
            let id = gl.create_texture().ok()?;
            Some(Arc::new(Self {
                program,
                vao,
                uniforms,
                texture: Mutex::new(Texture { id, upload: backdrop.clone() }),
                current: Mutex::new(backdrop),
                opacity,
            }))
        }
    }

    /// A new screenshot was taken: its blur replaces the old one at the next draw.
    pub fn set_backdrop(&self, backdrop: Option<Arc<Backdrop>>) {
        *self.current.lock().unwrap() = backdrop.clone();
        self.texture.lock().unwrap().upload = backdrop;
    }

    /// The surface as an egui shape that draws through the shader.
    pub fn callback(self: &Arc<Self>, rect: egui::Rect, scene: Scene) -> egui::Shape {
        let me = self.clone();
        let callback = egui_glow::CallbackFn::new(move |info, gl_painter| unsafe {
            me.draw(gl_painter.gl(), &info, &scene);
        });
        egui::Shape::Callback(egui::PaintCallback { rect, callback: Arc::new(callback) })
    }

    unsafe fn draw(&self, gl: &glow::Context, info: &egui::PaintCallbackInfo, scene: &Scene) {
        let Some(bounds) = scene.bounds() else { return };
        let ppp = info.pixels_per_point;
        let [fb_w, fb_h] = info.screen_size_px;
        unsafe {
            let mut tex = self.texture.lock().unwrap();
            if let Some(b) = tex.upload.take() {
                gl.bind_texture(glow::TEXTURE_2D, Some(tex.id));
                gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
                for (key, value) in [
                    (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                    (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                    (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                    (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
                ] {
                    gl.tex_parameter_i32(glow::TEXTURE_2D, key, value as i32);
                }
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    b.w as i32,
                    b.h as i32,
                    0,
                    glow::RGBA,
                    glow::UNSIGNED_BYTE,
                    glow::PixelUnpackData::Slice(Some(&b.rgba)),
                );
            }

            // Only shade where the surface can land.
            let pad = 34.0 * ppp;
            let x0 = (bounds.min.x * ppp - pad).floor().max(0.0);
            let x1 = (bounds.max.x * ppp + pad).ceil().min(fb_w as f32);
            let y0 = (bounds.min.y * ppp - pad).floor().max(0.0);
            let y1 = (bounds.max.y * ppp + pad).ceil().min(fb_h as f32);
            gl.enable(glow::SCISSOR_TEST);
            gl.scissor(x0 as i32, fb_h as i32 - y1 as i32, (x1 - x0) as i32, (y1 - y0) as i32);
            gl.viewport(0, 0, fb_w as i32, fb_h as i32);

            let (mut boxes, mut shapes) = ([0f32; 4 * MAX_BLOBS], [0f32; 2 * MAX_BLOBS]);
            let count = scene.blobs.len().min(MAX_BLOBS);
            for (i, b) in scene.blobs.iter().take(MAX_BLOBS).enumerate() {
                let (c, half) = (b.rect.center(), b.rect.size() / 2.0);
                boxes[i * 4..i * 4 + 4]
                    .copy_from_slice(&[c.x * ppp, c.y * ppp, half.x * ppp, half.y * ppp]);
                let radius = b.radius.min(half.x).min(half.y).max(0.0);
                shapes[i * 2] = radius * ppp;
                shapes[i * 2 + 1] = b.merge * ppp;
            }

            let u = &self.uniforms;
            gl.use_program(Some(self.program));
            gl.bind_vertex_array(Some(self.vao));
            gl.uniform_4_f32_slice(Some(&u.boxes), &boxes);
            gl.uniform_2_f32_slice(Some(&u.shapes), &shapes);
            gl.uniform_1_i32(Some(&u.count), count as i32);
            gl.uniform_1_f32(Some(&u.height), fb_h as f32);
            gl.uniform_1_f32(Some(&u.ppp), ppp);
            let [r, g, b, _] = SURFACE.to_normalized_gamma_f32();
            gl.uniform_3_f32(Some(&u.tint), r, g, b);
            gl.uniform_1_f32(Some(&u.opacity), self.opacity);
            let current = self.current.lock().unwrap();
            // Dragged off the captured monitor there's nothing to show: go solid.
            let shown = current.as_ref().filter(|b| {
                scene.blobs.first().is_none_or(|m| covers(b.area, scene.window_px, m.rect, ppp))
            });
            if let Some(backdrop) = shown {
                let a = backdrop.area;
                gl.uniform_4_f32(
                    Some(&u.map),
                    scene.window_px.0 - a.x as f32,
                    scene.window_px.1 - a.y as f32,
                    1.0 / a.w.max(1) as f32,
                    1.0 / a.h.max(1) as f32,
                );
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(tex.id));
                gl.uniform_1_i32(Some(&u.backdrop), 0);
            }
            gl.uniform_1_i32(Some(&u.has_backdrop), shown.is_some() as i32);
            gl.draw_arrays(glow::TRIANGLES, 0, 3);
            gl.bind_vertex_array(None);
        }
    }
}

unsafe fn link(gl: &glow::Context, head: &str) -> Option<glow::Program> {
    unsafe {
        let program = gl.create_program().ok()?;
        let mut parts = Vec::new();
        for (kind, body) in [(glow::VERTEX_SHADER, VERTEX), (glow::FRAGMENT_SHADER, FRAGMENT)] {
            let shader = gl.create_shader(kind).ok()?;
            gl.shader_source(shader, &format!("{head}{body}"));
            gl.compile_shader(shader);
            if !gl.get_shader_compile_status(shader) {
                crate::config::log(&format!("surface shader: {}", gl.get_shader_info_log(shader)));
                gl.delete_shader(shader);
                gl.delete_program(program);
                return None;
            }
            gl.attach_shader(program, shader);
            parts.push(shader);
        }
        gl.link_program(program);
        let ok = gl.get_program_link_status(program);
        for shader in parts {
            gl.detach_shader(program, shader);
            gl.delete_shader(shader);
        }
        if !ok {
            crate::config::log(&format!("surface shader: {}", gl.get_program_info_log(program)));
            gl.delete_program(program);
            return None;
        }
        Some(program)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backdrop_only_where_it_was_captured() {
        let area = crate::capture::Rect { x: -1920, y: 0, w: 1920, h: 1080 };
        let pill = egui::Rect::from_min_size(egui::pos2(24.0, 68.0), egui::vec2(400.0, 72.0));
        assert!(covers(area, (-1500.0, 100.0), pill, 1.0));
        assert!(!covers(area, (100.0, 100.0), pill, 1.0), "on another monitor");
        assert!(!covers(area, (-200.0, 100.0), pill, 1.0), "across the edge");
        assert!(covers(area, (-1500.0, 100.0), pill.with_max_y(5000.0), 1.0), "tall is fine");
    }

    fn linear(c: f32) -> f32 {
        if c <= 0.04045 { c / 12.92 } else { ((c + 0.055) / 1.055).powf(2.4) }
    }

    fn luminance(rgb: [f32; 3]) -> f32 {
        let [r, g, b] = rgb.map(|c| linear(c / 255.0));
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    fn contrast(a: [f32; 3], b: [f32; 3]) -> f32 {
        let (x, y) = (luminance(a), luminance(b));
        (x.max(y) + 0.05) / (x.min(y) + 0.05)
    }

    fn rgb(c: Color32) -> [f32; 3] {
        [c.r() as f32, c.g() as f32, c.b() as f32]
    }

    #[test]
    fn secondary_text_stays_readable_over_anything() {
        const PEAK: f32 = 70.0;
        let screens: [[f32; 3]; 5] = [[255.0, 255.0, 255.0], [255.0, 255.0, 0.0], [0.0, 255.0, 0.0], [255.0, 0.0, 0.0], [0.0, 0.0, 0.0]];
        for strength in [0.0, 1.0, 25.0, 60.0, 100.0, 150.0] {
            let a = glass_opacity(strength);
            for screen in screens {
                let peak = screen[0].max(screen[1]).max(screen[2]).max(1.0);
                let limit = (PEAK / peak).min(1.0);
                let tint = rgb(SURFACE);
                let bg = [0, 1, 2].map(|i| tint[i] * a + screen[i] * limit * (1.0 - a));
                let ratio = contrast(rgb(TEXT_2), bg);
                assert!(ratio >= 4.5, "strength {strength} over {screen:?}: {ratio:.2}:1");
            }
        }
    }

    #[test]
    fn black_behind_stays_close_to_black_at_the_default_strength() {
        let a = glass_opacity(60.0);
        let bg = rgb(SURFACE).map(|c| c * a);
        assert!(bg.iter().all(|&c| c < 10.0), "{bg:?}");
    }

    #[test]
    fn curves_hit_their_ends_and_rise() {
        for curve in [EASE_OUT, EASE_IN_OUT] {
            assert_eq!(bezier(curve, 0.0), 0.0);
            assert_eq!(bezier(curve, 1.0), 1.0);
            let mut last = 0.0;
            for i in 1..=20 {
                let y = bezier(curve, i as f32 / 20.0);
                assert!(y >= last - 1e-3, "{y} after {last}");
                last = y;
            }
        }
        // Strong ease-out: most of the way there in the first third.
        assert!(bezier(EASE_OUT, 0.33) > 0.75);
    }

    #[test]
    fn smooth_arrives_without_overshoot_at_any_frame_rate() {
        for dt in [1.0 / 240.0, 1.0 / 60.0, 1.0 / 20.0] {
            let mut s = Smooth::new(72.0);
            s.target = 372.0;
            let mut t = 0.0;
            while !s.settled() && t < 2.0 {
                s.step(dt, smooth_time_for(300.0));
                assert!(s.value <= 372.0 + 0.01, "overshot to {} at dt {dt}", s.value);
                t += dt;
            }
            assert!(s.settled() && t < 0.8, "took {t}s at dt {dt}");
        }
    }

    #[test]
    fn smooth_keeps_moving_through_a_retarget() {
        let mut s = Smooth::new(0.0);
        s.target = 100.0;
        for _ in 0..4 {
            s.step(1.0 / 60.0, 0.09);
        }
        let before = s.value;
        s.target = 300.0;
        s.step(1.0 / 60.0, 0.09);
        assert!(s.value > before);
    }

    #[test]
    fn blur_keeps_a_flat_color_and_spreads_an_edge() {
        let (w, h) = (16, 16);
        let mut px = vec![100f32; w * h * 3];
        box_blur(&mut px, w, h, 2);
        assert!(px.iter().all(|v| (v - 100.0).abs() < 0.01));

        let mut edge = vec![0f32; w * h * 3];
        for y in 0..h {
            for x in w / 2..w {
                edge[(y * w + x) * 3] = 255.0;
            }
        }
        box_blur(&mut edge, w, h, 2);
        let v = edge[(8 * w + w / 2 - 1) * 3];
        assert!(v > 0.0 && v < 255.0, "{v}");
    }

    #[test]
    fn stagger_shrinks() {
        let gaps: Vec<f32> = (1..4).map(|i| stagger(i) - stagger(i - 1)).collect();
        assert!(gaps.windows(2).all(|g| g[1] < g[0]), "{gaps:?}");
    }
}
