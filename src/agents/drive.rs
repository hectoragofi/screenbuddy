//! Drive mode: an agent uses the real mouse and keyboard while the user
//! watches. Agents work in the background by default (accessibility tree,
//! the real pointer never moves); `drive` is for when the user explicitly asks
//! to watch ("show me", "do it while I watch").
//!
//! - The first drive action asks for approval ("Let <name> use your mouse and
//!   keyboard?"); a yes covers the rest of that agent's conversation.
//! - While driving, a SIGNAL-tinted agent cursor follows the pointer and a
//!   low-level hook watches for *real* (non-injected) input: any of it pauses
//!   the agent at once (mid-action, between two injected events) and posts an
//!   Ask to continue. Yes resumes, no hands control back for good this turn.
//! - Only one agent drives at a time. `agents.drive = "never"` turns it off.
//!
//! The state machine (`Machine`) and the session logic (`Session`) are pure;
//! the OS sits behind `Input` (SendInput) and `Ui` (asks, events), so tests
//! use fakes and never touch real input.

use super::{AgentEvent, Answer, AskKind, Sink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DriveEvent {
    /// Waiting for the user's yes.
    Requested,
    /// Driving the real mouse and keyboard.
    Started,
    /// The user touched the mouse or keyboard; waiting to be allowed to continue.
    Paused,
    Resumed,
    /// Control is back with the user.
    Ended,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Off,
    Driving,
    Paused,
}

/// What a drive action needs before it can run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Need {
    Go,
    /// First drive in this conversation: ask for approval.
    Approval,
    /// Off again after an earlier yes: start without asking.
    Restart,
    /// Paused by the user: ask to continue.
    Continue,
    /// The user said no (or took over and said no): don't drive this turn.
    Refused,
}

/// The drive state machine for one agent conversation.
#[derive(Clone, Debug)]
pub struct Machine {
    pub state: State,
    /// A yes was given in this conversation.
    pub approved: bool,
    /// The user declined this turn.
    pub refused: bool,
}

impl Default for Machine {
    fn default() -> Self {
        Self {
            state: State::Off,
            approved: false,
            refused: false,
        }
    }
}

impl Machine {
    pub fn need(&self) -> Need {
        match self.state {
            State::Driving => Need::Go,
            State::Paused => Need::Continue,
            State::Off if self.refused => Need::Refused,
            State::Off if self.approved => Need::Restart,
            State::Off => Need::Approval,
        }
    }
    /// The answer to an Approval / Continue ask.
    pub fn answer(&mut self, yes: bool) {
        if yes {
            self.approved = true;
            self.state = State::Driving;
        } else {
            self.refused = true;
            self.state = State::Off;
        }
    }
    pub fn restart(&mut self) {
        self.state = State::Driving;
    }
    /// Real user input. True when it paused a drive.
    pub fn user_input(&mut self) -> bool {
        if self.state == State::Driving {
            self.state = State::Paused;
            return true;
        }
        false
    }
    /// The agent stopped driving (or its turn ended). A refusal lasts the turn.
    pub fn end(&mut self, turn_over: bool) -> bool {
        let was = self.state != State::Off;
        self.state = State::Off;
        if turn_over {
            self.refused = false;
        }
        was
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// One injected input event.
#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Move(i32, i32),
    Button(Button, bool),
    Key(u16, bool),
    Char(u16),
    Wheel(i32),
}

/// Where injected input goes. The real one is `SendInput`; tests record.
pub trait Input: Send {
    fn send(&mut self, op: &Op);
    fn cursor(&self) -> (i32, i32);
}

/// Asks and events (the agent's Sink in production).
pub trait Ui: Send + Sync {
    fn name(&self) -> String;
    fn ask(&self, text: &str, detail: &str) -> bool;
    fn event(&self, ev: DriveEvent);
    fn stopped(&self) -> bool;
}

/// The parsed `drive` tool call.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Move(i32, i32),
    Click {
        x: Option<i32>,
        y: Option<i32>,
        button: Button,
        double: bool,
    },
    Type(String),
    Press(Vec<u16>),
    Scroll {
        x: Option<i32>,
        y: Option<i32>,
        amount: i32,
    },
    Stop,
}

pub(crate) const DESC: &str = "Use the user's REAL mouse and keyboard while they watch. Only when the user explicitly asked to watch you do it ('show me', 'do it while I watch', 'do it on my screen'); otherwise always work in the background with `desktop`. The first call asks the user for permission; if the user touches the mouse or keyboard you're paused until they let you continue. Coordinates are physical screen pixels (from `desktop` bounds or a screenshot). Actions: move, click (button left|right|middle, double), type (text), press (keys like 'ctrl+s'), scroll (amount: +down/-up notches), stop (give control back when you're done driving). `why` is shown to the user in the permission prompt.";

