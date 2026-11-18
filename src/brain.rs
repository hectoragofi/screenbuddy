//! The brain: one small store of facts that the buddy and every agent read.
//! Agents don't pass summaries to each other; the user says "Pro is $10 now",
//! the buddy saves it here, and each agent picks it up (in its prompt at the
//! start of a turn, via `recall`, or as a `[Brain update]` appended to its next
//! tool result while it runs).
//!
//! Stored as `%APPDATA%\screenbuddy\brain.json`, written atomically. A fact
//! saved under the topic of a live fact (case-insensitive) supersedes it; the
//! old one is kept (hidden) with `superseded = true`.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

/// The user, as a fact's source.
pub const YOU: &str = "you";
const MAX_TOPIC: usize = 60;
const MAX_TEXT: usize = 300;
/// Total facts kept on disk; the oldest superseded ones go first.
const MAX_KEPT: usize = 400;
/// The always-on prompt block (pinned facts only) stays under this; the rest
/// is fetched with `recall` when a question touches it.
const PROMPT_CHARS: usize = 600;
/// `recall` returns at most this many facts.
const RECALL_MAX: usize = 8;

/// Pinned by default: preferences, and anything said to hold "always".
pub fn auto_pin(topic: &str, text: &str) -> bool {
    let t = topic.trim().to_lowercase();
    t.starts_with("preference") || format!(" {} ", text.to_lowercase()).contains(" always ")
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Fact {
    pub id: String,
    /// Short subject, e.g. "Relay pricing".
    pub topic: String,
    /// One line.
    pub text: String,
    /// "you" or the agent's name.
    pub source: String,
    /// Unix ms.
    pub created: u64,
    /// Unix ms of the last change (creation or edit).
    pub updated: u64,
    /// The fact this one replaced.
    pub replaced: Option<String>,
    pub superseded: bool,
    /// Core fact: always in the prompt (the rest comes via `recall`).
    pub pinned: bool,
}

impl Fact {
    /// "from you" / "from Writer".
    pub fn from_label(&self) -> String {
        format!("from {}", self.source)
    }
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// "just now", "4 min ago", "2 h ago", "3 d ago".
pub fn age_label(ms: u64, now: u64) -> String {
    let s = now.saturating_sub(ms) / 1000;
    match s {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", s / 60),
        3600..86_400 => format!("{} h ago", s / 3600),
        _ => format!("{} d ago", s / 86_400),
    }
}

/// Whether the brain is on.
pub fn enabled(cfg: &crate::config::Config) -> bool {
    cfg.brain.enabled
}

// ---------------------------------------------------------------- store

/// One brain file. The app uses [`global`]; tests use their own.
pub struct Store {
    path: PathBuf,
}

/// Serialises read-modify-write within this process (only the main process
/// writes; `screenbuddy --mcp` asks it over the loopback port).
static LOCK: Mutex<()> = Mutex::new(());
static SEQ: AtomicU64 = AtomicU64::new(0);

fn one_line(s: &str, max: usize) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() > max {
        let mut t: String = s.chars().take(max - 1).collect();
        t.push('…');
        t
    } else {
        s
    }
}

impl Store {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    #[allow(dead_code)]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn load(&self) -> Vec<Fact> {
        std::fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    fn save(&self, facts: &[Fact]) -> Result<(), String> {
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let body = serde_json::to_string_pretty(facts).map_err(|e| e.to_string())?;
        // Write-then-rename so a crash never leaves a half-written file.
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, body).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
    }

    /// Saves a fact; one under the topic of a live fact supersedes it.
    /// Pinned when it's a preference or says "always" (see [`auto_pin`]).
    pub fn remember(&self, topic: &str, text: &str, source: &str) -> Result<Fact, String> {
        self.remember_with(topic, text, source, None)
    }

    /// `remember` with an explicit pin (None = [`auto_pin`]).
    pub fn remember_with(&self, topic: &str, text: &str, source: &str, pinned: Option<bool>) -> Result<Fact, String> {
        let pinned = pinned.unwrap_or_else(|| auto_pin(topic, text));
        let topic = one_line(topic, MAX_TOPIC);
        let text = one_line(text, MAX_TEXT);
        if text.is_empty() {
            return Err("error: `text` is empty".into());
        }
        let topic = if topic.is_empty() { one_line(&text, 32) } else { topic };
        let source = match one_line(source, 40) {
            s if s.is_empty() => YOU.to_string(),
            s => s,
        };
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut facts = self.load();
        let now = now_ms().max(facts.iter().map(|f| f.updated + 1).max().unwrap_or(0));
        let key = topic.to_lowercase();
        let mut replaced = None;
        for f in facts.iter_mut().filter(|f| !f.superseded && f.topic.to_lowercase() == key) {
            f.superseded = true;
            replaced = Some(f.id.clone());
        }
        let fact = Fact {
            id: format!("f{now:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed) % 4096),
            topic,
            text,
            source,
            created: now,
            updated: now,
            replaced,
            superseded: false,
            pinned,
        };
        facts.push(fact.clone());
        if facts.len() > MAX_KEPT {
            let mut drop = facts.len() - MAX_KEPT;
            facts.retain(|f| {
                if drop > 0 && f.superseded {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        self.save(&facts)?;
        Ok(fact)
    }

    /// Live facts, newest first.
    pub fn live(&self) -> Vec<Fact> {
        let mut v: Vec<Fact> = self.load().into_iter().filter(|f| !f.superseded).collect();
        v.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
        v
    }

    pub fn forget(&self, id: &str) -> bool {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut facts = self.load();
        let n = facts.len();
        facts.retain(|f| f.id != id);
        n != facts.len() && self.save(&facts).is_ok()
    }

    /// Takes back a just-saved fact: forgets it and brings back the fact it
    /// superseded (if that one is still kept). False if `id` isn't there.
    pub fn undo(&self, id: &str) -> bool {
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut facts = self.load();
        let Some(i) = facts.iter().position(|f| f.id == id) else {
            return false;
        };
        let gone = facts.remove(i);
        if let Some(old) = &gone.replaced {
            if let Some(f) = facts.iter_mut().find(|f| &f.id == old) {
                f.superseded = false;
            }
        }
        self.save(&facts).is_ok()
    }

    pub fn edit(&self, id: &str, text: &str) -> Result<Fact, String> {
        let text = one_line(text, MAX_TEXT);
        if text.is_empty() {
            return Err("empty".into());
        }
        let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut facts = self.load();
        let now = now_ms().max(facts.iter().map(|f| f.updated + 1).max().unwrap_or(0));
        let f = facts
            .iter_mut()
            .find(|f| f.id == id && !f.superseded)
            .ok_or("no such fact")?;
        f.text = text;
        f.updated = now;
        let out = f.clone();
        self.save(&facts)?;
        Ok(out)
    }

    /// Live facts saved or edited after `ms`, oldest first.
    pub fn since(&self, ms: u64) -> Vec<Fact> {
        let mut v: Vec<Fact> = self.live().into_iter().filter(|f| f.updated > ms).collect();
        v.reverse();
        v
    }

    /// Up to 8 live facts matching any word of `query` (topic, text or
    /// source), best first; an empty query returns the newest 8.
    pub fn recall(&self, query: &str) -> Vec<Fact> {
        let mut v = self.search(query);
        v.truncate(RECALL_MAX);
        v
    }

    fn search(&self, query: &str) -> Vec<Fact> {
        let terms = crate::history::terms(query);
        let live = self.live();
        if terms.is_empty() {
            return live;
        }
        let mut scored: Vec<(usize, Fact)> = live
            .into_iter()
            .filter_map(|f| {
                let hay = format!("{} {} {}", f.topic, f.text, f.source).to_lowercase();
                let n = terms.iter().filter(|t| hay.contains(t.as_str())).count();
                (n > 0).then_some((n, f))
            })
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        scored.into_iter().map(|(_, f)| f).collect()
    }

    /// The tiny always-on block: pinned facts only, newest first, at most
    /// `PROMPT_CHARS` in all, plus how many more `recall` can find. Empty
    /// when there's nothing.
    pub fn prompt_block(&self) -> String {
        let facts = self.live();
        if facts.is_empty() {
            return String::new();
        }
        let mut out = String::from("Core facts (current; override anything older):");
        let mut shown = 0;
        for f in facts.iter().filter(|f| f.pinned) {
            let line = format!("\n- {}: {}", f.topic, f.text);
            // Room kept for the trailer line.
            if out.len() + line.len() > PROMPT_CHARS - 60 {
                break;
            }
            out.push_str(&line);
            shown += 1;
        }
        let rest = facts.len() - shown;
        if shown == 0 {
            out = format!("The brain holds {rest} facts; use recall when one may matter.");
        } else if rest > 0 {
            out.push_str(&format!("\n({rest} more in the brain; use recall when one may matter.)"));
        }
        out
    }
}

/// The app's brain file.
pub fn global() -> Store {
    Store::at(crate::config::data_dir().join("brain.json"))
}

#[allow(dead_code)]
pub fn remember(topic: &str, text: &str, source: &str) -> Result<Fact, String> {
    global().remember(topic, text, source)
}
/// `remember` from a tool call: `{topic, text, pinned?}`.
pub fn remember_input(input: &serde_json::Value, source: &str) -> Result<Fact, String> {
    global().remember_with(
        input["topic"].as_str().unwrap_or(""),
        input["text"].as_str().unwrap_or(""),
        source,
        input["pinned"].as_bool(),
    )
}
pub fn live() -> Vec<Fact> {
    global().live()
}
pub fn forget(id: &str) -> bool {
    global().forget(id)
}
pub fn edit(id: &str, text: &str) -> Result<Fact, String> {
    global().edit(id, text)
}
/// Undo a save (the island's "Remembered · Undo").
pub fn undo(id: &str) -> bool {
    global().undo(id)
}
pub fn since(ms: u64) -> Vec<Fact> {
    global().since(ms)
}
/// Brain facts, then turns of the thread that fell out of its window, sharing
/// the 8-result cap (an empty query lists facts only).
pub fn recall(query: &str) -> Vec<Fact> {
    let mut v = global().recall(query);
    let room = RECALL_MAX.saturating_sub(v.len());
    v.extend(crate::thread::recall(query, room).into_iter().map(|text| Fact {
        topic: "Earlier in this chat".into(),
        text,
        source: "this chat".into(),
        ..Default::default()
    }));
    v
}
pub fn prompt_block() -> String {
    global().prompt_block()
}

/// The `[Brain update]` lines appended to a tool result.
pub fn update_lines(facts: &[Fact]) -> String {
    facts
        .iter()
        .map(|f| format!("\n\n[Brain update] {}: {} ({}). Use this from now on.", f.topic, f.text, f.from_label()))
        .collect()
}

// ---------------------------------------------------------------- tools

/// Router rules for the palette / quick-ask brain.
pub const ROUTER_PROMPT: &str = "You are the user's personal assistant on this computer, and you keep a brain: what you know about them, their PC and their work (preferences, people, projects, where things live, decisions). Their background agents read the same brain. Use what's in it silently when you answer, as a good assistant who knows them would; never say \"according to my memory\" or point at the brain unless they ask what you know. Without being asked, call `remember` whenever the user (typing or speaking) states something durable about themselves or their work, a preference, a decision or a correction (\"I use Arc, not Chrome\", \"keep replies short\", \"Dana is my manager\", \"Pro is $10 now, not $12\"). Give it a topic as 'Area: thing' (\"Preferences: browser\", \"People: Dana\", \"Projects: Relay pricing\") and the fact as one plain line. Saving under an existing topic replaces that fact, so reuse the exact topic when they correct one. Never save one-off chatter, questions, requests or things only true right now. Mark it pinned only for standing preferences or rules they say hold always; everything else is found with `recall`. If the answer may depend on something the user told you before, call `recall` first; otherwise don't. Agents pick saved facts up on their own; never forward the change to them yourself. If saving was the whole point of the message, confirm in a few words.";

pub const REMEMBER_DESC: &str = "Save a durable fact about the user, their PC or their work (a preference, person, project, decision or correction) to the brain that you and every agent read. A fact under the same topic as an existing one replaces it.";

pub fn remember_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "topic": {"type": "string", "description": "'Area: thing', e.g. 'Preferences: browser', 'People: Dana', 'Projects: Relay pricing'; reuse an existing topic exactly to replace that fact"},
            "text": {"type": "string", "description": "The fact as one short plain line, e.g. 'Pro is $10 a seat'"},
            "pinned": {"type": "boolean", "description": "Always keep it in mind (standing preferences and 'always' rules). Default: preferences and 'always' facts are pinned, the rest aren't"}
        },
        "required": ["topic", "text"]
    })
}

pub const RECALL_DESC: &str = "Look up what the user told you before (the brain): preferences, people, projects, prices, decisions, where things live. Pass a few words; empty = the newest facts. Returns at most 8 short facts.";
pub const AGENT_REMEMBER_DESC: &str = "Save a durable fact to the shared brain so the user and other agents use it (a decision the user gave you or a detail you confirmed that others will need). Not for progress notes. Same topic replaces the old fact.";

pub fn recall_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {"query": {"type": "string", "description": "A few words; empty = all facts"}},
    })
}

