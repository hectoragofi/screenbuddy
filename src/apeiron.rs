//! Client for Apeiron, the owner's infinite-canvas window manager.
//!
//! Apeiron serves newline-delimited JSON on `\\.\pipe\apeiron-v1` (see
//! `docs/CLICKY_PLAN.md` §5). A background thread keeps one subscribed
//! connection open (retrying every 5 s while Apeiron is absent) and holds the
//! latest layout; requests open their own short-lived connection on a worker
//! so a slow or missing Apeiron never blocks the caller past its timeout.
//! Everything degrades to "not connected" when Apeiron isn't running.

use crate::capture::Rect;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

pub const PIPE: &str = r"\\.\pipe\apeiron-v1";
const RETRY: Duration = Duration::from_secs(5);
/// `focus_window` / `leave_stage` reply once the camera settles; cap the wait.
pub const SETTLE_TIMEOUT: Duration = Duration::from_millis(1500);
const QUICK_TIMEOUT: Duration = Duration::from_millis(600);

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Camera {
    pub x: f64,
    pub y: f64,
    pub zoom: f64,
}

/// One window Apeiron manages.
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub hwnd: isize,
    pub title: String,
    pub exe: String,
    /// `[x, y, w, h]` in canvas units.
    pub canvas_rect: [f64; 4],
    /// Physical screen pixels; None while the window isn't shown.
    pub screen_rect: Option<Rect>,
    pub on_screen: bool,
    pub tab_hidden: bool,
    pub away: bool,
}

impl Window {
    /// Shown on a monitor right now (not panned away, not a hidden tab).
    pub fn visible(&self) -> bool {
        self.on_screen && !self.tab_hidden && self.screen_rect.is_some()
    }
}

/// Latest known Apeiron state.
#[derive(Clone, Debug, Default)]
pub struct Layout {
    /// False in tabs-only mode.
    pub canvas: bool,
    pub camera: Camera,
    pub stage: bool,
    pub moving: bool,
    pub gesture: bool,
    /// A fresh `get_layout` is on its way (screen rects may be stale).
    pub refreshing: bool,
    pub windows: Vec<Window>,
    /// Bumped on every change that matters to followers (not per camera frame).
    pub generation: u64,
}

impl Layout {
    pub fn window(&self, hwnd: isize) -> Option<&Window> {
        self.windows.iter().find(|w| w.hwnd == hwnd)
    }

    /// Screen rects are in flux (camera moving, overview open, refresh pending).
    pub fn unsettled(&self) -> bool {
        self.moving || self.stage || self.refreshing
    }
}

/// A window and where it was on screen, captured with a screenshot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placed {
    pub hwnd: isize,
    pub rect: Rect,
}

type Waker = Arc<dyn Fn() + Send + Sync>;

/// The connection state for one pipe name (one global; tests make their own).
pub struct Client {
    pipe: String,
    state: Mutex<Option<Layout>>,
    wakers: Mutex<Vec<(u64, Waker)>>,
    next: AtomicU64,
    retry: Duration,
}

static CLIENT: OnceLock<Arc<Client>> = OnceLock::new();

/// Starts the background connection (idempotent).
pub fn start() {
    CLIENT.get_or_init(|| {
        let c = Client::new(PIPE, RETRY);
        c.clone().spawn();
        c
    });
}

fn client() -> Option<&'static Arc<Client>> {
    CLIENT.get()
}

/// Latest layout, or None when Apeiron isn't running.
pub fn get() -> Option<Layout> {
    client()?.get()
}

pub fn connected() -> bool {
    client().is_some_and(|c| c.connected())
}

pub fn stage_open() -> bool {
    client().is_some_and(|c| c.read(|l| l.stage))
}

/// Apeiron is in the middle of a mouse gesture (free-drag, pan); Alt-hold
/// voice should stand down while this is true.
pub fn gesture_active() -> bool {
    client().is_some_and(|c| c.read(|l| l.gesture))
}

fn need() -> Result<&'static Arc<Client>, String> {
    client()
        .filter(|c| c.connected())
        .ok_or_else(|| "Apeiron isn't running".to_string())
}

