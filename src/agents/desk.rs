//! The agent desk: with Apeiron's canvas on, every agent gets its own canvas
//! region (`screenbuddy:agent:<id>`, reserved lazily on its first app launch)
//! and the windows it opens are moved there, so it works without disturbing
//! the user's view. The windows stay when it finishes; `show` flies there.
//!
//! Which windows are the agent's is decided conservatively: a process counts
//! only when it was *started* by one of the agent's `launch` actions (created
//! within a few seconds after it, and either the pid the launch reported or an
//! exe that matches the launched app), or is a child started later by such a
//! process. A launch that attached to an app the user already had open adds
//! nothing, so the user's own windows are never moved.
//!
//! Without Apeiron nothing moves; in tabs-only mode windows are only tracked
//! (so `show` can focus them).

use super::{AgentEvent, Sink};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

/// A launched process may take this long to appear.
const LAUNCH_SLACK_MS: u64 = 8_000;
/// A process created this long *before* the action still counts (clock skew).
const EARLY_MS: u64 = 500;
/// How long after a launch / any other action new windows are looked for.
const WATCH_LAUNCH: Duration = Duration::from_secs(12);
const WATCH_ACTION: Duration = Duration::from_secs(4);
const POLL: Duration = Duration::from_millis(300);
/// Give up placing a window after this many failures.
const MAX_TRIES: u8 = 3;

pub fn region_name(id: &str) -> String {
    format!("screenbuddy:agent:{id}")
}

/// What the desk tells subscribers (also printed by `screenbuddy --agent`).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum DeskEvent {
    /// The agent's canvas region, `[x, y, w, h]` in canvas units.
    Region([f64; 4]),
    /// A window was moved onto (or, without the canvas, tracked for) the desk.
    Placed { hwnd: isize, title: String },
}

/// One running process.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    /// Lowercase file name, e.g. "notepad.exe".
    pub exe: String,
}

/// One top-level window Apeiron manages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Win {
    pub hwnd: isize,
    pub pid: u32,
    pub title: String,
    /// Hidden tab or minimized: can't be placed right now.
    pub hidden: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Launch {
    at_ms: u64,
    target: String,
    pid: Option<u32>,
}

/// Per-agent bookkeeping (pure; the OS and Apeiron come in through `Env`).
#[derive(Clone, Debug, Default)]
pub struct Book {
    pub region: Option<[f64; 4]>,
    /// Processes the agent started -> their creation time (unix ms).
    pub pids: HashMap<u32, u64>,
    /// Windows on its desk, in the order they arrived.
    pub hwnds: Vec<isize>,
    launches: Vec<Launch>,
    tries: HashMap<isize, u8>,
}

/// The world as the desk sees it.
pub trait Env {
    fn procs(&self) -> Vec<Proc>;
    /// Process creation time, unix ms.
    fn created_ms(&self, pid: u32) -> Option<u64>;
    /// None without Apeiron; `(canvas, windows)` otherwise.
    fn windows(&self) -> Option<(bool, Vec<Win>)>;
    fn reserve(&self, name: &str) -> Result<[f64; 4], String>;
    fn place(&self, hwnd: isize, name: &str) -> Result<(), String>;
}

/// "Google Chrome" -> "googlechrome"; "chrome.exe" -> "chrome".
fn norm(s: &str) -> String {
    let s = s.trim().to_lowercase();
    let s = s.rsplit(['\\', '/']).next().unwrap_or(&s);
    let s = s.strip_suffix(".exe").unwrap_or(s);
    s.chars().filter(|c| c.is_alphanumeric()).collect()
}

/// The launched app name matches this exe (strictly: equal, or one contains
/// the other and the shorter is at least 4 characters).
pub fn exe_matches(target: &str, exe: &str) -> bool {
    let (t, e) = (norm(target), norm(exe));
    if t.is_empty() || e.is_empty() {
        return false;
    }
    t == e || (e.len() >= 4 && t.contains(&e)) || (t.len() >= 4 && e.contains(&t))
}

impl Book {
    pub fn note_launch(&mut self, at_ms: u64, target: &str, pid: Option<u32>) {
        self.launches.push(Launch {
            at_ms,
            target: target.to_string(),
            pid,
        });
    }

