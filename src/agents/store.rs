//! Persisted agent records: `<root>/<id>/agent.json`, one folder per agent,
//! which is also the agent's working directory.

use super::{Question, Status};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub brief: String,
    pub status: Status,
    /// Unix seconds.
    pub created: u64,
    /// When the latest run started / ended (unix seconds).
    pub started: Option<u64>,
    pub ended: Option<u64>,
    /// Claude CLI session, for `--resume` on follow-ups.
    pub session_id: Option<String>,
    pub plan: Vec<String>,
    /// Progress lines, oldest first (capped).
    pub steps: Vec<String>,
    /// The latest reply.
    pub result: String,
    pub error: Option<String>,
    /// Files written during the latest reply.
    pub files: Vec<PathBuf>,
    /// False when stopped or cancelled within 10 s of starting.
    pub counted: bool,
    /// Every reply in this conversation (the transcript summary).
    pub turns: Vec<TurnRecord>,
    /// The question or approval waiting on the user, if any.
    pub ask: Option<Question>,
    pub provider: String,
    /// Windows on the agent's desk (hwnds, in arrival order).
    pub desk: Vec<isize>,
    /// Its Apeiron canvas region `[x, y, w, h]`, once reserved.
    pub region: Option<[f64; 4]>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TurnRecord {
    pub prompt: String,
    pub result: String,
    pub at: u64,
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// "20261108-153012-a3f1": sortable, filesystem-safe, unique per process.
pub fn new_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static N: AtomicU32 = AtomicU32::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mix = (nanos ^ std::process::id().rotate_left(16))
        .wrapping_add(N.fetch_add(1, Ordering::Relaxed).wrapping_mul(0x9E37_79B9));
    format!("{}-{:04x}", crate::util::file_stamp(), mix & 0xffff)
}

/// A short name from the brief: its first few words.
pub fn name_from(brief: &str) -> String {
    let words: Vec<&str> = brief.split_whitespace().take(4).collect();
    let mut s = words.join(" ");
    s = s.trim_end_matches(|c: char| !c.is_alphanumeric()).to_string();
    if s.is_empty() { "Agent".into() } else { s }
}

pub fn save(root: &Path, a: &Agent) -> std::io::Result<()> {
    let dir = root.join(&a.id);
    std::fs::create_dir_all(&dir)?;
    let body = serde_json::to_string_pretty(a).map_err(std::io::Error::other)?;
    let tmp = dir.join("agent.json.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, dir.join("agent.json"))
}

pub fn load(root: &Path, id: &str) -> Option<Agent> {
    let text = std::fs::read_to_string(root.join(id).join("agent.json")).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn load_all(root: &Path) -> Vec<Agent> {
    let Ok(rd) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| load(root, &e.file_name().to_string_lossy()))
        .collect()
}

/// All terms (lowercase) appear somewhere in the agent's text.
pub fn matches(a: &Agent, terms: &[String]) -> bool {
    if terms.is_empty() {
        return true;
    }
    let mut hay = format!("{}\n{}\n{}\n{}", a.name, a.brief, a.result, a.steps.join("\n"));
    for t in &a.turns {
        hay.push('\n');
        hay.push_str(&t.prompt);
        hay.push('\n');
        hay.push_str(&t.result);
    }
    let hay = hay.to_lowercase();
    terms.iter().all(|t| hay.contains(t.as_str()))
}

/// Files under `dir` written at or after `since` (our own bookkeeping and
/// dot-folders excluded), newest first.
pub fn files_since(dir: &Path, since: SystemTime) -> Vec<PathBuf> {
    let since = since - std::time::Duration::from_secs(1);
    let mut out = Vec::new();
    walk(dir, 0, since, &mut out);
    out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    out.into_iter().map(|(p, _)| p).take(50).collect()
}

fn walk(dir: &Path, depth: usize, since: SystemTime, out: &mut Vec<(PathBuf, SystemTime)>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || (depth == 0 && name.starts_with("agent.json")) {
            continue;
        }
        let p = e.path();
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if depth < 4 {
                walk(&p, depth + 1, since, out);
            }
        } else if let Ok(m) = md.modified()
            && m >= since
        {
            out.push((p, m));
        }
    }
}
