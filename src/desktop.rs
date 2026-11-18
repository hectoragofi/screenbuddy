//! agent-desktop integration (https://github.com/vercel-labs/agent-desktop):
//! lets the model read app UIs through the accessibility tree and act on them.
//! Actions run right away; only destructive, outbound or money actions (delete,
//! close without saving, send, post, submit, pay, buy) wait for the user's yes.
//! See [`needs_approval`].

use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const DESC: &str = "Operate apps on the user's desktop through agent-desktop (accessibility tree, not pixels). \
Pass CLI arguments as an array, e.g. [\"list-apps\"], [\"snapshot\",\"--app\",\"Slack\",\"-i\",\"--compact\"], \
[\"click\",\"@s8f3k2p9:e8\"], [\"type\",\"@s8f3k2p9:e3\",\"hello\"], [\"press\",\"ctrl+s\"], [\"launch\",\"Notepad\"]. \
To open a website or app, use one call: [\"open\",\"youtube.com\"] or [\"open\",\"https://example.com/page\"]; \
it opens with the default handler and needs no snapshot. Never type a URL into a browser's address bar instead. \
Every command returns a JSON envelope {ok, data|error}. Refs look like @<snapshot>:e<n> and belong to the snapshot \
that produced them; after the UI changes (STALE_REF), snapshot again. Run `<command> --help` if unsure of flags, or [\"skills\",\"get\",\"desktop\"] for the full guide. \
Actions that delete, send, post, submit or pay are shown to the user for approval first; if one is denied, stop and tell the user briefly.";

pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "args": {"type": "array", "items": {"type": "string"}, "description": "agent-desktop CLI arguments"},
            "intent": {"type": "string", "description": "Plain-language description of the step for the approval prompt, e.g. 'Click Send in Slack'"}
        },
        "required": ["args", "intent"]
    })
}

pub const PROMPT: &str = "You can also operate the user's apps with the `desktop` tool. Only act when the user asks \
you to do something for them (send, fill in, toggle, open...); never act on your own initiative, and answer \
questions in words without touching anything. To open a website or app, make ONE `desktop` call: \
`[\"open\",\"youtube.com\"]` (or the full URL). Do not snapshot first and do not type into a browser's address \
bar for this. For anything else, typical flow: `list-apps` to get the exact app name, \
`snapshot --app <name> -i --compact` to get refs, act on a ref, then snapshot again to confirm it worked. \
Keep each step's `intent` short and concrete. When done, say in one sentence what you did.";

/// Commands that only observe. Everything else needs the user's approval.
const READ_ONLY: &[&str] = &[
    "snapshot",
    "find",
    "get",
    "is",
    "screenshot",
    "list-surfaces",
    "list-apps",
    "list-windows",
    "list-displays",
    "list-notifications",
    "status",
    "version",
    "skills",
    "wait",
    "permissions",
    "clipboard-get",
    "trace",
];

/// Oldest agent-desktop release screenbuddy is tested against; older installs get an Update button.
pub const MIN_VERSION: &str = "1.1.0";

/// True when `v` ("1.0.2") is older than [`MIN_VERSION`].
pub fn outdated(v: &str) -> bool {
    let parse = |s: &str| s.trim().trim_start_matches('v').split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
    parse(v) < parse(MIN_VERSION)
}

pub fn is_read_only(args: &[String]) -> bool {
    let Some(cmd) = args.iter().find(|a| !a.starts_with('-')) else {
        return true;
    };
    // `--help` on anything is harmless.
    if args.iter().any(|a| a == "--help" || a == "-h") {
        return true;
    }
    READ_ONLY.contains(&cmd.as_str()) && !args.iter().any(|a| a == "--headed")
}

/// Words that mark an action as destructive, outbound or about money.
const RISKY: &[&str] = &[
    // destructive
    "delete", "deletes", "deleted", "deleting", "remove", "removes", "removing", "erase", "erasing",
    "trash", "discard", "discarding", "wipe", "purge", "uninstall", "uninstalling", "overwrite",
    "overwriting", "close", "closes", "closing", "quit", "exit", "kill", "terminate", "shutdown",
    "restart", "reboot", "logout", "unsubscribe", "cancel", "cancelling", "reset", "revoke", "block",
    // outbound
    "send", "sends", "sending", "post", "posts", "posting", "publish", "publishing", "submit",
    "submits", "submitting", "reply", "replies", "replying", "respond", "forward", "forwarding",
    "tweet", "share", "sharing", "upload", "uploading", "invite", "comment", "push", "deploy",
    "merge", "accept", "decline", "rsvp", "sign", "approve", "confirm", "confirming",
    // money
    "pay", "pays", "paying", "payment", "purchase", "purchasing", "buy", "buying", "checkout",
    "order", "ordering", "transfer", "donate", "subscribe", "subscription", "book", "booking",
    "reserve", "reservation", "bid", "refund", "wire", "tip",
];

