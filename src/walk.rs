//! Walkthroughs: "show me how to do X in this app".
//!
//! The brain (palette or quick ask) calls `start_walkthrough { goal }`; when
//! its turn ends the asking UI calls [`start`]. A walk runs on its own thread:
//! look at the screen (screenshot + UI Automation tree of the foreground app),
//! ask the model for exactly one next step, point at its target (a ring on
//! the overlay) with the instruction in the caption beside it, then wait for
//! the user. A click inside the target, keyboard focus landing on it, or
//! "Next" on the caption completes the step; a click elsewhere only makes it
//! look again, and the screen changing under it re-plans. Esc or "Stop" ends
//! it. The model says when the goal is reached.
//!
//! Loops: every screen the walk shows a step on is remembered (window title +
//! a fingerprint of the element names). Coming back to a screen that was
//! left, or proposing a step already done on the same screen, makes the
//! model re-plan with a warning (a few times at most, then it is trusted).
//!
//! The step logic is a pure state machine ([`Machine`]); the driver feeds it
//! hook, caption and model events. Hooks (`WH_MOUSE_LL` + `WH_KEYBOARD_LL`,
//! observe only, never inject) exist only while a walk runs.

use crate::caption::{self, CaptionEvent, Card, Chrome, Phase as Look};
use crate::capture::{self, Capture, Rect};
use crate::config::Config;
use crate::overlay::{Kind, Shape};
use crate::uia::{self, Element};
use crate::vision::Frame;
use crate::{apeiron, llm, quick};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- router tool

pub const PROMPT: &str = "You can walk the user through doing something in the app on their screen with the `start_walkthrough` tool: screenbuddy then points at one step at a time and waits for them to do it. Use it when they ask how to do something here (how do I, show me how, teach me, walk me through) and it takes clicking through the interface. Plain questions (what is this, why, explain, summarise, a single fact) stay normal answers. Put their goal in their own words in `goal`; after calling it, reply with at most a few words, since the walkthrough does the talking.";

pub const DESC: &str = "Start a step-by-step walkthrough of doing something in the app on screen: screenbuddy points at each step and waits for the user to do it. Use when the user asks how to do something in this app.";

/// Tool result for the brain.
pub const ACK: &str = "The walkthrough starts as soon as this reply ends. Say at most a few words, like 'Let's do it together.'";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "goal": {"type": "string", "description": "What the user wants to get done, in their words, e.g. 'add page numbers to this document'"}
        },
        "required": ["goal"]
    })
}

/// Whether the brain gets `start_walkthrough`.
pub fn router_enabled(cfg: &Config) -> bool {
    cfg.walk.enabled && (cfg.provider != "openai" || cfg.openai.tools)
}

static REQUEST: Mutex<Option<(String, Instant)>> = Mutex::new(None);
/// A request older than this belongs to a turn nobody finished.
const REQUEST_TTL: Duration = Duration::from_secs(180);

/// `start_walkthrough` from the brain: remembered until the turn ends.
pub fn request(input: &Value) -> (String, bool) {
    let goal = input["goal"].as_str().unwrap_or("").trim();
    if goal.is_empty() {
        return ("error: `goal` is empty".into(), true);
    }
    *REQUEST.lock().unwrap() = Some((goal.to_string(), Instant::now()));
    (ACK.into(), false)
}

/// The goal the brain asked for in the turn that just ended, if any.
pub fn take_request() -> Option<String> {
    let (goal, at) = REQUEST.lock().unwrap().take()?;
    (at.elapsed() < REQUEST_TTL).then_some(goal)
}

// ---------------------------------------------------------------- control

enum Msg {
    Caption(CaptionEvent),
    Click(i32, i32),
    Esc,
    Heard(String),
    Planned {
        round: u64,
        result: Result<(Plan, u64), String>,
        conv: llm::Conversation,
    },
    Stop,
}

struct Live {
    id: u64,
    tx: Sender<Msg>,
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);
static ASKING: AtomicBool = AtomicBool::new(false);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn send_live(m: Msg) -> bool {
    LIVE.lock()
        .unwrap()
        .as_ref()
        .is_some_and(|l| l.tx.send(m).is_ok())
}

/// A walkthrough is running.
pub fn active() -> bool {
    LIVE.lock().unwrap().is_some()
}

/// The running walkthrough waits for an answer to its question.
pub fn asking() -> bool {
    active() && ASKING.load(Ordering::Relaxed)
}

/// Text the user said (hold-Alt) or typed (palette). With `any`, it goes to
/// the walk whatever it is doing (an answer, or a remark that re-plans);
/// otherwise only as an answer. Empty text just puts the card back (after a
/// voice caption covered it). True if the walk took it.
pub fn hear(text: &str, any: bool) -> bool {
    if !any && !asking() {
        return false;
    }
    send_live(Msg::Heard(text.trim().to_string()))
}

/// Ends the running walkthrough, if any.
pub fn stop() {
    send_live(Msg::Stop);
}

/// Starts a walkthrough for `goal`, replacing one that is running.
pub fn start(goal: String, cfg: &Config) {
    stop();
    forward_captions();
    let (tx, rx) = channel();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    *LIVE.lock().unwrap() = Some(Live { id, tx: tx.clone() });
    let cfg = cfg.clone();
    let spawned = std::thread::Builder::new()
        .name("screenbuddy-walk".into())
        .spawn(move || {
            run(goal, &cfg, tx, rx);
            ASKING.store(false, Ordering::Relaxed);
            let mut l = LIVE.lock().unwrap();
            if l.as_ref().is_some_and(|l| l.id == id) {
                *l = None;
            }
        });
    if spawned.is_err() {
        *LIVE.lock().unwrap() = None;
    }
}

/// Caption clicks reach whichever walk is running (one forwarder, ever).
fn forward_captions() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        let (tx, rx) = channel();
        caption::subscribe(tx);
        let _ = std::thread::Builder::new()
            .name("screenbuddy-walk-captions".into())
            .spawn(move || {
                for ev in rx {
                    send_live(Msg::Caption(ev));
                }
            });
    });
}

// ---------------------------------------------------------------- model types

/// What a step points at, in screen px.
#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub name: String,
    pub automation_id: String,
    pub control: String,
    pub rect: Rect,
    /// The model gave a point, not a box.
    pub point: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Step {
    /// One short instruction (markdown-lite).
    pub say: String,
    /// None for keyboard steps: the user presses Next.
    pub target: Option<Target>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    Step(Step),
    /// One short question about where the user is starting from.
    Ask(String),
    /// Bring this (Apeiron) window on screen, then look again.
    Focus(isize),
    Done(String),
}

/// What keyboard focus is on.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FocusInfo {
    pub name: String,
    pub automation_id: String,
    pub control: String,
}

/// An Apeiron window the model may switch to.
#[derive(Clone, Debug, PartialEq)]
pub struct Win {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
    pub on_screen: bool,
}

/// What the model is shown besides the image: how screenshot pixels map to
/// the screen, the element list (index + 1 = element id) and windows.
#[derive(Clone, Debug)]
pub struct View {
    pub frame: Frame,
    pub elements: Vec<Element>,
    pub windows: Vec<Win>,
}

/// Elements listed to the model.
const MAX_LIST: usize = 150;

