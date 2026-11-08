//! Alt-key gestures from a low-level keyboard hook.
//!
//! - **Hold Alt alone** for `input.alt_hold_ms` → [`KeyEvent::HoldStart`];
//!   releasing it → [`KeyEvent::HoldEnd`]. Any other key, a mouse button or the
//!   wheel while Alt is down (Apeiron drags windows with Alt), or
//!   [`set_suppressed`], turns the gesture into [`KeyEvent::HoldCancel`] or
//!   keeps it from starting.
//! - **Double-tap Alt** (two lone taps, < 300 ms apart) → [`KeyEvent::DoubleTap`].
//!
//! Nothing is ever swallowed: every key passes on to the next hook (Apeiron's
//! Alt+Tab switcher) and the app. Once a hold starts, an unassigned key
//! (VK 0xE8) is injected while Alt is still down, so the focused app doesn't
//! treat the release as a lone Alt and open its menu bar. Injected events,
//! ours or anyone's, are ignored.
//!
//! The hook thread does nothing but this; the mouse hook is installed only
//! while Alt is down, so mouse input never pays for it otherwise.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Two taps closer than this (release to press) are a double tap; a tap is
/// also at most this long.
pub const TAP_MS: u32 = 300;
/// `dwExtraInfo` on the keys we inject ("SBKY").
pub const TAG: usize = 0x5342_4B59;
/// Unassigned virtual key used to mask a lone Alt.
const VK_MASK: u16 = 0xE8;

/// What the user did with Alt. `cursor` is in physical screen px; `target` is
/// what [`set_cursor_target`] found under the cursor (e.g. an agent card).
#[derive(Debug, Clone, PartialEq)]
pub enum KeyEvent {
    HoldStart {
        cursor: (i32, i32),
        target: Option<String>,
    },
    HoldEnd {
        cursor: (i32, i32),
        target: Option<String>,
    },
    HoldCancel,
    DoubleTap {
        cursor: (i32, i32),
    },
}

// ---------------------------------------------------------------- state machine

/// What the hook saw, with times in ms (`KBDLLHOOKSTRUCT::time` domain).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Input {
    /// Alt went down; `other_held` = a modifier or mouse button was already down.
    AltDown {
        t: u32,
        other_held: bool,
    },
    AltUp {
        t: u32,
    },
    /// Any non-Alt key, mouse button or wheel.
    Other,
    /// The hold timer fired.
    Tick {
        t: u32,
    },
    /// Gestures got suppressed (Apeiron gesture, `set_suppressed(true)`).
    Suppress,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Action {
    HoldStart,
    HoldEnd,
    HoldCancel,
    DoubleTap,
    /// Inject the mask key now.
    Mask,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Idle,
    /// Alt is down; `clean` while nothing else happened; `second` if this press
    /// follows a lone tap closely enough to be a double tap.
    Down {
        since: u32,
        clean: bool,
        second: bool,
    },
    Holding,
    /// The hold was cancelled; waiting for Alt to come up.
    Spent,
}

#[derive(Debug, Clone)]
pub struct Machine {
    pub hold_ms: u32,
    pub hold: bool,
    pub double_tap: bool,
    state: State,
    /// Release time of the last lone tap.
    last_tap: Option<u32>,
}

impl Machine {
    pub fn new(hold_ms: u32, hold: bool, double_tap: bool) -> Self {
        Self {
            hold_ms,
            hold,
            double_tap,
            state: State::Idle,
            last_tap: None,
        }
    }

    /// True while Alt is down (so the hook wants mouse input and the timer).
    pub fn alt_down(&self) -> bool {
        !matches!(self.state, State::Idle)
    }