const RISKY_PHRASES: &[&str] = &[
    "don't save", "dont save", "do not save", "without saving", "sign out", "log out", "end task",
    "check out", "empty the", "empty recycle",
];

/// Keys that commonly send, submit, delete or close.
const RISKY_KEYS: &[&str] = &[
    "return", "enter", "ctrl+return", "ctrl+enter", "cmd+return", "cmd+enter", "shift+return",
    "delete", "del", "shift+delete", "shift+del", "alt+f4", "ctrl+w", "ctrl+shift+w", "ctrl+q",
    "cmd+q", "cmd+w", "ctrl+f4",
];

/// Intent words that make pressing Enter clearly local (a search box, a path).
const SAFE_ENTER: &[&str] = &[
    "search", "find", "open", "navigate", "go", "url", "address", "filter", "line", "rename", "path",
    "folder", "launch", "run",
];

/// Commands that can't lose data or reach anyone even without a clear intent.
const BENIGN: &[&str] = &[
    "focus", "scroll", "scroll-to", "expand", "collapse", "hover", "mouse-move", "focus-window",
    "minimize", "maximize", "restore", "resize-window", "move-window", "launch", "clipboard-set",
    "cursor-overlay", "session", "open-system-surface",
];

fn words(s: &str) -> Vec<String> {
    s.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '\'')
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

fn risky_text(s: &str) -> bool {
    let lower = s.to_lowercase().replace('\u{2019}', "'");
    words(&lower).iter().any(|w| RISKY.contains(&w.as_str()))
        || RISKY_PHRASES.iter().any(|p| lower.contains(p))
}

/// The approval policy: true when this action must wait for the user's yes.
///
/// Reading never asks. An action asks when it is destructive (delete, close
/// without saving, quit), outbound (send, post, submit, reply, share, upload) or
/// about money (pay, buy, order, book), judged from the step's intent plus the
/// command line, keys like Enter / Delete / Alt+F4, and typed text that ends in
/// a newline. When the model gave no intent of its own, anything beyond plainly
/// harmless commands (focus, scroll, launch...) asks too: when unsure, ask.
pub fn needs_approval(args: &[String], intent: &str) -> bool {
    if is_read_only(args) {
        return false;
    }
    let Some(cmd_i) = args.iter().position(|a| !a.starts_with('-')) else {
        return false;
    };
    let cmd = args[cmd_i].to_lowercase();
    if cmd == "open" {
        return false; // navigation only; the user asked for it
    }
    if matches!(cmd.as_str(), "close-app" | "dismiss-all-notifications" | "clipboard-clear") {
        return true;
    }
    let positional: Vec<(usize, &String)> = args
        .iter()
        .enumerate()
        .skip(cmd_i + 1)
        .filter(|(_, a)| !a.starts_with('-'))
        .collect();
    // The typed text is content, not the action: typing "I'll send it" is fine.
    let payload = if matches!(cmd.as_str(), "type" | "set-value" | "clipboard-set") {
        positional.last().map(|p| p.0)
    } else {
        None
    };
    if let Some(i) = payload
        && cmd == "type"
        && (args[i].ends_with('\n') || args[i].ends_with('\r'))
    {
        return true; // a trailing newline usually sends
    }
    let described = args
        .iter()
        .enumerate()
        .filter(|(i, _)| Some(*i) != payload)
        .map(|(_, a)| a.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if risky_text(intent) || risky_text(&described) {
        return true;
    }
    let no_intent = intent.trim().is_empty() || intent.trim() == args.join(" ");
    if matches!(cmd.as_str(), "press" | "key-down") {
        let key = positional
            .first()
            .map(|p| p.1.to_lowercase().replace(' ', ""))
            .unwrap_or_default();
        if RISKY_KEYS.contains(&key.as_str()) {
            let local = key.contains("return") || key.contains("enter");
            let safe = local && words(intent).iter().any(|w| SAFE_ENTER.contains(&w.as_str()));
            return !safe || no_intent;
        }
    }
    if cmd == "batch" {
        let lower = described.to_lowercase();
        if lower.contains("close-app") || RISKY_KEYS.iter().any(|k| lower.contains(&format!("\"{k}\""))) {
            return true;
        }
    }
    no_intent && !BENIGN.contains(&cmd.as_str())
}

/// Finds the native agent-desktop binary: configured path, PATH, or the
/// global npm install location.
pub fn resolve(configured: &str) -> Option<PathBuf> {
    let configured = configured.trim();
    if !configured.is_empty() {
        let p = PathBuf::from(configured);
        return p.is_file().then(|| overlay_name(p));
    }
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let p = dir.join("agent-desktop.exe");
            if p.is_file() {
                return Some(p);
            }
        }
    }
    let arch = if cfg!(target_arch = "aarch64") {
        "arm64"
    } else {
        "x64"
    };
    let npm = PathBuf::from(std::env::var_os("APPDATA")?)
        .join(r"npm\node_modules\agent-desktop\bin")
        .join(format!("agent-desktop-win32-{arch}.exe"));
    npm.is_file().then(|| overlay_name(npm))
}