pub(crate) fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "action": {"type": "string", "enum": ["move", "click", "type", "press", "scroll", "stop"]},
            "x": {"type": "integer"}, "y": {"type": "integer"},
            "button": {"type": "string", "enum": ["left", "right", "middle"]},
            "double": {"type": "boolean"},
            "text": {"type": "string"},
            "keys": {"type": "string", "description": "e.g. 'ctrl+s', 'enter', 'alt+tab'"},
            "amount": {"type": "integer"},
            "why": {"type": "string", "description": "Why you need the user's mouse, in a few words"}
        },
        "required": ["action"]
    })
}

pub fn parse(v: &Value) -> Result<Action, String> {
    let x = v["x"].as_i64().map(|n| n as i32);
    let y = v["y"].as_i64().map(|n| n as i32);
    let button = match v["button"].as_str().unwrap_or("left") {
        "right" => Button::Right,
        "middle" => Button::Middle,
        _ => Button::Left,
    };
    Ok(match v["action"].as_str().unwrap_or("") {
        "move" => Action::Move(
            x.ok_or("move needs x and y")?,
            y.ok_or("move needs x and y")?,
        ),
        "click" => Action::Click {
            x,
            y,
            button,
            double: v["double"].as_bool().unwrap_or(false),
        },
        "type" => Action::Type(v["text"].as_str().ok_or("type needs text")?.to_string()),
        "press" => Action::Press(keys(
            v["keys"]
                .as_str()
                .or(v["text"].as_str())
                .ok_or("press needs keys")?,
        )?),
        "scroll" => Action::Scroll {
            x,
            y,
            amount: v["amount"].as_i64().unwrap_or(3) as i32,
        },
        "stop" => Action::Stop,
        a => return Err(format!("unknown action '{a}'")),
    })
}

/// "ctrl+shift+s" -> virtual-key codes, modifiers first.
pub fn keys(spec: &str) -> Result<Vec<u16>, String> {
    let mut out = Vec::new();
    for part in spec
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
    {
        let vk: u16 = match part.as_str() {
            "ctrl" | "control" => 0x11,
            "shift" => 0x10,
            "alt" => 0x12,
            "win" | "meta" | "cmd" | "super" => 0x5B,
            "enter" | "return" => 0x0D,
            "tab" => 0x09,
            "esc" | "escape" => 0x1B,
            "space" => 0x20,
            "backspace" => 0x08,
            "delete" | "del" => 0x2E,
            "insert" => 0x2D,
            "home" => 0x24,
            "end" => 0x23,
            "pageup" | "pgup" => 0x21,
            "pagedown" | "pgdn" => 0x22,
            "left" => 0x25,
            "up" => 0x26,
            "right" => 0x27,
            "down" => 0x28,
            f if f.len() >= 2
                && f.starts_with('f')
                && f[1..].parse::<u16>().is_ok_and(|n| (1..=24).contains(&n)) =>
            {
                0x6F + f[1..].parse::<u16>().unwrap()
            }
            c if c.chars().count() == 1 => {
                let ch = c.chars().next().unwrap().to_ascii_uppercase();
                if ch.is_ascii_alphanumeric() {
                    ch as u16
                } else {
                    return Err(format!("use `type` for '{c}'"));
                }
            }
            k => return Err(format!("unknown key '{k}'")),
        };
        out.push(vk);
    }
    if out.is_empty() {
        return Err("no keys".into());
    }
    Ok(out)
}

/// Mouse glide: this many steps, this far apart.
const GLIDE_STEPS: i32 = 14;
const GLIDE_STEP: Duration = Duration::from_millis(12);
const TYPE_GAP: Duration = Duration::from_millis(8);

/// The input ops for an action, starting from the cursor at `from`.
pub fn ops(a: &Action, from: (i32, i32)) -> Vec<(Op, Duration)> {
    let mut v = Vec::new();
    let glide = |v: &mut Vec<(Op, Duration)>, x: i32, y: i32| {
        for i in 1..=GLIDE_STEPS {
            let t = i as f32 / GLIDE_STEPS as f32;
            let e = 1.0 - (1.0 - t).powi(3);
            let px = from.0 + ((x - from.0) as f32 * e).round() as i32;
            let py = from.1 + ((y - from.1) as f32 * e).round() as i32;
            v.push((Op::Move(px, py), GLIDE_STEP));
        }
    };
    match a {
        Action::Move(x, y) => glide(&mut v, *x, *y),
        Action::Click {
            x,
            y,
            button,
            double,
        } => {
            if let (Some(x), Some(y)) = (x, y) {
                glide(&mut v, *x, *y);
            }
            for _ in 0..if *double { 2 } else { 1 } {
                v.push((Op::Button(*button, true), Duration::from_millis(30)));
                v.push((Op::Button(*button, false), Duration::from_millis(40)));
            }
        }
        Action::Type(t) => {
            for u in t.replace("\r\n", "\n").encode_utf16() {
                if u == u16::from(b'\n') {
                    v.push((Op::Key(0x0D, true), Duration::ZERO));
                    v.push((Op::Key(0x0D, false), TYPE_GAP));
                } else {
                    v.push((Op::Char(u), TYPE_GAP));
                }
            }
        }
        Action::Press(keys) => {
            for k in keys {
                v.push((Op::Key(*k, true), Duration::from_millis(10)));
            }
            for k in keys.iter().rev() {
                v.push((Op::Key(*k, false), Duration::from_millis(10)));
            }
        }
        Action::Scroll { x, y, amount } => {
            if let (Some(x), Some(y)) = (x, y) {
                glide(&mut v, *x, *y);
            }
            let notch = if *amount >= 0 { -120 } else { 120 }; // + = down
            for _ in 0..amount.unsigned_abs().min(30) {
                v.push((Op::Wheel(notch), Duration::from_millis(30)));
            }
        }
        Action::Stop => {}
    }
    v
}

