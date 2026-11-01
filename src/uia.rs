//! Compact UI Automation snapshot of the window the user is looking at:
//! element type, name and bounds, walked breadth-first under a hard time
//! budget so it never delays an answer noticeably.

use crate::capture::Rect;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
};
use windows::Win32::UI::Accessibility::*;
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GW_HWNDNEXT, GetAncestor, GetForegroundWindow, GetTopWindow, GetWindow,
    GetWindowRect, GetWindowThreadProcessId, IsIconic, IsWindowVisible, WindowFromPoint,
};

/// Default wall-clock budget for one snapshot.
const BUDGET: Duration = Duration::from_millis(150);

/// Snapshot budget; `SCREENBUDDY_UIA_MS` overrides it for debugging.
pub fn budget() -> Duration {
    std::env::var("SCREENBUDDY_UIA_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .map(Duration::from_millis)
        .unwrap_or(BUDGET)
}
/// Elements collected before giving up on the walk.
const MAX_COLLECT: usize = 400;
/// Deepest tree level expanded.
const MAX_DEPTH: usize = 14;

#[derive(Clone, Debug)]
pub struct Element {
    pub kind: &'static str,
    pub name: String,
    /// Bounds in virtual-screen pixels.
    pub rect: Rect,
    pub focused: bool,
}

/// Top-level windows worth describing, in priority order: the foreground
/// window and the one under the cursor (never screenbuddy's own windows),
/// falling back to the topmost visible window over `area`.
pub fn target_windows(area: Rect, cursor: (i32, i32)) -> Vec<HWND> {
    let mut out: Vec<HWND> = Vec::new();
    unsafe {
        let fg = GetForegroundWindow();
        let under = GetAncestor(WindowFromPoint(POINT { x: cursor.0, y: cursor.1 }), GA_ROOT);
        for h in [fg, under] {
            if usable(h, area) && !out.contains(&h) {
                out.push(h);
            }
        }
        if out.is_empty() {
            let mut h = GetTopWindow(None).unwrap_or_default();
            while !h.is_invalid() {
                if usable(h, area) {
                    out.push(h);
                    break;
                }
                h = GetWindow(h, GW_HWNDNEXT).unwrap_or_default();
            }
        }
    }
    out
}

unsafe fn usable(h: HWND, area: Rect) -> bool {
    unsafe {
        if h.is_invalid() || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return false;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(h, Some(&mut pid));
        if pid == std::process::id() {
            return false;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(
            h,
            DWMWA_CLOAKED,
            (&mut cloaked as *mut u32).cast(),
            std::mem::size_of::<u32>() as u32,
        );
        if cloaked != 0 {
            return false;
        }
        let mut r = RECT::default();
        if GetWindowRect(h, &mut r).is_err() {
            return false;
        }
        intersect(Rect::from_win(r), area).is_some()
    }
}

pub fn intersect(a: Rect, b: Rect) -> Option<Rect> {
    let x0 = a.x.max(b.x);
    let y0 = a.y.max(b.y);
    let x1 = (a.x + a.w).min(b.x + b.w);
    let y1 = (a.y + a.h).min(b.y + b.h);
    (x1 > x0 && y1 > y0).then_some(Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 })
}

/// Walks `roots` breadth-first and returns the named or focusable elements
/// that are visible inside `area`. Runs on its own thread; if the target
/// app is slow to answer, the caller gets whatever was collected (or
/// nothing) once `budget` has passed.
pub fn snapshot(roots: Vec<HWND>, area: Rect, budget: Duration) -> Vec<Element> {
    if roots.is_empty() {
        return Vec::new();
    }
    let deadline = Instant::now() + budget;
    let raw: Vec<isize> = roots.iter().map(|h| h.0 as isize).collect();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let roots: Vec<HWND> = raw.into_iter().map(|h| HWND(h as *mut _)).collect();
        let mut found = Vec::new();
        let t = Instant::now();
        let r = unsafe { walk(&roots, area, deadline, &mut found) };
        if std::env::var_os("SCREENBUDDY_UIA_MS").is_some() {
            eprintln!("[uia walk {:?}: {} elements, {:?}]", t.elapsed(), found.len(), r.err());
        }
        let _ = tx.send(found);
    });
    let wait = deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(30);
    rx.recv_timeout(wait).unwrap_or_default()
}