impl View {
    fn describe(&self) -> String {
        let mut s = String::new();
        let mut lines = Vec::new();
        for (i, e) in self.elements.iter().enumerate() {
            let Some((x, y, w, h)) = self.frame.rect_in(e.rect) else {
                continue;
            };
            if w < 2.0 || h < 2.0 {
                continue;
            }
            let id = if e.automation_id.is_empty() {
                String::new()
            } else {
                format!(" [{}]", e.automation_id)
            };
            lines.push(format!(
                "e{} {} \"{}\"{id} {},{},{},{}{}",
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
        if !lines.is_empty() {
            s.push_str("\n[UI elements: id type \"name\" [automation id] x,y,w,h in screenshot px; * = keyboard focus]\n");
            s.push_str(&lines.join("\n"));
        }
        if !self.windows.is_empty() {
            s.push_str("\n[Other windows (switch to one with kind switch): id \"title\" (app)]\n");
            let ws: Vec<String> = self
                .windows
                .iter()
                .enumerate()
                .map(|(i, w)| {
                    format!(
                        "w{} \"{}\" ({}){}",
                        i + 1,
                        w.title,
                        w.exe,
                        if w.on_screen { "" } else { " off screen" }
                    )
                })
                .collect();
            s.push_str(&ws.join("\n"));
        }
        s
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

/// Identity of a screen state: window title plus the set of element names.
/// Positions are left out so scrolling a little or moving the window keeps it.
pub fn fingerprint(title: &str, els: &[Element]) -> u64 {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut keys: Vec<(&str, &str)> = els
        .iter()
        .filter(|e| !e.name.is_empty())
        .map(|e| (e.kind, e.name.as_str()))
        .collect();
    keys.sort_unstable();
    keys.dedup();
    let mut h = DefaultHasher::new();
    title.hash(&mut h);
    keys.hash(&mut h);
    h.finish()
}

// ---------------------------------------------------------------- planner

const PLANNER: &str = r#"You are guiding the user through a task in the app on their screen, one step at a time. You never act yourself: the user does every step, and screenbuddy points at what you name and waits for them. Reply with exactly one JSON object and nothing else, one of:
{"kind":"step","say":"Click **Insert**","element":"e12"}
{"kind":"step","say":"Click the **+** button","x":410,"y":88,"w":32,"h":32}
{"kind":"step","say":"Press **Ctrl+S**"}
{"kind":"switch","window":"w2","say":"Switch to Excel"}
{"kind":"ask","question":"Do you already have the file open?"}
{"kind":"done","say":"That's it: page numbers are on."}
Rules:
- Exactly ONE next action: one click, key press or short bit of typing. `say` is one short imperative sentence (at most 12 words) with the control's name in **bold**.
- Point at the control: use its id from the UI element list when it is there, otherwise a box in screenshot pixels (x,y = top-left). Leave the target out only for keyboard steps.
- Start from what is on screen now. If you can't tell where the user is starting from (they may already have part of it), ask one short question first; never ask about what the screenshot shows.
- Steps already done are listed. Don't repeat one unless the screen shows it didn't work; if you are told you are going in circles, take a different way.
- If the goal needs a window from the window list that is not on screen, reply with switch.
- When the screen shows the goal is reached, reply done with one short sentence."#;

/// The brain config for planning: no tools, the planner prompt.
fn planner_cfg(cfg: &Config) -> Config {
    let mut c = cfg.clone();
    c.drawing.enabled = false;
    c.agents.enabled = false;
    c.walk.enabled = false;
    c.vision.ui_tree = false; // our own element list goes in the question
    c.vision.zoom = false;
    c.vision.guide = false;
    c.vision.web_search = false;
    c.agent_desktop.enabled = false;
    c.extra_instructions = PLANNER.into();
    c
}

/// The first `{ ... }` object in the model's reply.
fn json_in(text: &str) -> Option<Value> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    let mut in_str = false;
    let mut esc = false;
    for (i, c) in text[start..].char_indices() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return serde_json::from_str(&text[start..start + i + 1]).ok();
                }
            }
            _ => {}
        }
    }
    None
}

/// Smallest pointer box around a point, in screen px.
const POINT_BOX: i32 = 28;

/// Turns the model's reply into a plan, resolving targets to screen px.
pub fn parse_plan(text: &str, view: &View) -> Result<Plan, String> {
    let Some(v) = json_in(text) else {
        // Prose instead of JSON: show it as a step the user confirms.
        let t = text.trim();
        if t.is_empty() {
            return Err("the model gave no step".into());
        }
        return Ok(Plan::Step(Step {
            say: t.chars().take(240).collect(),
            target: None,
        }));
    };
    let say = v["say"].as_str().unwrap_or("").trim().to_string();
    match v["kind"].as_str().unwrap_or("step") {
        "done" => Ok(Plan::Done(if say.is_empty() {
            "Done.".into()
        } else {
            say
        })),
        "ask" => {
            let q = v["question"]
                .as_str()
                .map(str::trim)
                .unwrap_or(&say)
                .to_string();
            if q.is_empty() {
                return Err("the model asked an empty question".into());
            }
            Ok(Plan::Ask(q))
        }
        "switch" => {
            let id = v["window"].as_str().unwrap_or("");
            id.trim_start_matches(['w', 'W'])
                .parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| view.windows.get(n))
                .map(|w| Plan::Focus(w.hwnd))
                .ok_or_else(|| format!("unknown window '{id}'"))
        }
        _ => {
            if say.is_empty() {
                return Err("the model's step has no instruction".into());
            }
            Ok(Plan::Step(Step {
                say,
                target: target_of(&v, view),
            }))
        }
    }
}

fn target_of(v: &Value, view: &View) -> Option<Target> {
    if let Some(id) = v["element"].as_str() {
        let el = id
            .trim_start_matches(['e', 'E'])
            .parse::<usize>()
            .ok()
            .and_then(|n| n.checked_sub(1))
            .and_then(|n| view.elements.get(n));
        if let Some(e) = el {
            return Some(Target {
                name: e.name.clone(),
                automation_id: e.automation_id.clone(),
                control: e.kind.to_string(),
                rect: e.rect,
                point: false,
            });
        }
    }
    let n = |k: &str| v[k].as_f64().map(|x| x as f32);
    let (x, y) = (n("x")?, n("y")?);
    let f = view.frame;
    let (sx, sy) = f.to_screen(x, y);
    let rect = match (n("w"), n("h")) {
        (Some(w), Some(h)) if w > 0.0 && h > 0.0 => {
            let (ex, ey) = f.to_screen(x + w, y + h);
            Rect {
                x: sx.round() as i32,
                y: sy.round() as i32,
                w: (ex - sx).round() as i32,
                h: (ey - sy).round() as i32,
            }
        }
        _ => Rect {
            x: sx.round() as i32 - POINT_BOX / 2,
            y: sy.round() as i32 - POINT_BOX / 2,
            w: POINT_BOX,
            h: POINT_BOX,
        },
    };
    let rect = uia::intersect(rect, f.src)?;
    // A box that matches a listed element takes its identity (for focus).
    let same = view.elements.iter().find(|e| e.rect == rect);
    Some(Target {
        name: same.map(|e| e.name.clone()).unwrap_or_default(),
        automation_id: same.map(|e| e.automation_id.clone()).unwrap_or_default(),
        control: same.map(|e| e.kind.to_string()).unwrap_or_default(),
        rect,
        point: n("w").is_none(),
    })
}

// ---------------------------------------------------------------- state machine

/// How a step got done.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum How {
    Clicked,
    Focused,
    Next,
    /// The screen changed while waiting (maybe done another way).
    Changed,
}