/// One agent's drive session.
pub struct Session {
    pub machine: Mutex<Machine>,
    /// Set by the input hook on real user input.
    pub interrupted: Arc<AtomicBool>,
    /// Asks are posted from one place at a time.
    asking: Mutex<()>,
}

impl Default for Session {
    fn default() -> Self {
        Self {
            machine: Mutex::new(Machine::default()),
            interrupted: Arc::new(AtomicBool::new(false)),
            asking: Mutex::new(()),
        }
    }
}

const ASK_CONTINUE: &str = "You took over. Let {name} keep using your mouse and keyboard?";
pub(crate) const DECLINED: &str = "The user said no to you using their mouse and keyboard. Carry on in the background with `desktop` (accessibility tree), or stop and say what you need.";
const TOOK_BACK: &str = "The user took back control of their mouse and keyboard. Don't drive again this turn; carry on in the background with `desktop`, or stop.";

impl Session {
    fn state(&self) -> State {
        self.machine.lock().unwrap().state
    }

    /// Notes real user input: pauses a drive. Returns true when it did.
    pub fn user_input(&self, ui: &dyn Ui) -> bool {
        self.interrupted.store(true, Ordering::SeqCst);
        let paused = self.machine.lock().unwrap().user_input();
        if paused {
            ui.event(DriveEvent::Paused);
        }
        paused
    }

    /// Gets to `Driving` (asking as needed). Err = the text for the model.
    fn ensure(&self, ui: &dyn Ui, why: &str) -> Result<(), String> {
        let _one = self.asking.lock().unwrap();
        let need = self.machine.lock().unwrap().need();
        match need {
            Need::Go => Ok(()),
            Need::Refused => Err(DECLINED.into()),
            Need::Restart => {
                self.interrupted.store(false, Ordering::SeqCst);
                self.machine.lock().unwrap().restart();
                ui.event(DriveEvent::Started);
                Ok(())
            }
            Need::Approval => {
                ui.event(DriveEvent::Requested);
                let yes = ui.ask(
                    &format!("Let {} use your mouse and keyboard?", ui.name()),
                    why,
                );
                self.interrupted.store(false, Ordering::SeqCst);
                self.machine.lock().unwrap().answer(yes);
                if yes {
                    ui.event(DriveEvent::Started);
                    Ok(())
                } else {
                    ui.event(DriveEvent::Ended);
                    Err(DECLINED.into())
                }
            }
            Need::Continue => self.ask_continue(ui),
        }
    }

    fn ask_continue(&self, ui: &dyn Ui) -> Result<(), String> {
        let yes = ui.ask(&ASK_CONTINUE.replace("{name}", &ui.name()), "");
        self.interrupted.store(false, Ordering::SeqCst);
        self.machine.lock().unwrap().answer(yes);
        if yes {
            ui.event(DriveEvent::Resumed);
            Ok(())
        } else {
            ui.event(DriveEvent::Ended);
            Err(TOOK_BACK.into())
        }
    }

    /// Runs one drive action. Returns (text for the model, is_error).
    pub fn run(
        &self,
        ui: &dyn Ui,
        input: &mut dyn Input,
        action: &Action,
        why: &str,
    ) -> (String, bool) {
        if *action == Action::Stop {
            if self.machine.lock().unwrap().end(false) {
                ui.event(DriveEvent::Ended);
            }
            return ("Control is back with the user.".into(), false);
        }
        if let Err(e) = self.ensure(ui, why) {
            return (e, true);
        }
        let mut seq = ops(action, input.cursor());
        let mut i = 0;
        while i < seq.len() {
            if ui.stopped() {
                return ("stopped".into(), true);
            }
            if self.interrupted.load(Ordering::SeqCst) || self.state() != State::Driving {
                // Paused mid-action: release anything held, then ask.
                release(input, &seq[..i]);
                self.user_input(ui);
                return match self.ensure(ui, why) {
                    Ok(()) => (
                        "The user paused you mid-action, then let you continue. The action may be incomplete: check the screen (snapshot) before going on.".into(),
                        false,
                    ),
                    Err(e) => (e, true),
                };
            }
            let (op, gap) = seq[i].clone();
            input.send(&op);
            if !gap.is_zero() {
                std::thread::sleep(gap);
            }
            i += 1;
        }
        seq.clear();
        ("done".into(), false)
    }

