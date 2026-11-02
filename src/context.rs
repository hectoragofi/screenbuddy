//! Extra context for a question: files sent from Explorer's "Ask screenbuddy"
//! menu and text selected in the foreground app when the hotkey fires.
//!
//! Everything here is one-shot: nothing stays loaded between questions.

use crate::llm::Image;
use crate::util::wide;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use windows::Win32::Foundation::*;
use windows::Win32::System::DataExchange::COPYDATASTRUCT;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

/// Text files are sent up to this many bytes (the rest is cut off).
const TEXT_CAP: usize = 100_000;
/// Images larger than this on disk are attached as metadata only.
const IMAGE_CAP: u64 = 40 * 1024 * 1024;
/// Selected text is cut to this many characters.
const SELECTION_CAP: usize = 20_000;

/// A context chip shown in the palette and sent with the next question.
#[derive(Clone)]
pub struct Chip {
    /// Short label for the chip ("report.pdf", "Selected text").
    pub label: String,
    /// Longer hover text.
    pub hover: String,
    /// Text prepended to the question for the model.
    pub prompt: String,
    /// Image content (image files), sent as an extra image.
    pub image: Option<Image>,
}

impl Chip {
    pub fn selection(text: &str, app: &str) -> Self {
        let text = cap_chars(text.trim(), SELECTION_CAP);
        let from = if app.is_empty() {
            String::new()
        } else {
            format!(" in “{app}”")
        };
        Self {
            label: "Selected text".into(),
            hover: format!(
                "{}{}",
                from.trim_start(),
                if from.is_empty() { "" } else { "\n" }
            )
            .to_string()
                + &cap_chars(&text, 300),
            prompt: format!(
                "[Text the user has selected{from}]\n<selection>\n{text}\n</selection>"
            ),
            image: None,
        }
    }
}

/// Joins chip prompts in front of the user's question.
pub fn compose(chips: &[Chip], question: &str) -> String {
    if chips.is_empty() {
        return question.to_string();
    }
    let mut s = String::new();
    for c in chips {
        s.push_str(&c.prompt);
        s.push_str("\n\n");
    }
    s.push_str(question);
    s
}

fn cap_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_string(),
    }
}

fn human_size(n: u64) -> String {
    match n {
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.0} KB", n as f64 / 1024.0),
        n => format!("{n} bytes"),
    }
}

// ---------------------------------------------------------------- files

/// Builds a chip for a file: images as an image, text as text, anything
/// else (and anything too big) as its path and metadata.
pub fn file_chip(path: &Path) -> Chip {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    let meta = std::fs::metadata(path);
    let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
    let base = |note: &str| Chip {
        label: name.clone(),
        hover: format!("{}\n{}", path.display(), note),
        prompt: format!(
            "[Attached file: {} ({}). {note}]",
            path.display(),
            human_size(size)
        ),
        image: None,
    };
    let Ok(meta) = meta else {
        return base("The file could not be read.");
    };
    if meta.is_dir() {
        let mut names: Vec<String> = std::fs::read_dir(path)
            .map(|rd| {
                rd.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        let more = names.len().saturating_sub(200);
        names.truncate(200);
        let mut c = base("It is a folder.");
        c.prompt = format!(
            "[Attached folder: {}]\n<listing>\n{}{}\n</listing>",
            path.display(),
            names.join("\n"),
            if more > 0 {
                format!("\n… and {more} more")
            } else {
                String::new()
            }
        );
        return c;
    }
    if let Ok(fmt) = image::ImageFormat::from_path(path) {
        if fmt.reading_enabled() {
            if size > IMAGE_CAP {
                return base("It is an image, too large to attach.");
            }
            match load_image(path, fmt) {
                Ok(img) => {
                    return Chip {
                        label: name.clone(),
                        hover: format!("{}\nImage, {}x{} sent", path.display(), img.w, img.h),
                        prompt: format!(
                            "[Attached image file: {} ({}), sent as the image after the screenshot]",
                            path.display(),
                            human_size(size)
                        ),
                        image: Some(img),
                    };
                }
                Err(e) => {
                    return base(&format!(
                        "It looks like an image but could not be decoded ({e})."
                    ));
                }
            }
        }
    }
    match read_text(path) {
        Some((text, cut)) => Chip {
            label: name.clone(),
            hover: format!("{}\nText, {}", path.display(), human_size(size)),
            prompt: format!(
                "[Attached file: {} ({}){}]\n<file>\n{text}\n</file>",
                path.display(),
                human_size(size),
                if cut {
                    format!(", only the first {} shown", human_size(TEXT_CAP as u64))
                } else {
                    String::new()
                }
            ),
            image: None,
        },
        None => {
            let modified = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| format!(" Modified {} seconds after the Unix epoch.", d.as_secs()))
                .unwrap_or_default();
            base(&format!("Binary file, contents not included.{modified}"))
        }
    }
}

/// Decodes an image file and re-encodes it as a JPEG within the API's limits.
fn load_image(path: &Path, fmt: image::ImageFormat) -> anyhow::Result<Image> {
    let r =
        image::ImageReader::with_format(std::io::BufReader::new(std::fs::File::open(path)?), fmt);
    let img = r.decode()?;
    let (w, h) = (img.width().max(1), img.height().max(1));
    let long = 1568.0 / w.max(h) as f64;
    let area = (1_150_000.0 / (w as f64 * h as f64)).sqrt();
    let s = long.min(area).min(1.0);
    let img = if s < 1.0 {
        img.resize(
            ((w as f64 * s) as u32).max(1),
            ((h as f64 * s) as u32).max(1),
            image::imageops::FilterType::Triangle,
        )
    } else {
        img
    };
    let rgb = img.to_rgb8();
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 85).encode_image(&rgb)?;
    Ok(Image {
        jpeg: Arc::new(jpeg),
        w: rgb.width(),
        h: rgb.height(),
    })
}

