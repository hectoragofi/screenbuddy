//! `@` mentions: typing `@` in the bar lists things to attach (clipboard,
//! a file, another monitor, a web page, a past chat, a web search). Picking
//! one turns it into a context chip.

use crate::capture::{self, Rect};
use crate::config::Config;
use crate::context::{self, Chip, cap_chars};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};
use windows::Win32::UI::Controls::Dialogs::*;
use windows::core::{BOOL, PWSTR};

/// Pages are cut to this many characters of text.
const PAGE_CAP: usize = 30_000;
const PAGE_BYTES: u64 = 3 * 1024 * 1024;
/// Recent chats offered as `@chat` items.
const RECENT_CHATS: usize = 6;
/// Apeiron windows offered as `@window` items.
const MAX_WINDOWS: usize = 12;

#[derive(Clone)]
pub enum Kind {
    Clipboard,
    File,
    Web,
    Url(Option<String>),
    Screen(Rect),
    Chat(PathBuf),
    /// An Apeiron window (hwnd, title), captured even when off-screen.
    Window(isize, String),
}

#[derive(Clone)]
pub struct Item {
    /// What follows the `@` ("clipboard", "screen2", "chat").
    pub name: String,
    pub desc: String,
    pub kind: Kind,
    /// Shown dimmed and not pickable (with `desc` saying why).
    pub disabled: bool,
}

/// The mention menu while it is open.
pub struct Menu {
    items: Vec<Item>,
    pub selected: usize,
}

/// A mention being resolved off the UI thread.
pub type Pending = Receiver<Result<Chip, String>>;

/// The `@word` being typed at the end of the input, as (byte start, query).
pub fn active_token(input: &str) -> Option<(usize, &str)> {
    let start = input.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let tok = &input[start..];
    let q = tok.strip_prefix('@')?;
    // A typed link is resolved on space/Enter, not through the menu.
    if q.starts_with("http://") || q.starts_with("https://") {
        return None;
    }
    Some((start, q))
}

/// `@https://…` typed and finished with a space: (byte start, url).
pub fn typed_url(input: &str) -> Option<(usize, String)> {
    let body = input.strip_suffix(' ')?;
    let start = body.rfind(char::is_whitespace).map(|i| i + 1).unwrap_or(0);
    let url = body[start..].strip_prefix('@')?;
    (url.starts_with("http://") || url.starts_with("https://")).then(|| (start, url.to_string()))
}

impl Menu {
    pub fn open(cfg: &Config, own_monitor: Rect) -> Self {
        let web_ok = cfg.vision.web_search && cfg.provider != "openai";
        let clip_url = context::clipboard::read_text()
            .map(|t| t.trim().to_string())
            .filter(|t| (t.starts_with("http://") || t.starts_with("https://")) && !t.contains(char::is_whitespace));
        let mut items = vec![
            item("clipboard", "Clipboard text or image", Kind::Clipboard),
            item("file", "Pick a file to attach", Kind::File),
            Item {
                name: "url".into(),
                desc: match &clip_url {
                    Some(u) => format!("Read {}", cap_chars(u, 48)),
                    None => "Read a web page · type @https://… or copy a link first".into(),
                },
                disabled: clip_url.is_none(),
                kind: Kind::Url(clip_url),
            },
            Item {
                name: "web".into(),
                desc: if web_ok {
                    "Search the web for this question".into()
                } else {
                    "Web search is off (Settings › Seeing your screen)".into()
                },
                kind: Kind::Web,
                disabled: !web_ok,
            },
        ];
        for (i, r) in monitors().into_iter().enumerate() {
            if r == own_monitor {
                continue;
            }
            items.push(item(&format!("screen{}", i + 1), &format!("Screenshot of monitor {} ({}×{})", i + 1, r.w, r.h), Kind::Screen(r)));
        }
        // Apeiron windows, including ones panned away or hidden as tabs.
        if let Some(l) = crate::apeiron::get() {
            for w in l.windows.iter().filter(|w| !w.title.trim().is_empty()).take(MAX_WINDOWS) {
                let title = cap_chars(w.title.trim(), 60);
                let where_ = if w.visible() { "" } else { " · off-screen" };
                let exe = w.exe.trim_end_matches(".exe");
                items.push(item("window", &format!("Window: {title} · {exe}{where_}"), Kind::Window(w.hwnd, title)));
            }
        }
        if cfg.history {
            for dir in crate::history::list_dirs(&crate::config::history_dir()).into_iter().take(RECENT_CHATS * 2) {
                if items.iter().filter(|i| matches!(i.kind, Kind::Chat(_))).count() >= RECENT_CHATS {
                    break;
                }
                let Some(chat) = crate::history::load(&dir) else { continue };
                let Some(first) = chat.exchanges.first() else { continue };
                let title = cap_chars(first.question.lines().next().unwrap_or("").trim(), 60);
                items.push(item("chat", &format!("Past chat: {title}"), Kind::Chat(dir)));
            }
        }
        Self { items, selected: 0 }
    }

