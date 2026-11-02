//! History browser: past conversations saved under `%APPDATA%\screenbuddy\history`.
//!
//! Each chat is a folder named by its UTC start time (`YYYYMMDD-HHMMSS`) holding
//! `chat.json` (structured, written since 0.3), `chat.md` (readable) and
//! `screenshot.jpg`. There is no index: the browser scans the folder on a
//! background thread when it opens, and everything is dropped when the palette
//! closes.

use egui::{Color32, RichText};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, channel};

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct SavedExchange {
    pub question: String,
    pub answer: String,
    /// Raw error text, if the turn failed.
    pub error: Option<String>,
    /// Labels of context attached to this question (selected text, files).
    pub attached: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Chat {
    pub created: String,
    pub provider: String,
    pub exchanges: Vec<SavedExchange>,
}

impl Chat {
    /// Text handed to the model when a past chat is continued, so every
    /// provider gets the earlier turns without provider-specific state.
    pub fn preamble(&self) -> String {
        let mut s = String::from(
            "This continues an earlier conversation with the user. Earlier turns (the screenshot from back then is not available; a current one is attached):\n\n",
        );
        for x in &self.exchanges {
            s.push_str(&format!(
                "User: {}\n\nAssistant: {}\n\n",
                x.question.trim(),
                x.answer.trim()
            ));
        }
        s.push_str("Now the user asks:\n\n");
        s
    }
}

/// Writes `chat.json` and `chat.md` into `dir`.
pub fn write(dir: &Path, chat: &Chat) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let json = serde_json::to_string_pretty(chat).map_err(std::io::Error::other)?;
    std::fs::write(dir.join("chat.json"), json)?;
    let mut md = format!(
        "# screenbuddy · {}\n\n_{}_\n\n![screenshot](screenshot.jpg)\n\n",
        chat.created, chat.provider
    );
    for x in &chat.exchanges {
        md.push_str(&format!("## {}\n\n{}\n\n", x.question, x.answer));
        if let Some(e) = &x.error {
            md.push_str(&format!("> error: {e}\n\n"));
        }
    }
    std::fs::write(dir.join("chat.md"), md)
}

/// Loads a chat folder: `chat.json`, else the older `chat.md`-only format.
pub fn load(dir: &Path) -> Option<Chat> {
    if let Ok(t) = std::fs::read_to_string(dir.join("chat.json")) {
        if let Ok(c) = serde_json::from_str::<Chat>(&t) {
            return Some(c);
        }
    }
    let md = std::fs::read_to_string(dir.join("chat.md")).ok()?;
    Some(parse_md(&md))
}

/// Parses the markdown transcript: `# title`, `_provider_`, then one
/// `## question` heading per turn followed by the answer.
pub fn parse_md(md: &str) -> Chat {
    let mut chat = Chat::default();
    let mut cur: Option<SavedExchange> = None;
    let mut body: Vec<&str> = Vec::new();
    let finish = |cur: &mut Option<SavedExchange>, body: &mut Vec<&str>, chat: &mut Chat| {
        if let Some(mut x) = cur.take() {
            let mut lines: Vec<&str> = Vec::new();
            for l in body.drain(..) {
                match l.strip_prefix("> error: ") {
                    Some(e) => x.error = Some(e.to_string()),
                    None => lines.push(l),
                }
            }
            x.answer = lines.join("\n").trim().to_string();
            chat.exchanges.push(x);
        }
    };
    for line in md.lines() {
        if let Some(q) = line.strip_prefix("## ") {
            finish(&mut cur, &mut body, &mut chat);
            cur = Some(SavedExchange {
                question: q.trim().to_string(),
                ..Default::default()
            });
        } else if cur.is_some() {
            body.push(line);
        } else if let Some(t) = line.strip_prefix("# ") {
            chat.created = t.rsplit(" · ").next().unwrap_or("").trim().to_string();
        } else if line.len() > 2 && line.starts_with('_') && line.ends_with('_') {
            chat.provider = line.trim_matches('_').to_string();
        }
    }
    finish(&mut cur, &mut body, &mut chat);
    chat
}

/// One chat in the browser list.
pub struct Entry {
    pub dir: PathBuf,
    pub when: String,
    pub title: String,
    pub turns: usize,
    /// Lowercased questions and answers, for full-text search.
    hay: String,
}