/// agent-desktop's Windows cursor overlay only starts from an image whose stem
/// is exactly `agent-desktop`; the npm package installs
/// `agent-desktop-win32-x64.exe`, so the overlay silently never renders. Keep
/// a correctly-named copy beside the real binary and run that instead.
fn overlay_name(exe: PathBuf) -> PathBuf {
    if exe
        .file_stem()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("agent-desktop"))
    {
        return exe;
    }
    let Some(dir) = exe.parent() else {
        return exe;
    };
    let alias = dir.join("agent-desktop.exe");
    let stale = match (std::fs::metadata(&alias), std::fs::metadata(&exe)) {
        (Ok(a), Ok(b)) => a.len() != b.len() || a.modified().ok() < b.modified().ok(),
        _ => true,
    };
    if stale && std::fs::copy(&exe, &alias).is_ok() {
        alias
    } else if alias.is_file() {
        alias
    } else {
        exe
    }
}

/// Parses tool input into (args, intent).
pub fn parse(input: &Value) -> Result<(Vec<String>, String), String> {
    let args: Vec<String> = input["args"]
        .as_array()
        .ok_or("`args` must be an array of strings")?
        .iter()
        .map(|a| {
            a.as_str()
                .map(str::to_owned)
                .ok_or("`args` must contain only strings")
        })
        .collect::<Result<_, _>>()?;
    if args.is_empty() {
        return Err("`args` is empty".into());
    }
    let intent = input["intent"].as_str().unwrap_or("").trim().to_string();
    let intent = if intent.is_empty() {
        args.join(" ")
    } else {
        intent
    };
    Ok((args, intent))
}

/// Runs one command and returns its JSON output (truncated for the model).
pub fn run(exe: &std::path::Path, args: &[String]) -> String {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const MAX: usize = 40_000;

    let spawn = |flags: u32| {
        Command::new(exe)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(flags)
            .spawn()
    };
    // Outside the turn's kill-on-close job: whatever agent-desktop launches
    // must keep running after the answer ends. A foreign job that forbids
    // breakaway refuses the flag, so fall back to a plain spawn.
    let child = spawn(CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB)
        .or_else(|_| spawn(CREATE_NO_WINDOW));
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return json!({"ok": false, "error": {"message": format!("could not start agent-desktop: {e}")}}).to_string(),
    };
    // Read output on threads so a chatty command can't fill the pipe and stall.
    let mut so = child.stdout.take();
    let mut se = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(o) = so.as_mut() {
            let _ = std::io::Read::read_to_string(o, &mut s);
        }
        s
    });
    let err_t = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(e) = se.as_mut() {
            let _ = std::io::Read::read_to_string(e, &mut s);
        }
        s
    });
    let start = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if start.elapsed() > Duration::from_secs(60) => {
                let _ = child.kill();
                return json!({"ok": false, "error": {"message": "agent-desktop timed out after 60s"}}).to_string();
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return json!({"ok": false, "error": {"message": e.to_string()}}).to_string(),
        }
    }
    let mut out = out_t.join().unwrap_or_default();
    if out.trim().is_empty() {
        out = err_t.join().unwrap_or_default();
    }
    if out.len() > MAX {
        let mut cut = MAX;
        while !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str(
            "\n…[truncated: use --root <ref>, --app, -i or --compact to narrow the snapshot]",
        );
    }
    out
}

/// Per-conversation context: an agent-desktop session (keeps refs scoped and
/// carries the cursor overlay) plus the cursor look from settings.
#[derive(Clone, Debug)]
pub struct Ctx {
    pub session: String,
    pub cursor: Option<Cursor>,
}

#[derive(Clone, Debug)]
pub struct Cursor {
    pub fill: String,
    pub rim: String,
    pub accent: String,
    pub label: bool,
}

pub fn new_session_id() -> String {
    let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("screenbuddy-{:x}", n & 0xffff_ffff_ffff)
}

/// One desktop session for the whole app run. Every palette conversation shares
/// it, so opening and closing the palette reuses one cursor overlay instead of
/// leaving one behind each time. Ended on exit by [`shutdown`].
pub fn app_session() -> &'static str {
    static SESSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    SESSION.get_or_init(new_session_id)
}

