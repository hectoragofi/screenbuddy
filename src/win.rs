//! Win32 plumbing: hidden message window, global hotkeys, the app event queue,
//! autostart, memory trim, single-instance guard.

use crate::util::wide;
use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::atomic::{AtomicIsize, Ordering};
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

pub const HK_ASK: i32 = 1;
pub const HK_REGION: i32 = 2;
pub const HK_VOICE: i32 = 3;
const WM_WAKE: u32 = WM_APP + 1;

#[derive(Debug, Clone)]
pub enum AppEvent {
    Hotkey(i32),
    TrayClick,
    Menu(String),
    /// A file sent from Explorer's "Ask screenbuddy" menu.
    OpenFile(std::path::PathBuf),
}

static QUEUE: Mutex<VecDeque<AppEvent>> = Mutex::new(VecDeque::new());
static CTX: Mutex<Option<egui::Context>> = Mutex::new(None);
static HWND_MAIN: AtomicIsize = AtomicIsize::new(0);

/// Enqueues an event and wakes whichever loop is running (ours or winit's).
pub fn push(ev: AppEvent) {
    QUEUE.lock().unwrap().push_back(ev);
    if let Some(ctx) = CTX.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
    let h = HWND_MAIN.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as *mut _)), WM_WAKE, WPARAM(0), LPARAM(0));
        }
    }
}

pub fn pop() -> Option<AppEvent> {
    QUEUE.lock().unwrap().pop_front()
}

/// Registers the egui context of the open palette so events repaint it.
pub fn set_ctx(ctx: Option<egui::Context>) {
    *CTX.lock().unwrap() = ctx;
}

unsafe extern "system" fn main_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_HOTKEY => {
                push(AppEvent::Hotkey(wp.0 as i32));
                LRESULT(0)
            }
            WM_WAKE => LRESULT(0),
            WM_COPYDATA => match crate::context::path_from_copydata(lp) {
                Some(p) => {
                    push(AppEvent::OpenFile(p));
                    LRESULT(1)
                }
                None => LRESULT(0),
            },
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

pub fn create_main_window() -> anyhow::Result<HWND> {
    let cls = wide("screenbuddy_main");
    unsafe {
        let inst = HINSTANCE(GetModuleHandleW(None)?.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(main_proc),
            hInstance: inst,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(inst),
            None,
        )?;
        HWND_MAIN.store(hwnd.0 as isize, Ordering::Relaxed);
        Ok(hwnd)
    }
}

/// Posts a hotkey message to the already-running instance's message window.
pub fn signal_running(id: i32) {
    let cls = wide("screenbuddy_main");
    unsafe {
        if let Ok(h) = FindWindowExW(
            Some(HWND_MESSAGE),
            None,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
        ) {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = PostMessageW(Some(h), WM_HOTKEY, WPARAM(id as usize), LPARAM(0));
        }
    }
}

pub fn main_hwnd() -> HWND {
    HWND(HWND_MAIN.load(Ordering::Relaxed) as *mut _)
}

/// Parses "Ctrl+Shift+Space" style strings.
pub fn parse_hotkey(s: &str) -> Result<(HOT_KEY_MODIFIERS, u32), String> {
    let mut mods = MOD_NOREPEAT;
    let mut key = None;
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => mods |= MOD_CONTROL,
            "alt" => mods |= MOD_ALT,
            "shift" => mods |= MOD_SHIFT,
            "win" | "super" | "meta" => mods |= MOD_WIN,
            "" => return Err(format!("bad hotkey '{s}'")),
            k => key = Some(vk_from_name(k).ok_or_else(|| format!("unknown key '{k}' in '{s}'"))?),
        }
    }
    let key = key.ok_or_else(|| format!("hotkey '{s}' has no key"))?;
    if mods == MOD_NOREPEAT {
        return Err(format!("hotkey '{s}' needs a modifier"));
    }
    Ok((mods, key))
}

fn vk_from_name(k: &str) -> Option<u32> {
    if k.len() == 1 {
        let c = k.chars().next()?.to_ascii_uppercase();
        if c.is_ascii_alphanumeric() {
            return Some(c as u32);
        }
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=24).contains(&n) {
            return Some(VK_F1.0 as u32 + n - 1);
        }
    }
    Some(match k {
        "space" => VK_SPACE.0,
        "enter" | "return" => VK_RETURN.0,
        "tab" => VK_TAB.0,
        "esc" | "escape" => VK_ESCAPE.0,
        "backspace" => VK_BACK.0,
        "insert" => VK_INSERT.0,
        "delete" | "del" => VK_DELETE.0,
        "home" => VK_HOME.0,
        "end" => VK_END.0,
        "pageup" => VK_PRIOR.0,
        "pagedown" => VK_NEXT.0,
        "up" => VK_UP.0,
        "down" => VK_DOWN.0,
        "left" => VK_LEFT.0,
        "right" => VK_RIGHT.0,
        "printscreen" | "print" => VK_SNAPSHOT.0,
        "`" | "backtick" | "grave" => VK_OEM_3.0,
        "/" | "slash" => VK_OEM_2.0,
        "." | "period" => VK_OEM_PERIOD.0,
        "," | "comma" => VK_OEM_COMMA.0,
        ";" | "semicolon" => VK_OEM_1.0,
        _ => return None,
    } as u32)
}