    /// Items matching what follows the `@`.
    pub fn filtered(&self, q: &str) -> Vec<&Item> {
        let q = q.to_lowercase();
        let mut v: Vec<&Item> = self
            .items
            .iter()
            .filter(|i| i.name.starts_with(&q) || (q.len() >= 2 && i.desc.to_lowercase().contains(&q)))
            .collect();
        // Pickable ones first, keeping the original order otherwise.
        v.sort_by_key(|i| i.disabled);
        v
    }
}

fn item(name: &str, desc: &str, kind: Kind) -> Item {
    Item { name: name.into(), desc: desc.into(), kind, disabled: false }
}

/// Turns a picked item into a chip. The file dialog runs here, on the UI
/// thread; everything slow runs on a worker. None = nothing to attach.
pub fn resolve(kind: Kind, owner: Option<HWND>) -> Option<Pending> {
    let (tx, rx) = channel();
    if let Kind::File = kind {
        let path = pick_file(owner)?;
        let _ = tx.send(Ok(context::file_chip(&path)));
        return Some(rx);
    }
    std::thread::spawn(move || {
        let r = match kind {
            Kind::Clipboard => clipboard_chip(),
            Kind::Web => Ok(Chip {
                label: "Web search".into(),
                hover: "Searches the web before answering".into(),
                prompt: "[The user asks you to search the web before answering this question.]".into(),
                image: None,
            }),
            Kind::Url(Some(u)) => url_chip(&u),
            Kind::Url(None) => Err("Copy a link first, or type @https://… and a space".into()),
            Kind::Screen(r) => screen_chip(r),
            Kind::Chat(dir) => chat_chip(&dir),
            Kind::Window(hwnd, title) => window_chip(hwnd, &title),
            Kind::File => unreachable!(),
        };
        let _ = tx.send(r);
    });
    Some(rx)
}

/// Resolves a typed `@https://…` link.
pub fn resolve_url(url: String) -> Pending {
    resolve(Kind::Url(Some(url)), None).expect("url resolves on a worker")
}

fn clipboard_chip() -> Result<Chip, String> {
    if let Some(bmp) = context::clipboard::read_bmp() {
        let img = image::load_from_memory_with_format(&bmp, image::ImageFormat::Bmp).map_err(|e| format!("Clipboard image could not be read: {e}"))?;
        let img = context::encode_image(img).map_err(|e| e.to_string())?;
        return Ok(Chip {
            label: "Clipboard image".into(),
            hover: format!("Image, {}x{} sent", img.w, img.h),
            prompt: "[Image from the user's clipboard, sent as the image after the screenshot]".into(),
            image: Some(img),
        });
    }
    let text = context::clipboard::read_text().filter(|t| !t.trim().is_empty()).ok_or("The clipboard is empty")?;
    let text = cap_chars(text.trim(), 20_000);
    Ok(Chip {
        label: "Clipboard".into(),
        hover: cap_chars(&text, 300),
        prompt: format!("[Text from the user's clipboard]\n<clipboard>\n{text}\n</clipboard>"),
        image: None,
    })
}

fn screen_chip(r: Rect) -> Result<Chip, String> {
    let shot = capture::grab_rect(r).map_err(|e| format!("Could not capture that monitor: {e}"))?;
    let cap = capture::encode(&shot, None).map_err(|e| e.to_string())?;
    Ok(Chip {
        label: format!("Monitor {}×{}", r.w, r.h),
        hover: format!("Screenshot of the monitor at {},{}", r.x, r.y),
        prompt: "[Screenshot of the user's other monitor, sent as the image after the main screenshot]".into(),
        image: Some(crate::llm::Image { jpeg: std::sync::Arc::new(cap.jpeg), w: cap.img_w, h: cap.img_h }),
    })
}

fn window_chip(hwnd: isize, title: &str) -> Result<Chip, String> {
    let cap = capture::window_capture(hwnd).map_err(|e| format!("Could not capture that window: {e}"))?;
    Ok(Chip {
        label: format!("Window: {}", cap_chars(title, 32)),
        hover: format!("Screenshot of \"{title}\" ({}×{})", cap.src.w, cap.src.h),
        prompt: format!("[Screenshot of the user's window \"{title}\", sent as the image after the main screenshot]"),
        image: Some(crate::llm::Image { jpeg: std::sync::Arc::new(cap.jpeg), w: cap.img_w, h: cap.img_h }),
    })
}

fn chat_chip(dir: &std::path::Path) -> Result<Chip, String> {
    let chat = crate::history::load(dir).ok_or("That chat could not be read")?;
    let mut t = String::new();
    for x in &chat.exchanges {
        t.push_str(&format!("User: {}\n\nAssistant: {}\n\n", x.question.trim(), x.answer.trim()));
    }
    let title = chat.exchanges.first().map(|x| cap_chars(x.question.trim(), 40)).unwrap_or_default();
    Ok(Chip {
        label: format!("Chat: {title}"),
        hover: cap_chars(&t, 300),
        prompt: format!("[An earlier conversation the user refers to]\n<chat>\n{}\n</chat>", cap_chars(&t, 30_000)),
        image: None,
    })
}