impl Entry {
    fn from_chat(dir: PathBuf, chat: &Chat) -> Self {
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut hay = String::new();
        for x in &chat.exchanges {
            hay.push_str(&x.question.to_lowercase());
            hay.push('\n');
            hay.push_str(&x.answer.to_lowercase());
            hay.push('\n');
        }
        Self {
            when: local_label(&name).unwrap_or(name),
            title: chat
                .exchanges
                .first()
                .map(|x| one_line(&x.question))
                .unwrap_or_else(|| "(empty)".into()),
            turns: chat.exchanges.len(),
            dir,
            hay,
        }
    }

    /// True if every whitespace-separated term occurs somewhere.
    pub fn matches(&self, terms: &[String]) -> bool {
        terms.iter().all(|t| {
            self.hay.contains(t.as_str()) || self.title.to_lowercase().contains(t.as_str())
        })
    }

    /// A short excerpt around the first term found in the transcript.
    pub fn snippet(&self, terms: &[String]) -> Option<String> {
        let t = terms.first()?;
        let i = self.hay.find(t.as_str())?;
        let start = self.hay[..i]
            .char_indices()
            .rev()
            .nth(40)
            .map(|(j, _)| j)
            .unwrap_or(0);
        let tail: String = self.hay[start..].chars().take(120).collect();
        Some(format!(
            "{}{}",
            if start > 0 { "…" } else { "" },
            one_line(&tail)
        ))
    }
}

pub fn terms(query: &str) -> Vec<String> {
    query.split_whitespace().map(str::to_lowercase).collect()
}

fn one_line(s: &str) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > 110 {
        format!("{}…", s.chars().take(110).collect::<String>())
    } else {
        s
    }
}

/// Chat folders, newest first.
pub fn list_dirs(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|rd| {
            rd.flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect()
        })
        .unwrap_or_default();
    v.sort_by(|a, b| b.file_name().cmp(&a.file_name()));
    v
}

/// "YYYYMMDD-HHMMSS" (UTC) → local "Nov 1, 14:03".
fn local_label(stamp: &str) -> Option<String> {
    let b = stamp.as_bytes();
    if b.len() < 15 || b[8] != b'-' {
        return None;
    }
    let n = |r: std::ops::Range<usize>| stamp.get(r)?.parse::<u16>().ok();
    use windows::Win32::Foundation::SYSTEMTIME;
    let utc = SYSTEMTIME {
        wYear: n(0..4)?,
        wMonth: n(4..6)?,
        wDay: n(6..8)?,
        wHour: n(9..11)?,
        wMinute: n(11..13)?,
        wSecond: n(13..15)?,
        ..Default::default()
    };
    let mut local = utc;
    unsafe {
        let _ =
            windows::Win32::System::Time::SystemTimeToTzSpecificLocalTime(None, &utc, &mut local);
    }
    const M: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let mon = M.get((local.wMonth as usize).checked_sub(1)?)?;
    Some(format!(
        "{mon} {}, {} · {:02}:{:02}",
        local.wDay, local.wYear, local.wHour, local.wMinute
    ))
}

/// What the user picked in the browser.
pub enum Action {
    Open(PathBuf),
}

/// The in-palette history list. Loads chats on a background thread.
pub struct Browser {
    entries: Vec<Entry>,
    rx: Option<Receiver<Entry>>,
    pub selected: usize,
}

impl Browser {
    pub fn open() -> Self {
        let (tx, rx) = channel();
        let root = crate::config::history_dir();
        std::thread::spawn(move || {
            for dir in list_dirs(&root) {
                if let Some(chat) = load(&dir) {
                    if chat.exchanges.is_empty() {
                        continue;
                    }
                    if tx.send(Entry::from_chat(dir, &chat)).is_err() {
                        return;
                    }
                }
            }
        });
        Self {
            entries: Vec::new(),
            rx: Some(rx),
            selected: 0,
        }
    }

    pub fn loading(&self) -> bool {
        self.rx.is_some()
    }