/// The `recall` tool's text for these facts.
pub fn recall_text(facts: &[Fact]) -> String {
    if facts.is_empty() {
        return "Nothing in the brain about that.".into();
    }
    facts
        .iter()
        .map(|f| format!("- {}: {} ({})", f.topic, f.text, f.from_label()))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The agent prompt's brain section.
pub fn agent_section() -> String {
    let block = prompt_block();
    let mut p = String::from(
        "The user keeps a shared brain of facts that you and the other agents read instead of handing summaries around. Only a few core facts are below; if the work may depend on something the user decided or told you before (prices, names, preferences, where things live), call `recall` first. Use `remember` for a durable fact others will need. If a tool result ends with a [Brain update], that fact is new: follow it from then on, including in work you already did.",
    );
    if !block.is_empty() {
        p.push_str("\n\n");
        p.push_str(&block);
    }
    p
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(tag: &str) -> Store {
        let p = std::env::temp_dir().join(format!("sb-brain-{tag}-{}-{}", std::process::id(), now_ms()));
        let _ = std::fs::remove_file(&p);
        Store::at(p)
    }

    #[test]
    fn undo_forgets_and_restores_the_superseded_fact() {
        let b = store("undo");
        let a = b.remember("Relay pricing", "Pro is $12 a seat", YOU).unwrap();
        let n = b.remember("relay pricing", "Pro is $10 a seat", YOU).unwrap();
        assert_eq!(b.live().len(), 1);
        assert!(b.undo(&n.id));
        let live = b.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].id, a.id);
        assert_eq!(live[0].text, "Pro is $12 a seat");
        assert!(!b.undo(&n.id), "already undone");
        // A brand-new fact just goes away.
        let c = b.remember("Coffee", "Oat flat white", YOU).unwrap();
        assert!(b.undo(&c.id));
        assert_eq!(b.live().len(), 1);
    }

    #[test]
    fn remember_supersedes_same_topic() {
        let b = store("supersede");
        let a = b.remember("Relay pricing", "Pro is $12 a seat", YOU).unwrap();
        let n = b.remember("relay PRICING ", "Pro is $10 a seat", YOU).unwrap();
        assert_eq!(n.replaced.as_deref(), Some(a.id.as_str()));
        let live = b.live();
        assert_eq!(live.len(), 1);
        assert_eq!(live[0].text, "Pro is $10 a seat");
        // The old one is kept on disk, hidden.
        assert_eq!(b.load().len(), 2);
        assert!(b.load().iter().any(|f| f.id == a.id && f.superseded));
        let _ = std::fs::remove_file(b.path());
    }

    #[test]
    fn edit_forget_since_recall() {
        let b = store("api");
        let a = b.remember("Relay pricing", "Pro is $10", YOU).unwrap();
        let mark = a.updated;
        let c = b.remember("Launch date", "Ships Dec 3", "Writer").unwrap();
        assert!(c.updated > mark);
        assert_eq!(b.since(mark).iter().map(|f| f.id.clone()).collect::<Vec<_>>(), vec![c.id.clone()]);
        assert_eq!(b.live()[0].id, c.id, "newest first");
        let e = b.edit(&a.id, "  Pro is   $9 ").unwrap();
        assert_eq!(e.text, "Pro is $9");
        assert_eq!(b.since(c.updated).len(), 1, "an edit counts as new");
        assert_eq!(b.recall("pricing")[0].id, a.id);
        assert_eq!(b.recall("writer dec")[0].id, c.id);
        assert!(b.recall("nothing-like-this").is_empty());
        assert_eq!(b.recall("").len(), 2);
        assert!(b.forget(&a.id));
        assert!(!b.forget(&a.id));
        assert_eq!(b.live().len(), 1);
        assert!(b.remember("x", "  ", YOU).is_err());
        let _ = std::fs::remove_file(b.path());
    }

    #[test]
    fn prompt_block_is_pinned_only_and_capped() {
        let b = store("prompt");
        assert_eq!(b.prompt_block(), "");
        b.remember("Projects: Relay pricing", "Pro is $10 a seat", YOU).unwrap();
        let p = b.prompt_block();
        assert!(!p.contains("Pro is $10"), "unpinned facts stay out: {p}");
        assert!(p.contains("recall"));
        for i in 0..60 {
            b.remember(&format!("Preferences: thing {i}"), &"word ".repeat(30), YOU).unwrap();
        }
        b.remember("Replies", "Always keep replies short", YOU).unwrap();
        let p = b.prompt_block();
        assert!(p.len() <= PROMPT_CHARS, "{} chars", p.len());
        assert!(p.contains("Always keep replies short"), "newest pinned first");
        assert!(!p.contains("Pro is $10"));
        assert!(p.contains("more in the brain"));
        let _ = std::fs::remove_file(b.path());
    }

    #[test]
    fn pins_and_recall_cap() {
        assert!(auto_pin("Preferences: browser", "Arc, not Chrome"));
        assert!(auto_pin("Email", "Always cc Dana"));
        assert!(!auto_pin("People: Dana", "Dana is my manager"));
        let b = store("recallcap");
        for i in 0..20 {
            b.remember_with(&format!("Projects: p{i}"), "relay detail", YOU, Some(false)).unwrap();
        }
        assert_eq!(b.recall("relay").len(), RECALL_MAX);
        assert_eq!(b.recall("").len(), RECALL_MAX);
        assert!(b.remember_with("Pin", "x", YOU, Some(true)).unwrap().pinned);
        let _ = std::fs::remove_file(b.path());
    }

    #[test]
    fn labels() {
        assert_eq!(age_label(1_000, 30_000), "just now");
        assert_eq!(age_label(0, 5 * 60_000), "5 min ago");
        assert!(update_lines(&[Fact { topic: "T".into(), text: "x".into(), source: YOU.into(), ..Default::default() }]).contains("[Brain update] T: x (from you)"));
    }
}