    pub fn feed(&mut self, input: Input) -> Vec<Action> {
        let mut out = Vec::new();
        match (input, self.state) {
            // Auto-repeat while held.
            (Input::AltDown { .. }, s) if s != State::Idle => {}
            (Input::AltDown { t, other_held }, _) => {
                let second = !other_held
                    && self.double_tap
                    && self.last_tap.is_some_and(|u| t.wrapping_sub(u) < TAP_MS);
                if second {
                    // Keep this second press from toggling the menu bar back.
                    out.push(Action::Mask);
                }
                self.last_tap = None;
                self.state = State::Down {
                    since: t,
                    clean: !other_held,
                    second,
                };
            }
            (
                Input::Tick { t },
                State::Down {
                    since,
                    clean: true,
                    second: false,
                },
            ) if self.hold && t.wrapping_sub(since) >= self.hold_ms => {
                self.state = State::Holding;
                out.extend([Action::Mask, Action::HoldStart]);
            }
            (Input::Tick { .. }, _) => {}
            (Input::Other | Input::Suppress, State::Down { since, second, .. }) => {
                self.state = State::Down {
                    since,
                    clean: false,
                    second,
                };
                self.last_tap = None;
            }
            (Input::Other | Input::Suppress, State::Holding) => {
                self.state = State::Spent;
                out.push(Action::HoldCancel);
            }
            (Input::Other, State::Idle) => self.last_tap = None,
            (Input::Other | Input::Suppress, _) => {}
            (
                Input::AltUp { t },
                State::Down {
                    since,
                    clean,
                    second,
                },
            ) => {
                let tap = clean && t.wrapping_sub(since) < TAP_MS;
                if tap && second {
                    out.push(Action::DoubleTap);
                } else if tap {
                    self.last_tap = Some(t);
                }
                self.state = State::Idle;
            }
            (Input::AltUp { .. }, State::Holding) => {
                self.state = State::Idle;
                out.extend([Action::Mask, Action::HoldEnd]);
            }
            (Input::AltUp { .. }, _) => self.state = State::Idle,
        }
        out
    }
}

// ---------------------------------------------------------------- public API

type Sink = Box<dyn Fn(KeyEvent) + Send + Sync>;
type TargetFn = Box<dyn Fn((i32, i32)) -> Option<String> + Send + Sync>;

static SINK: OnceLock<Sink> = OnceLock::new();
static TARGET: Mutex<Option<TargetFn>> = Mutex::new(None);
static SUPPRESSED: AtomicBool = AtomicBool::new(false);
static HOLD: AtomicBool = AtomicBool::new(true);
static DOUBLE_TAP: AtomicBool = AtomicBool::new(true);
static HOLD_MS: AtomicU32 = AtomicU32::new(600);
static THREAD: AtomicU32 = AtomicU32::new(0);
const WM_SUPPRESS: u32 = WM_APP + 7;

/// Starts the hook thread (once); `sink` runs on it, so keep it short (send on a channel).
pub fn start(sink: impl Fn(KeyEvent) + Send + Sync + 'static) {
    if SINK.set(Box::new(sink)).is_err() {
        return;
    }
    std::thread::Builder::new()
        .name("screenbuddy-keys".into())
        .spawn(hook_thread)
        .ok();
}

/// Applies `[input]` settings; takes effect on the next Alt press.
pub fn configure(hold: bool, hold_ms: u32, double_tap: bool) {
    HOLD.store(hold, Ordering::Relaxed);
    DOUBLE_TAP.store(double_tap, Ordering::Relaxed);
    HOLD_MS.store(hold_ms.clamp(200, 3000), Ordering::Relaxed);
}

/// While true, Alt gestures don't start, and one in progress is cancelled
/// (Apeiron calls this while a drag/pan gesture runs).
#[allow(dead_code)] // wired by the Apeiron / island / cards slices
pub fn set_suppressed(on: bool) {
    SUPPRESSED.store(on, Ordering::Relaxed);
    let t = THREAD.load(Ordering::Relaxed);
    if on && t != 0 {
        unsafe {
            let _ = PostThreadMessageW(t, WM_SUPPRESS, WPARAM(0), LPARAM(0));
        }
    }
}

