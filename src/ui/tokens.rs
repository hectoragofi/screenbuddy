//! Design tokens shared by every always-on surface (island, captions, cards).
//!
//! All sizes are **logical px**; multiply by the monitor scale before drawing.
//! Colours are straight (non-premultiplied) sRGB.

/// A straight-alpha sRGB colour.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rgba {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba {
    pub const fn rgb(hex: u32) -> Self {
        Self {
            r: (hex >> 16) as u8,
            g: (hex >> 8) as u8,
            b: hex as u8,
            a: 255,
        }
    }

    pub const fn rgba(hex: u32, a: u8) -> Self {
        let c = Self::rgb(hex);
        Self { a, ..c }
    }

    /// Same colour with its alpha multiplied by `f` (0..1).
    pub fn fade(self, f: f32) -> Self {
        Self {
            a: (self.a as f32 * f.clamp(0.0, 1.0)).round() as u8,
            ..self
        }
    }

    /// Linear blend towards `o` by `t` (0 = self, 1 = o), alpha included.
    pub fn mix(self, o: Rgba, t: f32) -> Self {
        let t = t.clamp(0.0, 1.0);
        let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Self {
            r: l(self.r, o.r),
            g: l(self.g, o.g),
            b: l(self.b, o.b),
            a: l(self.a, o.a),
        }
    }

    pub fn skia(self) -> tiny_skia::Color {
        tiny_skia::Color::from_rgba8(self.r, self.g, self.b, self.a)
    }
}

// ---------------------------------------------------------------- colour

/// Surfaces (dark, default).
pub const INK: Rgba = Rgba::rgb(0x0E0E0E);
/// Raised surface: hovered rows, chips.
pub const INK_2: Rgba = Rgba::rgb(0x1A1A19);
/// Text on ink.
pub const PAPER: Rgba = Rgba::rgb(0xF2F0EB);
/// Secondary text, idle dots.
pub const MUTE: Rgba = Rgba::rgb(0x8A8780);
/// Hairlines: white at 8 %.
pub const LINE: Rgba = Rgba::rgba(0xFFFFFF, 20);
/// The one accent: listening, working, live.
pub const SIGNAL: Rgba = Rgba::rgb(0xFF5A1F);
/// Approvals / questions.
pub const ASK: Rgba = Rgba::rgb(0x4C82FF);
/// Done dot.
pub const OK: Rgba = Rgba::rgb(0x3DDC84);
pub const ERR: Rgba = Rgba::rgb(0xFF3B30);

// ---------------------------------------------------------------- type

/// UI labels: Geist Mono, ALL CAPS, +6 % tracking.
pub const LABEL_PX: f32 = 10.5;
pub const LABEL_TRACKING: f32 = 0.06;
/// Body text: Geist Mono.
pub const BODY_PX: f32 = 12.5;
/// Doto readouts (`03`, `00:41`) in the island.
pub const READOUT_PX: f32 = 15.0;

// ---------------------------------------------------------------- shape

pub const RADIUS_CARD: f32 = 16.0;
pub const RADIUS_CHIP: f32 = 8.0;

/// Drop shadow: 0 8 24 rgba(0,0,0,.35) (CSS blur radius, i.e. sigma = blur / 2).
pub const SHADOW_Y: f32 = 8.0;
pub const SHADOW_BLUR: f32 = 24.0;
pub const SHADOW_ALPHA: f32 = 0.35;

// ---------------------------------------------------------------- motion

/// A damped spring, integrated on wall-clock `dt` so it looks the same at
/// 60 Hz and 144 Hz. Retarget it any time; velocity carries over, so motion
/// never pops.
#[derive(Clone, Copy, Debug)]
pub struct Spring {
    pub value: f32,
    pub velocity: f32,
    pub target: f32,
    pub stiffness: f32,
    pub damping: f32,
}

impl Spring {
    /// Default feel: stiffness 380, damping 30 (just under critical, ~1 % overshoot).
    pub const fn new(value: f32) -> Self {
        Self {
            value,
            velocity: 0.0,
            target: value,
            stiffness: 380.0,
            damping: 30.0,
        }
    }

    pub const fn with(mut self, stiffness: f32, damping: f32) -> Self {
        self.stiffness = stiffness;
        self.damping = damping;
        self
    }

    pub fn set(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump straight to `v`, no motion.
    pub fn snap(&mut self, v: f32) {
        self.value = v;
        self.target = v;
        self.velocity = 0.0;
    }

    /// Kick the spring (e.g. a shake); it settles back on its target.
    pub fn kick(&mut self, velocity: f32) {
        self.velocity += velocity;
    }

    pub fn settled(&self) -> bool {
        self.velocity == 0.0 && self.value == self.target
    }

    /// Advances by `dt` seconds; returns true while still moving.
    pub fn tick(&mut self, dt: f32) -> bool {
        if self.settled() {
            return false;
        }
        // Semi-implicit Euler in small fixed sub-steps stays stable for any dt.
        let mut left = dt.clamp(0.0, 0.1);
        while left > 0.0 {
            let h = left.min(1.0 / 480.0);
            let force = -self.stiffness * (self.value - self.target) - self.damping * self.velocity;
            self.velocity += force * h;
            self.value += self.velocity * h;
            left -= h;
        }
        let scale = self.target.abs().max(1.0);
        if (self.value - self.target).abs() < 0.002 * scale && self.velocity.abs() < 0.02 * scale {
            self.snap(self.target);
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_settles_framerate_independent() {
        let run = |hz: f32| {
            let mut s = Spring::new(0.0);
            s.set(100.0);
            let mut t = 0.0;
            while s.tick(1.0 / hz) {
                t += 1.0 / hz;
                assert!(t < 2.0, "spring never settled");
            }
            (t, s.value)
        };
        let (t60, v60) = run(60.0);
        let (t144, v144) = run(144.0);
        assert_eq!(v60, 100.0);
        assert_eq!(v144, 100.0);
        assert!((t60 - t144).abs() < 0.05, "{t60} vs {t144}");
    }

    #[test]
    fn spring_retargets_mid_flight() {
        let mut s = Spring::new(0.0);
        s.set(100.0);
        for _ in 0..5 {
            s.tick(1.0 / 120.0);
        }
        let v = s.velocity;
        assert!(v > 0.0);
        s.set(-50.0);
        s.tick(1.0 / 120.0);
        // Velocity carries over instead of resetting.
        assert!(s.velocity > 0.0 && s.velocity < v * 1.5);
    }
}