impl How {
    fn describe(self) -> &'static str {
        match self {
            How::Clicked => "the user clicked it",
            How::Focused => "keyboard focus moved to it",
            How::Next => "the user said it's done",
            How::Changed => {
                "the screen changed without a click on the target; check whether it happened"
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Done {
    pub step: Step,
    pub how: How,
    /// The screen the step was shown on.
    pub fp: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Phase {
    Planning,
    Waiting { step: Step, fp: u64 },
    Asking(String),
    Finished,
    Stopped,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Input {
    /// A plan for the screen with this fingerprint.
    Planned(Plan, u64),
    /// Mouse button released at this screen point (not on screenbuddy).
    Click(i32, i32),
    Focus(FocusInfo),
    /// The screen now has this fingerprint (differs from the step's).
    Changed(u64),
    Next,
    Stop,
    Heard(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Look at the screen and ask for the next step.
    Plan,
    /// Point at the step; `n` counts from 1.
    Show(Step, usize),
    Ask(String),
    Focus(isize),
    /// Look again shortly: did that click change anything?
    Recheck,
    Finish(String),
    Fail(String),
    Stopped,
}

/// Re-plans per step when the model would go in circles, before trusting it.
const MAX_REPLANS: u32 = 2;
const MAX_ASKS: u32 = 1;
const MAX_FOCUS: u32 = 3;
const MAX_STEPS: usize = 30;
/// Clicks this close outside the target still count (px).
const SLOP: i32 = 6;

#[derive(Clone, Debug)]
pub struct Machine {
    pub goal: String,
    pub done: Vec<Done>,
    /// Screens a step was shown on, in order.
    pub visits: Vec<u64>,
    pub answers: Vec<(String, String)>,
    /// What the user said mid-walk.
    pub remarks: Vec<String>,
    pub phase: Phase,
    /// Extra word for the next plan (a loop warning, a rule reminder).
    pub note: Option<String>,
    replans: u32,
    asks: u32,
    focuses: u32,
}

impl Machine {
    pub fn new(goal: &str) -> Self {
        Self {
            goal: goal.trim().to_string(),
            done: Vec::new(),
            visits: Vec::new(),
            answers: Vec::new(),
            remarks: Vec::new(),
            phase: Phase::Planning,
            note: None,
            replans: 0,
            asks: 0,
            focuses: 0,
        }
    }

    pub fn ended(&self) -> bool {
        matches!(self.phase, Phase::Finished | Phase::Stopped)
    }

    /// This screen was shown before, then left: coming back is a cycle.
    pub fn cycle(&self, fp: u64) -> bool {
        self.visits
            .iter()
            .position(|&f| f == fp)
            .is_some_and(|i| self.visits[i + 1..].iter().any(|&f| f != fp))
    }

    /// The model proposes a step it already gave (and the user did) on this screen.
    fn again(&self, fp: u64, s: &Step) -> bool {
        self.done
            .iter()
            .any(|d| d.fp == fp && same_step(&d.step, s))
    }

    pub fn feed(&mut self, input: Input) -> Vec<Effect> {
        if self.ended() {
            return Vec::new();
        }
        match input {
            Input::Stop => {
                self.phase = Phase::Stopped;
                vec![Effect::Stopped]
            }
            Input::Failed(e) => {
                self.phase = Phase::Finished;
                vec![Effect::Fail(e)]
            }
            Input::Planned(plan, fp) if self.phase == Phase::Planning => self.planned(plan, fp),
            Input::Planned(..) => Vec::new(),
            Input::Click(x, y) => match &self.phase {
                Phase::Waiting { step, .. } => match &step.target {
                    Some(t) if inside(t.rect, x, y, SLOP) => self.complete(How::Clicked),
                    // Elsewhere: don't hold on to the pointer, look again.
                    _ => vec![Effect::Recheck],
                },
                _ => Vec::new(),
            },
            Input::Focus(f) => match &self.phase {
                Phase::Waiting {
                    step: Step {
                        target: Some(t), ..
                    },
                    ..
                } if focus_matches(t, &f) => self.complete(How::Focused),
                _ => Vec::new(),
            },
            Input::Changed(now) => match &self.phase {
                Phase::Waiting { fp, .. } if *fp != now => self.complete(How::Changed),
                _ => Vec::new(),
            },
            Input::Next => match &self.phase {
                Phase::Waiting { .. } => self.complete(How::Next),
                Phase::Asking(q) => {
                    self.answers.push((
                        q.clone(),
                        "(skipped; assume the usual starting point)".into(),
                    ));
                    self.phase = Phase::Planning;
                    vec![Effect::Plan]
                }
                _ => Vec::new(),
            },
            Input::Heard(t) => match &self.phase {
                Phase::Asking(q) => {
                    self.answers.push((q.clone(), t));
                    self.phase = Phase::Planning;
                    vec![Effect::Plan]
                }
                Phase::Waiting { .. } => {
                    self.remarks.push(t);
                    self.phase = Phase::Planning;
                    vec![Effect::Plan]
                }
                // Picked up by the plan after the one running.
                _ => {
                    self.remarks.push(t);
                    Vec::new()
                }
            },
        }
    }

    fn planned(&mut self, plan: Plan, fp: u64) -> Vec<Effect> {
        match plan {
            Plan::Done(say) => {
                self.phase = Phase::Finished;
                vec![Effect::Finish(say)]
            }
            Plan::Ask(_) if self.asks >= MAX_ASKS && self.replans < MAX_REPLANS => {
                self.replans += 1;
                self.note = Some("Don't ask another question: assume the most likely starting point and give the next step.".into());
                vec![Effect::Plan]
            }
            Plan::Ask(q) => {
                self.asks += 1;
                self.phase = Phase::Asking(q.clone());
                vec![Effect::Ask(q)]
            }
            Plan::Focus(_) if self.focuses >= MAX_FOCUS => {
                self.note = Some(
                    "Switching windows didn't work; guide the user to the window instead.".into(),
                );
                vec![Effect::Plan]
            }
            Plan::Focus(h) => {
                self.focuses += 1;
                vec![Effect::Focus(h), Effect::Plan]
            }
            Plan::Step(s) => {
                if self.done.len() >= MAX_STEPS {
                    self.phase = Phase::Finished;
                    return vec![Effect::Finish(format!(
                        "That's {MAX_STEPS} steps, so I'll stop here. Ask again to carry on."
                    ))];
                }
                if (self.cycle(fp) || self.again(fp, &s)) && self.replans < MAX_REPLANS {
                    self.replans += 1;
                    self.note = Some(format!(
                        "Your last suggestion (\"{}\") would repeat a step the user already did here, or this screen is one they already left. You are going in circles: take a different way to the goal, or reply done if it is reached.",
                        s.say
                    ));
                    return vec![Effect::Plan];
                }
                self.visits.push(fp);
                let n = self.done.len() + 1;
                self.phase = Phase::Waiting {
                    step: s.clone(),
                    fp,
                };
                vec![Effect::Show(s, n)]
            }
        }
    }

    fn complete(&mut self, how: How) -> Vec<Effect> {
        let Phase::Waiting { step, fp } = std::mem::replace(&mut self.phase, Phase::Planning)
        else {
            return Vec::new();
        };
        self.done.push(Done { step, how, fp });
        self.replans = 0;
        vec![Effect::Plan]
    }

    /// The question for the planner, given what is on screen now.
    pub fn prompt(&self, title: &str, fp: u64, view: &View) -> String {
        let mut s = format!("Goal: {}\n", self.goal);
        if !self.answers.is_empty() {
            s.push_str("\nThe user answered:\n");
            for (q, a) in &self.answers {
                s.push_str(&format!("- {q} -> {a}\n"));
            }
        }
        if !self.remarks.is_empty() {
            s.push_str("\nThe user said during the walkthrough (most recent last):\n");
            for r in &self.remarks {
                s.push_str(&format!("- {r}\n"));
            }
        }
        if self.done.is_empty() {
            s.push_str("\nNo steps done yet.\n");
        } else {
            s.push_str("\nSteps done so far:\n");
            for (i, d) in self.done.iter().enumerate() {
                s.push_str(&format!(
                    "{}. {} ({})\n",
                    i + 1,
                    d.step.say,
                    d.how.describe()
                ));
            }
        }
        if self.cycle(fp) {
            s.push_str("\nWarning: this screen looks like one the user already left earlier in this walkthrough. Don't send them round in circles.\n");
        }
        if let Some(n) = &self.note {
            s.push_str(&format!("\nNote: {n}\n"));
        }
        s.push_str(&format!("\nForeground window: \"{title}\"\n"));
        s.push_str(&view.describe());
        s.push_str("\n\nReply with the JSON for the one next step.");
        s
    }
}

fn inside(r: Rect, x: i32, y: i32, slop: i32) -> bool {
    x >= r.x - slop && x < r.x + r.w + slop && y >= r.y - slop && y < r.y + r.h + slop
}

fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn same_step(a: &Step, b: &Step) -> bool {
    if norm(&a.say) == norm(&b.say) {
        return true;
    }
    match (&a.target, &b.target) {
        (Some(x), Some(y)) => {
            (!x.automation_id.is_empty()
                && x.automation_id == y.automation_id
                && x.control == y.control)
                || (!x.name.is_empty() && x.name == y.name && x.control == y.control)
        }
        _ => false,
    }
}

fn focus_matches(t: &Target, f: &FocusInfo) -> bool {
    (!t.automation_id.is_empty() && t.automation_id == f.automation_id)
        || (!t.name.is_empty() && t.name == f.name && t.control == f.control)
}

// ---------------------------------------------------------------- cards

fn step_card(step: &Step, n: usize, scale: f32) -> Card {
    Card {
        phase: Look::Step,
        label: format!("Step {n}"),
        body: step.say.clone(),
        chrome: Chrome::Step {
            next: "NEXT  \u{203a}",
        },
        near: step.target.as_ref().map(|t| t.rect),
        pointer: step.target.as_ref().map(|t| pointer(t, scale)),
    }
}

/// The ring around the target, in the one accent colour.
pub fn pointer(t: &Target, scale: f32) -> Shape {
    let pad = 4.0 * scale;
    let r = t.rect;
    let (cx, cy) = (r.x as f32 + r.w as f32 / 2.0, r.y as f32 + r.h as f32 / 2.0);
    Shape {
        kind: if t.point { Kind::Circle } else { Kind::Box },
        x: if t.point { cx } else { r.x as f32 - pad },
        y: if t.point { cy } else { r.y as f32 - pad },
        x2: cx,
        y2: cy,
        w: r.w as f32 + pad * 2.0,
        h: r.h as f32 + pad * 2.0,
        r: 18.0 * scale,
        label: None,
        color: caption::SIGNAL,
        diagram: None,
        hidden: false,
        anchor: None,
        away: false,
    }
}

fn thinking_card(m: &Machine) -> Card {
    Card {
        phase: Look::Thinking,
        label: if m.done.is_empty() && m.answers.is_empty() {
            format!("Show me how \u{00b7} {}", m.goal)
        } else {
            format!("Step {}", m.done.len() + 1)
        },
        body: String::new(),
        chrome: Chrome::Step { next: "" },
        near: None,
        pointer: None,
    }
}

fn ask_card(q: &str) -> Card {
    Card {
        phase: Look::Ask,
        label: "Quick question".into(),
        body: format!("{q}\n\n_Hold Alt and say it, or double-tap Alt to type._"),
        chrome: Chrome::Step {
            next: "SKIP  \u{203a}",
        },
        near: None,
        pointer: None,
    }
}

fn end_card(phase: Look, label: &str, body: String) -> Card {
    Card {
        phase,
        label: label.into(),
        body,
        chrome: Chrome::Plain,
        near: None,
        pointer: None,
    }
}

// ---------------------------------------------------------------- driver

/// Screen state probed cheaply while waiting.
#[derive(Clone, Debug, PartialEq, Default)]
struct Probe {
    fg: isize,
    title: String,
}

/// Wait after a step before looking (menus open, dialogs animate).
const SETTLE: Duration = Duration::from_millis(600);
/// Wait after an off-target click before checking whether it changed anything.
const RECHECK: Duration = Duration::from_millis(700);
const POLL: Duration = Duration::from_millis(400);
const PLAN_TIMEOUT: Duration = Duration::from_secs(150);

struct Driver {
    cfg: Config,
    m: Machine,
    conv: Option<llm::Conversation>,
    tx: Sender<Msg>,
    round: u64,
    cancel: Arc<AtomicBool>,
    last_card: Option<Card>,
    probe: Probe,
    focus: Option<FocusReader>,
    last_focus: Option<FocusInfo>,
    recheck_at: Option<Instant>,
    planned_once: bool,
}

fn run(goal: String, cfg: &Config, tx: Sender<Msg>, rx: Receiver<Msg>) {
    let _hooks = Hooks::start(tx.clone());
    // Started from the palette: let it get out of the way first.
    let t = Instant::now();
    while crate::win::ui_open() && t.elapsed() < Duration::from_secs(10) {
        std::thread::sleep(Duration::from_millis(50));
    }
    crate::config::log(&format!("walkthrough: {goal}"));
    let mut d = Driver {
        cfg: planner_cfg(cfg),
        m: Machine::new(&goal),
        conv: Some(Default::default()),
        tx,
        round: 0,
        cancel: Arc::new(AtomicBool::new(false)),
        last_card: None,
        probe: Probe::default(),
        focus: FocusReader::new(),
        last_focus: None,
        recheck_at: None,
        planned_once: false,
    };
    llm::prewarm(&d.cfg, d.conv.as_ref().unwrap());
    let mut queue: VecDeque<Effect> = VecDeque::from([Effect::Plan]);
    loop {
        while let Some(e) = queue.pop_front() {
            d.apply(e);
        }
        if d.m.ended() {
            break;
        }
        let wait = match d.recheck_at {
            Some(at) => at.saturating_duration_since(Instant::now()).min(POLL),
            None => POLL,
        };
        let input = match rx.recv_timeout(wait) {
            Ok(Msg::Caption(CaptionEvent::Next)) => Some(Input::Next),
            Ok(Msg::Caption(CaptionEvent::Stop) | Msg::Esc | Msg::Stop) => Some(Input::Stop),
            Ok(Msg::Caption(_)) => None,
            Ok(Msg::Click(x, y)) => (!ours_at(x, y)).then_some(Input::Click(x, y)),
            Ok(Msg::Heard(t)) if t.is_empty() => {
                d.reshow();
                None
            }
            Ok(Msg::Heard(t)) => Some(Input::Heard(t)),
            Ok(Msg::Planned {
                round,
                result,
                conv,
            }) => {
                d.conv = Some(conv);
                (round == d.round).then(|| match result {
                    Ok((p, fp)) => Input::Planned(p, fp),
                    Err(e) => Input::Failed(e),
                })
            }
            Err(RecvTimeoutError::Timeout) => d.poll(),
            Err(RecvTimeoutError::Disconnected) => Some(Input::Stop),
        };
        if let Some(i) = input {
            queue.extend(d.m.feed(i));
        }
    }
    d.cancel.store(true, Ordering::Relaxed);
}

impl Driver {
    fn card(&mut self, c: Card) {
        self.last_card = Some(c.clone());
        caption::send(caption::Cmd::Walk(c));
    }

    fn reshow(&mut self) {
        if let Some(c) = self.last_card.clone() {
            caption::send(caption::Cmd::Walk(c));
        }
    }

    fn apply(&mut self, e: Effect) {
        match e {
            Effect::Plan => self.plan(),
            Effect::Show(step, n) => {
                ASKING.store(false, Ordering::Relaxed);
                let (x, y) = step.target.as_ref().map_or_else(capture::cursor_pos, |t| {
                    (t.rect.x + t.rect.w / 2, t.rect.y + t.rect.h / 2)
                });
                let scale = capture::monitor_at(x, y).scale;
                self.card(step_card(&step, n, scale));
                quick::status(quick::Status::Idle);
                self.probe = probe();
                self.last_focus = self.focus.as_ref().and_then(FocusReader::get);
                self.recheck_at = None;
            }
            Effect::Ask(q) => {
                ASKING.store(true, Ordering::Relaxed);
                self.card(ask_card(&q));
                quick::status(quick::Status::Idle);
            }
            Effect::Focus(h) => {
                if let Err(e) = apeiron::focus_window(h) {
                    crate::config::log(&format!("walkthrough: focus_window failed: {e}"));
                }
            }
            Effect::Recheck => self.recheck_at = Some(Instant::now() + RECHECK),
            Effect::Finish(say) => {
                self.card(end_card(Look::Done, "Done", say));
                quick::status(quick::Status::Done);
            }
            Effect::Fail(e) => {
                crate::config::log(&format!("walkthrough failed: {e}"));
                self.card(end_card(Look::Error, "Walkthrough stopped", short(&e)));
                quick::status(quick::Status::Error(e));
            }
            Effect::Stopped => {
                caption::send(caption::Cmd::Hide);
                quick::status(quick::Status::Idle);
            }
        }
    }

    /// Looks at the screen and asks for the next step on a worker thread, so
    /// Stop and Esc stay instant.
    fn plan(&mut self) {
        ASKING.store(false, Ordering::Relaxed);
        self.recheck_at = None;
        self.cancel.store(true, Ordering::Relaxed);
        self.cancel = Arc::new(AtomicBool::new(false));
        self.round += 1;
        self.card(thinking_card(&self.m));
        quick::status(quick::Status::Thinking);
        let m = self.m.clone();
        self.m.note = None;
        let settle = if self.planned_once {
            SETTLE
        } else {
            Duration::ZERO
        };
        self.planned_once = true;
        let (cfg, round, tx, cancel) = (
            self.cfg.clone(),
            self.round,
            self.tx.clone(),
            self.cancel.clone(),
        );
        // The Claude CLI keeps its warm process (and session) across steps;
        // the HTTP providers start clean each step.
        let conv = match self.conv.take() {
            Some(c) if cfg.provider == "claude-cli" => c,
            _ => Default::default(),
        };
        std::thread::spawn(move || {
            std::thread::sleep(settle);
            let (result, conv) = match Snapshot::take() {
                Ok(snap) => {
                    let q = m.prompt(&snap.title, snap.fp, &snap.view);
                    let (r, conv) = ask_model(&cfg, conv, q, &snap.cap, &cancel);
                    (
                        r.and_then(|t| parse_plan(&t, &snap.view))
                            .map(|p| (p, snap.fp)),
                        conv,
                    )
                }
                Err(e) => (Err(e), conv),
            };
            let _ = tx.send(Msg::Planned {
                round,
                result,
                conv,
            });
        });
    }

    /// While waiting: notices focus landing on the target and the screen
    /// changing under the step.
    fn poll(&mut self) -> Option<Input> {
        let Phase::Waiting { fp, .. } = &self.m.phase else {
            return None;
        };
        let fp = *fp;
        if let Some(f) = self.focus.as_ref().and_then(FocusReader::get) {
            if self.last_focus.as_ref() != Some(&f) {
                self.last_focus = Some(f.clone());
                self.recheck_at.get_or_insert(Instant::now() + RECHECK);
                return Some(Input::Focus(f));
            }
        }
        let p = probe();
        if p != self.probe {
            self.probe = p;
            self.recheck_at.get_or_insert(Instant::now() + RECHECK);
        }
        if self.recheck_at.is_some_and(|at| Instant::now() >= at) {
            self.recheck_at = None;
            let now = foreground_fingerprint();
            if now != 0 && now != fp {
                return Some(Input::Changed(now));
            }
        }
        None
    }
}

fn short(e: &str) -> String {
    let e = e.strip_prefix(llm::INTERRUPTED).unwrap_or(e);
    e.lines().next().unwrap_or(e).chars().take(160).collect()
}

/// Sends one planning question and collects the reply text.
fn ask_model(
    cfg: &Config,
    conv: llm::Conversation,
    question: String,
    cap: &Capture,
    cancel: &Arc<AtomicBool>,
) -> (Result<String, String>, llm::Conversation) {
    let (tx, rx) = channel();
    let keep = conv.clone();
    llm::spawn(
        llm::Turn {
            cfg: cfg.clone(),
            conv,
            question,
            image: Some(llm::Image {
                jpeg: Arc::new(cap.jpeg.clone()),
                w: cap.img_w,
                h: cap.img_h,
            }),
            attachments: Vec::new(),
            cancel: cancel.clone(),
        },
        llm::Out::new(tx, egui::Context::default()),
    );
    let deadline = Instant::now() + PLAN_TIMEOUT;
    let mut text = String::new();
    let mut err = None;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(llm::Ev::Delta(d)) => text.push_str(&d),
            Ok(llm::Ev::Error(e)) => err = Some(e),
            Ok(llm::Ev::Draw(_, reply)) => {
                let _ = reply.send(Err("drawing is off during a walkthrough".into()));
            }
            Ok(llm::Ev::Approve(_, _, reply)) => {
                let _ = reply.send(false);
            }
            Ok(llm::Ev::Status(_) | llm::Ev::Spawned(_)) => {}
            Ok(llm::Ev::Done(c)) => {
                return match err {
                    Some(e) if text.trim().is_empty() => (Err(e), c),
                    _ => (Ok(text), c),
                };
            }
            Err(_) => {
                cancel.store(true, Ordering::Relaxed);
                return (Err("the model took too long to answer".into()), keep);
            }
        }
    }
}

// ---------------------------------------------------------------- screen

/// What the planner looks at for one step.
struct Snapshot {
    cap: Capture,
    title: String,
    fp: u64,
    view: View,
}

impl Snapshot {
    fn take() -> Result<Self, String> {
        let cap = capture::grab_monitor_under_cursor()
            .and_then(|s| capture::encode(&s, None))
            .map_err(|e| format!("screen capture failed: {e}"))?;
        let roots = uia::target_windows(cap.src, cap.cursor);
        let elements = select(uia::snapshot(roots, cap.src, budget()), MAX_LIST);
        let (title, fp) = foreground_state();
        let windows = apeiron::get()
            .filter(|l| l.canvas)
            .map(|l| {
                l.windows
                    .iter()
                    .filter(|w| !w.title.trim().is_empty())
                    .map(|w| Win {
                        hwnd: w.hwnd,
                        title: w.title.clone(),
                        exe: w.exe.clone(),
                        on_screen: w.visible(),
                    })
                    .take(24)
                    .collect()
            })
            .unwrap_or_default();
        let view = View {
            frame: Frame::of(&cap),
            elements,
            windows,
        };
        Ok(Self {
            cap,
            title,
            fp,
            view,
        })
    }
}

/// A walkthrough can afford a deeper look than a quick answer.
fn budget() -> Duration {
    uia::budget().max(Duration::from_millis(400))
}

fn foreground() -> windows::Win32::Foundation::HWND {
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }
}

fn title_of(h: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 512];
    let n = unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

fn probe() -> Probe {
    let h = foreground();
    Probe {
        fg: h.0 as isize,
        title: title_of(h),
    }
}

/// Title and fingerprint of the foreground window only, so the cursor
/// wandering over other windows doesn't change it.
fn foreground_state() -> (String, u64) {
    use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
    let h = foreground();
    let title = title_of(h);
    let mut r = windows::Win32::Foundation::RECT::default();
    if h.is_invalid() || unsafe { GetWindowRect(h, &mut r) }.is_err() {
        return (title, 0);
    }
    let els = uia::snapshot(vec![h], Rect::from_win(r), budget());
    let fp = fingerprint(&title, &els);
    (title, fp)
}

fn foreground_fingerprint() -> u64 {
    foreground_state().1
}

/// The point is on one of screenbuddy's own windows (caption, island).
fn ours_at(x: i32, y: i32) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowThreadProcessId, WindowFromPoint};
    unsafe {
        let h = WindowFromPoint(POINT { x, y });
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        pid == std::process::id()
    }
}

/// Reads what keyboard focus is on.
struct FocusReader {
    ua: windows::Win32::UI::Accessibility::IUIAutomation,
}

impl FocusReader {
    fn new() -> Option<Self> {
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        };
        use windows::Win32::UI::Accessibility::CUIAutomation;
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .ok()
                .map(|ua| Self { ua })
        }
    }

    fn get(&self) -> Option<FocusInfo> {
        unsafe {
            let el = self.ua.GetFocusedElement().ok()?;
            Some(FocusInfo {
                name: el
                    .CurrentName()
                    .map(|b| b.to_string())
                    .unwrap_or_default()
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
                automation_id: el
                    .CurrentAutomationId()
                    .map(|b| b.to_string())
                    .unwrap_or_default(),
                control: uia::control_type(el.CurrentControlType().map(|c| c.0).unwrap_or(0))
                    .to_string(),
            })
        }
    }
}