/// Ends the desktop session behind `wanted`, which stops its cursor overlay.
/// A no-op when that session was never used.
///
/// The session may have been started by another process (the MCP server the
/// Claude CLI talks to), so the local map is only a hint: sessions are also
/// found by the name they were started with.
pub fn end_session(wanted: &str) {
    let Some(exe) = crate::config::load().0.desktop_exe() else {
        return;
    };
    let mut ids: Vec<String> = Vec::new();
    if let Some(real) = SESSIONS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .remove(wanted)
    {
        ids.push(real);
    }
    for (name, id) in screenbuddy_sessions(&exe) {
        if name == wanted && !ids.iter().any(|x| x == &id) {
            ids.push(id);
        }
    }
    for id in ids {
        let args = vec!["session".to_string(), "end".to_string(), "--session".to_string(), id];
        let _ = run(&exe, &args);
    }
}

/// Live agent-desktop sessions that screenbuddy started, as `(name, id)`.
/// The MCP child starts sessions for the palette and for agents, so the local
/// map alone cannot find them; their name is the caller's session id.
fn screenbuddy_sessions(exe: &std::path::Path) -> Vec<(String, String)> {
    let out = run(exe, &["session".to_string(), "list".to_string()]);
    let Ok(v) = serde_json::from_str::<Value>(&out) else {
        return Vec::new();
    };
    let Some(list) = v["data"]["sessions"].as_array() else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|s| {
            let name = s["name"].as_str()?;
            let id = s["session_id"].as_str()?;
            (name.starts_with("screenbuddy") && s["ended_at"].is_null())
                .then(|| (name.to_string(), id.to_string()))
        })
        .collect()
}

/// Hides every live screenbuddy cursor overlay. Apeiron moves the windows under
/// a screen-space overlay, so a visible cursor would point at the wrong thing
/// after a pan; the next action enables it again (see [`exec`]).
pub fn hide_overlays() {
    let Some(exe) = crate::config::load().0.desktop_exe() else {
        return;
    };
    for (_, id) in screenbuddy_sessions(&exe) {
        let args = vec![
            "cursor-overlay".to_string(),
            "disable".to_string(),
            "--session".to_string(),
            id,
        ];
        let _ = run(&exe, &args);
    }
}

/// Hides the agent cursor when the Apeiron camera starts moving. Polls the
/// cached layout (cheap); a waker would run process spawns on the pipe thread.
pub fn watch_camera() {
    std::thread::spawn(|| {
        let mut was_moving = false;
        loop {
            std::thread::sleep(Duration::from_millis(150));
            let moving = crate::apeiron::get().is_some_and(|l| l.moving);
            if moving && !was_moving {
                hide_overlays();
            }
            was_moving = moving;
        }
    });
}

/// Ends a desktop session when dropped, for work that owns one (a background
/// agent turn).
pub struct SessionGuard(pub String);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        end_session(&self.0);
    }
}

/// Ends the app-wide session on the way out, so no overlay process is left.
pub fn shutdown() {
    end_session(app_session());
}

/// agent-desktop refuses a `--session` id it did not create: every id needs a
/// manifest from `session start` (the cursor overlay included). Our chosen id
/// is mapped to the one it hands back, once per conversation.
static SESSIONS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