    /// Adds the processes the agent started. `now_ms` expires old launches.
    pub fn adopt(&mut self, procs: &[Proc], created: impl Fn(u32) -> Option<u64>, now_ms: u64) {
        self.launches.retain(|l| {
            now_ms.saturating_sub(l.at_ms) <= LAUNCH_SLACK_MS + WATCH_LAUNCH.as_millis() as u64
        });
        let me = std::process::id();
        loop {
            let mut grew = false;
            for p in procs {
                if p.pid == 0 || p.pid == me || self.pids.contains_key(&p.pid) {
                    continue;
                }
                // With a reported pid only that process counts; otherwise an exe
                // match that isn't a child of an older instance of the same app.
                let hit = |l: &Launch| match l.pid {
                    Some(pid) => pid == p.pid,
                    None => {
                        exe_matches(&l.target, &p.exe)
                            && !procs.iter().any(|q| {
                                q.pid == p.ppid && q.exe == p.exe && !self.pids.contains_key(&q.pid)
                            })
                    }
                };
                let parent = self.pids.get(&p.ppid).copied();
                if parent.is_none() && !self.launches.iter().any(hit) {
                    continue;
                }
                let Some(c) = created(p.pid) else { continue };
                let by_launch = self
                    .launches
                    .iter()
                    .any(|l| hit(l) && c + EARLY_MS >= l.at_ms && c <= l.at_ms + LAUNCH_SLACK_MS);
                // A child started after its parent (guards against pid reuse).
                let by_parent = parent.is_some_and(|pc| c >= pc);
                if by_launch || by_parent {
                    self.pids.insert(p.pid, c);
                    grew = true;
                }
            }
            if !grew {
                break;
            }
        }
    }

    /// New windows that belong to the agent (not on its desk yet, not claimed
    /// by another agent).
    pub fn claim(&self, wins: &[Win], taken: &HashSet<isize>) -> Vec<Win> {
        wins.iter()
            .filter(|w| self.pids.contains_key(&w.pid))
            .filter(|w| !self.hwnds.contains(&w.hwnd) && !taken.contains(&w.hwnd))
            .filter(|w| self.tries.get(&w.hwnd).copied().unwrap_or(0) < MAX_TRIES)
            .cloned()
            .collect()
    }

    /// One pass: adopt processes, reserve the region on first need, place new
    /// windows. Returns what changed.
    pub fn tick(
        &mut self,
        env: &dyn Env,
        id: &str,
        taken: &HashSet<isize>,
        now_ms: u64,
    ) -> Vec<DeskEvent> {
        let mut out = Vec::new();
        let Some((canvas, wins)) = env.windows() else {
            return out;
        };
        if self.launches.is_empty() && self.pids.is_empty() {
            return out;
        }
        let procs = env.procs();
        self.adopt(&procs, |p| env.created_ms(p), now_ms);
        let name = region_name(id);
        if canvas && self.region.is_none() {
            match env.reserve(&name) {
                Ok(r) => {
                    self.region = Some(r);
                    out.push(DeskEvent::Region(r));
                }
                Err(e) => crate::config::log(&format!("agent {id}: reserve_region failed: {e}")),
            }
        }
        for w in self.claim(&wins, taken) {
            if w.hidden {
                continue;
            }
            let placed = if canvas && self.region.is_some() {
                env.place(w.hwnd, &name)
            } else {
                Ok(()) // tabs-only: just track it
            };
            match placed {
                Ok(()) => {
                    self.hwnds.push(w.hwnd);
                    out.push(DeskEvent::Placed {
                        hwnd: w.hwnd,
                        title: w.title.clone(),
                    });
                }
                Err(e) => {
                    *self.tries.entry(w.hwnd).or_default() += 1;
                    crate::config::log(&format!(
                        "agent {id}: place_window {:#x} failed: {e}",
                        w.hwnd
                    ));
                }
            }
        }
        out
    }
}

/// Flags of agent-desktop commands that take a value.
const VALUED: &[&str] = &[
    "--timeout",
    "--arg",
    "--env",
    "--cwd",
    "--session",
    "--agent-id",
    "--trace",
    "--screenshot",
    "-w",
    "--wait-for",
    "--wait-for-gone",
    "--wait-timeout",
    "--app",
    "--window",
];