/// (Re)registers both hotkeys; returns human-readable errors.
pub fn register_hotkeys(ask: &str, region: &str) -> Vec<String> {
    let hwnd = main_hwnd();
    let mut errs = Vec::new();
    for (id, spec) in [(HK_ASK, ask), (HK_REGION, region)] {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
        if spec.trim().is_empty() {
            continue;
        }
        match parse_hotkey(spec) {
            Ok((mods, vk)) => unsafe {
                if RegisterHotKey(Some(hwnd), id, mods, vk).is_err() {
                    errs.push(format!("hotkey {spec} is taken by another app"));
                }
            },
            Err(e) => errs.push(e),
        }
    }
    errs
}

/// (Re)registers the push-to-talk hotkey; "" unregisters it.
pub fn register_voice_hotkey(spec: &str) -> Option<String> {
    let hwnd = main_hwnd();
    unsafe {
        let _ = UnregisterHotKey(Some(hwnd), HK_VOICE);
    }
    if spec.trim().is_empty() {
        return None;
    }
    match parse_hotkey(spec) {
        Ok((mods, vk)) => unsafe {
            RegisterHotKey(Some(hwnd), HK_VOICE, mods, vk)
                .is_err()
                .then(|| format!("hotkey {spec} is taken by another app"))
        },
        Err(e) => Some(e),
    }
}

/// True while the non-modifier key of a hotkey spec is physically held.
pub fn hotkey_held(spec: &str) -> bool {
    parse_hotkey(spec)
        .is_ok_and(|(_, vk)| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0)
}

/// Our windows are hidden from screen capture (so re-captures stay clean)
/// unless SCREENBUDDY_ALLOW_CAPTURE is set, e.g. for screenshots of the app.
pub fn exclude_from_capture() -> bool {
    std::env::var_os("SCREENBUDDY_ALLOW_CAPTURE").is_none()
}

/// Returns working-set memory to the OS after the palette closes.
pub fn trim_memory() {
    unsafe {
        let _ = windows::Win32::System::ProcessStatus::K32EmptyWorkingSet(
            windows::Win32::System::Threading::GetCurrentProcess(),
        );
    }
}

pub fn set_dpi_awareness() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
}

/// True if another screenbuddy is already running.
pub fn already_running() -> bool {
    let name = wide("Local\\screenbuddy-single-instance");
    unsafe {
        let h = windows::Win32::System::Threading::CreateMutexW(None, true, PCWSTR(name.as_ptr()));
        // Leak the handle on purpose: it must live as long as the process.
        h.is_ok() && GetLastError() == ERROR_ALREADY_EXISTS
    }
}

pub fn shell_open(path: &std::path::Path) {
    let p = wide(&path.to_string_lossy());
    let verb = wide("open");
    unsafe {
        let r = windows::Win32::UI::Shell::ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(p.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        // <= 32 means failure (e.g. no app associated with .toml).
        if r.0 as isize <= 32 {
            let _ = std::process::Command::new("notepad.exe").arg(path).spawn();
        }
    }
}

pub fn message_box(text: &str) {
    let t = wide(text);
    let c = wide("screenbuddy");
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(t.as_ptr()),
            PCWSTR(c.as_ptr()),
            MB_OK | MB_ICONWARNING,
        );
    }
}

// ---------------------------------------------------------------- autostart

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

pub fn autostart_enabled() -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(RUN_KEY)
        .and_then(|k| k.get_value::<String, _>("screenbuddy"))
        .is_ok()
}

pub fn set_autostart(on: bool) -> std::io::Result<()> {
    let (key, _) =
        winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER).create_subkey(RUN_KEY)?;
    if on {
        let exe = std::env::current_exe()?;
        key.set_value("screenbuddy", &format!("\"{}\"", exe.display()))
    } else {
        match key.delete_value("screenbuddy") {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hotkeys() {
        let (m, k) = parse_hotkey("Ctrl+Shift+Space").unwrap();
        assert_eq!(k, 0x20);
        assert_eq!(m, MOD_NOREPEAT | MOD_CONTROL | MOD_SHIFT);
        assert_eq!(parse_hotkey("Alt+F12").unwrap().1, VK_F12.0 as u32);
        assert!(parse_hotkey("Space").is_err());
        assert!(parse_hotkey("Ctrl+Nope").is_err());
    }
}