    fn pump(&mut self) {
        if let Some(rx) = &self.rx {
            loop {
                match rx.try_recv() {
                    Ok(e) => self.entries.push(e),
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        self.rx = None;
                        break;
                    }
                }
            }
        }
    }

    fn visible(&self, terms: &[String]) -> Vec<usize> {
        (0..self.entries.len())
            .filter(|&i| self.entries[i].matches(terms))
            .collect()
    }

    /// Draws the list; `up`/`down`/`enter` come from the palette's key handling.
    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        query: &str,
        keys: (bool, bool, bool),
        accent: Color32,
        dim: Color32,
        dimmer: Color32,
    ) -> Option<Action> {
        self.pump();
        let terms = terms(query);
        let vis = self.visible(&terms);
        let (up, down, enter) = keys;
        if down && self.selected + 1 < vis.len() {
            self.selected += 1;
        }
        if up {
            self.selected = self.selected.saturating_sub(1);
        }
        self.selected = self.selected.min(vis.len().saturating_sub(1));
        let mut action = None;
        if enter {
            if let Some(&i) = vis.get(self.selected) {
                action = Some(Action::Open(self.entries[i].dir.clone()));
            }
        }
        if vis.is_empty() {
            let msg = if self.loading() {
                "Loading…"
            } else if self.entries.is_empty() {
                "No saved conversations yet. Turn on “Save conversations” in Settings → General."
            } else {
                "No conversations match."
            };
            ui.label(RichText::new(msg).color(dim));
        }
        for (row, &i) in vis.iter().enumerate() {
            let e = &self.entries[i];
            let sel = row == self.selected;
            let frame = egui::Frame::new()
                .fill(if sel {
                    accent.gamma_multiply(0.22)
                } else {
                    Color32::TRANSPARENT
                })
                .corner_radius(7)
                .inner_margin(egui::Margin::symmetric(10, 6));
            let r = frame
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&e.title).strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let turns = if e.turns == 1 {
                                "1 turn".to_string()
                            } else {
                                format!("{} turns", e.turns)
                            };
                            ui.label(
                                RichText::new(format!("{} · {turns}", e.when))
                                    .color(dimmer)
                                    .size(12.0),
                            );
                        });
                    });
                    if let Some(s) = e.snippet(&terms) {
                        ui.label(RichText::new(s).color(dim).size(12.5));
                    }
                })
                .response
                .interact(egui::Sense::click());
            if sel && (up || down) {
                r.scroll_to_me(None);
            }
            if r.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if r.clicked() {
                action = Some(Action::Open(e.dir.clone()));
            }
        }
        if self.loading() {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(50));
        }
        action
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Chat {
        Chat {
            created: "2026-11-01 10:00:00".into(),
            provider: "anthropic · m".into(),
            exchanges: vec![
                SavedExchange {
                    question: "What is this error?".into(),
                    answer: "A null pointer.\n\n## not a heading in json".into(),
                    ..Default::default()
                },
                SavedExchange {
                    question: "Fix it".into(),
                    answer: "Check for None.".into(),
                    error: Some("HTTP 500".into()),
                    ..Default::default()
                },
            ],
        }
    }

    #[test]
    fn json_roundtrip_and_md_fallback() {
        let dir = std::env::temp_dir().join(format!("sb-hist-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let chat = sample();
        write(&dir, &chat).unwrap();
        assert_eq!(load(&dir).unwrap(), chat);
        // Older chats only have chat.md.
        std::fs::remove_file(dir.join("chat.json")).unwrap();
        let old = load(&dir).unwrap();
        assert_eq!(old.provider, "anthropic · m");
        assert_eq!(old.created, "2026-11-01 10:00:00");
        assert_eq!(old.exchanges[0].question, "What is this error?");
        assert_eq!(old.exchanges.last().unwrap().question, "Fix it");
        assert_eq!(old.exchanges.last().unwrap().answer, "Check for None.");
        assert_eq!(
            old.exchanges.last().unwrap().error.as_deref(),
            Some("HTTP 500")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn search_all_terms_case_insensitive() {
        let e = Entry::from_chat(PathBuf::from("20261101-100000"), &sample());
        assert!(e.matches(&terms("NULL pointer")));
        assert!(e.matches(&terms("error none")));
        assert!(!e.matches(&terms("null banana")));
        assert!(e.matches(&terms("")));
        assert!(e.snippet(&terms("pointer")).unwrap().contains("pointer"));
        assert_eq!(e.title, "What is this error?");
    }

    #[test]
    fn preamble_has_turns() {
        let p = sample().preamble();
        assert!(p.contains("User: Fix it") && p.contains("Assistant: Check for None."));
    }

    #[test]
    fn stamp_label() {
        assert!(local_label("20261101-100000").unwrap().contains("2026"));
        assert!(local_label("notastamp").is_none());
    }
}
