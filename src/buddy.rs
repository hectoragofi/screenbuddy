//! The buddy: screenbuddy's face, a 9 × 7 dot-matrix glyph. Every dot has a
//! continuous brightness, so the states morph into each other instead of
//! flipping frames. Drawn by the island; pure rendering, no windows.
#![allow(dead_code)] // shared API; captions, cards and agents use the rest

use crate::ui::tokens::{self, Rgba};
use crate::ui::{self, Spring};
use std::time::{Duration, Instant};
use tiny_skia::Pixmap;

pub const COLS: usize = 9;
pub const ROWS: usize = 7;

/// What the buddy is doing.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BuddyState {
    /// Calm: slow breathe, blinks every 4–7 s.
    Idle,
    /// Hearing the user; the face becomes a level meter (0..1 input level).
    Listening(f32),
    /// Waiting on the model: a dot orbits.
    Thinking,
    /// The answer is arriving: a ripple.
    Streaming,
    /// Finished: a happy face with a green flash.
    Done,
    Error,
    /// Idle with `n` agents running in the background (a small SIGNAL badge).
    Working(u32),
}

impl BuddyState {
    fn kind(self) -> u8 {
        match self {
            BuddyState::Idle | BuddyState::Working(_) => 0,
            BuddyState::Listening(_) => 1,
            BuddyState::Thinking => 2,
            BuddyState::Streaming => 3,
            BuddyState::Done => 4,
            BuddyState::Error => 5,
        }
    }
}

/// How often the face needs a new frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Motion {
    /// Something is moving: render every display frame.
    Fast,
    /// Only slow ambient motion (breathing): ~30 fps is plenty.
    Slow,
    /// Static; next change (a blink) at the given time, if any.
    Still(Option<Instant>),
}

#[derive(Clone, Copy, Debug)]
struct Dot {
    b: f32,
    color: Rgba,
}

const DARK: Dot = Dot {
    b: 0.0,
    color: tokens::PAPER,
};

type Grid = [[Dot; COLS]; ROWS];

// '#' full, '+' soft, '.' off. Ours: two tall pill eyes, a short calm mouth.
const IDLE: [&str; ROWS] = [
    ".........",
    ".........",
    "..#...#..",
    "..#...#..",
    ".........",
    "...+++...",
    ".........",
];
const HAPPY: [&str; ROWS] = [
    ".........",
    "..#...#..",
    ".#.#.#.#.",
    ".........",
    ".#.....#.",
    "..#####..",
    ".........",
];
const ERROR: [&str; ROWS] = [
    ".........",
    ".#.#.#.#.",
    "..#...#..",
    ".#.#.#.#.",
    ".........",
    "..+++++..",
    ".........",
];

fn bitmap(rows: &[&str; ROWS], color: Rgba) -> Grid {
    let mut g = [[DARK; COLS]; ROWS];
    for (r, line) in rows.iter().enumerate() {
        for (c, ch) in line.chars().enumerate().take(COLS) {
            g[r][c] = Dot {
                b: match ch {
                    '#' => 1.0,
                    '+' => 0.5,
                    _ => 0.0,
                },
                color,
            };
        }
    }
    g
}

const CENTER: (f32, f32) = (4.0, 3.0);
const MORPH: f32 = 0.24;
const BLINK: f32 = 0.17;

pub struct Buddy {
    state: BuddyState,
    since: Instant,
    /// Grid at the moment of the last state change, cross-faded out over MORPH.
    from: Option<Grid>,
    level: Spring,
    next_blink: Instant,
    awake: bool,
    seed: u32,
    epoch: Instant,
}

impl Buddy {
    pub fn new(now: Instant) -> Self {
        let seed = (std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(7))
            | 1;
        let mut b = Self {
            state: BuddyState::Idle,
            since: now - Duration::from_secs(10),
            from: None,
            level: Spring::new(0.0).with(520.0, 34.0),
            next_blink: now,
            awake: true,
            seed,
            epoch: now,
        };
        b.schedule_blink(now);
        b
    }

    pub fn state(&self) -> BuddyState {
        self.state
    }