/// Flies the camera to the window, lands at 1:1 and activates it. Waits for
/// the "settled" reply (at most `SETTLE_TIMEOUT`).
pub fn focus_window(hwnd: isize) -> Result<(), String> {
    need()?
        .request("focus_window", json!({ "hwnd": hwnd }), SETTLE_TIMEOUT)
        .map(drop)
}

/// Drops the overview to 1:1; waits for it to settle.
pub fn leave_stage() -> Result<(), String> {
    need()?
        .request("leave_stage", json!({}), SETTLE_TIMEOUT)
        .map(drop)
}

#[allow(dead_code)] // used by the input + agents slices
pub fn fly_to(canvas_rect: [f64; 4]) -> Result<(), String> {
    need()?
        .request(
            "fly_to",
            json!({ "canvas_rect": canvas_rect }),
            SETTLE_TIMEOUT,
        )
        .map(drop)
}

/// Returns the camera to where it was before the last fly.
#[allow(dead_code)] // used by the input + agents slices
pub fn back() -> Result<(), String> {
    need()?.request("back", json!({}), SETTLE_TIMEOUT).map(drop)
}

/// A named canvas region (stable per name), away from the user's windows.
#[allow(dead_code)] // used by the input + agents slices
pub fn reserve_region(name: &str, w: f64, h: f64) -> Result<[f64; 4], String> {
    let d = need()?.request(
        "reserve_region",
        json!({ "name": name, "w": w, "h": h }),
        QUICK_TIMEOUT,
    )?;
    rect4(d.get("canvas_rect")).ok_or_else(|| "reserve_region: no canvas_rect".into())
}

/// Puts the window inside a reserved region without moving the camera.
#[allow(dead_code)] // used by the input + agents slices
pub fn place_window(hwnd: isize, region: &str) -> Result<(), String> {
    need()?
        .request(
            "place_window",
            json!({ "hwnd": hwnd, "region": region }),
            QUICK_TIMEOUT,
        )
        .map(drop)
}

/// Fetches the layout now (blocking up to ~0.6 s) and stores it.
pub fn refresh() -> Option<Layout> {
    let c = need().ok()?;
    c.refresh_now().ok()?;
    c.get()
}

/// Calls `f` (from the client thread) whenever the layout changes in a way a
/// follower cares about. Returns an id for `remove_waker`.
pub fn add_waker(f: impl Fn() + Send + Sync + 'static) -> u64 {
    start();
    client().map(|c| c.add_waker(Arc::new(f))).unwrap_or(0)
}

pub fn remove_waker(id: u64) {
    if let Some(c) = client() {
        c.wakers.lock().unwrap().retain(|(i, _)| *i != id);
    }
}

/// Apeiron windows visible on screen right now, topmost first. Empty (and
/// free) when Apeiron isn't running.
pub fn placed() -> Vec<Placed> {
    let Some(l) = get() else { return Vec::new() };
    let mut out: Vec<Placed> = Vec::new();
    unsafe {
        use windows::Win32::Foundation::HWND;
        use windows::Win32::UI::WindowsAndMessaging::{GW_HWNDNEXT, GetTopWindow, GetWindow};
        let mut h = GetTopWindow(None).unwrap_or_default();
        let mut guard = 0;
        while !h.0.is_null() && guard < 4096 {
            guard += 1;
            if let Some(w) = l.window(h.0 as isize).filter(|w| w.visible()) {
                out.push(Placed {
                    hwnd: w.hwnd,
                    rect: w.screen_rect.unwrap(),
                });
            }
            h = GetWindow(h, GW_HWNDNEXT).unwrap_or(HWND::default());
        }
    }
    out
}

/// The topmost placed window containing the point.
pub fn window_at(placed: &[Placed], x: f32, y: f32) -> Option<Placed> {
    placed.iter().copied().find(|p| {
        let r = p.rect;
        x >= r.x as f32 && y >= r.y as f32 && x < (r.x + r.w) as f32 && y < (r.y + r.h) as f32
    })
}

// ---------------------------------------------------------------- client

impl Client {
    pub fn new(pipe: &str, retry: Duration) -> Arc<Self> {
        Arc::new(Self {
            pipe: pipe.to_string(),
            state: Mutex::new(None),
            wakers: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
            retry,
        })
    }

