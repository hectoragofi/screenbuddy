//! The thread: one ongoing conversation with the buddy, typed or spoken, kept
//! across palette opens and restarts (`%APPDATA%\screenbuddy\thread.json`).
//!
//! It never goes to the model whole. Each request carries only the last
//! `thread.window` turns (text only; the screenshot is the current turn's),
//! plus a one-line, model-free summary of what fell out ("Earlier in this
//! chat: …", at most 200 chars). Turns that fell out stay searchable through
//! `recall`, sharing its 8-result cap with brain facts.
//!
//! Finished background agents post a compact line onto the latest turn
//! ("‹Flight to Lisbon› finished: …") so the buddy can follow up on them.
//!
//! The Claude CLI resumes sessions, which would grow without bound, so it gets
//! a fresh session seeded with the last `window / 2` turns whenever its
//! session would hold more than `window` turns or missed a change (a quick
//! ask, an agent line); see [`Thread::cli_plan`].

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Mutex;

/// Turns kept on disk; older ones drop off (they are in History if it's on).
const MAX_KEPT: usize = 400;
/// What one turn may contribute to a request, per side.
const TURN_CHARS: usize = 1200;
/// The "earlier in this chat" line.
const EARLIER_CHARS: usize = 200;
/// Agent lines.
const NOTE_CHARS: usize = 240;
/// Turns the palette shows when it reopens.
pub const SHOWN: usize = 8;

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Turn {
    pub question: String,
    pub answer: String,
    /// Unix ms.
    pub at: u64,
    /// Compact assistant-side lines that arrived after this turn (agent results).
    pub notes: Vec<String>,
}

/// The Claude CLI session that carries the end of the thread.
#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct CliState {
    pub session: String,
    /// The thread's `version` right after this session's last turn.
    pub version: u64,
    /// Turns in the session's context (seed + asked).
    pub held: usize,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Thread {
    pub created: String,
    /// History folder this thread is saved to (when History is on).
    pub history_dir: Option<PathBuf>,
    pub turns: Vec<Turn>,
    /// Bumped on every change, so a CLI session can tell it missed one.
    pub version: u64,
    pub cli: Option<CliState>,
}

/// What the Claude CLI should do for the next turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CliPlan {
    /// Keep the session; send only the new question.
    Resume,
    /// New session, seeded with the last `seed` turns.
    Fresh { seed: usize },
}

/// What a request carries from the thread.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Context {
    /// (user, assistant) text pairs, oldest first.
    pub turns: Vec<(String, String)>,
    /// "Earlier in this chat: …" ("" = nothing dropped).
    pub earlier: String,
    /// Claude CLI: start a fresh session seeded with `turns`.
    pub cli_fresh: bool,
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max - 1).collect();
    t.push('…');
    t
}