fn real_session(exe: &std::path::Path, wanted: &str) -> String {
    if let Some(id) = SESSIONS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(wanted)
        .cloned()
    {
        return id;
    }
    // The name is how `end_session` finds sessions started by another process
    // (the MCP child), so it must be the caller's session id, not a constant.
    let args: Vec<String> = ["session", "start", "--name", wanted, "--no-trace"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = run(exe, &args);
    let id = serde_json::from_str::<Value>(&out)
        .ok()
        .and_then(|v| v["data"]["session_id"].as_str().map(str::to_string));
    match id {
        Some(id) => {
            SESSIONS
                .lock()
                .unwrap()
                .get_or_insert_with(HashMap::new)
                .insert(wanted.to_string(), id.clone());
            id
        }
        None => {
            crate::config::log(&format!(
                "agent-desktop session start failed: {}",
                out.trim()
            ));
            wanted.to_string()
        }
    }
}

impl Ctx {
    pub fn from_config(cfg: &crate::config::Config, session: String) -> Self {
        let a = &cfg.agent_desktop;
        let cursor = a.cursor.then(|| Cursor {
            fill: a.cursor_fill.clone(),
            rim: a.cursor_rim.clone(),
            accent: a.cursor_accent.clone(),
            label: a.cursor_label,
        });
        Self { session, cursor }
    }
}

/// A bare "open youtube" the palette can do without a model round trip.
#[derive(Debug, PartialEq)]
pub struct QuickOpen {
    /// What to hand to the shell (a full URL).
    pub target: String,
    /// What the user called it, for the one-line answer.
    pub label: String,
}

/// Common site names, so "open youtube" does not need a dot or the model.
const SITES: &[(&str, &str)] = &[
    ("youtube", "https://www.youtube.com"),
    ("google", "https://www.google.com"),
    ("gmail", "https://mail.google.com"),
    ("mail", "https://mail.google.com"),
    ("github", "https://github.com"),
    ("twitter", "https://x.com"),
    ("x", "https://x.com"),
    ("reddit", "https://www.reddit.com"),
    ("wikipedia", "https://www.wikipedia.org"),
    ("amazon", "https://www.amazon.com"),
    ("netflix", "https://www.netflix.com"),
    ("spotify", "https://open.spotify.com"),
    ("chatgpt", "https://chatgpt.com"),
    ("claude", "https://claude.ai"),
    ("maps", "https://maps.google.com"),
    ("drive", "https://drive.google.com"),
    ("docs", "https://docs.google.com"),
    ("calendar", "https://calendar.google.com"),
    ("stackoverflow", "https://stackoverflow.com"),
    ("linkedin", "https://www.linkedin.com"),
    ("instagram", "https://www.instagram.com"),
    ("facebook", "https://www.facebook.com"),
    ("twitch", "https://www.twitch.tv"),
    ("discord", "https://discord.com/app"),
    ("whatsapp", "https://web.whatsapp.com"),
    ("telegram", "https://web.telegram.org"),
    ("notion", "https://www.notion.so"),
    ("figma", "https://www.figma.com"),
    ("zoom", "https://zoom.us"),
    ("meet", "https://meet.google.com"),
    ("classroom", "https://classroom.google.com"),
    ("translate", "https://translate.google.com"),
    ("bluesky", "https://bsky.app"),
    ("threads", "https://www.threads.net"),
    ("dropbox", "https://www.dropbox.com"),
    ("onedrive", "https://onedrive.live.com"),
    ("ebay", "https://www.ebay.com"),
    ("booking", "https://www.booking.com"),
    ("airbnb", "https://www.airbnb.com"),
    ("paypal", "https://www.paypal.com"),
    ("news", "https://news.google.com"),
    ("weather", "https://weather.com"),
];

/// Web suffixes accepted without a scheme, so "open readme.md" stays a file
/// question instead of becoming https://readme.md.
const TLDS: &[&str] = &[
    "com", "org", "net", "edu", "gov", "mil", "int", "io", "ai", "app", "dev", "co", "me",
    "tv", "xyz", "info", "biz", "us", "uk", "de", "fr", "es", "it", "nl", "be", "ch", "at",
    "se", "no", "dk", "fi", "ie", "pt", "pl", "cz", "gr", "tr", "ru", "ua", "jp", "cn", "kr",
    "in", "br", "mx", "ar", "cl", "au", "nz", "ca", "za", "sg", "hk", "tw", "eu", "online",
    "site", "shop", "store", "blog", "page", "link", "live", "news", "cloud", "space",
    "world", "today", "travel", "studio", "software", "network", "digital", "media", "group",
    "team", "pro", "name", "mobi", "asia", "social", "tech", "design", "art", "life", "email",
    "chat", "wiki", "zone", "city", "global", "academy", "school", "foundation", "company",
    "business", "services", "support", "center", "community", "events", "photos", "video",
    "music", "film", "games", "play", "sports", "health", "care", "finance", "bank",
    "insurance", "properties", "homes", "deals", "market", "exchange", "trade", "capital",
    "money", "credit", "loans", "mortgage", "tax", "consulting", "management", "marketing",
];

/// Recognises only a bare open command ("open youtube", "visit https://…",
/// "go to example.com") so a request with more to it still goes to the model.
pub fn quick_open(question: &str) -> Option<QuickOpen> {
    let q = question.trim();
    if q.is_empty() || q.len() > 240 || q.contains('\n') {
        return None;
    }
    let q = q.trim_end_matches(['.', ',', ';', '!', '?']);
    let lower = q.to_ascii_lowercase();
    let rest = ["please ", "can you ", "could you ", "would you "]
        .iter()
        .find_map(|p| lower.strip_prefix(p).map(|_| &q[p.len()..]))
        .unwrap_or(q)
        .trim();
    let lower = rest.to_ascii_lowercase();
    let target = ["open ", "visit ", "go to "]
        .iter()
        .find_map(|p| lower.strip_prefix(p).map(|_| rest[p.len()..].trim()))?;
    if target.is_empty() || target.split_whitespace().count() != 1 {
        return None;
    }
    let target = target.trim_end_matches(['.', ',', ';', '!', '?']);
    let name = target.to_ascii_lowercase();
    if let Some((_, url)) = SITES.iter().find(|(n, _)| *n == name) {
        return Some(QuickOpen { target: url.to_string(), label: target.to_string() });
    }
    if name.starts_with("http://") || name.starts_with("https://") {
        return Some(QuickOpen { target: target.to_string(), label: target.to_string() });
    }
    let host = target.split('/').next().unwrap_or(target);
    let tld = host.rsplit('.').next().unwrap_or("");
    if host.contains('.')
        && TLDS.contains(&tld)
        && !host.starts_with('.')
        && !host.ends_with('.')
        && !host.contains('@')
        && !host.contains(' ')
    {
        return Some(QuickOpen { target: format!("https://{target}"), label: target.to_string() });
    }
    None
}

/// Opens a URL or app name with the user's default handler.
pub fn open_target(target: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    let target = target.trim();
    if target.is_empty() {
        return Err("nothing to open".into());
    }
    let wide = |s: &str| s.encode_utf16().chain(std::iter::once(0)).collect::<Vec<u16>>();
    let t = wide(target);
    let verb = wide("open");
    let r = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(t.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecute returns <= 32 on failure (no handler, bad path).
    if r.0 as isize <= 32 {
        Err(format!("Windows could not open {target}"))
    } else {
        Ok(())
    }
}

/// The window of the last snapshot in this session, which is what a ref
/// action acts on.
static SNAPSHOT_WINDOWS: Mutex<Option<HashMap<String, isize>>> = Mutex::new(None);

fn remember_snapshot_window(session: &str, out: &str) {
    let Some(i) = out.find("\"w-") else { return };
    let digits: String = out[i + 3..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    if let Ok(hwnd) = digits.parse::<isize>() {
        SNAPSHOT_WINDOWS
            .lock()
            .unwrap()
            .get_or_insert_with(HashMap::new)
            .insert(session.to_string(), hwnd);
    }
}

fn snapshot_window(session: &str) -> Option<isize> {
    SNAPSHOT_WINDOWS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .get(session)
        .copied()
}

fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == flag {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix(&format!("{flag}=")) {
            return Some(v);
        }
    }
    None
}

/// Whether an action happens where the user can see it. Apeiron parks agent
/// desks off-screen; an overlay there is invisible at best and a stray sliver
/// at the screen edge at worst. Unknown targets keep the cursor (the palette's
/// normal case).
fn target_visible(l: &crate::apeiron::Layout, args: &[String], window: Option<isize>) -> bool {
    if let Some(app) = arg_value(args, "--app") {
        let app = app.trim().to_ascii_lowercase();
        if app.is_empty() {
            return true;
        }
        let stem = app.strip_suffix(".exe").unwrap_or(&app);
        let mut known = false;
        for w in &l.windows {
            let exe = w.exe.to_ascii_lowercase();
            let name = exe.strip_suffix(".exe").unwrap_or(&exe);
            if exe == app || name == stem {
                known = true;
                if w.visible() {
                    return true;
                }
            }
        }
        return !known;
    }
    if args.iter().any(|a| a.starts_with('@')) {
        if let Some(h) = window {
            return l.window(h).map_or(true, |w| w.visible());
        }
    }
    true
}

/// Whether to show the agent cursor for this action. With Apeiron in canvas
/// mode, only actions on windows the camera is showing get one.
fn cursor_visible(ctx: &Ctx, args: &[String]) -> bool {
    if args.iter().any(|a| a == "--headed") {
        return true; // the user asked to watch
    }
    let Some(l) = crate::apeiron::get() else {
        return true;
    };
    if !l.canvas {
        return true;
    }
    target_visible(&l, args, snapshot_window(&ctx.session))
}

/// Apeiron owns window focus: its `focus_window` flies the camera to the
/// window and activates it. agent-desktop's own focus call can be refused
/// while another Apeiron window covers the screen, so route it through
/// Apeiron when the target is one of its windows.
fn apeiron_focus(args: &[String]) -> Option<String> {
    if !args.first()?.eq_ignore_ascii_case("focus-window") {
        return None;
    }
    let l = crate::apeiron::get()?;
    if !l.canvas {
        return None;
    }
    let target = if let Some(id) = arg_value(args, "--window-id") {
        let digits = id.trim().trim_start_matches("w-");
        digits.parse::<isize>().ok().and_then(|h| l.window(h))
    } else if let Some(app) = arg_value(args, "--app") {
        let app = app.trim().to_ascii_lowercase();
        let stem = app.strip_suffix(".exe").unwrap_or(&app);
        l.windows.iter().find(|w| {
            let exe = w.exe.to_ascii_lowercase();
            exe == app || exe.strip_suffix(".exe").unwrap_or(&exe) == stem
        })
    } else if let Some(title) = arg_value(args, "--title") {
        let title = title.to_ascii_lowercase();
        l.windows
            .iter()
            .find(|w| w.title.to_ascii_lowercase().contains(&title))
    } else {
        None
    }?;
    let (hwnd, title) = (target.hwnd, target.title.clone());
    match crate::apeiron::focus_window(hwnd) {
        Ok(()) => Some(
            json!({
                "ok": true,
                "data": {
                    "action": "focus-window",
                    "window": {"id": format!("w-{hwnd}"), "title": title}
                }
            })
            .to_string(),
        ),
        Err(e) => {
            crate::config::log(&format!("apeiron focus_window {hwnd:#x} failed: {e}"));
            None
        }
    }
}

/// Runs a command inside the conversation's session. Before an action, the
/// agent's cursor overlay is (re)enabled with the step's intent as its label.
pub fn exec(exe: &std::path::Path, args: &[String], intent: &str, ctx: &Ctx) -> String {
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("open")) {
        let Some(target) = args.get(1) else {
            return json!({"ok": false, "error": {"message": "open needs a URL or app name"}}).to_string();
        };
        return match open_target(target) {
            Ok(()) => json!({"ok": true, "data": {"opened": target}}).to_string(),
            Err(e) => json!({"ok": false, "error": {"message": e}}).to_string(),
        };
    }
    if let Some(out) = apeiron_focus(args) {
        return out;
    }
    let session = real_session(exe, &ctx.session);
    let mut args = args.to_vec();
    if !args.iter().any(|a| a == "--session") {
        args.push("--session".into());
        args.push(session.clone());
    }
    // The cursor is only drawn where Apeiron's camera is looking. Start the
    // layout client now, so the first snapshot's decision has data to use.
    crate::apeiron::start();
    if let Some(c) = ctx
        .cursor
        .as_ref()
        .filter(|_| !is_read_only(&args) && cursor_visible(ctx, &args))
    {
        let mut ov: Vec<String> = ["cursor-overlay", "enable", "--session", &session, "--fill", &c.fill, "--rim", &c.rim, "--accent", &c.accent]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if c.label && !intent.trim().is_empty() {
            ov.push("--label".into());
            ov.push(intent.to_string());
        }
        let out = run(exe, &ov);
        let ok = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v["ok"].as_bool())
            .unwrap_or(false);
        if !ok {
            crate::config::log(&format!(
                "agent-desktop cursor overlay: {}",
                out.trim()
            ));
        }
    }
    let out = run(exe, &args);
    if args.first().is_some_and(|a| a.eq_ignore_ascii_case("snapshot")) {
        remember_snapshot_window(&ctx.session, &out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    fn v(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }
    #[test]
    fn classifies() {
        assert!(is_read_only(&v(&["snapshot", "--app", "Slack", "-i"])));
        assert!(is_read_only(&v(&["click", "--help"])));
        assert!(!is_read_only(&v(&["click", "@s1:e2"])));
        assert!(!is_read_only(&v(&["snapshot", "--headed"])));
        assert!(!is_read_only(&v(&["batch", "x"])));
        assert!(is_read_only(&v(&["clipboard-get"])));
        assert!(!is_read_only(&v(&["clipboard-set", "x"])));
    }
    #[test]
    fn approval_policy() {
        let ask = |a: &[&str], i: &str| needs_approval(&v(a), i);
        // reading and everyday actions run right away
        assert!(!ask(&["snapshot", "--app", "Slack"], "Read Slack"));
        assert!(!ask(&["click", "@s1:e2"], "Open the Downloads folder"));
        assert!(!ask(&["type", "@s1:e3", "I'll send it tomorrow"], "Type the note into Notepad"));
        assert!(!ask(&["launch", "Notepad"], "launch Notepad"));
        assert!(!ask(&["press", "ctrl+s"], "Save the document"));
        assert!(!ask(&["press", "return"], "Run the search"));
        assert!(!ask(&["scroll", "@s1:e2", "--direction", "down"], ""));
        // destructive / outbound / money
        assert!(ask(&["click", "@s1:e9"], "Click Send in Outlook"));
        assert!(ask(&["click", "@s1:e9"], "Delete the selected files"));
        assert!(ask(&["click", "@s1:e9"], "Click \u{201c}Don\u{2019}t Save\u{201d}"));
        assert!(ask(&["click", "@s1:e9"], "Place the order"));
        assert!(ask(&["click", "@s1:e9"], "Pay with the saved card"));
        assert!(ask(&["close-app", "Word"], "Leave Word"));
        assert!(ask(&["press", "alt+f4"], "Leave the app"));
        assert!(ask(&["press", "Return"], "Press Enter in the chat box"));
        assert!(ask(&["type", "@s1:e3", "see you soon\n"], "Type the reply text"));
        // no intent of its own: unsure, so ask
        assert!(ask(&["click", "@s1:e2"], "click @s1:e2"));
        assert!(ask(&["press", "return"], ""));
    }
    #[test]
    fn cursor_overlay_gets_the_expected_image_name() {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("screenbuddy-overlay-{n}"));
        std::fs::create_dir_all(&dir).unwrap();
        let real = dir.join("agent-desktop-win32-x64.exe");
        std::fs::write(&real, b"binary").unwrap();
        let alias = overlay_name(real);
        assert_eq!(alias.file_name().unwrap(), "agent-desktop.exe");
        assert_eq!(std::fs::read(&alias).unwrap(), b"binary");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cursor_only_where_the_camera_shows() {
        use crate::apeiron::{Layout, Window};
        let win = |hwnd: isize, exe: &str, on_screen: bool| Window {
            hwnd,
            title: String::new(),
            exe: exe.into(),
            canvas_rect: [0.0; 4],
            screen_rect: on_screen.then(|| crate::capture::Rect { x: 0, y: 0, w: 100, h: 100 }),
            on_screen,
            tab_hidden: false,
            away: false,
        };
        let mut l = Layout::default();
        l.canvas = true;
        l.windows = vec![win(1, "notepad.exe", true), win(2, "code.exe", false)];
        let a = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // The app is on the part of the canvas the camera shows: draw the cursor.
        assert!(target_visible(&l, &a(&["snapshot", "--app", "Notepad.exe"]), None));
        // The app sits on an agent desk off-screen: no stray overlay.
        assert!(!target_visible(&l, &a(&["snapshot", "--app", "Code.exe"]), None));
        // A ref acts on the window its snapshot came from.
        assert!(!target_visible(&l, &a(&["click", "@s1:e2"]), Some(2)));
        assert!(target_visible(&l, &a(&["click", "@s1:e2"]), Some(1)));
        // Unknown targets keep the palette's cursor.
        assert!(target_visible(&l, &a(&["snapshot", "--app", "Slack"]), None));
        assert!(target_visible(&l, &a(&["press", "ctrl+s"]), None));
        // Outside Apeiron there is no canvas to be off.
        assert!(target_visible(&Layout::default(), &a(&["snapshot", "--app", "Code.exe"]), None));
    }

    #[test]
    fn snapshot_remembers_its_window() {
        let out = r#"{"ok":true,"data":{"tree":{"window":{"id":"w-12652010","title":"x"}}}}"#;
        remember_snapshot_window("screenbuddy-test", out);
        assert_eq!(snapshot_window("screenbuddy-test"), Some(12652010));
        assert_eq!(snapshot_window("nobody"), None);
    }

    #[test]
    fn quick_open_only_takes_bare_requests() {
        let q = |s| quick_open(s).map(|q| q.target);
        assert_eq!(q("open youtube").as_deref(), Some("https://www.youtube.com"));
        assert_eq!(q("Open YouTube.").as_deref(), Some("https://www.youtube.com"));
        assert_eq!(q("please open github").as_deref(), Some("https://github.com"));
        assert_eq!(q("visit example.com").as_deref(), Some("https://example.com"));
        assert_eq!(
            q("go to https://example.com/a?b=1").as_deref(),
            Some("https://example.com/a?b=1")
        );
        assert_eq!(q("open example.com/path").as_deref(), Some("https://example.com/path"));
        // Anything with more to it still goes to the model.
        assert_eq!(q("open youtube and play lofi"), None);
        assert_eq!(q("what is youtube"), None);
        assert_eq!(q("open my project"), None);
        assert_eq!(q("open source"), None);
        assert_eq!(q("open readme.md"), None);
        assert_eq!(q("open notes.txt"), None);
        assert_eq!(q("open youtube\nand more"), None);
    }

    #[test]
    fn versions() {
        assert!(outdated("1.0.2"));
        assert!(outdated("0.11.1"));
        assert!(!outdated("1.1.0"));
        assert!(!outdated("1.2.0"));
    }
    #[test]
    fn parses() {
        let (a, i) = parse(&json!({"args": ["press", "ctrl+s"], "intent": ""})).unwrap();
        assert_eq!(a, v(&["press", "ctrl+s"]));
        assert_eq!(i, "press ctrl+s");
        assert!(parse(&json!({"args": "click"})).is_err());
    }
}