    /// How long the current state has lasted.
    pub fn age(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.since)
    }

    pub fn set(&mut self, s: BuddyState, now: Instant) {
        if let BuddyState::Listening(l) = s {
            self.level.set(l.clamp(0.0, 1.0));
        }
        if s.kind() == self.state.kind() {
            self.state = s; // level / agent count only: no morph
            return;
        }
        self.from = Some(self.grid(now));
        if !matches!(s, BuddyState::Listening(_)) {
            self.level.snap(0.0);
        }
        self.state = s;
        self.since = now;
    }

    /// Awake = ambient motion (breathing) allowed. Asleep, the face is still
    /// except for blinks, so the island costs nothing while idle.
    pub fn set_awake(&mut self, awake: bool) {
        self.awake = awake;
    }

    pub fn tick(&mut self, dt: f32) {
        self.level.tick(dt);
    }

    pub fn motion(&self, now: Instant) -> Motion {
        let t = self.age(now).as_secs_f32();
        if t < MORPH || !self.level.settled() {
            return Motion::Fast;
        }
        match self.state {
            BuddyState::Listening(_) | BuddyState::Thinking | BuddyState::Streaming => Motion::Fast,
            BuddyState::Done if t < 1.6 => Motion::Fast,
            BuddyState::Error if t < 0.8 => Motion::Fast,
            BuddyState::Idle | BuddyState::Working(_) => {
                let blink = now.saturating_duration_since(self.next_blink).as_secs_f32();
                if now >= self.next_blink && blink < BLINK {
                    Motion::Fast
                } else if self.awake || matches!(self.state, BuddyState::Working(_)) {
                    Motion::Slow
                } else {
                    Motion::Still(Some(self.next_blink.max(now)))
                }
            }
            _ => Motion::Still(None),
        }
    }

    fn rand(&mut self) -> f32 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 17;
        self.seed ^= self.seed << 5;
        (self.seed % 10_000) as f32 / 10_000.0
    }

    fn schedule_blink(&mut self, now: Instant) {
        let secs = 4.0 + 3.0 * self.rand();
        self.next_blink = now + Duration::from_secs_f32(secs);
    }

    /// Width and height of the face at dot `pitch` (physical px).
    pub fn size(pitch: f32) -> (f32, f32) {
        (COLS as f32 * pitch, ROWS as f32 * pitch)
    }

    /// The dot brightnesses + colours for this instant.
    fn grid(&mut self, now: Instant) -> Grid {
        let t = self.age(now).as_secs_f32();
        let clock = now.saturating_duration_since(self.epoch).as_secs_f32();
        let mut g = match self.state {
            BuddyState::Idle | BuddyState::Working(_) => self.idle(now, clock),
            BuddyState::Listening(_) => self.listening(clock),
            BuddyState::Thinking => thinking(t),
            BuddyState::Streaming => streaming(t),
            BuddyState::Done => {
                // Flash OK green, then settle into paper.
                let k = ((t - 0.45) / 0.9).clamp(0.0, 1.0);
                bitmap(&HAPPY, tokens::OK.mix(tokens::PAPER, ease(k)))
            }
            BuddyState::Error => bitmap(&ERROR, tokens::ERR),
        };
        if let Some(from) = &self.from {
            let k = ease((t / MORPH).clamp(0.0, 1.0));
            if k >= 1.0 {
                self.from = None;
            } else {
                for r in 0..ROWS {
                    for c in 0..COLS {
                        let (a, b) = (from[r][c], g[r][c]);
                        // Fade out the old dot, then light the new one, with
                        // colours blended so nothing flickers.
                        let color = if b.b > 0.0 {
                            a.color.mix(b.color, k)
                        } else {
                            a.color
                        };
                        g[r][c] = Dot {
                            b: a.b * (1.0 - k) + b.b * k,
                            color,
                        };
                    }
                }
            }
        }
        g
    }

    fn idle(&mut self, now: Instant, clock: f32) -> Grid {
        let mut g = bitmap(&IDLE, tokens::PAPER);
        if now >= self.next_blink {
            let bt = now.duration_since(self.next_blink).as_secs_f32();
            if bt >= BLINK {
                self.schedule_blink(now);
            } else {
                // Lids close from the top: the upper eye dots dim, the lower dip.
                let close = (bt / BLINK * std::f32::consts::PI).sin();
                for c in [2, 6] {
                    g[2][c].b *= 1.0 - close;
                    g[3][c].b *= 1.0 - 0.35 * close;
                }
            }
        }
        if self.awake {
            // A slow breath (~5 s), never fully dim.
            let breath = 0.5 + 0.5 * (clock * std::f32::consts::TAU / 5.2).sin();
            for row in g.iter_mut() {
                for d in row.iter_mut() {
                    d.b *= 0.8 + 0.2 * breath;
                }
            }
        }
        g
    }

    fn listening(&self, clock: f32) -> Grid {
        let level = self.level.value.clamp(0.0, 1.0);
        let mut g = [[Dot {
            b: 0.0,
            color: tokens::SIGNAL,
        }; COLS]; ROWS];
        // Five bars on the even columns read as a meter, not a blob.
        for c in (0..COLS).step_by(2) {
            let off = (c as f32 - CENTER.0).abs() / 4.0;
            let profile = 1.0 - 0.55 * off * off;
            let jitter = 0.72
                + 0.28
                    * (clock * 8.7 + c as f32 * 1.9).sin()
                    * (clock * 5.1 + c as f32 * 0.7).cos();
            // Half-height in rows above/below the centre line.
            let half = 0.35 + level * profile * jitter * 3.2;
            for (r, row) in g.iter_mut().enumerate() {
                let d = (r as f32 - CENTER.1).abs();
                row[c].b = (half - d + 0.5).clamp(0.0, 1.0) * if d == 0.0 { 1.0 } else { 0.92 };
            }
        }
        g
    }

    /// Draws the face with its grid's top-left at (x, y). `opacity` scales all
    /// of it (for cross-fading the island's content).
    pub fn draw(
        &mut self,
        pm: &mut Pixmap,
        x: f32,
        y: f32,
        pitch: f32,
        opacity: f32,
        now: Instant,
    ) {
        let g = self.grid(now);
        let t = self.age(now).as_secs_f32();
        let (mut ox, mut oy) = (0.0, 0.0);
        match self.state {
            BuddyState::Error if t < 0.8 => ox = 0.7 * (-6.0 * t).exp() * (t * 40.0).sin() * pitch,
            BuddyState::Done if t < 0.42 => {
                oy = -(t / 0.42 * std::f32::consts::PI).sin() * 0.55 * pitch
            }
            _ => {}
        }
        let off_r = pitch * 0.17;
        let on_r = pitch * 0.36;
        let matrix = tokens::PAPER.fade(0.10 * opacity);
        for (r, row) in g.iter().enumerate() {
            for (c, d) in row.iter().enumerate() {
                let cx = x + (c as f32 + 0.5) * pitch;
                let cy = y + (r as f32 + 0.5) * pitch;
                // The unlit matrix stays put; only lit dots move.
                ui::fill_circle(pm, cx, cy, off_r, matrix);
                if d.b <= 0.01 {
                    continue;
                }
                let b = d.b.clamp(0.0, 1.0);
                let rad = off_r + (on_r - off_r) * b.sqrt();
                let (lx, ly) = (cx + ox, cy + oy);
                if d.color != tokens::PAPER {
                    // Coloured dots get a faint bloom, like a lit LED.
                    ui::fill_circle(pm, lx, ly, rad * 2.1, d.color.fade(0.10 * b * opacity));
                }
                ui::fill_circle(pm, lx, ly, rad, d.color.fade(b.powf(0.8) * opacity));
            }
        }
        if let BuddyState::Working(_) = self.state {
            // Badge just off the top-right corner, breathing gently.
            let clock = now.saturating_duration_since(self.epoch).as_secs_f32();
            let p = 0.5 + 0.5 * (clock * std::f32::consts::TAU / 1.8).sin();
            let (bx, by) = (x + COLS as f32 * pitch + pitch * 0.55, y + pitch * 0.4);
            let k = ease((t / MORPH).clamp(0.0, 1.0));
            ui::fill_circle(
                pm,
                bx,
                by,
                pitch * (0.9 + 0.5 * p),
                tokens::SIGNAL.fade(0.16 * k * opacity),
            );
            ui::fill_circle(pm, bx, by, pitch * 0.55 * k, tokens::SIGNAL.fade(opacity));
        }
    }
}

