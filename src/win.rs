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
    /// The island's pill (or its "Ask…" row) was clicked: open the palette.
    IslandClick,
    Island(crate::island::IslandEvent),
    /// Alt tapped twice: open the palette.
    AltDoubleTap,
    /// Continue a saved conversation (a quick-ask caption was clicked).
    #[allow(dead_code)] // quick asks now land on the thread; kept for History links
    OpenChat(std::path::PathBuf),
    /// An agent card's "Open in palette": its full chat in the palette.
    AgentPalette(String),
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

/// Repaints the open palette, if any (from any thread).
pub fn request_repaint() {
    if let Some(ctx) = CTX.lock().unwrap().as_ref() {
        ctx.request_repaint();
    }
}

/// Registers the egui context of the open palette so events repaint it.
pub fn set_ctx(ctx: Option<egui::Context>) {
    *CTX.lock().unwrap() = ctx;
}

/// True while the palette or Settings is open.
pub fn ui_open() -> bool {
    CTX.lock().unwrap().is_some()
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
    parse_hotkey(spec).is_ok_and(|(_, vk)| unsafe { GetAsyncKeyState(vk as i32) } as u16 & 0x8000 != 0)
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

/// File types that run code when opened (programs, scripts, shortcuts,
/// installers, registry merges).
const RUNNABLE_EXTS: &[&str] = &[
    "exe", "bat", "cmd", "com", "scr", "pif", "lnk", "js", "jse", "vbs", "vbe", "wsf", "wsh",
    "hta", "ps1", "msi", "msp", "cpl", "jar", "reg", "url", "msc",
];

/// What a bare name ("notepad") would start through ShellExecute.
#[derive(Debug, PartialEq)]
pub enum Launch {
    /// An installed app: App Paths or a PATH folder, outside the places agents write.
    Installed(std::path::PathBuf),
    /// Something of that name sits where an agent can write (the current
    /// directory, screenbuddy's data and agent folders, temp): maybe planted.
    Planted(std::path::PathBuf),
    /// Nothing known.
    Unknown,
}

/// `path` lies in (or is) `dir`, compared the way Windows does (case-blind,
/// either slash, `..` resolved where the path exists).
fn under(path: &std::path::Path, dir: &std::path::Path) -> bool {
    let norm = |p: &std::path::Path| {
        let p = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        let s = p.to_string_lossy().to_lowercase().replace('/', "\\");
        s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_string()
    };
    let (p, d) = (norm(path), norm(dir));
    !d.is_empty() && (p == d || p.starts_with(&format!("{d}\\")))
}

/// Where `name` would come from, in ShellExecute's places: the `writable`
/// folders first (anything of that name there, bare or with an executable or
/// runnable extension, counts as planted), then the App Paths entry, then each
/// absolute `path` folder with an extension from `exts` (PATHEXT: "exe"...).
pub fn locate(
    name: &str,
    writable: &[std::path::PathBuf],
    app_path: Option<std::path::PathBuf>,
    path: &[std::path::PathBuf],
    exts: &[String],
) -> Launch {
    // App execution aliases (WindowsApps) are reparse points `metadata` can't follow.
    let exists = |p: &std::path::Path| std::fs::symlink_metadata(p).is_ok();
    let with_ext = |d: &std::path::Path, exts: &mut dyn Iterator<Item = &str>| {
        exts.map(|e| d.join(format!("{name}.{e}"))).find(|p| exists(p))
    };
    for w in writable {
        let bare = w.join(name);
        if exists(&bare) {
            return Launch::Planted(bare);
        }
        let mut any = exts.iter().map(String::as_str).chain(RUNNABLE_EXTS.iter().copied());
        if let Some(p) = with_ext(w, &mut any) {
            return Launch::Planted(p);
        }
    }
    let inside = |p: &std::path::Path| writable.iter().any(|w| under(p, w));
    if let Some(p) = app_path.filter(|p| p.is_absolute() && exists(p)) {
        return if inside(&p) {
            Launch::Planted(p)
        } else {
            Launch::Installed(p)
        };
    }
    for d in path.iter().filter(|d| d.is_absolute() && !inside(d)) {
        if let Some(p) = with_ext(d, &mut exts.iter().map(String::as_str)) {
            return Launch::Installed(p);
        }
    }
    Launch::Unknown
}

/// The App Paths registration for `name`.exe (per user, then machine).
fn app_path(name: &str) -> Option<std::path::PathBuf> {
    use winreg::RegKey;
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    let key = format!(r"Software\Microsoft\Windows\CurrentVersion\App Paths\{name}.exe");
    [HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE].into_iter().find_map(|root| {
        let v: String = RegKey::predef(root).open_subkey(&key).ok()?.get_value("").ok()?;
        Some(std::path::PathBuf::from(expand_env(v.trim().trim_matches('"'))))
    })
}

/// "%ProgramFiles%\x" -> "C:\Program Files\x" (REG_EXPAND_SZ values).
fn expand_env(s: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find('%') {
        out.push_str(&rest[..i]);
        let after = &rest[i + 1..];
        match after.find('%') {
            Some(j) => {
                match std::env::var(&after[..j]) {
                    Ok(v) => out.push_str(&v),
                    Err(_) => out.push_str(&rest[i..i + j + 2]),
                }
                rest = &after[j + 1..];
            }
            None => {
                out.push_str(&rest[i..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

/// [`locate`] with this machine's folders. Writable places are the current
/// directory (an agent's folder in its `--mcp` child), screenbuddy's data
/// folder (all agent folders) and temp, unless one is under Windows or
/// Program Files (an autostart's System32, say).
pub fn bare_launch(name: &str) -> Launch {
    let env = |k: &str| std::env::var_os(k).map(std::path::PathBuf::from);
    let protected: Vec<std::path::PathBuf> = ["SystemRoot", "ProgramFiles", "ProgramFiles(x86)"]
        .iter()
        .filter_map(|k| env(k))
        .collect();
    let writable: Vec<std::path::PathBuf> = [
        std::env::current_dir().ok(),
        Some(crate::config::data_dir()),
        Some(std::env::temp_dir()),
    ]
    .into_iter()
    .flatten()
    .filter(|w| !protected.iter().any(|p| under(w, p)))
    .collect();
    let path: Vec<std::path::PathBuf> = env("PATH")
        .map(|p| std::env::split_paths(&p).collect())
        .unwrap_or_default();
    let exts: Vec<String> = std::env::var("PATHEXT")
        .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD;.VBS;.VBE;.JS;.JSE;.WSF;.WSH;.MSC".into())
        .split(';')
        .map(|e| e.trim().trim_start_matches('.').to_ascii_lowercase())
        .filter(|e| !e.is_empty())
        .collect();
    locate(name, &writable, app_path(name), &path, &exts)
}

/// True when opening `target` would run code: its file name (or any NTFS
/// stream name after a `:`) has a runnable extension. Windows drops trailing
/// dots and spaces from names, so "x.bat." and "x.bat " count as x.bat.
pub fn runnable(target: &str) -> bool {
    let name = target.rsplit(['/', '\\']).next().unwrap_or(target);
    name.split(':').any(|part| {
        let part = part.trim_end_matches(['.', ' ']);
        part.rsplit_once('.').is_some_and(|(_, ext)| {
            RUNNABLE_EXTS.iter().any(|e| e.eq_ignore_ascii_case(ext))
        })
    })
}

/// Opens Explorer with the file selected.
pub fn reveal(path: &std::path::Path) {
    let _ = std::process::Command::new("explorer.exe")
        .arg(format!("/select,{}", path.display()))
        .spawn();
}

/// Opens a file an agent produced. Programs and scripts are only shown in
/// Explorer: one click on a chip must never run what an agent wrote.
pub fn open_agent_file(path: &std::path::Path) {
    if runnable(&path.to_string_lossy()) {
        reveal(path);
    } else {
        shell_open(path);
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
    #[test]
    fn runnable_files() {
        for t in [
            "x.bat", "X.BAT", "x.bat.", "x.bat ", "x.bat. . ", r"C:\out\report.hta",
            "//srv/share/a.LNK", "x.txt:evil.exe", "x.bat::$DATA", "C:x.ps1", "a.Url",
        ] {
            assert!(runnable(t), "{t}");
        }
        for t in [
            "report.pdf", "notes.txt", r"C:\out\batch", "x.bat.txt", "notepad", "bat", ".bat2",
            r"C:\dir.exe\file.md",
        ] {
            assert!(!runnable(t), "{t}");
        }
    }
    #[test]
    fn bare_names_resolve_outside_agent_folders() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!("screenbuddy-locate-{n}"));
        let (work, bin, app) = (root.join("agent"), root.join("bin"), root.join("App"));
        for d in [&work, &bin, &app] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(work.join("payload.bat"), b"x").unwrap();
        std::fs::write(work.join("notes"), b"x").unwrap();
        std::fs::write(work.join("tool.exe"), b"x").unwrap();
        std::fs::write(bin.join("tool.exe"), b"x").unwrap();
        std::fs::write(bin.join("editor.cmd"), b"x").unwrap();
        std::fs::write(app.join("Spot.exe"), b"x").unwrap();
        let exts: Vec<String> = ["com", "exe", "bat", "cmd"].iter().map(|s| s.to_string()).collect();
        let writable = std::slice::from_ref(&work);
        let path = [std::path::PathBuf::from("."), work.clone(), bin.clone()];
        let at = |name: &str, app_path: Option<std::path::PathBuf>| {
            locate(name, writable, app_path, &path, &exts)
        };
        // Planted in the agent's folder: never "installed", even when PATH has one too.
        assert_eq!(at("payload", None), Launch::Planted(work.join("payload.bat")));
        assert_eq!(at("notes", None), Launch::Planted(work.join("notes")));
        assert_eq!(at("tool", None), Launch::Planted(work.join("tool.exe")));
        // Installed: a PATH folder outside the writable ones, or App Paths.
        assert_eq!(at("editor", None), Launch::Installed(bin.join("editor.cmd")));
        assert_eq!(at("spot", Some(app.join("Spot.exe"))), Launch::Installed(app.join("Spot.exe")));
        // An App Paths entry pointing into an agent folder is no install.
        let sneaky = work.join("SUB").join("x.exe");
        std::fs::create_dir_all(sneaky.parent().unwrap()).unwrap();
        std::fs::write(&sneaky, b"x").unwrap();
        assert_eq!(at("spot", Some(sneaky.clone())), Launch::Planted(sneaky));
        // Nothing known, or a relative registration.
        assert_eq!(at("nothing", None), Launch::Unknown);
        assert_eq!(at("nothing", Some("x.exe".into())), Launch::Unknown);
        assert!(under(&work.join("a"), &std::path::PathBuf::from(work.to_string_lossy().to_uppercase().replace('\\', "/"))));
        assert!(!under(&root.join("agent2"), &work));
        let _ = std::fs::remove_dir_all(&root);
    }
    #[test]
    fn expands_registry_paths() {
        // SAFETY: test-only, single variable no other test reads.
        unsafe { std::env::set_var("SB_TEST_DIR", r"C:\Apps") };
        assert_eq!(expand_env(r"%SB_TEST_DIR%\x.exe"), r"C:\Apps\x.exe");
        assert_eq!(expand_env(r"%SB_NOPE_X%\x.exe"), r"%SB_NOPE_X%\x.exe");
        assert_eq!(expand_env("50%"), "50%");
    }
}