/// The command and its first positional argument (flag values skipped).
fn command(args: &[String]) -> (String, Option<String>) {
    let mut pos = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a.starts_with('-') {
            let takes = VALUED.contains(&a.as_str())
                || (a == "--cdp" && args.get(i + 1).is_some_and(|n| n.parse::<u16>().is_ok()));
            i += if takes && !a.contains('=') { 2 } else { 1 };
            continue;
        }
        pos.push(a.clone());
        i += 1;
    }
    let mut it = pos.into_iter();
    (
        it.next().map(|s| s.to_lowercase()).unwrap_or_default(),
        it.next(),
    )
}

/// A pid anywhere in agent-desktop's JSON output (`pid`, `process_id`, `processId`).
pub fn find_pid(out: &str) -> Option<u32> {
    fn walk(v: &Value) -> Option<u32> {
        match v {
            Value::Object(m) => {
                for k in ["pid", "process_id", "processId"] {
                    if let Some(n) = m
                        .get(k)
                        .and_then(Value::as_u64)
                        .filter(|n| *n > 0 && *n <= u32::MAX as u64)
                    {
                        return Some(n as u32);
                    }
                }
                m.values().find_map(walk)
            }
            Value::Array(a) => a.iter().find_map(walk),
            _ => None,
        }
    }
    walk(&serde_json::from_str::<Value>(out).ok()?)
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------- live desk (parent process)

struct Live {
    book: Book,
    /// A watcher thread polls until then.
    until: Option<std::time::Instant>,
}

fn desks() -> &'static Mutex<HashMap<String, Live>> {
    static D: OnceLock<Mutex<HashMap<String, Live>>> = OnceLock::new();
    D.get_or_init(Default::default)
}

/// Called after every desktop action an agent ran (in this process or in its
/// `screenbuddy --mcp`): a `launch` starts the desk; later actions look for
/// new windows of the apps it started. `t0_ms` = when the action began.
pub(crate) fn after_action(sink: &Sink, args: &[String], out: &str, t0_ms: u64) {
    if crate::desktop::is_read_only(args) {
        return;
    }
    crate::apeiron::start(); // idempotent; headless runs have no client yet
    let (cmd, target) = command(args);
    let launch = cmd == "launch";
    let wait = if launch { WATCH_LAUNCH } else { WATCH_ACTION };
    let start = {
        let mut d = desks().lock().unwrap();
        let live = d.entry(sink.id.clone()).or_insert_with(|| Live {
            book: Book {
                hwnds: sink.rt.get(&sink.id).map(|r| r.desk).unwrap_or_default(),
                region: sink.rt.get(&sink.id).and_then(|r| r.region),
                ..Default::default()
            },
            until: None,
        });
        if launch {
            live.book
                .note_launch(t0_ms, target.as_deref().unwrap_or(""), find_pid(out));
        } else if live.book.pids.is_empty() && live.book.launches.is_empty() {
            return; // nothing on the desk to grow
        }
        let until = std::time::Instant::now() + wait;
        let running = live.until.is_some();
        live.until = Some(live.until.map_or(until, |u| u.max(until)));
        !running
    };
    if start {
        let sink = sink.clone();
        std::thread::spawn(move || watch(sink, &RealEnv));
    }
}