/// The orbit: a 12-dot circle around the centre, clockwise from the top.
const ORBIT: [(usize, usize); 12] = [
    (1, 3),
    (1, 4),
    (1, 5),
    (2, 6),
    (3, 6),
    (4, 6),
    (5, 5),
    (5, 4),
    (5, 3),
    (4, 2),
    (3, 2),
    (2, 2),
];

fn thinking(t: f32) -> Grid {
    let mut g = [[DARK; COLS]; ROWS];
    let n = ORBIT.len() as f32;
    // Eased laps: the comet surges and glides instead of ticking round.
    let lap = 1.3;
    let k = (t / lap).fract();
    let head = ((t / lap).floor() + k - 0.08 * (k * std::f32::consts::TAU).sin()) * n;
    for (i, &(r, c)) in ORBIT.iter().enumerate() {
        let behind = (head - i as f32).rem_euclid(n);
        let b = if behind > n - 1.0 {
            // The next dot warms up as the head arrives.
            (behind - (n - 1.0)).powi(2)
        } else {
            (-behind / 1.3).exp()
        };
        g[r][c].b = 0.08 + 0.92 * b;
    }
    // A still core for the orbit to circle.
    g[3][4].b = 0.35;
    g
}

fn streaming(t: f32) -> Grid {
    let mut g = [[Dot {
        b: 0.0,
        color: tokens::PAPER,
    }; COLS]; ROWS];
    for (r, row) in g.iter_mut().enumerate() {
        for (c, d) in row.iter_mut().enumerate() {
            let (dx, dy) = (c as f32 - CENTER.0, (r as f32 - CENTER.1) * 1.15);
            let dist = (dx * dx + dy * dy).sqrt();
            let wave = 0.5 + 0.5 * (dist * 1.25 - t * 6.5).cos();
            let fall = (1.0 - dist / 5.6).clamp(0.0, 1.0);
            d.b = wave.powi(3) * fall.powf(0.6);
        }
    }
    g
}

/// Smooth ease-in-out (cubic).
fn ease(k: f32) -> f32 {
    if k < 0.5 {
        4.0 * k * k * k
    } else {
        1.0 - (-2.0 * k + 2.0).powi(3) / 2.0
    }
}