    pub fn spawn(self: Arc<Self>) {
        let _ = std::thread::Builder::new()
            .name("apeiron".into())
            .spawn(move || {
                loop {
                    let _ = self.session();
                    if self.state.lock().unwrap().take().is_some() {
                        self.wake();
                    }
                    std::thread::sleep(self.retry);
                }
            });
    }

    pub fn get(&self) -> Option<Layout> {
        self.state.lock().unwrap().clone()
    }

    pub fn connected(&self) -> bool {
        self.state.lock().unwrap().is_some()
    }

    fn read(&self, f: impl Fn(&Layout) -> bool) -> bool {
        self.state.lock().unwrap().as_ref().is_some_and(f)
    }

    fn add_waker(&self, f: Waker) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        self.wakers.lock().unwrap().push((id, f));
        id
    }

    fn wake(&self) {
        let list: Vec<Waker> = self
            .wakers
            .lock()
            .unwrap()
            .iter()
            .map(|(_, f)| f.clone())
            .collect();
        for f in list {
            f();
        }
    }

    /// Applies `f` to the layout; bumps the generation and wakes followers if it says so.
    fn update(&self, f: impl FnOnce(&mut Layout) -> bool) {
        let changed = {
            let mut s = self.state.lock().unwrap();
            match s.as_mut() {
                Some(l) => {
                    let c = f(l);
                    if c {
                        l.generation += 1;
                    }
                    c
                }
                None => false,
            }
        };
        if changed {
            self.wake();
        }
    }

    /// One subscribed connection, until it drops.
    fn session(self: &Arc<Self>) -> Result<(), String> {
        let pipe = open_pipe(&self.pipe).map_err(|e| e.to_string())?;
        let mut w = pipe.try_clone().map_err(|e| e.to_string())?;
        let mut r = BufReader::new(pipe);
        let mut call = |id: u64, op: &str| -> Result<Value, String> {
            writeln!(w, "{}", json!({ "id": id, "op": op })).map_err(|e| e.to_string())?;
            read_reply(&mut r, id)
        };
        let hello = call(1, "hello")?;
        let layout = call(2, "get_layout")?;
        call(3, "subscribe")?;
        let mut l = parse_layout(&layout);
        l.canvas = hello.get("canvas").and_then(Value::as_bool).unwrap_or(true);
        l.generation = 1;
        *self.state.lock().unwrap() = Some(l);
        self.wake();
        let mut line = String::new();
        loop {
            line.clear();
            match r.read_line(&mut line) {
                Ok(0) | Err(_) => return Ok(()),
                Ok(_) => {}
            }
            let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            if v.get("event").is_some() {
                self.on_event(&v);
            }
        }
    }

    fn on_event(self: &Arc<Self>, v: &Value) {
        let mut refresh = false;
        match v.get("event").and_then(Value::as_str).unwrap_or("") {
            "camera" => {
                let moving = v.get("moving").and_then(Value::as_bool).unwrap_or(false);
                self.update(|l| {
                    l.camera = Camera {
                        x: num(v, "x"),
                        y: num(v, "y"),
                        zoom: v
                            .get("zoom")
                            .and_then(Value::as_f64)
                            .unwrap_or(l.camera.zoom),
                    };
                    if l.moving == moving {
                        return false;
                    }
                    l.moving = moving;
                    if !moving {
                        // Every window's screen rect changed: fetch them fresh.
                        l.refreshing = true;
                        refresh = true;
                    }
                    true
                });
            }
            "stage" => {
                let open = v.get("open").and_then(Value::as_bool).unwrap_or(false);
                self.update(|l| {
                    l.stage = open;
                    l.refreshing = true;
                    refresh = true;
                    true
                });
            }
            "gesture" => {
                let active = v.get("active").and_then(Value::as_bool).unwrap_or(false);
                self.update(|l| {
                    let c = l.gesture != active;
                    l.gesture = active;
                    c
                });
            }
            "window_moved" => {
                let Some(hwnd) = v.get("hwnd").and_then(as_hwnd) else {
                    return;
                };
                self.update(|l| {
                    if let Some(w) = l.windows.iter_mut().find(|w| w.hwnd == hwnd) {
                        if let Some(c) = rect4(v.get("canvas_rect")) {
                            w.canvas_rect = c;
                        }
                        w.screen_rect = rect_px(v.get("screen_rect"));
                        if w.screen_rect.is_none() {
                            w.on_screen = false;
                        }
                    } else {
                        refresh = true;
                    }
                    // Mid-flight moves are noise; only settled ones matter.
                    !l.moving
                });
            }
            "window_closed" => {
                let Some(hwnd) = v.get("hwnd").and_then(as_hwnd) else {
                    return;
                };
                self.update(|l| {
                    let n = l.windows.len();
                    l.windows.retain(|w| w.hwnd != hwnd);
                    n != l.windows.len()
                });
            }
            "tab_switch" | "focus" => {
                refresh = true;
                self.update(|l| {
                    l.refreshing = true;
                    true
                });
            }
            _ => {}
        }
        if refresh {
            let me = self.clone();
            std::thread::spawn(move || {
                if me.refresh_now().is_err() {
                    me.update(|l| std::mem::replace(&mut l.refreshing, false));
                }
            });
        }
    }

    fn refresh_now(&self) -> Result<(), String> {
        let d = self.request("get_layout", json!({}), QUICK_TIMEOUT)?;
        let fresh = parse_layout(&d);
        self.update(|l| {
            l.windows = fresh.windows;
            l.stage = fresh.stage;
            l.moving = fresh.moving;
            l.camera = fresh.camera;
            l.refreshing = false;
            true
        });
        Ok(())
    }

    /// Sends one request on its own connection; waits at most `timeout`.
    pub fn request(&self, op: &str, mut args: Value, timeout: Duration) -> Result<Value, String> {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        if let Value::Object(m) = &mut args {
            m.insert("id".into(), json!(id));
            m.insert("op".into(), json!(op));
        }
        let pipe = self.pipe.clone();
        let (tx, rx) = channel();
        let worker = move || -> Result<Value, String> {
            let f = open_pipe(&pipe).map_err(|e| format!("Apeiron pipe: {e}"))?;
            let mut w = f.try_clone().map_err(|e| e.to_string())?;
            writeln!(w, "{args}").map_err(|e| e.to_string())?;
            read_reply(&mut BufReader::new(f), id)
        };
        // The worker may outlive a timeout; it ends when Apeiron answers or hangs up.
        std::thread::spawn(move || {
            let _ = tx.send(worker());
        });
        rx.recv_timeout(timeout)
            .unwrap_or_else(|_| Err(format!("Apeiron didn't answer {op} in time")))
    }
}