fn watch(sink: Sink, env: &dyn Env) {
    loop {
        let taken: HashSet<isize> = {
            let d = desks().lock().unwrap();
            d.iter()
                .filter(|(k, _)| **k != sink.id)
                .flat_map(|(_, l)| l.book.hwnds.iter().copied())
                .collect()
        };
        // Tick on a copy so the OS / Apeiron calls run outside the lock.
        let Some(mut book) = desks()
            .lock()
            .unwrap()
            .get(&sink.id)
            .map(|l| l.book.clone())
        else {
            return;
        };
        let events = book.tick(env, &sink.id, &taken, now_ms());
        let done = {
            let mut d = desks().lock().unwrap();
            let Some(live) = d.get_mut(&sink.id) else {
                return;
            };
            // Launches noted meanwhile survive the copy.
            for l in &live.book.launches {
                if !book.launches.contains(l) {
                    book.launches.push(l.clone());
                }
            }
            live.book = book.clone();
            let over = live.until.is_none_or(|u| std::time::Instant::now() >= u);
            if over {
                live.until = None;
            }
            over
        };
        if !events.is_empty() {
            sink.rt.update(&sink.id, |r| {
                r.desk = book.hwnds.clone();
                r.region = book.region;
            });
            for e in events {
                sink.rt.emit(&sink.id, AgentEvent::Desk(e));
            }
        }
        if done {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// Windows on the agent's desk that still exist, as `(hwnd, title)`.
pub fn windows(id: &str) -> Vec<(isize, String)> {
    super::get(id).map(|a| windows_of(&a)).unwrap_or_default()
}

pub(crate) fn windows_of(a: &super::Agent) -> Vec<(isize, String)> {
    a.desk
        .iter()
        .copied()
        .filter(|h| win::alive(*h))
        .map(|h| (h, win::title(h)))
        .collect()
}

/// Shows the agent's desk: flies the Apeiron camera to its windows (or its
/// region), or focuses its main window. False if there's nothing to show.
pub fn show(id: &str) -> bool {
    let alive: Vec<isize> = windows(id).into_iter().map(|w| w.0).collect();
    let Some(&main) = alive.first() else {
        return false;
    };
    match crate::apeiron::get() {
        Some(l) if l.canvas => {
            let rects: Vec<[f64; 4]> = alive
                .iter()
                .filter_map(|h| l.window(*h))
                .map(|w| w.canvas_rect)
                .collect();
            let target = union(&rects).or_else(|| super::get(id).and_then(|a| a.region));
            match target {
                Some(r) => crate::apeiron::fly_to(r).is_ok(),
                None => crate::apeiron::focus_window(main).is_ok(),
            }
        }
        Some(_) => crate::apeiron::focus_window(main).is_ok(),
        None => win::focus(main),
    }
}

/// Returns the camera to where it was before `show`.
#[allow(dead_code)] // the cards slice binds to it
pub fn back() -> bool {
    crate::apeiron::back().is_ok()
}

/// Bounding box of canvas rects.
pub fn union(rects: &[[f64; 4]]) -> Option<[f64; 4]> {
    let first = rects.first()?;
    let (mut x0, mut y0, mut x1, mut y1) =
        (first[0], first[1], first[0] + first[2], first[1] + first[3]);
    for r in &rects[1..] {
        x0 = x0.min(r[0]);
        y0 = y0.min(r[1]);
        x1 = x1.max(r[0] + r[2]);
        y1 = y1.max(r[1] + r[3]);
    }
    Some([x0, y0, x1 - x0, y1 - y0])
}

// ---------------------------------------------------------------- MCP: window screenshot

pub(crate) const SHOT_DESC: &str = "Screenshot one window, even when it's off-screen on your desk (the canvas area where the apps you launch are kept, out of the user's view). Without `hwnd` it captures your desk's most recent window and lists the others. Use it when the accessibility tree doesn't show what you need.";

pub(crate) fn shot_schema() -> Value {
    json!({"type": "object", "properties": {"hwnd": {"type": "integer", "description": "Window handle (from this tool's list or agent-desktop list-windows)"}}})
}

/// Captures a window; returns (jpeg, text) or an error message.
pub(crate) fn shot(
    hwnd: Option<isize>,
    desk: &[(isize, String)],
) -> Result<(Vec<u8>, String), String> {
    let hwnd = hwnd.or_else(|| desk.last().map(|w| w.0)).ok_or(
        "No window on your desk yet. Pass `hwnd` (agent-desktop list-windows shows them).",
    )?;
    let cap = crate::capture::window_capture(hwnd).map_err(|e| format!("error: {e:#}"))?;
    let mut text = format!("Window {hwnd:#x} ({}x{} px).", cap.img_w, cap.img_h);
    if !desk.is_empty() {
        let list: Vec<String> = desk.iter().map(|(h, t)| format!("{h} \"{t}\"")).collect();
        text.push_str(&format!(" Your desk: {}.", list.join(", ")));
    }
    Ok((cap.jpeg, text))
}

/// `window_screenshot` inside `screenbuddy --mcp` (agent role): asks the
/// parent for the desk, captures here.
pub fn mcp_screenshot(args: &Value) -> Value {
    use base64::Engine;
    let desk: Vec<(isize, String)> = super::cli::ask_parent_line(&json!({"kind": "desk_windows"}))
        .and_then(|l| serde_json::from_str::<Value>(&l).ok())
        .and_then(|v| v["windows"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|w| {
            Some((
                w["hwnd"].as_i64()? as isize,
                w["title"].as_str().unwrap_or("").to_string(),
            ))
        })
        .collect();
    match shot(args["hwnd"].as_i64().map(|h| h as isize), &desk) {
        Ok((jpeg, text)) => json!({"content": [
            {"type": "image", "data": base64::engine::general_purpose::STANDARD.encode(jpeg), "mimeType": "image/jpeg"},
            {"type": "text", "text": text}
        ], "isError": false}),
        Err(e) => json!({"content": [{"type": "text", "text": e}], "isError": true}),
    }
}

// ---------------------------------------------------------------- the real world

struct RealEnv;

impl Env for RealEnv {
    fn procs(&self) -> Vec<Proc> {
        win::procs()
    }
    fn created_ms(&self, pid: u32) -> Option<u64> {
        win::created_ms(pid)
    }
    fn windows(&self) -> Option<(bool, Vec<Win>)> {
        let l = crate::apeiron::refresh().or_else(crate::apeiron::get)?;
        let wins = l
            .windows
            .iter()
            .map(|w| Win {
                hwnd: w.hwnd,
                pid: win::pid_of(w.hwnd),
                title: w.title.clone(),
                hidden: w.tab_hidden || w.away,
            })
            .collect();
        Some((l.canvas, wins))
    }
    fn reserve(&self, name: &str) -> Result<[f64; 4], String> {
        // A bit smaller than the monitor, so flying there lands at 1:1.
        let (cx, cy) = crate::capture::cursor_pos();
        let m = crate::capture::monitor_at(cx, cy).work;
        crate::apeiron::reserve_region(name, (m.w as f64 * 0.9).round(), (m.h as f64 * 0.9).round())
    }
    fn place(&self, hwnd: isize, name: &str) -> Result<(), String> {
        crate::apeiron::place_window(hwnd, name)
    }
}

mod win {
    use super::Proc;
    use windows::Win32::Foundation::{CloseHandle, FILETIME, HWND};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::System::Threading::{
        GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::*;

    pub fn procs() -> Vec<Proc> {
        let mut out = Vec::new();
        unsafe {
            let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
                return out;
            };
            let mut e = PROCESSENTRY32W {
                dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            let mut ok = Process32FirstW(snap, &mut e).is_ok();
            while ok {
                let n = e
                    .szExeFile
                    .iter()
                    .position(|c| *c == 0)
                    .unwrap_or(e.szExeFile.len());
                out.push(Proc {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&e.szExeFile[..n]).to_lowercase(),
                });
                ok = Process32NextW(snap, &mut e).is_ok();
            }
            let _ = CloseHandle(snap);
        }
        out
    }

    pub fn created_ms(pid: u32) -> Option<u64> {
        unsafe {
            let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let (mut c, mut x, mut k, mut u) = (
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
                FILETIME::default(),
            );
            let r = GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u);
            let _ = CloseHandle(h);
            r.ok()?;
            let t = ((c.dwHighDateTime as u64) << 32) | c.dwLowDateTime as u64;
            // 100 ns ticks since 1601 -> unix ms.
            (t / 10_000).checked_sub(11_644_473_600_000)
        }
    }

    pub fn pid_of(hwnd: isize) -> u32 {
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(HWND(hwnd as *mut _), Some(&mut pid)) };
        pid
    }

    pub fn alive(hwnd: isize) -> bool {
        unsafe { IsWindow(Some(HWND(hwnd as *mut _))).as_bool() }
    }

    pub fn title(hwnd: isize) -> String {
        let mut buf = [0u16; 256];
        let n = unsafe { GetWindowTextW(HWND(hwnd as *mut _), &mut buf) };
        String::from_utf16_lossy(&buf[..n.max(0) as usize])
    }

    pub fn focus(hwnd: isize) -> bool {
        let h = HWND(hwnd as *mut _);
        unsafe {
            if IsIconic(h).as_bool() {
                let _ = ShowWindow(h, SW_RESTORE);
            }
            SetForegroundWindow(h).as_bool()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        procs: Vec<Proc>,
        created: HashMap<u32, u64>,
        wins: Option<(bool, Vec<Win>)>,
        placed: RefCell<Vec<(isize, String)>>,
        reserved: RefCell<u32>,
        fail: HashSet<isize>,
    }

    impl Env for Fake {
        fn procs(&self) -> Vec<Proc> {
            self.procs.clone()
        }
        fn created_ms(&self, pid: u32) -> Option<u64> {
            self.created.get(&pid).copied()
        }
        fn windows(&self) -> Option<(bool, Vec<Win>)> {
            self.wins.clone()
        }
        fn reserve(&self, _: &str) -> Result<[f64; 4], String> {
            *self.reserved.borrow_mut() += 1;
            Ok([5000.0, 0.0, 1600.0, 900.0])
        }
        fn place(&self, hwnd: isize, name: &str) -> Result<(), String> {
            if self.fail.contains(&hwnd) {
                return Err("no".into());
            }
            self.placed.borrow_mut().push((hwnd, name.into()));
            Ok(())
        }
    }

    fn p(pid: u32, ppid: u32, exe: &str) -> Proc {
        Proc {
            pid,
            ppid,
            exe: exe.into(),
        }
    }
    fn w(hwnd: isize, pid: u32) -> Win {
        Win {
            hwnd,
            pid,
            title: format!("w{hwnd}"),
            hidden: false,
        }
    }

    /// The user's notepad (pid 10, old) and chrome (20, old); the agent
    /// launches Notepad at t=1000 -> pid 30 (new) with a child 31.
    fn world(canvas: bool) -> Fake {
        Fake {
            procs: vec![
                p(10, 1, "notepad.exe"),
                p(20, 1, "chrome.exe"),
                p(30, 5, "notepad.exe"),
                p(31, 30, "helper.exe"),
                p(32, 20, "chrome.exe"),
            ],
            created: HashMap::from([(10, 100), (20, 100), (30, 1200), (31, 1300), (32, 1250)]),
            wins: Some((
                canvas,
                vec![
                    w(0x10, 10),
                    w(0x20, 20),
                    w(0x30, 30),
                    w(0x31, 31),
                    w(0x32, 32),
                ],
            )),
            placed: Default::default(),
            reserved: Default::default(),
            fail: Default::default(),
        }
    }

    #[test]
    fn matches_exes_strictly() {
        assert!(exe_matches("Notepad", "notepad.exe"));
        assert!(exe_matches("Google Chrome", "chrome.exe"));
        assert!(exe_matches("code", "Code.exe"));
        assert!(!exe_matches("Excel", "explorer.exe"));
        assert!(!exe_matches("", "a.exe"));
        assert!(!exe_matches("Word", "w.exe"));
    }

    #[test]
    fn finds_pids() {
        assert_eq!(
            find_pid(r#"{"ok":true,"data":{"app":{"name":"Notepad","pid":4242}}}"#),
            Some(4242)
        );
        assert_eq!(find_pid(r#"{"ok":true,"data":[{"processId":7}]}"#), Some(7));
        assert_eq!(find_pid(r#"{"ok":true,"data":{}}"#), None);
        assert_eq!(find_pid("not json"), None);
    }

    #[test]
    fn places_only_what_the_agent_started() {
        let env = world(true);
        let mut b = Book::default();
        // Nothing launched yet: nothing happens, no region.
        assert!(b.tick(&env, "a1", &HashSet::new(), 1500).is_empty());
        assert_eq!(*env.reserved.borrow(), 0);

        b.note_launch(1000, "Notepad", None);
        let ev = b.tick(&env, "a1", &HashSet::new(), 1500);
        assert_eq!(ev[0], DeskEvent::Region([5000.0, 0.0, 1600.0, 900.0]));
        // The new notepad and its child, not the user's notepad or chrome
        // (chrome 32 is new but neither launched nor a child of the agent's apps).
        assert_eq!(b.hwnds, vec![0x30, 0x31]);
        assert_eq!(
            *env.placed.borrow(),
            vec![
                (0x30, "screenbuddy:agent:a1".to_string()),
                (0x31, "screenbuddy:agent:a1".to_string())
            ]
        );
        // Idempotent: a second pass places nothing and reserves once.
        assert!(b.tick(&env, "a1", &HashSet::new(), 1800).is_empty());
        assert_eq!(*env.reserved.borrow(), 1);
    }

    #[test]
    fn attach_to_users_app_moves_nothing() {
        let env = world(true);
        let mut b = Book::default();
        // agent-desktop attached to the user's running chrome (pid 20, old).
        b.note_launch(1000, "Google Chrome", Some(20));
        let ev = b.tick(&env, "a1", &HashSet::new(), 1500);
        assert!(ev.iter().all(|e| matches!(e, DeskEvent::Region(_))));
        assert!(b.hwnds.is_empty());
        assert!(!b.pids.contains_key(&20) && !b.pids.contains_key(&32));
    }

    #[test]
    fn reported_pid_and_late_windows() {
        let mut env = world(true);
        let mut b = Book::default();
        b.note_launch(1000, "Some App", Some(30));
        env.wins = Some((true, vec![]));
        b.tick(&env, "a1", &HashSet::new(), 1500);
        assert!(b.pids.contains_key(&30) && b.pids.contains_key(&31));
        // The window shows up later.
        env.wins = Some((true, vec![w(0x30, 30)]));
        b.tick(&env, "a1", &HashSet::new(), 3000);
        assert_eq!(b.hwnds, vec![0x30]);
    }

    #[test]
    fn launch_window_expires() {
        let env = world(true);
        let mut b = Book::default();
        b.note_launch(1000, "Notepad", None);
        // Far later the launch is forgotten and a matching process isn't adopted.
        b.adopt(&env.procs, |p| env.created.get(&p).copied(), 1000 + 60_000);
        assert!(b.pids.is_empty());
        // Created too long after the launch: not ours either.
        let mut b = Book::default();
        b.note_launch(1000, "Notepad", None);
        b.adopt(
            &[p(40, 1, "notepad.exe")],
            |_| Some(1000 + LAUNCH_SLACK_MS + 1),
            2000,
        );
        assert!(b.pids.is_empty());
    }

    #[test]
    fn other_agents_and_failures() {
        let mut env = world(true);
        env.fail.insert(0x31);
        let mut b = Book::default();
        b.note_launch(1000, "Notepad", None);
        let taken = HashSet::from([0x30]);
        b.tick(&env, "a2", &taken, 1500);
        assert!(b.hwnds.is_empty()); // 0x30 is another agent's, 0x31 failed
        for t in 0..5 {
            b.tick(&env, "a2", &taken, 1600 + t);
        }
        assert_eq!(b.tries[&0x31], MAX_TRIES);
        assert!(b.claim(&env.wins.as_ref().unwrap().1, &taken).is_empty());
    }

    #[test]
    fn tabs_only_tracks_without_moving() {
        let env = world(false);
        let mut b = Book::default();
        b.note_launch(1000, "Notepad", None);
        let ev = b.tick(&env, "a1", &HashSet::new(), 1500);
        assert_eq!(b.hwnds, vec![0x30, 0x31]);
        assert!(env.placed.borrow().is_empty());
        assert_eq!(*env.reserved.borrow(), 0);
        assert!(ev.iter().all(|e| matches!(e, DeskEvent::Placed { .. })));
        // Without Apeiron: nothing at all.
        let mut env = world(true);
        env.wins = None;
        let mut b = Book::default();
        b.note_launch(1000, "Notepad", None);
        assert!(b.tick(&env, "a1", &HashSet::new(), 1500).is_empty());
        assert!(b.hwnds.is_empty());
    }

    #[test]
    fn unions_rects() {
        assert_eq!(union(&[]), None);
        assert_eq!(
            union(&[[0.0, 0.0, 10.0, 10.0], [20.0, -5.0, 5.0, 5.0]]),
            Some([0.0, -5.0, 25.0, 15.0])
        );
    }

    #[test]
    fn parses_commands() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            command(&v(&["launch", "--activate", "Notepad"])),
            ("launch".into(), Some("Notepad".into()))
        );
        assert_eq!(
            command(&v(&[
                "launch",
                "--timeout",
                "500",
                "Excel",
                "--session",
                "s"
            ]))
            .1,
            Some("Excel".into())
        );
        assert_eq!(
            command(&v(&["launch", "--cdp", "9222", "Edge"])).1,
            Some("Edge".into())
        );
        assert_eq!(command(&v(&["--session", "x"])).0, "");
    }
}
