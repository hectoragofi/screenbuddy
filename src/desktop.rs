//! agent-desktop integration (https://github.com/vercel-labs/agent-desktop):
//! lets the model read app UIs through the accessibility tree and act on them.
//! Read-only commands run freely; anything that changes state needs approval.

use serde_json::{Value, json};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const DESC: &str = "Operate apps on the user's desktop through agent-desktop (accessibility tree, not pixels). \
Pass CLI arguments as an array, e.g. [\"list-apps\"], [\"snapshot\",\"--app\",\"Slack\",\"-i\",\"--compact\"], \
[\"click\",\"@s8f3k2p9:e8\"], [\"type\",\"@s8f3k2p9:e3\",\"hello\"], [\"press\",\"ctrl+s\"], [\"launch\",\"Notepad\"]. \
Every command returns a JSON envelope {ok, data|error}. Refs look like @<snapshot>:e<n> and belong to the snapshot \
that produced them; after the UI changes (STALE_REF), snapshot again. Run `<command> --help` if unsure of flags. \
Actions are shown to the user for approval first; if one is denied, stop and tell the user briefly.";

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
];

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
    const MAX: usize = 40_000;

    let child = Command::new(exe)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
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
    }
    #[test]
    fn parses() {
        let (a, i) = parse(&json!({"args": ["press", "ctrl+s"], "intent": ""})).unwrap();
        assert_eq!(a, v(&["press", "ctrl+s"]));
        assert_eq!(i, "press ctrl+s");
        assert!(parse(&json!({"args": "click"})).is_err());
    }
}