    /// The turn ended: give control back.
    pub fn end(&self, ui: &dyn Ui) {
        if self.machine.lock().unwrap().end(true) {
            ui.event(DriveEvent::Ended);
        }
    }
}

/// Key / button ups for downs in `sent` that weren't released yet.
fn release(input: &mut dyn Input, sent: &[(Op, Duration)]) {
    let mut keys: Vec<u16> = Vec::new();
    let mut buttons: Vec<Button> = Vec::new();
    for (op, _) in sent {
        match op {
            Op::Key(k, true) => keys.push(*k),
            Op::Key(k, false) => keys.retain(|x| x != k),
            Op::Button(b, true) => buttons.push(*b),
            Op::Button(b, false) => buttons.retain(|x| x != b),
            _ => {}
        }
    }
    for k in keys.into_iter().rev() {
        input.send(&Op::Key(k, false));
    }
    for b in buttons {
        input.send(&Op::Button(b, false));
    }
}

// ---------------------------------------------------------------- live (parent process)

/// The agent currently holding the mouse, and its session.
struct Driver {
    owner: Option<String>,
    sessions: std::collections::HashMap<String, Arc<Session>>,
}

fn driver() -> &'static Mutex<Driver> {
    static D: OnceLock<Mutex<Driver>> = OnceLock::new();
    D.get_or_init(|| {
        Mutex::new(Driver {
            owner: None,
            sessions: Default::default(),
        })
    })
}

impl Ui for Sink {
    fn name(&self) -> String {
        self.rt
            .get(&self.id)
            .map(|r| r.name)
            .unwrap_or_else(|| "the agent".into())
    }
    fn ask(&self, text: &str, detail: &str) -> bool {
        matches!(
            Sink::ask(self, AskKind::Approval, text, detail, Vec::new()),
            Answer::Allow
        )
    }
    fn event(&self, ev: DriveEvent) {
        match ev {
            DriveEvent::Started | DriveEvent::Resumed => {
                hook::start(self.clone());
                hook::arm();
                cursor::show(true);
            }
            DriveEvent::Paused | DriveEvent::Ended => cursor::show(false),
            DriveEvent::Requested => {}
        }
        if ev == DriveEvent::Ended {
            hook::stop();
            let mut d = driver().lock().unwrap();
            if d.owner.as_deref() == Some(self.id.as_str()) {
                d.owner = None;
            }
        }
        self.rt.emit(&self.id, AgentEvent::Drive(ev));
    }
    fn stopped(&self) -> bool {
        Sink::stopped(self)
    }
}

pub fn enabled(cfg: &crate::config::Config) -> bool {
    cfg.agents.drive.trim() != "never"
}

/// Handles a `drive` call for the agent behind `sink`.
pub(crate) fn call(sink: &Sink, input: &Value) -> (String, bool) {
    if !enabled(&(sink.rt.config)()) {
        return ("Driving the user's mouse is turned off in settings. Work in the background with `desktop`.".into(), true);
    }
    let action = match parse(input) {
        Ok(a) => a,
        Err(e) => return (format!("error: {e}"), true),
    };
    let session = {
        let mut d = driver().lock().unwrap();
        if action != Action::Stop {
            match &d.owner {
                Some(o) if *o != sink.id => {
                    return ("Another agent is using the mouse right now. Work in the background with `desktop`.".into(), true);
                }
                _ => d.owner = Some(sink.id.clone()),
            }
        }
        d.sessions.entry(sink.id.clone()).or_default().clone()
    };
    let first = session.machine.lock().unwrap().state == State::Off;
    let why = input["why"].as_str().unwrap_or("").trim().to_string();
    let mut real = Real;
    let out = session.run(sink, &mut real, &action, &why);
    if first && session.machine.lock().unwrap().state == State::Driving {
        super::desk::show(&sink.id); // bring its desk into view to watch
    }
    if session.machine.lock().unwrap().state == State::Off {
        let mut d = driver().lock().unwrap();
        if d.owner.as_deref() == Some(sink.id.as_str()) {
            d.owner = None;
        }
    }
    out
}

/// The turn (or the agent) ended: drop the mouse.
pub(crate) fn end(sink: &Sink) {
    let s = driver().lock().unwrap().sessions.get(&sink.id).cloned();
    if let Some(s) = s {
        s.end(sink);
    }
    let mut d = driver().lock().unwrap();
    if d.owner.as_deref() == Some(sink.id.as_str()) {
        d.owner = None;
        hook::stop();
        cursor::show(false);
    }
}

/// Follow-up / retry starts a new conversation state for retry only.
pub(crate) fn forget(id: &str) {
    driver().lock().unwrap().sessions.remove(id);
}