fn one_line(s: &str, max: usize) -> String {
    clip(&s.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

impl Turn {
    /// The assistant side as sent: the answer, then agent lines.
    fn reply(&self) -> String {
        let mut a = clip(&self.answer, TURN_CHARS);
        if a.is_empty() {
            a = "(no answer)".into();
        }
        for n in &self.notes {
            a.push_str("\n\n[");
            a.push_str(n);
            a.push(']');
        }
        a
    }
}

impl Thread {
    /// Window size from config (at least 1).
    pub fn window_of(cfg: &crate::config::Config) -> usize {
        cfg.thread.window.max(1)
    }

    /// Turns that fell out of a window of `n`.
    pub fn dropped(&self, n: usize) -> &[Turn] {
        &self.turns[..self.turns.len().saturating_sub(n)]
    }

    /// The last `n` turns.
    pub fn window(&self, n: usize) -> &[Turn] {
        &self.turns[self.turns.len().saturating_sub(n)..]
    }

    /// Topics of the dropped turns, newest first, from their first words (no
    /// model call). "" when nothing dropped.
    pub fn earlier(&self, n: usize) -> String {
        let dropped = self.dropped(n);
        if dropped.is_empty() {
            return String::new();
        }
        let head = "Earlier in this chat (use recall for details): ";
        let mut out = String::from(head);
        for t in dropped.iter().rev() {
            let words: Vec<&str> = t.question.split_whitespace().take(5).collect();
            if words.is_empty() {
                continue;
            }
            let topic = words.join(" ");
            let sep = if out.len() > head.len() { "; " } else { "" };
            if out.chars().count() + sep.len() + topic.chars().count() > EARLIER_CHARS {
                break;
            }
            out.push_str(sep);
            out.push_str(&topic);
        }
        if out.len() == head.len() { String::new() } else { out }
    }

    /// Dropped turns matching any word of `query`, best first, as short snippets.
    pub fn search_dropped(&self, query: &str, n: usize, max: usize) -> Vec<String> {
        let terms = crate::history::terms(query);
        if terms.is_empty() || max == 0 {
            return Vec::new();
        }
        let mut hits: Vec<(usize, usize, String)> = self
            .dropped(n)
            .iter()
            .enumerate()
            .filter_map(|(i, t)| {
                let hay = format!("{} {} {}", t.question, t.answer, t.notes.join(" ")).to_lowercase();
                let score = terms.iter().filter(|w| hay.contains(w.as_str())).count();
                (score > 0).then(|| {
                    let mut s = format!("You asked \"{}\"; answer: {}", one_line(&t.question, 60), one_line(&t.answer, 100));
                    if let Some(n) = t.notes.last() {
                        s.push_str(&format!(" [{}]", one_line(n, 60)));
                    }
                    (score, i, s)
                })
            })
            .collect();
        hits.sort_by(|a, b| b.0.cmp(&a.0).then(b.1.cmp(&a.1)));
        hits.into_iter().take(max).map(|h| h.2).collect()
    }

    /// Resume the CLI session while it holds fewer than `n` turns and saw
    /// every change; otherwise a fresh one seeded with `n / 2` turns, so a
    /// session never carries more than `n`.
    pub fn cli_plan(&self, n: usize) -> CliPlan {
        match &self.cli {
            Some(c) if c.version == self.version && c.held < n && !c.session.is_empty() => CliPlan::Resume,
            _ => CliPlan::Fresh { seed: self.turns.len().min(n / 2) },
        }
    }

    /// The request context for `provider`. `resumable`: the caller can keep a
    /// Claude CLI session going (the palette); a one-shot CLI (quick ask)
    /// always starts fresh with the whole window.
    pub fn context(&self, n: usize, provider: &str, resumable: bool) -> Context {
        let (turns, cli_fresh) = if provider == "claude-cli" && !resumable {
            (self.window(n), true)
        } else if provider == "claude-cli" {
            match self.cli_plan(n) {
                CliPlan::Resume => (&[][..], false),
                CliPlan::Fresh { seed } => (self.window(seed), true),
            }
        } else {
            (self.window(n), false)
        };
        // Everything before the turns this request carries; a resumed CLI
        // session got its line when it started.
        let earlier = if provider == "claude-cli" && !cli_fresh {
            String::new()
        } else {
            self.earlier(turns.len())
        };
        Context {
            turns: turns.iter().map(|t| (clip(&t.question, TURN_CHARS), t.reply())).collect(),
            earlier,
            cli_fresh,
        }
    }

    /// Adds a finished turn; `cli` = (session, turns it now holds).
    pub fn push(&mut self, question: &str, answer: &str, cli: Option<(String, usize)>) {
        self.turns.push(Turn {
            question: question.trim().to_string(),
            answer: answer.trim().to_string(),
            at: crate::brain::now_ms(),
            notes: Vec::new(),
        });
        if self.turns.len() > MAX_KEPT {
            let cut = self.turns.len() - MAX_KEPT;
            self.turns.drain(..cut);
        }
        self.version += 1;
        if self.created.is_empty() {
            self.created = crate::util::timestamp();
        }
        if let Some((session, held)) = cli {
            self.cli = Some(CliState { session, version: self.version, held });
        }
    }

    /// An agent line on the latest turn.
    pub fn note(&mut self, line: &str) {
        let line = one_line(line, NOTE_CHARS);
        if line.is_empty() {
            return;
        }
        match self.turns.last_mut() {
            Some(t) => t.notes.push(line),
            None => self.turns.push(Turn {
                question: "(while you were away)".into(),
                answer: String::new(),
                at: crate::brain::now_ms(),
                notes: vec![line],
            }),
        }
        self.version += 1;
    }

    /// Everything as a History chat.
    pub fn chat(&self, provider: String) -> crate::history::Chat {
        crate::history::Chat {
            created: self.created.clone(),
            provider,
            exchanges: self
                .turns
                .iter()
                .map(|t| crate::history::SavedExchange {
                    question: t.question.clone(),
                    answer: if t.notes.is_empty() {
                        t.answer.clone()
                    } else {
                        format!("{}\n\n_{}_", t.answer, t.notes.join(" · "))
                    },
                    ..Default::default()
                })
                .collect(),
        }
    }

    /// A thread continuing a saved chat.
    pub fn from_chat(chat: &crate::history::Chat, dir: PathBuf) -> Self {
        let mut t = Thread {
            created: chat.created.clone(),
            history_dir: Some(dir),
            ..Default::default()
        };
        for x in &chat.exchanges {
            t.push(&x.question, &x.answer, None);
        }
        t
    }
}

// ---------------------------------------------------------------- the file

static LOCK: Mutex<()> = Mutex::new(());

fn path() -> PathBuf {
    crate::config::data_dir().join("thread.json")
}

pub fn load() -> Thread {
    std::fs::read_to_string(path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save(t: &Thread) {
    let p = path();
    let tmp = p.with_extension("json.tmp");
    let ok = serde_json::to_string(t)
        .map_err(std::io::Error::other)
        .and_then(|s| std::fs::write(&tmp, s))
        .and_then(|_| std::fs::rename(&tmp, &p));
    if let Err(e) = ok {
        crate::config::log(&format!("thread: saving failed: {e}"));
    }
}

/// Changes the thread on disk under the lock and returns the new state.
pub fn update(f: impl FnOnce(&mut Thread)) -> Thread {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut t = load();
    f(&mut t);
    save(&t);
    t
}

/// Starts a new thread (the old one stays in History when that's on).
pub fn reset(to: Thread) {
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    save(&to);
}

/// Dropped turns matching `query`, for `recall`.
pub fn recall(query: &str, max: usize) -> Vec<String> {
    let n = Thread::window_of(&crate::config::load().0);
    load().search_dropped(query, n, max)
}

/// Posts each finished agent's short result onto the thread (once).
pub fn watch_agents() {
    use crate::agents::{AgentEvent, Status};
    let rx = crate::agents::subscribe();
    let _ = std::thread::Builder::new().name("screenbuddy-thread".into()).spawn(move || {
        for (id, ev) in rx {
            if !matches!(ev, AgentEvent::Status(Status::Done | Status::Failed)) {
                continue;
            }
            let Some(a) = crate::agents::get(&id) else { continue };
            update(|t| t.note(&agent_line(&a)));
        }
    });
}

/// "‹Flight to Lisbon› finished: KLM €89 … (files: C:\…\fares.md)".
pub fn agent_line(a: &crate::agents::Agent) -> String {
    if a.status == crate::agents::Status::Failed {
        return format!("\u{2039}{}\u{203a} failed: {}", a.name, one_line(a.error.as_deref().unwrap_or("unknown error"), 100));
    }
    let mut s = format!("\u{2039}{}\u{203a} finished: {}", a.name, one_line(&a.result, 140));
    if !a.files.is_empty() {
        let files: Vec<String> = a.files.iter().take(2).map(|p| p.display().to_string()).collect();
        s.push_str(&format!(" (files: {})", files.join(", ")));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(n: usize) -> Thread {
        let mut t = Thread::default();
        for i in 0..n {
            t.push(&format!("question number {i} about topic{i}"), &format!("answer {i}"), None);
        }
        t
    }

    #[test]
    fn window_is_last_n_and_text_only() {
        let t = thread(30);
        let c = t.context(6, "anthropic", true);
        assert_eq!(c.turns.len(), 6);
        assert!(c.turns[0].0.contains("number 24"));
        assert!(c.turns[5].0.contains("number 29"));
        assert!(!c.cli_fresh);
        // Dropped turns become one short line, newest first.
        assert!(c.earlier.starts_with("Earlier in this chat"));
        assert!(c.earlier.contains("number 23"));
        assert!(!c.earlier.contains("number 24"));
        assert!(c.earlier.chars().count() <= EARLIER_CHARS);
        assert_eq!(thread(4).context(6, "openai", true).earlier, "");
        // Long answers are clipped.
        let mut l = Thread::default();
        l.push("q", &"x".repeat(5000), None);
        assert!(l.context(6, "anthropic", true).turns[0].1.chars().count() <= TURN_CHARS);
    }

    #[test]
    fn recall_searches_only_dropped_turns() {
        let t = thread(30);
        let hits = t.search_dropped("topic3", 6, 8);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(hits[0].contains("number 3 "));
        assert!(t.search_dropped("topic27", 6, 8).is_empty(), "still in the window");
        assert!(t.search_dropped("", 6, 8).is_empty());
        assert_eq!(t.search_dropped("question", 6, 8).len(), 8, "capped");
    }

    #[test]
    fn agent_notes_ride_on_the_reply() {
        let mut t = thread(2);
        t.note("‹Flight› finished: KLM €89");
        let c = t.context(6, "anthropic", true);
        assert!(c.turns[1].1.contains("[‹Flight› finished: KLM €89]"));
        let mut e = Thread::default();
        e.note("x finished");
        assert_eq!(e.turns.len(), 1);
    }

    #[test]
    fn cli_rotation() {
        let n = 6;
        let mut t = thread(10);
        assert_eq!(t.cli_plan(n), CliPlan::Fresh { seed: 3 });
        let c = t.context(n, "claude-cli", true);
        assert!(c.cli_fresh);
        assert_eq!(c.turns.len(), 3);
        assert!(c.earlier.contains("number 6"), "{}", c.earlier);
        // Fresh session: seed 3 + this turn = 4 held.
        t.push("q", "a", Some(("s1".into(), 4)));
        assert_eq!(t.cli_plan(n), CliPlan::Resume);
        let c = t.context(n, "claude-cli", true);
        assert!(!c.cli_fresh && c.turns.is_empty() && c.earlier.is_empty());
        t.push("q", "a", Some(("s1".into(), 5)));
        assert_eq!(t.cli_plan(n), CliPlan::Resume);
        t.push("q", "a", Some(("s1".into(), 6)));
        assert_eq!(t.cli_plan(n), CliPlan::Fresh { seed: 3 }, "full: rotate");
        // A change the session didn't see (quick ask, agent line) rotates too.
        t.push("q", "a", Some(("s2".into(), 4)));
        assert_eq!(t.cli_plan(n), CliPlan::Resume);
        t.note("agent finished");
        assert_eq!(t.cli_plan(n), CliPlan::Fresh { seed: 3 });
        t.push("quick", "a", None);
        assert!(matches!(t.cli_plan(n), CliPlan::Fresh { .. }));
    }
}