// ---------------------------------------------------------------- hooks

/// The walk the hooks report to, tagged so an ending walk can't unhook the next.
static HOOK_TX: Mutex<Option<(u64, Sender<Msg>)>> = Mutex::new(None);
static HOOK_GEN: AtomicU64 = AtomicU64::new(1);

/// Low-level mouse + keyboard hooks for one walk (observe only).
struct Hooks {
    thread: u32,
    round: u64,
}

impl Hooks {
    fn start(tx: Sender<Msg>) -> Option<Self> {
        let round = HOOK_GEN.fetch_add(1, Ordering::Relaxed);
        *HOOK_TX.lock().unwrap() = Some((round, tx));
        let (ready, got) = channel();
        std::thread::Builder::new()
            .name("screenbuddy-walk-hooks".into())
            .spawn(move || hook_thread(ready))
            .ok()?;
        let thread = got.recv_timeout(Duration::from_secs(2)).ok()?;
        Some(Self { thread, round })
    }
}

impl Drop for Hooks {
    fn drop(&mut self) {
        use windows::Win32::Foundation::{LPARAM, WPARAM};
        use windows::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        let mut g = HOOK_TX.lock().unwrap();
        if g.as_ref().is_some_and(|(n, _)| *n == self.round) {
            *g = None;
        }
    }
}