fn on_user_input() {
    let (owner, session) = {
        let d = driver().lock().unwrap();
        let Some(o) = d.owner.clone() else { return };
        let s = d.sessions.get(&o).cloned();
        (o, s)
    };
    if let (Some(s), Some(sink)) = (session, hook::sink_for(&owner)) {
        if s.user_input(&sink) {
            // Ask now rather than on the next action, so the user sees it at once.
            std::thread::spawn(move || {
                let _one = s.asking.lock().unwrap();
                if s.machine.lock().unwrap().state == State::Paused {
                    let _ = s.ask_continue(&sink);
                }
            });
        }
    }
}

/// `dwExtraInfo` on the input we inject ("SBDR").
const TAG: usize = 0x5342_4452;

struct Real;

impl Input for Real {
    fn send(&mut self, op: &Op) {
        use windows::Win32::UI::Input::KeyboardAndMouse::*;
        use windows::Win32::UI::WindowsAndMessaging::*;
        let mouse = |dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS| INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: TAG,
                },
            },
        };
        let key = |vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk),
                    wScan: scan,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: TAG,
                },
            },
        };
        let input = match *op {
            Op::Move(x, y) => unsafe {
                let (vx, vy) = (
                    GetSystemMetrics(SM_XVIRTUALSCREEN),
                    GetSystemMetrics(SM_YVIRTUALSCREEN),
                );
                let (vw, vh) = (
                    GetSystemMetrics(SM_CXVIRTUALSCREEN).max(2),
                    GetSystemMetrics(SM_CYVIRTUALSCREEN).max(2),
                );
                let nx = ((x - vx) as i64 * 65535 / (vw - 1) as i64) as i32;
                let ny = ((y - vy) as i64 * 65535 / (vh - 1) as i64) as i32;
                cursor::move_to(x, y);
                mouse(
                    nx,
                    ny,
                    0,
                    MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                )
            },
            Op::Button(b, down) => {
                let f = match (b, down) {
                    (Button::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (Button::Left, false) => MOUSEEVENTF_LEFTUP,
                    (Button::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (Button::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (Button::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (Button::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                mouse(0, 0, 0, f)
            }
            Op::Wheel(d) => mouse(0, 0, d, MOUSEEVENTF_WHEEL),
            Op::Key(vk, down) => {
                let ext = matches!(vk, 0x21..=0x28 | 0x2D | 0x2E | 0x5B);
                let mut f = if down {
                    KEYBD_EVENT_FLAGS(0)
                } else {
                    KEYEVENTF_KEYUP
                };
                if ext {
                    f |= KEYEVENTF_EXTENDEDKEY;
                }
                key(vk, 0, f)
            }
            Op::Char(u) => {
                let inputs = [
                    key(0, u, KEYEVENTF_UNICODE),
                    key(0, u, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP),
                ];
                unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
                return;
            }
        };
        unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };
    }
    fn cursor(&self) -> (i32, i32) {
        crate::capture::cursor_pos()
    }
}

/// Low-level mouse + keyboard hooks, installed only while an agent drives.
mod hook {
    use super::Sink;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
    use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::*;

    static THREAD: AtomicU32 = AtomicU32::new(0);
    static SINK: Mutex<Option<Sink>> = Mutex::new(None);

    pub fn sink_for(id: &str) -> Option<Sink> {
        SINK.lock().unwrap().clone().filter(|s| s.id == id)
    }

    pub fn start(sink: Sink) {
        *SINK.lock().unwrap() = Some(sink);
        if THREAD.load(Ordering::SeqCst) != 0 {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || unsafe {
            let module = GetModuleHandleW(None)
                .ok()
                .map(|m| windows::Win32::Foundation::HINSTANCE(m.0));
            let ms = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module, 0).ok();
            let kb = SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), module, 0).ok();
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
            let _ = tx.send(());
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            if let Some(h) = ms {
                let _ = UnhookWindowsHookEx(h);
            }
            if let Some(h) = kb {
                let _ = UnhookWindowsHookEx(h);
            }
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
    }

    pub fn stop() {
        let t = THREAD.swap(0, Ordering::SeqCst);
        if t != 0 {
            unsafe {
                let _ = PostThreadMessageW(t, WM_QUIT, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// One report per pause; real input is ignored until then (unix ms) so the
    /// hand leaving the mouse after "Yes" doesn't pause again at once.
    static REPORTED: AtomicBool = AtomicBool::new(false);
    static QUIET_UNTIL: AtomicU64 = AtomicU64::new(0);

    /// Driving (again): the next real input pauses.
    pub fn arm() {
        QUIET_UNTIL.store(super::super::desk::now_ms() + 600, Ordering::SeqCst);
        REPORTED.store(false, Ordering::SeqCst);
    }

    fn real() {
        if super::super::desk::now_ms() < QUIET_UNTIL.load(Ordering::SeqCst)
            || REPORTED.swap(true, Ordering::SeqCst)
        {
            return;
        }
        // Off the hook thread: hooks must return fast.
        std::thread::spawn(super::on_user_input);
    }

    unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if code == HC_ACTION as i32 {
                let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
                if m.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) == 0
                    && m.dwExtraInfo != super::TAG
                {
                    real();
                }
            }
            CallNextHookEx(None, code, wp, lp)
        }
    }

    unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if code == HC_ACTION as i32 {
                let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
                let injected = k.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0;
                if !injected
                    && k.dwExtraInfo != super::TAG
                    && matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN)
                {
                    real();
                }
            }
            CallNextHookEx(None, code, wp, lp)
        }
    }
}