fn open_pipe(name: &str) -> std::io::Result<std::fs::File> {
    let mut tries = 0;
    loop {
        match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(name)
        {
            Ok(f) => return Ok(f),
            // ERROR_PIPE_BUSY: every instance is taken. ERROR_FILE_NOT_FOUND can
            // also mean the server is between instances, so retry that briefly.
            Err(e)
                if tries < 10
                    && (e.raw_os_error() == Some(231)
                        || (tries < 3 && e.raw_os_error() == Some(2))) =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return Err(e),
        }
    }
}

/// Reads lines until the reply to `id`, skipping events.
fn read_reply(r: &mut impl BufRead, id: u64) -> Result<Value, String> {
    let mut line = String::new();
    loop {
        line.clear();
        if r.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            return Err("Apeiron closed the pipe".into());
        }
        let Ok(v) = serde_json::from_str::<Value>(line.trim()) else {
            continue;
        };
        if v.get("id").and_then(Value::as_u64) != Some(id) {
            continue;
        }
        return if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Err(v
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("Apeiron refused")
                .to_string())
        };
    }
}

// ---------------------------------------------------------------- parsing

fn num(v: &Value, k: &str) -> f64 {
    v.get(k).and_then(Value::as_f64).unwrap_or(0.0)
}

/// Accepts an hwnd as a number or a (decimal / 0x-hex) string.
fn as_hwnd(v: &Value) -> Option<isize> {
    match v {
        Value::Number(n) => n.as_i64().map(|n| n as isize),
        Value::String(s) => {
            let s = s.trim();
            match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                Some(h) => i64::from_str_radix(h, 16).ok().map(|n| n as isize),
                None => s.parse::<i64>().ok().map(|n| n as isize),
            }
        }
        _ => None,
    }
}