unsafe fn walk(
    roots: &[HWND],
    area: Rect,
    deadline: Instant,
    found: &mut Vec<Element>,
) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let ua: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
        let cache = ua.CreateCacheRequest()?;
        for p in [
            UIA_NamePropertyId,
            UIA_ControlTypePropertyId,
            UIA_BoundingRectanglePropertyId,
            UIA_IsOffscreenPropertyId,
            UIA_IsKeyboardFocusablePropertyId,
            UIA_HasKeyboardFocusPropertyId,
        ] {
            cache.AddProperty(p)?;
        }
        let cond = ua.ControlViewCondition()?;
        // (element, depth, parent name, parent rect)
        let mut queue = std::collections::VecDeque::new();
        for &h in roots {
            if Instant::now() >= deadline {
                break;
            }
            if let Ok(el) = ua.ElementFromHandleBuildCache(h, &cache) {
                queue.push_back((el, 0usize, String::new(), Rect::default()));
            }
        }
        while let Some((el, depth, pname, prect)) = queue.pop_front() {
            if Instant::now() >= deadline || found.len() >= MAX_COLLECT {
                break;
            }
            let rect = Rect::from_win(el.CachedBoundingRectangle().unwrap_or_default());
            let offscreen = el.CachedIsOffscreen().map(|b| b.as_bool()).unwrap_or(false);
            if offscreen || rect.w <= 0 || rect.h <= 0 || intersect(rect, area).is_none() {
                continue;
            }
            let name = clean(&el.CachedName().map(|b| b.to_string()).unwrap_or_default());
            let focusable = el.CachedIsKeyboardFocusable().map(|b| b.as_bool()).unwrap_or(false);
            let kind = control_type(el.CachedControlType().map(|c| c.0).unwrap_or(0));
            // Skip wrappers that only repeat their parent.
            let repeat = name == pname && rect == prect;
            if (!name.is_empty() || focusable) && !repeat {
                found.push(Element {
                    kind,
                    name: name.clone(),
                    rect,
                    focused: el.CachedHasKeyboardFocus().map(|b| b.as_bool()).unwrap_or(false),
                });
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            let Ok(kids) = el.FindAllBuildCache(TreeScope_Children, &cond, &cache) else {
                continue;
            };
            for i in 0..kids.Length().unwrap_or(0) {
                if let Ok(k) = kids.GetElement(i) {
                    queue.push_back((k, depth + 1, name.clone(), rect));
                }
            }
        }
        Ok(())
    }
}

/// A short identity of the focused element (type, name, bounds), used to
/// notice focus moving while watching. None if UIA is unavailable.
pub struct FocusProbe {
    ua: IUIAutomation,
}

impl FocusProbe {
    pub fn new() -> Option<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)
                .ok()
                .map(|ua| Self { ua })
        }
    }

    pub fn signature(&self) -> Option<String> {
        unsafe {
            let el = self.ua.GetFocusedElement().ok()?;
            let r = el.CurrentBoundingRectangle().unwrap_or_default();
            let name = el.CurrentName().map(|b| b.to_string()).unwrap_or_default();
            let ct = el.CurrentControlType().map(|c| c.0).unwrap_or(0);
            Some(format!("{ct}|{name}|{},{},{},{}", r.left, r.top, r.right, r.bottom))
        }
    }
}

fn clean(s: &str) -> String {
    let one: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let one = one.replace('"', "'");
    if one.chars().count() > 48 {
        let mut t: String = one.chars().take(47).collect();
        t.push('…');
        t
    } else {
        one
    }
}

/// UIA control type ids (UIA_*ControlTypeId, 50000..=50040) to short names.
pub fn control_type(id: i32) -> &'static str {
    const NAMES: [&str; 41] = [
        "button", "calendar", "checkbox", "combobox", "edit", "link", "image", "listitem",
        "list", "menu", "menubar", "menuitem", "progressbar", "radio", "scrollbar", "slider",
        "spinner", "statusbar", "tab", "tabitem", "text", "toolbar", "tooltip", "tree",
        "treeitem", "custom", "group", "thumb", "datagrid", "dataitem", "document",
        "splitbutton", "window", "pane", "header", "headeritem", "table", "titlebar",
        "separator", "semanticzoom", "appbar",
    ];
    usize::try_from(id - 50000)
        .ok()
        .and_then(|i| NAMES.get(i).copied())
        .unwrap_or("element")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn types_and_names() {
        assert_eq!(control_type(UIA_ButtonControlTypeId.0), "button");
        assert_eq!(control_type(UIA_AppBarControlTypeId.0), "appbar");
        assert_eq!(control_type(UIA_WindowControlTypeId.0), "window");
        assert_eq!(control_type(1), "element");
        assert_eq!(clean("  a \"b\"\n c "), "a 'b' c");
        assert_eq!(clean(&"x".repeat(60)).chars().count(), 48);
    }
    #[test]
    fn intersects() {
        let a = Rect { x: 0, y: 0, w: 10, h: 10 };
        assert_eq!(intersect(a, Rect { x: 5, y: 5, w: 10, h: 10 }), Some(Rect { x: 5, y: 5, w: 5, h: 5 }));
        assert_eq!(intersect(a, Rect { x: 10, y: 0, w: 5, h: 5 }), None);
    }
}