/// Reads a file as text if it looks like text. Returns (text, truncated).
fn read_text(path: &Path) -> Option<(String, bool)> {
    use std::io::Read;
    let mut buf = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(TEXT_CAP as u64 + 1)
        .read_to_end(&mut buf)
        .ok()?;
    let cut = buf.len() > TEXT_CAP;
    buf.truncate(TEXT_CAP);
    decode_text(&buf).map(|t| (t, cut))
}

/// UTF-16 (with BOM) or UTF-8; None for binary data.
pub fn decode_text(buf: &[u8]) -> Option<String> {
    if let Some(rest) = buf.strip_prefix(&[0xFF, 0xFE]) {
        let u: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u));
    }
    let buf = buf.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(buf);
    if buf.iter().take(8192).any(|&b| b == 0) {
        return None;
    }
    match std::str::from_utf8(buf) {
        Ok(s) => Some(s.to_string()),
        // Cut in the middle of a character at the size cap.
        Err(e) if e.error_len().is_none() => {
            Some(String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned())
        }
        Err(_) => {
            // Legacy 8-bit text: accept if it is mostly printable.
            let printable = buf
                .iter()
                .take(8192)
                .filter(|&&b| b >= 0x20 || b == b'\n' || b == b'\r' || b == b'\t')
                .count();
            (printable * 100 >= buf.len().min(8192) * 95)
                .then(|| String::from_utf8_lossy(buf).into_owned())
        }
    }
}

// ---------------------------------------------------------------- Explorer menu

const MENU_KEY: &str = r"Software\Classes\*\shell\screenbuddy";
const COPYDATA_FILE: usize = 0x5342_4649; // "SBFI"

/// Adds or removes "Ask screenbuddy" in Explorer's right-click menu for files.
pub fn set_explorer_menu(on: bool) -> std::io::Result<()> {
    set_menu_at(MENU_KEY, on, &std::env::current_exe()?)
}