fn rect4(v: Option<&Value>) -> Option<[f64; 4]> {
    let a = v?.as_array()?;
    if a.len() != 4 {
        return None;
    }
    let mut out = [0.0; 4];
    for (o, x) in out.iter_mut().zip(a) {
        *o = x.as_f64()?;
    }
    Some(out)
}

fn rect_px(v: Option<&Value>) -> Option<Rect> {
    let [x, y, w, h] = rect4(v)?;
    Some(Rect {
        x: x.round() as i32,
        y: y.round() as i32,
        w: w.round() as i32,
        h: h.round() as i32,
    })
}

pub fn parse_layout(d: &Value) -> Layout {
    let cam = d.get("camera").cloned().unwrap_or(Value::Null);
    let windows = d
        .get("windows")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|w| {
                    let screen_rect = rect_px(w.get("screen_rect"));
                    Some(Window {
                        hwnd: as_hwnd(w.get("hwnd")?)?,
                        title: w
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        exe: w
                            .get("exe")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                        canvas_rect: rect4(w.get("canvas_rect")).unwrap_or_default(),
                        on_screen: w
                            .get("on_screen")
                            .and_then(Value::as_bool)
                            .unwrap_or(screen_rect.is_some()),
                        screen_rect,
                        tab_hidden: w
                            .get("tab_hidden")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        away: w.get("away").and_then(Value::as_bool).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Layout {
        canvas: true,
        camera: Camera {
            x: num(&cam, "x"),
            y: num(&cam, "y"),
            zoom: cam.get("zoom").and_then(Value::as_f64).unwrap_or(1.0),
        },
        stage: d.get("stage").and_then(Value::as_bool).unwrap_or(false),
        moving: d.get("moving").and_then(Value::as_bool).unwrap_or(false),
        gesture: false,
        refreshing: false,
        windows,
        generation: 0,
    }
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
pub(crate) mod mock {
    //! An in-process pipe server speaking the §5 wire format.
    use serde_json::{Value, json};
    use std::io::{BufRead, BufReader, Write};
    use std::os::windows::io::FromRawHandle;
    use std::sync::{Arc, Mutex};
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX};
    use windows::Win32::System::Pipes::*;
    use windows::core::PCWSTR;

    pub struct Mock {
        pub name: String,
        /// Requests seen, in order (op names).
        pub ops: Arc<Mutex<Vec<String>>>,
        /// Connections that subscribed; events pushed with `push`.
        subs: Arc<Mutex<Vec<std::fs::File>>>,
        pub layout: Arc<Mutex<Value>>,
    }

    impl Mock {
        pub fn start(tag: &str, layout: Value) -> Mock {
            let name = format!(r"\\.\pipe\screenbuddy-test-{tag}-{}", std::process::id());
            let m = Mock {
                name: name.clone(),
                ops: Default::default(),
                subs: Default::default(),
                layout: Arc::new(Mutex::new(layout)),
            };
            let (ops, subs, layout) = (m.ops.clone(), m.subs.clone(), m.layout.clone());
            let ready = Arc::new(std::sync::Barrier::new(2));
            let r2 = ready.clone();
            std::thread::spawn(move || {
                let wide: Vec<u16> = name.encode_utf16().chain([0]).collect();
                let mut first = true;
                loop {
                    let flags = if first {
                        PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
                    } else {
                        PIPE_ACCESS_DUPLEX
                    };
                    let h = unsafe {
                        CreateNamedPipeW(
                            PCWSTR(wide.as_ptr()),
                            flags,
                            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                            PIPE_UNLIMITED_INSTANCES,
                            65536,
                            65536,
                            0,
                            None,
                        )
                    };
                    if first {
                        first = false;
                        r2.wait();
                    }
                    if h.is_invalid() {
                        return;
                    }
                    let _ = unsafe { ConnectNamedPipe(h, None) };
                    let file = unsafe { std::fs::File::from_raw_handle(h.0 as _) };
                    let (ops, subs, layout) = (ops.clone(), subs.clone(), layout.clone());
                    std::thread::spawn(move || serve(file, ops, subs, layout));
                }
            });
            ready.wait();
            m
        }

        pub fn push(&self, ev: Value) {
            for s in self.subs.lock().unwrap().iter_mut() {
                let _ = writeln!(s, "{ev}");
            }
        }

        /// Hangs up every subscriber.
        pub fn drop_subscribers(&self) {
            use std::os::windows::io::AsRawHandle;
            for s in self.subs.lock().unwrap().drain(..) {
                let _ = unsafe { DisconnectNamedPipe(HANDLE(s.as_raw_handle() as _)) };
            }
        }
    }

    fn serve(
        file: std::fs::File,
        ops: Arc<Mutex<Vec<String>>>,
        subs: Arc<Mutex<Vec<std::fs::File>>>,
        layout: Arc<Mutex<Value>>,
    ) {
        let mut w = file.try_clone().unwrap();
        let mut r = BufReader::new(file);
        let mut line = String::new();
        loop {
            line.clear();
            if r.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            let Ok(req) = serde_json::from_str::<Value>(line.trim()) else {
                continue;
            };
            let id = req["id"].clone();
            let op = req["op"].as_str().unwrap_or("").to_string();
            ops.lock().unwrap().push(op.clone());
            let reply = match op.as_str() {
                "hello" => {
                    json!({ "id": id, "ok": true, "data": { "version": 1, "canvas": true } })
                }
                "get_layout" => {
                    json!({ "id": id, "ok": true, "data": layout.lock().unwrap().clone() })
                }
                "subscribe" => {
                    let _ = writeln!(w, "{}", json!({ "id": id, "ok": true, "data": {} }));
                    // Stop reading: a pending synchronous read on this pipe would
                    // block `push`'s writes. (The client sends nothing more here.)
                    subs.lock().unwrap().push(w);
                    std::mem::forget(r);
                    return;
                }
                "focus_window" | "leave_stage" | "back" | "fly_to" | "place_window" => {
                    if op == "leave_stage" {
                        layout.lock().unwrap()["stage"] = json!(false);
                    }
                    std::thread::sleep(std::time::Duration::from_millis(30));
                    json!({ "id": id, "ok": true, "data": {} })
                }
                "reserve_region" => {
                    json!({ "id": id, "ok": true, "data": { "canvas_rect": [5000, 0, req["w"], req["h"]] } })
                }
                "hang" => continue,
                _ => json!({ "id": id, "ok": false, "error": "unknown op" }),
            };
            // An event first, to check clients skip events while waiting for replies.
            let _ = writeln!(
                w,
                "{}",
                json!({ "event": "camera", "x": 0, "y": 0, "zoom": 1, "moving": false })
            );
            let _ = writeln!(w, "{reply}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::mock::Mock;
    use super::*;
    use std::time::Instant;

    fn layout() -> Value {
        json!({
            "camera": { "x": 0, "y": 0, "zoom": 1 },
            "stage": false,
            "moving": false,
            "windows": [
                { "hwnd": 100, "title": "Notes", "exe": "notepad.exe", "canvas_rect": [0, 0, 800, 600], "screen_rect": [10, 20, 800, 600], "on_screen": true, "tab_hidden": false, "away": false },
                { "hwnd": "0x2A", "title": "Hidden tab", "exe": "code.exe", "canvas_rect": [0, 0, 800, 600], "screen_rect": null, "on_screen": false, "tab_hidden": true, "away": false }
            ]
        })
    }

    fn wait(mut f: impl FnMut() -> bool) -> bool {
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(3) {
            if f() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn parses_layout() {
        let l = parse_layout(&layout());
        assert_eq!(l.windows.len(), 2);
        assert_eq!(
            l.windows[0].screen_rect,
            Some(Rect {
                x: 10,
                y: 20,
                w: 800,
                h: 600
            })
        );
        assert!(l.windows[0].visible());
        assert_eq!(l.windows[1].hwnd, 42);
        assert!(!l.windows[1].visible());
    }

    #[test]
    fn subscribes_and_follows_events() {
        let m = Mock::start("follow", layout());
        let c = Client::new(&m.name, Duration::from_millis(50));
        c.clone().spawn();
        assert!(wait(|| c.connected()), "never connected");
        assert_eq!(
            m.ops.lock().unwrap()[..3],
            ["hello", "get_layout", "subscribe"]
        );
        let woke = Arc::new(AtomicU64::new(0));
        let w2 = woke.clone();
        c.add_waker(Arc::new(move || {
            w2.fetch_add(1, Ordering::Relaxed);
        }));
        // Camera starts moving: one change, then frames are coalesced away.
        m.push(json!({ "event": "camera", "x": 5, "y": 0, "zoom": 1, "moving": true }));
        assert!(wait(|| c.read(|l| l.moving)));
        let g = c.get().unwrap().generation;
        m.push(json!({ "event": "camera", "x": 9, "y": 0, "zoom": 1, "moving": true }));
        assert!(wait(|| c.get().unwrap().camera.x == 9.0));
        assert_eq!(
            c.get().unwrap().generation,
            g,
            "per-frame camera events don't bump"
        );
        // Settles: refreshed from get_layout with the new rects.
        m.layout.lock().unwrap()["windows"][0]["screen_rect"] = json!([110, 20, 800, 600]);
        m.push(json!({ "event": "camera", "x": 9, "y": 0, "zoom": 1, "moving": false }));
        assert!(wait(|| {
            let l = c.get().unwrap();
            !l.unsettled() && l.window(100).unwrap().screen_rect.unwrap().x == 110
        }));
        m.push(json!({ "event": "gesture", "active": true }));
        assert!(wait(|| c.read(|l| l.gesture)));
        m.push(json!({ "event": "window_closed", "hwnd": 100 }));
        assert!(wait(|| c.get().unwrap().window(100).is_none()));
        assert!(woke.load(Ordering::Relaxed) >= 3);
        // Apeiron goes away: disconnected, then reconnects.
        m.drop_subscribers();
        assert!(
            wait(|| m
                .ops
                .lock()
                .unwrap()
                .iter()
                .filter(|o| *o == "hello")
                .count()
                >= 2),
            "never reconnected"
        );
        assert!(wait(|| c.connected()));
    }

    #[test]
    fn requests_reply_and_time_out() {
        let m = Mock::start("req", layout());
        let c = Client::new(&m.name, Duration::from_millis(50));
        let d = c
            .request(
                "reserve_region",
                json!({ "name": "desk", "w": 640, "h": 480 }),
                QUICK_TIMEOUT,
            )
            .unwrap();
        assert_eq!(
            rect4(d.get("canvas_rect")),
            Some([5000.0, 0.0, 640.0, 480.0])
        );
        assert!(
            c.request("focus_window", json!({ "hwnd": 100 }), SETTLE_TIMEOUT)
                .is_ok()
        );
        assert_eq!(
            c.request("nope", json!({}), QUICK_TIMEOUT).unwrap_err(),
            "unknown op"
        );
        let t = Instant::now();
        assert!(
            c.request("hang", json!({}), Duration::from_millis(200))
                .is_err()
        );
        assert!(t.elapsed() < Duration::from_millis(600));
    }

    #[test]
    fn absent_apeiron_fails_fast() {
        let c = Client::new(
            r"\\.\pipe\screenbuddy-test-nobody-home",
            Duration::from_secs(5),
        );
        let t = Instant::now();
        assert!(c.request("hello", json!({}), SETTLE_TIMEOUT).is_err());
        assert!(t.elapsed() < Duration::from_millis(500));
        assert!(!c.connected());
    }

    #[test]
    fn window_at_prefers_topmost() {
        let a = Placed {
            hwnd: 1,
            rect: Rect {
                x: 0,
                y: 0,
                w: 100,
                h: 100,
            },
        };
        let b = Placed {
            hwnd: 2,
            rect: Rect {
                x: 50,
                y: 50,
                w: 100,
                h: 100,
            },
        };
        assert_eq!(window_at(&[b, a], 60.0, 60.0).unwrap().hwnd, 2);
        assert_eq!(window_at(&[b, a], 10.0, 10.0).unwrap().hwnd, 1);
        assert!(window_at(&[b, a], 300.0, 10.0).is_none());
    }
}