fn url_chip(url: &str) -> Result<Chip, String> {
    use std::io::Read;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) screenbuddy")
        .build()
        .map_err(|e| e.to_string())?;
    let resp = client.get(url).send().map_err(|e| format!("Could not open {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("{url} returned {}", resp.status()));
    }
    let html = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|c| c.contains("html"));
    let mut buf = Vec::new();
    resp.take(PAGE_BYTES).read_to_end(&mut buf).map_err(|e| e.to_string())?;
    let raw = String::from_utf8_lossy(&buf);
    let (title, text) = if html { html_text(&raw) } else { (String::new(), raw.into_owned()) };
    let text = cap_chars(text.trim(), PAGE_CAP);
    if text.is_empty() {
        return Err(format!("{url} has no readable text"));
    }
    let host = url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url).to_string();
    Ok(Chip {
        label: if title.is_empty() { host } else { cap_chars(&title, 40) },
        hover: format!("{url}\n{}", cap_chars(&text, 200)),
        prompt: format!("[Web page the user attached: {url}{}]\n<page>\n{text}\n</page>", if title.is_empty() { String::new() } else { format!(" — “{title}”") }),
        image: None,
    })
}

/// Crude HTML → (title, readable text): drops script/style/head noise and
/// tags, keeps line breaks at block elements.
pub fn html_text(html: &str) -> (String, String) {
    let lower = html.to_ascii_lowercase();
    let title = lower
        .find("<title")
        .and_then(|i| lower[i..].find('>').map(|j| i + j + 1))
        .and_then(|s| lower[s..].find("</title>").map(|e| decode(html[s..s + e].trim())))
        .unwrap_or_default();
    let mut out = String::new();
    let mut i = 0;
    let b = html.as_bytes();
    while i < b.len() {
        if b[i] == b'<' {
            let rest = &lower[i..];
            let skip = ["script", "style", "noscript", "svg", "head"].iter().find(|t| rest[1..].starts_with(**t));
            if let Some(t) = skip {
                let close = format!("</{t}");
                i = rest
                    .find(&close)
                    .and_then(|e| lower[i + e..].find('>').map(|g| i + e + g + 1))
                    .unwrap_or(b.len());
                continue;
            }
            let end = rest.find('>').map(|e| i + e + 1).unwrap_or(b.len());
            let tag = &lower[i + 1..end.saturating_sub(1).max(i + 1)];
            let name = tag.trim_start_matches('/').split(|c: char| !c.is_ascii_alphanumeric()).next().unwrap_or("");
            if matches!(name, "p" | "br" | "div" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "section" | "article" | "pre" | "blockquote") {
                out.push('\n');
            }
            i = end;
        } else {
            let next = lower[i..].find('<').map(|e| i + e).unwrap_or(b.len());
            out.push_str(&html[i..next]);
            i = next;
        }
    }
    let text = decode(&out);
    let mut clean = String::new();
    for line in text.lines() {
        let l = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if !l.is_empty() {
            clean.push_str(&l);
            clean.push('\n');
        }
    }
    (title, clean)
}

fn decode(s: &str) -> String {
    s.replace("&nbsp;", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

/// Every monitor's rectangle, in the order Windows enumerates them.
fn monitors() -> Vec<Rect> {
    unsafe extern "system" fn cb(_: HMONITOR, _: HDC, r: *mut RECT, data: LPARAM) -> BOOL {
        let v = unsafe { &mut *(data.0 as *mut Vec<Rect>) };
        v.push(Rect::from_win(unsafe { *r }));
        BOOL(1)
    }
    let mut v: Vec<Rect> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut v as *mut _ as isize));
    }
    v
}

/// Standard Open dialog. None if cancelled.
fn pick_file(owner: Option<HWND>) -> Option<PathBuf> {
    let mut buf = vec![0u16; 4096];
    let filter: Vec<u16> = "All files\0*.*\0\0".encode_utf16().collect();
    let mut ofn = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: owner.unwrap_or_default(),
        lpstrFilter: windows::core::PCWSTR(filter.as_ptr()),
        lpstrFile: PWSTR(buf.as_mut_ptr()),
        nMaxFile: buf.len() as u32,
        Flags: OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_EXPLORER,
        ..Default::default()
    };
    unsafe { GetOpenFileNameW(&mut ofn) }.as_bool().then(|| {
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        PathBuf::from(String::from_utf16_lossy(&buf[..end]))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens() {
        assert_eq!(active_token("hi @cl"), Some((3, "cl")));
        assert_eq!(active_token("@"), Some((0, "")));
        assert_eq!(active_token("hi @cl "), None);
        assert_eq!(active_token("a@b"), None);
        assert_eq!(active_token("@https://x"), None);
        assert_eq!(typed_url("see @https://x.com/a "), Some((4, "https://x.com/a".into())));
        assert_eq!(typed_url("see @https://x.com/a"), None);
    }

    #[test]
    fn html() {
        let (t, x) = html_text("<html><head><title>Hi &amp; bye</title><style>p{}</style></head><body><p>One</p><script>x()</script><div>Two  three</div></body></html>");
        assert_eq!(t, "Hi & bye");
        assert_eq!(x, "One\nTwo three\n");
    }
}