fn set_menu_at(key: &str, on: bool, exe: &Path) -> std::io::Result<()> {
    let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
    if on {
        let (k, _) = hkcu.create_subkey(key)?;
        k.set_value("MUIVerb", &"Ask screenbuddy")?;
        k.set_value("Icon", &format!("\"{}\",0", exe.display()))?;
        let (c, _) = k.create_subkey("command")?;
        c.set_value("", &format!("\"{}\" --open-file \"%1\"", exe.display()))
    } else {
        match hkcu.delete_subkey_all(key) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}

fn menu_registered_at(key: &str) -> bool {
    winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey(key)
        .is_ok()
}

/// Makes the registry match the setting (and point at this exe) at startup.
pub fn sync_explorer_menu(on: bool) {
    if on || menu_registered_at(MENU_KEY) {
        if let Err(e) = set_explorer_menu(on) {
            crate::config::log(&format!("explorer menu: {e}"));
        }
    }
}

/// Hands a file to the running instance (WM_COPYDATA to its message window).
/// Retries briefly in case that instance is still starting up.
pub fn send_file_to_running(path: &Path) -> bool {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text: Vec<u16> = path.as_os_str().to_string_lossy().encode_utf16().collect();
    let cls = wide("screenbuddy_main");
    for _ in 0..40 {
        unsafe {
            if let Ok(h) = FindWindowExW(
                Some(HWND_MESSAGE),
                None,
                PCWSTR(cls.as_ptr()),
                PCWSTR::null(),
            ) {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let cds = COPYDATASTRUCT {
                    dwData: COPYDATA_FILE,
                    cbData: (text.len() * 2) as u32,
                    lpData: text.as_ptr() as *mut _,
                };
                let r = SendMessageW(
                    h,
                    WM_COPYDATA,
                    None,
                    Some(LPARAM(&cds as *const _ as isize)),
                );
                return r.0 != 0;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// Reads a file path out of a WM_COPYDATA message, if it is ours.
///
/// # Safety
/// `lp` must be the LPARAM of a WM_COPYDATA message.
pub unsafe fn path_from_copydata(lp: LPARAM) -> Option<PathBuf> {
    let cds = unsafe { (lp.0 as *const COPYDATASTRUCT).as_ref()? };
    if cds.dwData != COPYDATA_FILE
        || cds.lpData.is_null()
        || cds.cbData == 0
        || cds.cbData > 64 * 1024
    {
        return None;
    }
    let s =
        unsafe { std::slice::from_raw_parts(cds.lpData as *const u16, cds.cbData as usize / 2) };
    let p = PathBuf::from(String::from_utf16_lossy(s));
    p.exists().then_some(p)
}

// ---------------------------------------------------------------- selected text

/// Text selected in the foreground app, with that app's window title.
/// Tries UI Automation first; if the focused element has no text pattern
/// and `copy_fallback` is on, simulates Ctrl+C and restores the clipboard.
/// Runs on a helper thread with a timeout so a hung app can't stall us.
pub fn selected_text(copy_fallback: bool) -> Option<(String, String)> {
    let fg = unsafe { GetForegroundWindow() };
    if fg.is_invalid() || is_own_window(fg) {
        return None;
    }
    let title = window_title(fg);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = match uia_selection() {
            UiaResult::Text(t) => Some(t),
            UiaResult::NoSelection => None,
            UiaResult::Unsupported if copy_fallback => clipboard::copy_selection(),
            UiaResult::Unsupported => None,
        };
        let _ = tx.send(r);
    });
    let text = rx
        .recv_timeout(std::time::Duration::from_millis(900))
        .ok()
        .flatten()?;
    let text = text.trim().to_string();
    (!text.is_empty()).then_some((text, title))
}

fn is_own_window(h: HWND) -> bool {
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(h, Some(&mut pid)) };
    pid == std::process::id()
}

fn window_title(h: HWND) -> String {
    let mut buf = [0u16; 256];
    let n = unsafe { GetWindowTextW(h, &mut buf) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

enum UiaResult {
    Text(String),
    NoSelection,
    Unsupported,
}

fn uia_selection() -> UiaResult {
    use windows::Win32::System::Com::*;
    use windows::Win32::UI::Accessibility::*;
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let res = (|| -> windows::core::Result<UiaResult> {
            let uia: IUIAutomation = CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER)?;
            let el = uia.GetFocusedElement()?;
            let Ok(tp) = el.GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
            else {
                return Ok(UiaResult::Unsupported);
            };
            let ranges = tp.GetSelection()?;
            let mut out = String::new();
            for i in 0..ranges.Length()? {
                let t = ranges
                    .GetElement(i)?
                    .GetText(SELECTION_CAP as i32 + 1)?
                    .to_string();
                if !t.trim().is_empty() {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(&t);
                }
            }
            Ok(if out.trim().is_empty() {
                UiaResult::NoSelection
            } else {
                UiaResult::Text(out)
            })
        })();
        CoUninitialize();
        res.unwrap_or(UiaResult::Unsupported)
    }
}

mod clipboard {
    //! Ctrl+C fallback that puts the user's clipboard back afterwards.
    use windows::Win32::Foundation::*;
    use windows::Win32::System::DataExchange::*;
    use windows::Win32::System::Memory::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    const CF_UNICODETEXT: u32 = 13;
    /// Formats whose handles are GDI objects, not global memory. Bitmaps are
    /// synthesized again from CF_DIB; metafiles can't be restored.
    const GDI_FORMATS: [u32; 7] = [2, 3, 9, 14, 0x82, 0x83, 0x8E];
    const METAFILES: [u32; 4] = [3, 14, 0x83, 0x8E];
    const MAX_SAVE: usize = 64 * 1024 * 1024;

    struct Open;
    impl Open {
        fn new() -> Option<Self> {
            for _ in 0..20 {
                if unsafe { OpenClipboard(None) }.is_ok() {
                    return Some(Open);
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            None
        }
    }
    impl Drop for Open {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    /// Copies every memory-backed format. None if the clipboard can't be put
    /// back exactly (then the fallback is skipped).
    fn save() -> Option<Vec<(u32, Vec<u8>)>> {
        let _o = Open::new()?;
        let mut out = Vec::new();
        let mut total = 0;
        let mut f = 0;
        unsafe {
            loop {
                f = EnumClipboardFormats(f);
                if f == 0 {
                    break;
                }
                if METAFILES.contains(&f) {
                    return None;
                }
                if GDI_FORMATS.contains(&f) {
                    continue;
                }
                let Ok(h) = GetClipboardData(f) else { continue };
                let g = HGLOBAL(h.0);
                let n = GlobalSize(g);
                let p = GlobalLock(g);
                if p.is_null() {
                    continue;
                }
                total += n;
                if total > MAX_SAVE {
                    let _ = GlobalUnlock(g);
                    return None;
                }
                out.push((f, std::slice::from_raw_parts(p as *const u8, n).to_vec()));
                let _ = GlobalUnlock(g);
            }
        }
        Some(out)
    }

    fn restore(saved: &[(u32, Vec<u8>)]) {
        let Some(_o) = Open::new() else { return };
        unsafe {
            let _ = EmptyClipboard();
            for (f, bytes) in saved {
                let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) else {
                    continue;
                };
                let p = GlobalLock(g);
                if p.is_null() {
                    continue;
                }
                std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
                let _ = GlobalUnlock(g);
                if SetClipboardData(*f, Some(HANDLE(g.0))).is_err() {
                    let _ = GlobalFree(Some(g));
                }
            }
            // Keep the restored copy out of clipboard history / cloud sync.
            let name = crate::util::wide("ExcludeClipboardContentFromMonitorProcessing");
            let fmt = RegisterClipboardFormatW(windows::core::PCWSTR(name.as_ptr()));
            if let Ok(g) = GlobalAlloc(GMEM_MOVEABLE, 4) {
                if SetClipboardData(fmt, Some(HANDLE(g.0))).is_err() {
                    let _ = GlobalFree(Some(g));
                }
            }
        }
    }

    fn read_text() -> Option<String> {
        let _o = Open::new()?;
        unsafe {
            let h = GetClipboardData(CF_UNICODETEXT).ok()?;
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if p.is_null() {
                return None;
            }
            let n = GlobalSize(g) / 2;
            let s = std::slice::from_raw_parts(p, n);
            let end = s.iter().position(|&c| c == 0).unwrap_or(n);
            let t = String::from_utf16_lossy(&s[..end]);
            let _ = GlobalUnlock(g);
            Some(t)
        }
    }

    fn key(vk: VIRTUAL_KEY, up: bool) -> INPUT {
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: vk,
                    dwFlags: if up {
                        KEYEVENTF_KEYUP
                    } else {
                        KEYBD_EVENT_FLAGS(0)
                    },
                    ..Default::default()
                },
            },
        }
    }

    pub fn copy_selection() -> Option<String> {
        let saved = save()?;
        let seq = unsafe { GetClipboardSequenceNumber() };
        // Ctrl goes down first so lifting Alt doesn't open a menu bar; then the
        // hotkey's other modifiers are lifted so the app sees a plain Ctrl+C.
        let mut inputs = vec![key(VK_CONTROL, false)];
        for vk in [VK_LSHIFT, VK_RSHIFT, VK_LMENU, VK_RMENU, VK_LWIN, VK_RWIN] {
            if unsafe { GetAsyncKeyState(vk.0 as i32) } as u16 & 0x8000 != 0 {
                inputs.push(key(vk, true));
            }
        }
        inputs.extend([key(VK_C, false), key(VK_C, true), key(VK_CONTROL, true)]);
        unsafe { SendInput(&inputs, std::mem::size_of::<INPUT>() as i32) };
        let mut changed = false;
        for _ in 0..30 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if unsafe { GetClipboardSequenceNumber() } != seq {
                changed = true;
                // Let the app finish writing all of its formats.
                std::thread::sleep(std::time::Duration::from_millis(30));
                break;
            }
        }
        if !changed {
            return None;
        }
        let text = read_text();
        restore(&saved);
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_detection() {
        assert_eq!(
            decode_text(b"hello\nworld").as_deref(),
            Some("hello\nworld")
        );
        assert_eq!(decode_text(b"\xEF\xBB\xBFbom").as_deref(), Some("bom"));
        assert!(decode_text(b"\x89PNG\r\n\x1a\n\0\0\0").is_none());
        assert_eq!(
            decode_text(&[0xFF, 0xFE, b'h', 0, b'i', 0]).as_deref(),
            Some("hi")
        );
        // Truncated in the middle of a multi-byte character.
        assert_eq!(
            decode_text("héllo".as_bytes()[..2].as_ref()).as_deref(),
            Some("h")
        );
    }

    #[test]
    fn file_chips() {
        let dir = std::env::temp_dir().join(format!("sb-ctx-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let txt = dir.join("notes.txt");
        std::fs::write(&txt, "the quick brown fox").unwrap();
        let c = file_chip(&txt);
        assert_eq!(c.label, "notes.txt");
        assert!(c.prompt.contains("the quick brown fox") && c.image.is_none());

        let png = dir.join("pic.png");
        image::RgbImage::from_pixel(3000, 1000, image::Rgb([200, 10, 10]))
            .save(&png)
            .unwrap();
        let c = file_chip(&png);
        let img = c.image.expect("image chip");
        assert!(img.w <= 1568 && img.w * img.h <= 1_150_000 && img.jpeg.starts_with(&[0xFF, 0xD8]));

        let bin = dir.join("blob.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3, 0, 0]).unwrap();
        let c = file_chip(&bin);
        assert!(c.prompt.contains("Binary file") && !c.prompt.contains('\u{0}'));

        let big = dir.join("big.log");
        std::fs::write(&big, "x".repeat(TEXT_CAP + 50)).unwrap();
        assert!(file_chip(&big).prompt.contains("only the first"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn compose_puts_context_first() {
        let c = Chip::selection("  some text  ", "Notepad");
        let q = compose(&[c], "what is this?");
        assert!(q.starts_with("[Text the user has selected in “Notepad”]"));
        assert!(q.contains("<selection>\nsome text\n</selection>") && q.ends_with("what is this?"));
        assert_eq!(compose(&[], "q"), "q");
    }

    #[test]
    fn explorer_menu_register_and_remove() {
        let key = r"Software\screenbuddy-test\shell\screenbuddy";
        let exe = Path::new(r"C:\x\screenbuddy.exe");
        set_menu_at(key, true, exe).unwrap();
        assert!(menu_registered_at(key));
        let cmd: String = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .open_subkey(format!(r"{key}\command"))
            .unwrap()
            .get_value("")
            .unwrap();
        assert_eq!(cmd, r#""C:\x\screenbuddy.exe" --open-file "%1""#);
        set_menu_at(key, false, exe).unwrap();
        assert!(!menu_registered_at(key));
        set_menu_at(key, false, exe).unwrap(); // removing twice is fine
        let _ = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
            .delete_subkey_all(r"Software\screenbuddy-test");
    }
}