fn hook_thread(ready: Sender<u32>) {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        let mut msg = MSG::default();
        let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
        let module = GetModuleHandleW(None).ok().map(|m| HINSTANCE(m.0));
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0).ok();
        let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), module, 0).ok();
        let _ = ready.send(windows::Win32::System::Threading::GetCurrentThreadId());
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        for h in [mouse, kb].into_iter().flatten() {
            let _ = UnhookWindowsHookEx(h);
        }
    }
}

fn hook_send(m: Msg) {
    if let Some((_, tx)) = HOOK_TX.lock().unwrap().as_ref() {
        let _ = tx.send(m);
    }
}

unsafe extern "system" fn mouse_proc(
    code: i32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        if code == HC_ACTION as i32 && matches!(wp.0 as u32, WM_LBUTTONUP | WM_RBUTTONUP) {
            let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
            if m.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0 {
                hook_send(Msg::Click(m.pt.x, m.pt.y));
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn kb_proc(
    code: i32,
    wp: windows::Win32::Foundation::WPARAM,
    lp: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
    use windows::Win32::UI::WindowsAndMessaging::*;
    unsafe {
        if code == HC_ACTION as i32 && wp.0 as u32 == WM_KEYDOWN {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let injected = k.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0;
            if !injected && k.vkCode == VK_ESCAPE.0 as u32 {
                hook_send(Msg::Esc);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

// ---------------------------------------------------------------- headless

/// `screenbuddy --walk "goal" [--provider p]`: plans the next step for the
/// screen as it is now and prints it. Nothing is clicked or drawn.
pub fn headless(goal: &str, provider: Option<String>) -> i32 {
    crate::win::set_dpi_awareness();
    let (mut cfg, _) = crate::config::load();
    if let Some(p) = provider {
        cfg.provider = p;
    }
    if goal.trim().is_empty() {
        eprintln!("usage: screenbuddy --walk \"goal\" [--provider p]");
        return 1;
    }
    let snap = match Snapshot::take() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            return 1;
        }
    };
    let m = Machine::new(goal);
    let q = m.prompt(&snap.title, snap.fp, &snap.view);
    println!(
        "[{} | \"{}\" | {} elements | fp {:016x}]",
        cfg.provider_label(),
        snap.title,
        snap.view.elements.len(),
        snap.fp
    );
    if std::env::args().any(|a| a == "--verbose") {
        println!("{q}\n");
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let (r, _conv) = ask_model(
        &planner_cfg(&cfg),
        Default::default(),
        q,
        &snap.cap,
        &cancel,
    );
    match r {
        Ok(text) => {
            println!("model: {}", text.trim());
            match parse_plan(&text, &snap.view) {
                Ok(p) => {
                    println!("plan: {p:?}");
                    0
                }
                Err(e) => {
                    println!("unusable: {e}");
                    2
                }
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            2
        }
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    fn el(kind: &'static str, name: &str, id: &str, rect: Rect) -> Element {
        Element {
            kind,
            name: name.into(),
            rect,
            focused: false,
            automation_id: id.into(),
        }
    }

    fn view() -> View {
        View {
            frame: Frame {
                src: Rect {
                    x: 0,
                    y: 0,
                    w: 2000,
                    h: 1000,
                },
                w: 1000,
                h: 500,
            },
            elements: vec![
                el(
                    "tabitem",
                    "Home",
                    "TabHome",
                    Rect {
                        x: 100,
                        y: 20,
                        w: 120,
                        h: 40,
                    },
                ),
                el(
                    "tabitem",
                    "Insert",
                    "TabInsert",
                    Rect {
                        x: 240,
                        y: 20,
                        w: 120,
                        h: 40,
                    },
                ),
                el(
                    "button",
                    "Page Number",
                    "PageNumber",
                    Rect {
                        x: 600,
                        y: 80,
                        w: 160,
                        h: 60,
                    },
                ),
            ],
            windows: vec![Win {
                hwnd: 77,
                title: "Book1 - Excel".into(),
                exe: "EXCEL.EXE".into(),
                on_screen: false,
            }],
        }
    }

    fn step(say: &str, e: usize) -> Plan {
        parse_plan(
            &format!(r#"{{"kind":"step","say":"{say}","element":"e{e}"}}"#),
            &view(),
        )
        .unwrap()
    }

    #[test]
    fn parses_plans() {
        let v = view();
        let Plan::Step(s) = step("Click **Insert**", 2) else {
            panic!()
        };
        let t = s.target.unwrap();
        assert_eq!(
            (
                t.name.as_str(),
                t.automation_id.as_str(),
                t.control.as_str()
            ),
            ("Insert", "TabInsert", "tabitem")
        );
        assert_eq!(
            t.rect,
            Rect {
                x: 240,
                y: 20,
                w: 120,
                h: 40
            }
        );
        // A box in screenshot px (half scale here) maps to the screen.
        let p = parse_plan(
            r#"Sure. {"kind":"step","say":"Click +","x":10,"y":10,"w":20,"h":10} done"#,
            &v,
        )
        .unwrap();
        let Plan::Step(s) = p else { panic!() };
        assert_eq!(
            s.target.unwrap().rect,
            Rect {
                x: 20,
                y: 20,
                w: 40,
                h: 20
            }
        );
        // A point becomes a small ring.
        let Plan::Step(s) = parse_plan(r#"{"say":"Click","x":50,"y":50}"#, &v).unwrap() else {
            panic!()
        };
        let t = s.target.unwrap();
        assert!(t.point && inside(t.rect, 100, 100, 0));
        assert_eq!(
            parse_plan(r#"{"kind":"step","say":"Press **Ctrl+S**"}"#, &v).unwrap(),
            Plan::Step(Step {
                say: "Press **Ctrl+S**".into(),
                target: None
            })
        );
        assert_eq!(
            parse_plan(r#"{"kind":"ask","question":"Is the file open?"}"#, &v).unwrap(),
            Plan::Ask("Is the file open?".into())
        );
        assert_eq!(
            parse_plan(r#"{"kind":"switch","window":"w1"}"#, &v).unwrap(),
            Plan::Focus(77)
        );
        assert_eq!(
            parse_plan(r#"{"kind":"done","say":"All set."}"#, &v).unwrap(),
            Plan::Done("All set.".into())
        );
        // Braces inside strings don't end the object early.
        assert!(
            matches!(parse_plan(r#"{"kind":"done","say":"use {x}"}"#, &v), Ok(Plan::Done(s)) if s == "use {x}")
        );
        // Prose is shown as a step to confirm.
        assert!(matches!(
            parse_plan("Open the File menu.", &v),
            Ok(Plan::Step(Step { target: None, .. }))
        ));
        assert!(parse_plan(r#"{"kind":"switch","window":"w9"}"#, &v).is_err());
    }

    #[test]
    fn advances_on_target_click_and_rechecks_elsewhere() {
        let mut m = Machine::new("add page numbers");
        let fx = m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        assert!(matches!(&fx[..], [Effect::Show(_, 1)]));
        // Off target: look again, keep waiting.
        assert_eq!(m.feed(Input::Click(10, 900)), vec![Effect::Recheck]);
        assert!(matches!(m.phase, Phase::Waiting { .. }));
        // The screen didn't change: nothing.
        assert!(m.feed(Input::Changed(1)).is_empty());
        // On target: done, plan the next one.
        assert_eq!(m.feed(Input::Click(250, 30)), vec![Effect::Plan]);
        assert_eq!(m.done.len(), 1);
        assert_eq!(m.done[0].how, How::Clicked);
        let fx = m.feed(Input::Planned(step("Click **Page Number**", 3), 2));
        assert!(matches!(&fx[..], [Effect::Show(_, 2)]));
        // Focus landing on it completes it too.
        let f = FocusInfo {
            name: "x".into(),
            automation_id: "PageNumber".into(),
            control: "button".into(),
        };
        assert_eq!(m.feed(Input::Focus(f)), vec![Effect::Plan]);
        assert_eq!(m.done[1].how, How::Focused);
        assert_eq!(
            m.feed(Input::Planned(Plan::Done("Done.".into()), 3)),
            vec![Effect::Finish("Done.".into())]
        );
        assert!(m.ended());
        assert!(m.feed(Input::Next).is_empty(), "nothing after the end");
    }

    #[test]
    fn next_and_screen_change_advance() {
        let mut m = Machine::new("save");
        m.feed(Input::Planned(
            parse_plan(r#"{"say":"Press **Ctrl+S**"}"#, &view()).unwrap(),
            5,
        ));
        assert_eq!(
            m.feed(Input::Click(250, 30)),
            vec![Effect::Recheck],
            "no target: a click only rechecks"
        );
        assert_eq!(m.feed(Input::Next), vec![Effect::Plan]);
        m.feed(Input::Planned(step("Click **Home**", 1), 6));
        assert_eq!(m.feed(Input::Changed(7)), vec![Effect::Plan]);
        assert_eq!(
            m.done.iter().map(|d| d.how).collect::<Vec<_>>(),
            [How::Next, How::Changed]
        );
    }

    #[test]
    fn repeats_make_it_replan_then_trust_it() {
        let mut m = Machine::new("x");
        m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        m.feed(Input::Click(250, 30));
        // Same step on the same screen again: re-plan, with a warning.
        assert_eq!(
            m.feed(Input::Planned(step("Click **Insert**", 2), 1)),
            vec![Effect::Plan]
        );
        assert!(m.note.as_deref().is_some_and(|n| n.contains("circles")));
        let q = m.prompt("Doc", 1, &view());
        assert!(
            q.contains("Note:") && q.contains("1. Click **Insert** (the user clicked it)"),
            "{q}"
        );
        assert_eq!(
            m.feed(Input::Planned(step("click insert", 2), 1)),
            vec![Effect::Plan]
        );
        // After MAX_REPLANS it's shown anyway rather than spinning forever.
        assert!(matches!(
            &m.feed(Input::Planned(step("Click Insert", 2), 1))[..],
            [Effect::Show(..)]
        ));
        // A different step on the same screen is fine.
        let mut m = Machine::new("x");
        m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        m.feed(Input::Next);
        assert!(matches!(
            &m.feed(Input::Planned(step("Click **Page Number**", 3), 1))[..],
            [Effect::Show(..)]
        ));
    }

    #[test]
    fn returning_to_a_left_screen_is_a_cycle() {
        let mut m = Machine::new("x");
        m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        m.feed(Input::Click(250, 30));
        m.feed(Input::Planned(step("Click **Page Number**", 3), 2));
        m.feed(Input::Click(610, 90));
        assert!(m.cycle(1) && !m.cycle(2));
        assert!(m.prompt("Doc", 1, &view()).contains("Warning"));
        assert_eq!(
            m.feed(Input::Planned(step("Click **Home**", 1), 1)),
            vec![Effect::Plan]
        );
        // Off-target re-plans on the same screen are not cycles.
        let mut m = Machine::new("x");
        m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        m.feed(Input::Heard("I can't see it".into()));
        assert!(matches!(
            &m.feed(Input::Planned(step("Click **Insert**", 2), 1))[..],
            [Effect::Show(..)]
        ));
        assert!(m.prompt("Doc", 1, &view()).contains("I can't see it"));
    }

    #[test]
    fn asks_once_then_answers_feed_the_plan() {
        let mut m = Machine::new("x");
        assert_eq!(
            m.feed(Input::Planned(Plan::Ask("Is the file open?".into()), 1)),
            vec![Effect::Ask("Is the file open?".into())]
        );
        assert_eq!(m.feed(Input::Heard("yes".into())), vec![Effect::Plan]);
        assert!(
            m.prompt("Doc", 1, &view())
                .contains("Is the file open? -> yes")
        );
        // A second question is refused.
        assert_eq!(
            m.feed(Input::Planned(Plan::Ask("Which one?".into()), 1)),
            vec![Effect::Plan]
        );
        assert!(m.note.is_some());
        // Skip on a question.
        let mut m = Machine::new("x");
        m.feed(Input::Planned(Plan::Ask("Q?".into()), 1));
        assert_eq!(m.feed(Input::Next), vec![Effect::Plan]);
        assert!(m.answers[0].1.contains("skipped"));
    }

    #[test]
    fn stop_cancel_and_failures() {
        let mut m = Machine::new("x");
        assert!(
            m.feed(Input::Click(1, 1)).is_empty(),
            "clicks while planning do nothing"
        );
        assert_eq!(m.feed(Input::Stop), vec![Effect::Stopped]);
        assert!(
            m.feed(Input::Planned(step("Click **Insert**", 2), 1))
                .is_empty(),
            "a late plan is dropped"
        );
        let mut m = Machine::new("x");
        m.feed(Input::Planned(step("Click **Insert**", 2), 1));
        assert_eq!(m.feed(Input::Stop), vec![Effect::Stopped]);
        let mut m = Machine::new("x");
        assert_eq!(
            m.feed(Input::Failed("boom".into())),
            vec![Effect::Fail("boom".into())]
        );
        let mut m = Machine::new("x");
        assert_eq!(
            m.feed(Input::Planned(Plan::Focus(77), 1)),
            vec![Effect::Focus(77), Effect::Plan]
        );
        assert_eq!(m.phase, Phase::Planning);
    }

    #[test]
    fn fingerprints_ignore_positions() {
        let a = [
            el(
                "button",
                "Save",
                "",
                Rect {
                    x: 0,
                    y: 0,
                    w: 10,
                    h: 10,
                },
            ),
            el("edit", "Name", "", Rect::default()),
        ];
        let mut b = a.clone();
        b[0].rect.x = 400;
        b.reverse();
        assert_eq!(fingerprint("Doc", &a), fingerprint("Doc", &b));
        assert_ne!(fingerprint("Doc", &a), fingerprint("Doc2", &a));
        assert_ne!(fingerprint("Doc", &a), fingerprint("Doc", &a[..1]));
    }

    #[test]
    fn router_follows_settings() {
        let mut cfg = Config::default();
        assert!(router_enabled(&cfg));
        assert!(
            !router_enabled(&planner_cfg(&cfg)),
            "the planner never starts a walk"
        );
        cfg.walk.enabled = false;
        assert!(!router_enabled(&cfg));
        assert!(request(&json!({"goal": " "})).1);
        assert!(!request(&json!({"goal": "export a pdf"})).1);
        assert_eq!(take_request().as_deref(), Some("export a pdf"));
        assert_eq!(take_request(), None);
    }

    /// `SCREENBUDDY_WALK_PNG=dir` writes the step card + pointer over a mock
    /// app (and the question card) as PNGs to look at.
    #[test]
    fn renders() {
        use tiny_skia::{Color, Paint, Pixmap, PixmapPaint, Rect as R, Transform};
        let scale = 1.5;
        let screen = Rect {
            x: 0,
            y: 0,
            w: 1500,
            h: 840,
        };
        let fill = |pm: &mut Pixmap, x: f32, y: f32, w: f32, h: f32, c: [u8; 3]| {
            let mut p = Paint::default();
            p.set_color(Color::from_rgba8(c[0], c[1], c[2], 255));
            if let Some(r) = R::from_xywh(x, y, w, h) {
                pm.fill_rect(r, &p, Transform::identity(), None);
            }
        };
        let dir = std::env::var_os("SCREENBUDDY_WALK_PNG").map(std::path::PathBuf::from);
        let target = Target {
            name: "Insert".into(),
            automation_id: "TabInsert".into(),
            control: "tabitem".into(),
            rect: Rect {
                x: 222,
                y: 60,
                w: 96,
                h: 42,
            },
            point: false,
        };
        let step = Step {
            say: "Click **Insert** in the ribbon".into(),
            target: Some(target),
        };
        let cards = [
            ("step", step_card(&step, 2, scale)),
            ("ask", ask_card("Do you already have the document open?")),
        ];
        for (name, card) in cards {
            let mut pm = Pixmap::new(screen.w as u32, screen.h as u32).unwrap();
            // A light app window with a ribbon of tabs.
            fill(&mut pm, 0.0, 0.0, 1500.0, 840.0, [236, 236, 238]);
            fill(&mut pm, 0.0, 0.0, 1500.0, 52.0, [43, 87, 154]);
            fill(&mut pm, 0.0, 52.0, 1500.0, 60.0, [248, 248, 250]);
            for i in 0..6 {
                fill(
                    &mut pm,
                    30.0 + i as f32 * 192.0,
                    68.0,
                    80.0,
                    26.0,
                    [222, 222, 228],
                );
            }
            fill(&mut pm, 240.0, 160.0, 1020.0, 680.0, [255, 255, 255]);
            if let Some(p) = &card.pointer {
                crate::overlay::draw_shape(&mut pm, p, scale, screen, Transform::identity());
            }
            let view = caption::View {
                phase: card.phase,
                label: card.label.clone(),
                body: card.body.clone(),
                chrome: card.chrome,
                ..Default::default()
            };
            let cp = caption::render(&view, scale).expect(name);
            let m = (32.0 * scale) as i32; // the caption's shadow margin
            let (cw, ch) = (cp.width() as i32 - 2 * m, cp.height() as i32 - 2 * m);
            let (x, y) = match card.near {
                Some(r) => caption::beside((cw, ch), screen, r, (18.0 * scale) as i32),
                None => (700, 300),
            };
            if let Some(r) = card.near {
                // Never on top of what it points at.
                assert!(
                    uia::intersect(Rect { x, y, w: cw, h: ch }, r).is_none(),
                    "{name}"
                );
            }
            pm.draw_pixmap(
                x - m,
                y - m,
                cp.as_ref(),
                &PixmapPaint::default(),
                Transform::identity(),
                None,
            );
            if let Some(d) = &dir {
                let _ = std::fs::create_dir_all(d);
                pm.save_png(d.join(format!("walk-{name}.png"))).unwrap();
            }
        }
    }
}