/// Hook point: maps the cursor position to something under it (e.g. an agent
/// card id), reported as `target` on hold events. Runs on the hook thread at
/// hold start and end, so it must be fast and must not block.
#[allow(dead_code)] // wired by the Apeiron / island / cards slices
pub fn set_cursor_target(f: impl Fn((i32, i32)) -> Option<String> + Send + Sync + 'static) {
    *TARGET.lock().unwrap() = Some(Box::new(f));
}

/// What [`set_cursor_target`] reports for the current cursor position.
pub fn cursor_target() -> ((i32, i32), Option<String>) {
    let mut p = POINT::default();
    unsafe {
        let _ = GetCursorPos(&mut p);
    }
    let at = (p.x, p.y);
    let target = TARGET.lock().unwrap().as_ref().and_then(|f| f(at));
    (at, target)
}

// ---------------------------------------------------------------- hook thread

struct Hook {
    machine: Machine,
    mouse: Option<HHOOK>,
    timer: usize,
    /// Foreground window when Alt went down. If it changes before the hold
    /// starts, another hook (Apeiron's Alt+Tab switcher) swallowed a key we
    /// never saw, so this isn't a lone Alt.
    fg: isize,
}

thread_local! {
    static HOOK: std::cell::RefCell<Option<Hook>> = const { std::cell::RefCell::new(None) };
}

fn hook_thread() {
    unsafe {
        THREAD.store(
            windows::Win32::System::Threading::GetCurrentThreadId(),
            Ordering::Relaxed,
        );
        let m = Machine::new(
            HOLD_MS.load(Ordering::Relaxed),
            HOLD.load(Ordering::Relaxed),
            DOUBLE_TAP.load(Ordering::Relaxed),
        );
        HOOK.with(|h| {
            *h.borrow_mut() = Some(Hook {
                machine: m,
                mouse: None,
                timer: 0,
                fg: 0,
            })
        });
        let kb = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(kb_proc), module(), 0) {
            Ok(h) => h,
            Err(e) => {
                crate::config::log(&format!("keyboard hook failed: {e}"));
                return;
            }
        };
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            match msg.message {
                WM_TIMER if msg.hwnd.0.is_null() => {
                    // A release we never saw (secure desktop, elevated window)
                    // must not leave the gesture stuck.
                    if held(VK_MENU) {
                        if foreground_changed() {
                            feed(Input::Other);
                        }
                        feed(Input::Tick { t: msg.time });
                    } else {
                        feed(Input::AltUp { t: msg.time });
                    }
                }
                WM_SUPPRESS => feed(Input::Suppress),
                _ => {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }
        let _ = UnhookWindowsHookEx(kb);
    }
}

fn module() -> Option<HINSTANCE> {
    unsafe {
        windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
            .ok()
            .map(|m| HINSTANCE(m.0))
    }
}

/// Timer period while Alt is down: hold resolution and missed-release check.
const POLL_MS: u32 = 50;

fn foreground_changed() -> bool {
    let now = unsafe { GetForegroundWindow().0 as isize };
    HOOK.with(|h| h.borrow().as_ref().is_some_and(|h| h.fg != now))
}

/// Runs the state machine and carries out its actions.
fn feed(input: Input) {
    let actions = HOOK.with(|h| {
        let mut h = h.borrow_mut();
        let Some(h) = h.as_mut() else {
            return Vec::new();
        };
        if let Input::AltDown { .. } = input {
            if !h.machine.alt_down() {
                // Pick up settings changes at the start of each press.
                h.machine.hold = HOLD.load(Ordering::Relaxed);
                h.machine.double_tap = DOUBLE_TAP.load(Ordering::Relaxed);
                h.machine.hold_ms = HOLD_MS.load(Ordering::Relaxed);
            }
        }
        let was_down = h.machine.alt_down();
        let actions = h.machine.feed(input);
        let down = h.machine.alt_down();
        unsafe {
            if down && !was_down {
                h.timer = SetTimer(None, 0, POLL_MS, None);
                h.fg = GetForegroundWindow().0 as isize;
                h.mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), module(), 0).ok();
            } else if !down && was_down {
                if h.timer != 0 {
                    let _ = KillTimer(None, h.timer);
                    h.timer = 0;
                }
                if let Some(m) = h.mouse.take() {
                    let _ = UnhookWindowsHookEx(m);
                }
            }
        }
        actions
    });
    for a in actions {
        match a {
            Action::Mask => inject_mask(),
            Action::HoldStart => {
                let (cursor, target) = cursor_target();
                emit(KeyEvent::HoldStart { cursor, target });
            }
            Action::HoldEnd => {
                let (cursor, target) = cursor_target();
                emit(KeyEvent::HoldEnd { cursor, target });
            }
            Action::HoldCancel => emit(KeyEvent::HoldCancel),
            Action::DoubleTap => emit(KeyEvent::DoubleTap {
                cursor: cursor_target().0,
            }),
        }
    }
}

