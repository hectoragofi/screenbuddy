//! agent-desktop integration (https://github.com/vercel-labs/agent-desktop):
//! lets the model read app UIs through the accessibility tree and act on them.
//! Actions run right away; only destructive, outbound or money actions (delete,
//! close without saving, send, post, submit, pay, buy) wait for the user's yes.
//! See [`needs_approval`].

use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const DESC: &str = "Operate apps on the user's desktop through agent-desktop (accessibility tree, not pixels). \
Pass CLI arguments as an array, e.g. [\"list-apps\"], [\"snapshot\",\"--app\",\"Slack\",\"-i\",\"--compact\"], \
[\"click\",\"@s8f3k2p9:e8\"], [\"type\",\"@s8f3k2p9:e3\",\"hello\"], [\"press\",\"ctrl+s\"], [\"launch\",\"Notepad\"]. \
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
questions in words without touching anything. Typical flow: `list-apps` to get the exact app name, \
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
        return p.is_file().then_some(p);
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
    npm.is_file().then_some(npm)
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

/// Runs a command inside the conversation's session. Before an action, the
/// agent's cursor overlay is (re)enabled with the step's intent as its label.
pub fn exec(exe: &std::path::Path, args: &[String], intent: &str, ctx: &Ctx) -> String {
    let mut args = args.to_vec();
    if !args.iter().any(|a| a == "--session") {
        args.push("--session".into());
        args.push(ctx.session.clone());
    }
    if let Some(c) = ctx.cursor.as_ref().filter(|_| !is_read_only(&args)) {
        let mut ov: Vec<String> = ["cursor-overlay", "enable", "--session", &ctx.session, "--fill", &c.fill, "--rim", &c.rim, "--accent", &c.accent]
            .iter()
            .map(|s| s.to_string())
            .collect();
        if c.label && !intent.trim().is_empty() {
            ov.push("--label".into());
            ov.push(intent.to_string());
        }
        let _ = run(exe, &ov);
    }
    run(exe, &args)
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