/// The agent cursor: a small SIGNAL ring that follows the pointer while an
/// agent drives (raw layered window, click-through, own thread).
mod cursor {
    use crate::ui::tokens;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};
    use windows::Win32::Foundation::*;
    use windows::Win32::Graphics::Gdi::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::*;
    use windows::core::PCWSTR;

    const CLASS: &str = "screenbuddy.agentcursor";
    const WM_MOVE_TO: u32 = WM_APP + 1;
    const WM_SHOW: u32 = WM_APP + 2;
    static THREAD: AtomicU32 = AtomicU32::new(0);
    /// Logical size; the hotspot sits at the centre.
    const SIZE: f32 = 40.0;

    fn post(msg: u32, wp: usize, lp: isize) {
        let t = THREAD.load(Ordering::SeqCst);
        if t != 0 {
            unsafe {
                let _ = PostThreadMessageW(t, msg, WPARAM(wp), LPARAM(lp));
            }
        }
    }

    pub fn show(on: bool) {
        if on {
            ensure();
            let (x, y) = crate::capture::cursor_pos();
            post(WM_MOVE_TO, x as isize as usize, y as isize);
        }
        post(WM_SHOW, on as usize, 0);
    }

    pub fn move_to(x: i32, y: i32) {
        post(WM_MOVE_TO, x as isize as usize, y as isize);
    }

    fn ensure() {
        if THREAD.load(Ordering::SeqCst) != 0 {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || thread_main(tx));
        let _ = rx.recv_timeout(std::time::Duration::from_secs(1));
    }

    fn draw(scale: f32) -> Option<Pixmap> {
        let px = (SIZE * scale).round() as u32;
        let mut pm = Pixmap::new(px, px)?;
        let c = px as f32 / 2.0;
        let mut paint = Paint {
            anti_alias: true,
            ..Default::default()
        };
        // Soft halo, ring, centre dot.
        paint.set_color(tokens::SIGNAL.fade(0.18).skia());
        let halo = PathBuilder::from_circle(c, c, 15.0 * scale)?;
        pm.fill_path(
            &halo,
            &paint,
            FillRule::Winding,
            Transform::identity(),
            None,
        );
        paint.set_color(tokens::SIGNAL.skia());
        let ring = PathBuilder::from_circle(c, c, 11.0 * scale)?;
        let stroke = Stroke {
            width: 2.5 * scale,
            ..Default::default()
        };
        pm.stroke_path(&ring, &paint, &stroke, Transform::identity(), None);
        let dot = PathBuilder::from_circle(c, c, 3.0 * scale)?;
        pm.fill_path(&dot, &paint, FillRule::Winding, Transform::identity(), None);
        Some(pm)
    }

    unsafe extern "system" fn wnd_proc(h: HWND, m: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe {
            if m == WM_NCHITTEST {
                return LRESULT(HTTRANSPARENT as isize);
            }
            DefWindowProcW(h, m, wp, lp)
        }
    }

    fn thread_main(ready: std::sync::mpsc::Sender<()>) {
        unsafe {
            let mut msg = MSG::default();
            let _ = PeekMessageW(&mut msg, None, 0, 0, PM_NOREMOVE);
            THREAD.store(GetCurrentThreadId(), Ordering::SeqCst);
            let hinst = GetModuleHandleW(None)
                .map(|m| HINSTANCE(m.0))
                .unwrap_or_default();
            let cls = crate::util::wide(CLASS);
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinst,
                lpszClassName: PCWSTR(cls.as_ptr()),
                ..Default::default()
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(cls.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                0,
                0,
                1,
                1,
                None,
                None,
                Some(hinst),
                None,
            );
            let _ = ready.send(());
            let Ok(hwnd) = hwnd else {
                THREAD.store(0, Ordering::SeqCst);
                return;
            };
            if crate::win::exclude_from_capture() {
                let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
            }
            let mut scale = 0.0f32;
            let mut size = 0i32;
            let mut visible = false;
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                match msg.message {
                    WM_MOVE_TO => {
                        let pos = (msg.wParam.0 as isize as i32, msg.lParam.0 as i32);
                        let s = crate::capture::monitor_at(pos.0, pos.1).scale.max(1.0);
                        if (s - scale).abs() > 0.01 {
                            if let Some(pm) = draw(s) {
                                size = pm.width() as i32;
                                present(hwnd, &pm, pos.0 - size / 2, pos.1 - size / 2);
                                scale = s;
                            }
                        }
                        let _ = SetWindowPos(
                            hwnd,
                            Some(HWND_TOPMOST),
                            pos.0 - size / 2,
                            pos.1 - size / 2,
                            0,
                            0,
                            SWP_NOSIZE
                                | SWP_NOACTIVATE
                                | if visible {
                                    SWP_SHOWWINDOW
                                } else {
                                    SWP_HIDEWINDOW
                                },
                        );
                    }
                    WM_SHOW => {
                        visible = msg.wParam.0 != 0;
                        let _ = ShowWindow(hwnd, if visible { SW_SHOWNOACTIVATE } else { SW_HIDE });
                    }
                    _ => {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }
                }
            }
        }
    }

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
            if let Ok(dib) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0)
            {
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
                    SourceConstantAlpha: 255,
                    AlphaFormat: AC_SRC_ALPHA as u8,
                };
                let (pos, size, src) =
                    (POINT { x, y }, SIZE { cx: w, cy: h }, POINT { x: 0, y: 0 });
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
            }
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    struct FakeUi {
        answers: Mutex<Vec<bool>>,
        asked: Mutex<Vec<String>>,
        events: Mutex<Vec<DriveEvent>>,
    }
    impl FakeUi {
        fn new(answers: &[bool]) -> Self {
            Self {
                answers: Mutex::new(answers.iter().rev().copied().collect()),
                asked: Default::default(),
                events: Default::default(),
            }
        }
        fn events(&self) -> Vec<DriveEvent> {
            self.events.lock().unwrap().clone()
        }
    }
    impl Ui for FakeUi {
        fn name(&self) -> String {
            "Flights".into()
        }
        fn ask(&self, text: &str, _: &str) -> bool {
            self.asked.lock().unwrap().push(text.into());
            self.answers.lock().unwrap().pop().unwrap_or(false)
        }
        fn event(&self, ev: DriveEvent) {
            self.events.lock().unwrap().push(ev);
        }
        fn stopped(&self) -> bool {
            false
        }
    }

    /// Records ops; simulates the user grabbing the mouse after `touch_after` ops.
    struct FakeInput {
        sent: Vec<Op>,
        touch_after: Option<usize>,
        flag: Arc<AtomicBool>,
        n: AtomicUsize,
    }
    impl Input for FakeInput {
        fn send(&mut self, op: &Op) {
            self.sent.push(op.clone());
            let n = self.n.fetch_add(1, Ordering::SeqCst) + 1;
            if Some(n) == self.touch_after {
                self.flag.store(true, Ordering::SeqCst); // what the hook does
            }
        }
        fn cursor(&self) -> (i32, i32) {
            (0, 0)
        }
    }
    fn fake(s: &Session, touch_after: Option<usize>) -> FakeInput {
        FakeInput {
            sent: Vec::new(),
            touch_after,
            flag: s.interrupted.clone(),
            n: AtomicUsize::new(0),
        }
    }

    #[test]
    fn machine_transitions() {
        let mut m = Machine::default();
        assert_eq!(m.need(), Need::Approval);
        m.answer(true);
        assert_eq!(m.need(), Need::Go);
        assert!(m.user_input());
        assert!(!m.user_input()); // already paused
        assert_eq!(m.need(), Need::Continue);
        m.answer(true);
        assert_eq!(m.state, State::Driving);
        assert!(m.end(false));
        assert_eq!(m.need(), Need::Restart); // the yes covers the conversation
        m.restart();
        m.user_input();
        m.answer(false);
        assert_eq!(m.need(), Need::Refused);
        m.end(true); // next turn may ask again, still approved
        assert_eq!(m.need(), Need::Restart);
        assert!(!Machine::default().clone().user_input()); // input while off is ignored
    }

    #[test]
    fn asks_once_per_conversation() {
        let s = Session::default();
        let ui = FakeUi::new(&[true]);
        let mut inp = fake(&s, None);
        let (t, err) = s.run(
            &ui,
            &mut inp,
            &Action::Click {
                x: Some(10),
                y: Some(10),
                button: Button::Left,
                double: false,
            },
            "show you",
        );
        assert!(!err, "{t}");
        assert_eq!(
            ui.asked.lock().unwrap()[0],
            "Let Flights use your mouse and keyboard?"
        );
        assert_eq!(inp.sent.last(), Some(&Op::Button(Button::Left, false)));
        assert_eq!(
            inp.sent
                .iter()
                .filter(|o| matches!(o, Op::Move(..)))
                .count(),
            GLIDE_STEPS as usize
        );
        assert_eq!(inp.sent[GLIDE_STEPS as usize - 1], Op::Move(10, 10));
        s.run(&ui, &mut inp, &Action::Type("hi".into()), "");
        s.run(&ui, &mut inp, &Action::Stop, "");
        s.run(&ui, &mut inp, &Action::Press(vec![0x11, 0x53]), "");
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert_eq!(
            ui.events(),
            vec![
                DriveEvent::Requested,
                DriveEvent::Started,
                DriveEvent::Ended,
                DriveEvent::Started
            ]
        );
    }

    #[test]
    fn declined_means_no_input() {
        let s = Session::default();
        let ui = FakeUi::new(&[false]);
        let mut inp = fake(&s, None);
        let (t, err) = s.run(&ui, &mut inp, &Action::Move(5, 5), "");
        assert!(err);
        assert_eq!(t, DECLINED);
        assert!(inp.sent.is_empty());
        // No second ask this turn.
        let (_, err) = s.run(&ui, &mut inp, &Action::Move(5, 5), "");
        assert!(err);
        assert_eq!(ui.asked.lock().unwrap().len(), 1);
        assert!(inp.sent.is_empty());
    }

    #[test]
    fn user_input_pauses_mid_action_and_asks() {
        let s = Session::default();
        let ui = FakeUi::new(&[true, true]);
        // Ctrl down, S down, then the user touches the keyboard.
        let mut inp = fake(&s, Some(2));
        let (t, err) = s.run(&ui, &mut inp, &Action::Press(vec![0x11, 0x53]), "");
        assert!(!err);
        assert!(t.contains("paused"), "{t}");
        // Held keys were released, nothing else injected after the touch.
        assert_eq!(
            inp.sent,
            vec![
                Op::Key(0x11, true),
                Op::Key(0x53, true),
                Op::Key(0x53, false),
                Op::Key(0x11, false)
            ]
        );
        assert_eq!(
            ui.asked.lock().unwrap()[1],
            "You took over. Let Flights keep using your mouse and keyboard?"
        );
        assert_eq!(
            ui.events(),
            vec![
                DriveEvent::Requested,
                DriveEvent::Started,
                DriveEvent::Paused,
                DriveEvent::Resumed
            ]
        );
        assert_eq!(s.state(), State::Driving);
    }

    #[test]
    fn user_takes_back_control() {
        let s = Session::default();
        let ui = FakeUi::new(&[true, false]);
        let mut inp = fake(&s, Some(3));
        let (t, err) = s.run(&ui, &mut inp, &Action::Type("hello".into()), "");
        assert!(err);
        assert_eq!(t, TOOK_BACK);
        assert_eq!(inp.sent.len(), 3);
        assert_eq!(s.state(), State::Off);
        assert_eq!(ui.events().last(), Some(&DriveEvent::Ended));
        // Refused for the rest of the turn; nothing more injected.
        let (_, err) = s.run(&ui, &mut inp, &Action::Move(1, 1), "");
        assert!(err && inp.sent.len() == 3);
        s.end(&ui);
        // Next turn: approved earlier, so it restarts without asking.
        let mut inp = fake(&s, None);
        let (_, err) = s.run(&ui, &mut inp, &Action::Move(1, 1), "");
        assert!(!err);
        assert_eq!(ui.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn pause_between_actions() {
        let s = Session::default();
        let ui = FakeUi::new(&[true, true]);
        let mut inp = fake(&s, None);
        s.run(&ui, &mut inp, &Action::Move(3, 3), "");
        // The hook fires while the agent is thinking.
        assert!(s.user_input(&ui));
        assert_eq!(s.state(), State::Paused);
        let n = inp.sent.len();
        let (_, err) = s.run(
            &ui,
            &mut inp,
            &Action::Click {
                x: None,
                y: None,
                button: Button::Right,
                double: true,
            },
            "",
        );
        assert!(!err);
        assert_eq!(inp.sent.len(), n + 4);
        assert_eq!(ui.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn parses_actions_and_keys() {
        assert_eq!(keys("ctrl+shift+s").unwrap(), vec![0x11, 0x10, 0x53]);
        assert_eq!(keys("F5").unwrap(), vec![0x74]);
        assert_eq!(keys("enter").unwrap(), vec![0x0D]);
        assert!(keys("ctrl+?").is_err());
        assert_eq!(
            parse(&json!({"action": "move", "x": 1, "y": 2})).unwrap(),
            Action::Move(1, 2)
        );
        assert!(parse(&json!({"action": "move"})).is_err());
        assert_eq!(
            parse(&json!({"action": "press", "keys": "ctrl+s"})).unwrap(),
            Action::Press(vec![0x11, 0x53])
        );
        assert!(matches!(
            parse(&json!({"action": "scroll", "amount": -2})).unwrap(),
            Action::Scroll { amount: -2, .. }
        ));
        let o = ops(&Action::Type("a\nb".into()), (0, 0));
        assert_eq!(
            o.iter().map(|x| x.0.clone()).collect::<Vec<_>>(),
            vec![
                Op::Char(97),
                Op::Key(0x0D, true),
                Op::Key(0x0D, false),
                Op::Char(98)
            ]
        );
        let o = ops(
            &Action::Scroll {
                x: None,
                y: None,
                amount: 2,
            },
            (0, 0),
        );
        assert_eq!(o.len(), 2);
        assert_eq!(o[0].0, Op::Wheel(-120));
    }
}