fn emit(ev: KeyEvent) {
    if let Some(s) = SINK.get() {
        s(ev);
    }
}

fn inject_mask() {
    let key = |flags| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(VK_MASK),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: TAG,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

fn held(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// Something that makes this Alt press part of a chord or a drag.
fn other_held() -> bool {
    [
        VK_CONTROL,
        VK_SHIFT,
        VK_LWIN,
        VK_RWIN,
        VK_LBUTTON,
        VK_RBUTTON,
        VK_MBUTTON,
        VK_XBUTTON1,
        VK_XBUTTON2,
    ]
    .into_iter()
    .any(held)
}

unsafe extern "system" fn kb_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
            let injected = k.flags.0 & (LLKHF_INJECTED.0 | LLKHF_LOWER_IL_INJECTED.0) != 0;
            if !injected && k.dwExtraInfo != TAG {
                let down = matches!(wp.0 as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
                let alt = matches!(VIRTUAL_KEY(k.vkCode as u16), VK_MENU | VK_LMENU | VK_RMENU);
                let input = match (alt, down) {
                    (true, true) => Some(Input::AltDown {
                        t: k.time,
                        other_held: other_held() || SUPPRESSED.load(Ordering::Relaxed),
                    }),
                    (true, false) => Some(Input::AltUp { t: k.time }),
                    (false, true) => Some(Input::Other),
                    (false, false) => None,
                };
                if let Some(i) = input {
                    feed(i);
                }
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        if code == HC_ACTION as i32 {
            let m = &*(lp.0 as *const MSLLHOOKSTRUCT);
            let injected = m.flags & (LLMHF_INJECTED | LLMHF_LOWER_IL_INJECTED) != 0;
            let press = matches!(
                wp.0 as u32,
                WM_LBUTTONDOWN
                    | WM_RBUTTONDOWN
                    | WM_MBUTTONDOWN
                    | WM_XBUTTONDOWN
                    | WM_MOUSEWHEEL
                    | WM_MOUSEHWHEEL
            );
            if press && !injected {
                feed(Input::Other);
            }
        }
        CallNextHookEx(None, code, wp, lp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m() -> Machine {
        Machine::new(600, true, true)
    }
    fn down(t: u32) -> Input {
        Input::AltDown {
            t,
            other_held: false,
        }
    }

    #[test]
    fn hold_starts_after_threshold_and_ends_on_release() {
        let mut k = m();
        assert!(k.feed(down(1000)).is_empty());
        assert!(k.feed(Input::Tick { t: 1500 }).is_empty(), "too early");
        // Auto-repeat doesn't restart the clock.
        assert!(k.feed(down(1550)).is_empty());
        assert_eq!(
            k.feed(Input::Tick { t: 1600 }),
            vec![Action::Mask, Action::HoldStart]
        );
        assert!(k.feed(Input::Tick { t: 1700 }).is_empty(), "fires once");
        assert_eq!(
            k.feed(Input::AltUp { t: 2400 }),
            vec![Action::Mask, Action::HoldEnd]
        );
        assert!(!k.alt_down());
    }

    #[test]
    fn other_key_before_threshold_means_no_hold() {
        // Alt+Tab, Alt+F4, ...
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::Other);
        assert!(k.feed(Input::Tick { t: 700 }).is_empty());
        assert!(k.feed(Input::AltUp { t: 900 }).is_empty());
    }

    #[test]
    fn other_key_or_mouse_during_hold_cancels() {
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::Tick { t: 600 });
        assert_eq!(k.feed(Input::Other), vec![Action::HoldCancel]);
        assert!(k.feed(Input::Other).is_empty(), "cancels once");
        assert!(
            k.feed(Input::AltUp { t: 900 }).is_empty(),
            "no HoldEnd after cancel"
        );
    }

    #[test]
    fn suppressed_or_chorded_alt_never_holds() {
        let mut k = m();
        k.feed(Input::AltDown {
            t: 0,
            other_held: true,
        }); // Ctrl+Alt, or mid-drag
        assert!(k.feed(Input::Tick { t: 800 }).is_empty());
        k.feed(Input::AltUp { t: 900 });

        k.feed(down(1000));
        k.feed(Input::Suppress);
        assert!(k.feed(Input::Tick { t: 1700 }).is_empty());
        k.feed(Input::AltUp { t: 1800 });

        k.feed(down(2000));
        k.feed(Input::Tick { t: 2600 });
        assert_eq!(k.feed(Input::Suppress), vec![Action::HoldCancel]);
    }

    #[test]
    fn hold_disabled() {
        let mut k = Machine::new(600, false, true);
        k.feed(down(0));
        assert!(k.feed(Input::Tick { t: 5000 }).is_empty());
        assert!(k.feed(Input::AltUp { t: 5000 }).is_empty());
    }

    #[test]
    fn double_tap() {
        let mut k = m();
        k.feed(down(0));
        assert!(k.feed(Input::AltUp { t: 80 }).is_empty());
        assert_eq!(k.feed(down(250)), vec![Action::Mask]);
        assert_eq!(k.feed(Input::AltUp { t: 320 }), vec![Action::DoubleTap]);
        // A third tap starts over rather than firing again.
        assert!(k.feed(down(400)).is_empty());
        assert!(k.feed(Input::AltUp { t: 450 }).is_empty());
    }

    #[test]
    fn slow_or_dirty_taps_are_not_double() {
        let mut k = m();
        // Too far apart.
        k.feed(down(0));
        k.feed(Input::AltUp { t: 80 });
        k.feed(down(500));
        assert!(k.feed(Input::AltUp { t: 560 }).is_empty());
        // First press too long.
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::AltUp { t: 450 });
        k.feed(down(500));
        assert!(k.feed(Input::AltUp { t: 560 }).is_empty());
        // A key between the taps.
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::AltUp { t: 80 });
        k.feed(Input::Other);
        k.feed(down(150));
        assert!(k.feed(Input::AltUp { t: 200 }).is_empty());
        // Alt+key as the second press.
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::AltUp { t: 80 });
        k.feed(down(150));
        k.feed(Input::Other);
        assert!(k.feed(Input::AltUp { t: 200 }).is_empty());
        // Double tap turned off.
        let mut k = Machine::new(600, true, false);
        k.feed(down(0));
        k.feed(Input::AltUp { t: 80 });
        assert!(k.feed(down(150)).is_empty());
        assert!(k.feed(Input::AltUp { t: 200 }).is_empty());
    }

    #[test]
    fn second_press_held_does_not_hold() {
        // Tap then hold: the second press is a would-be double tap; holding it
        // shouldn't record (it was masked already and is ambiguous).
        let mut k = m();
        k.feed(down(0));
        k.feed(Input::AltUp { t: 80 });
        k.feed(down(200));
        assert!(k.feed(Input::Tick { t: 800 }).is_empty());
        assert!(k.feed(Input::AltUp { t: 900 }).is_empty());
    }

    #[test]
    fn tick_wraparound() {
        let mut k = m();
        k.feed(down(u32::MAX - 100));
        assert_eq!(
            k.feed(Input::Tick { t: 600 }),
            vec![Action::Mask, Action::HoldStart]
        );
    }
}
