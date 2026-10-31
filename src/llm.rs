//! Providers: Anthropic Messages API, OpenAI-compatible chat completions, and
//! the Claude Code CLI (subscription). All stream into the same event channel.

use crate::config::Config;
use base64::Engine;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;

pub const SYSTEM_PROMPT: &str = "You are screenbuddy, a calm, precise assistant built into the user's desktop. With each question you get a screenshot of what the user is looking at. Answer like a thoughtful expert sitting next to them: lead with the answer, keep it short (one to three sentences unless they ask for more), and use Markdown only when it genuinely helps, such as numbered steps or code. No preamble, no restating the question, no sign-offs.

You can also point at things on their screen with the `draw` tool, but most questions do not need it. Do not draw for explanations, summaries, translations, opinions, code or writing help, or anything you can answer clearly in words. Draw only when the user wants to find something in the interface (where is X, what do I click, which one, how do I do this here) and pointing will save them from searching. When you do draw, mark exactly one target (at most three for multi-step instructions), prefer a box or circle around it with a 1-3 word label, and keep your text short since the drawing does the work. Never mention coordinates or the tool itself.";

pub const DRAW_DESC: &str = "Point at something on the user's screen. Use only when the user needs to locate a UI element and words alone would leave them searching; never for decoration or emphasis. Coordinates are screenshot pixels, origin top-left. Kinds: box (x,y,w,h outline around the target; the default choice), circle (x,y center, r), highlight (x,y,w,h translucent fill for a text span or region), arrow (x,y tail -> x2,y2 head on a small target), text (a label at x,y). One shape is usually right.";

fn anthropic_tools(desktop: bool) -> Value {
    let mut tools = vec![json!({
        "name": "draw",
        "description": DRAW_DESC,
        "input_schema": draw_schema(),
        "eager_input_streaming": true
    })];
    if desktop {
        tools.push(json!({
            "name": "desktop",
            "description": crate::desktop::DESC,
            "input_schema": crate::desktop::schema(),
            "eager_input_streaming": true
        }));
    }
    Value::Array(tools)
}

pub fn draw_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "clear": {"type": "boolean", "description": "Remove existing annotations before drawing these."},
            "shapes": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string", "enum": ["arrow", "box", "highlight", "circle", "text"]},
                        "x": {"type": "number"},
                        "y": {"type": "number"},
                        "x2": {"type": "number", "description": "arrow head x"},
                        "y2": {"type": "number", "description": "arrow head y"},
                        "w": {"type": "number"},
                        "h": {"type": "number"},
                        "r": {"type": "number", "description": "circle radius"},
                        "label": {"type": "string", "description": "short label, 1-4 words"},
                        "color": {"type": "string", "description": "red, orange, yellow, green, blue, purple, pink, white or #rrggbb"}
                    },
                    "required": ["kind", "x", "y"]
                }
            }
        },
        "required": ["shapes"]
    })
}

pub enum Ev {
    Delta(String),
    Status(String),
    /// A `draw` tool call; the reply channel returns the tool result text.
    Draw(Value, Sender<Result<String, String>>),
    /// An agent-desktop action awaiting the user's approval: (intent, command line, reply).
    Approve(String, String, Sender<bool>),
    Error(String),
    Done(Conversation),
}

/// Provider-specific conversation history, handed to the worker per turn
/// and handed back when the turn completes.
#[derive(Default, Clone)]
pub struct Conversation {
    pub anthropic: Vec<Value>,
    pub openai: Vec<Value>,
    pub cli_session: Option<String>,
    pub image_sent: bool,
    /// Warm Claude CLI process (claude-cli provider only).
    pub cli: CliSlot,
}

pub struct Image {
    pub jpeg: Arc<Vec<u8>>,
    pub w: u32,
    pub h: u32,
}

pub struct Turn {
    pub cfg: Config,
    pub conv: Conversation,
    pub question: String,
    pub image: Option<Image>,
    pub cancel: Arc<AtomicBool>,
}

/// Sender that wakes the UI after each event.
#[derive(Clone)]
pub struct Out {
    tx: Sender<Ev>,
    ctx: egui::Context,
}

impl Out {
    pub fn new(tx: Sender<Ev>, ctx: egui::Context) -> Self {
        Self { tx, ctx }
    }
    fn send(&self, ev: Ev) {
        let _ = self.tx.send(ev);
        self.ctx.request_repaint();
    }
    fn delta(&self, s: impl Into<String>) {
        self.send(Ev::Delta(s.into()));
    }
    fn status(&self, s: impl Into<String>) {
        self.send(Ev::Status(s.into()));
    }
    /// Asks the user to approve an action; false on deny or timeout.
    pub fn approve(&self, intent: &str, detail: &str) -> bool {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Ev::Approve(intent.to_string(), detail.to_string(), tx));
        rx.recv_timeout(std::time::Duration::from_secs(300))
            .unwrap_or(false)
    }

    /// Runs one `desktop` tool call: read-only commands directly, actions after approval.
    fn desktop(&self, input: &Value, exe: &std::path::Path) -> (String, bool) {
        let (args, intent) = match crate::desktop::parse(input) {
            Ok(v) => v,
            Err(e) => return (format!("error: {e}"), true),
        };
        if !crate::desktop::is_read_only(&args) {
            if !self.approve(&intent, &args.join(" ")) {
                return (
                    "The user declined this action. Stop and tell them briefly.".into(),
                    true,
                );
            }
            self.status("acting…");
        }
        let out = crate::desktop::run(exe, &args);
        let ok = serde_json::from_str::<Value>(&out)
            .ok()
            .and_then(|v| v["ok"].as_bool())
            .unwrap_or(true);
        self.status("thinking…");
        (out, !ok)
    }

    /// Dispatches a tool call by name.
    fn tool(&self, name: &str, input: Value, exe: Option<&std::path::Path>) -> (String, bool) {
        match (name, exe) {
            ("draw", _) => match self.draw(input) {
                Ok(t) => (t, false),
                Err(e) => (e, true),
            },
            ("desktop", Some(exe)) => self.desktop(&input, exe),
            _ => ("error: unknown tool".into(), true),
        }
    }

    /// Sends a draw call to the UI thread and waits for its result.
    fn draw(&self, input: Value) -> Result<String, String> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.send(Ev::Draw(input, tx));
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| Err("overlay unavailable".into()))
    }
}

pub fn spawn(turn: Turn, out: Out) {
    std::thread::spawn(move || {
        let mut conv = turn.conv.clone();
        let res = match turn.cfg.provider.as_str() {
            "openai" => openai(&turn, &mut conv, &out),
            "claude-cli" => claude_cli(&turn, &mut conv, &out),
            _ => anthropic(&turn, &mut conv, &out),
        };
        if let Err(e) = res {
            if !turn.cancel.load(Ordering::Relaxed) {
                crate::config::log(&format!("provider error: {e:#}"));
                out.send(Ev::Error(format!("{e:#}")));
            }
        }
        out.send(Ev::Done(conv));
    });
}

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(None)
        .build()?)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn system_prompt(cfg: &Config) -> String {
    let mut p = SYSTEM_PROMPT.to_string();
    if cfg.desktop_exe().is_some() {
        p.push_str("\n\n");
        p.push_str(crate::desktop::PROMPT);
    }
    if !cfg.extra_instructions.trim().is_empty() {
        p.push_str("\n\n");
        p.push_str(cfg.extra_instructions.trim());
    }
    p
}

fn question_text(turn: &Turn) -> String {
    match &turn.image {
        Some(img) => format!(
            "[Screenshot attached: {}x{} px]\n\n{}",
            img.w, img.h, turn.question
        ),
        None => turn.question.clone(),
    }
}

fn check_status(resp: reqwest::blocking::Response) -> anyhow::Result<reqwest::blocking::Response> {
    if resp.status().is_success() {
        return Ok(resp);
    }
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    let msg = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .unwrap_or(body);
    anyhow::bail!("HTTP {status}: {msg}")
}

/// Iterates SSE `data:` payloads until the stream ends or is cancelled.
fn sse_lines(
    resp: reqwest::blocking::Response,
    cancel: &AtomicBool,
    mut f: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    for line in BufReader::new(resp).lines() {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let line = line?;
        if let Some(data) = line.strip_prefix("data:") {
            if !f(data.trim_start())? {
                break;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- Anthropic

fn anthropic(turn: &Turn, conv: &mut Conversation, out: &Out) -> anyhow::Result<()> {
    let cfg = &turn.cfg.anthropic;
    let key = if cfg.api_key.trim().is_empty() {
        std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
    } else {
        cfg.api_key.trim().to_string()
    };
    if key.is_empty() {
        anyhow::bail!(
            "No Anthropic API key. Set anthropic.api_key in config.toml or the ANTHROPIC_API_KEY env var."
        );
    }
    let mut content = Vec::new();
    if let Some(img) = &turn.image {
        content.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64(&img.jpeg)}}));
    }
    content.push(json!({"type": "text", "text": question_text(turn)}));
    conv.anthropic
        .push(json!({"role": "user", "content": content}));

    let client = http()?;
    let exe = turn.cfg.desktop_exe();
    let url = format!("{}/v1/messages", cfg.base_url.trim_end_matches('/'));
    let mut wrote_text = false;
    for _round in 0..8 {
        let mut body = json!({
            "model": cfg.model,
            "max_tokens": cfg.max_tokens,
            "system": system_prompt(&turn.cfg),
            "messages": conv.anthropic,
            "stream": true,
            "output_config": {"effort": cfg.effort},
            "tools": anthropic_tools(exe.is_some()),
        });
        let mut req = client
            .post(&url)
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json");
        if cfg.fallbacks {
            body["fallbacks"] = json!("default");
            req = req.header("anthropic-beta", "server-side-fallback-2026-07-01");
        }
        out.status("thinking…");
        let resp = check_status(req.body(body.to_string()).send()?)?;

        let mut blocks: BTreeMap<u64, Value> = BTreeMap::new();
        let mut partial: BTreeMap<u64, String> = BTreeMap::new();
        let mut stop_reason = String::new();
        let mut stream_err: Option<String> = None;
        let mut tool_results: Vec<Value> = Vec::new();

        sse_lines(resp, &turn.cancel, |data| {
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                return Ok(true);
            };
            let idx = ev["index"].as_u64().unwrap_or(0);
            match ev["type"].as_str().unwrap_or("") {
                "content_block_start" => {
                    let block = ev["content_block"].clone();
                    match block["type"].as_str() {
                        Some("tool_use") if block["name"] == "draw" => out.status("drawing…"),
                        Some("text") => {
                            if wrote_text && !blocks.values().any(|b| b["type"] == "text") {
                                out.delta("\n\n");
                            }
                        }
                        _ => {}
                    }
                    blocks.insert(idx, block);
                }
                "content_block_delta" => {
                    let d = &ev["delta"];
                    let Some(b) = blocks.get_mut(&idx) else {
                        return Ok(true);
                    };
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or("");
                            append(b, "text", t);
                            wrote_text = true;
                            out.delta(t);
                        }
                        "thinking_delta" => {
                            append(b, "thinking", d["thinking"].as_str().unwrap_or(""))
                        }
                        "signature_delta" => b["signature"] = d["signature"].clone(),
                        "input_json_delta" => partial
                            .entry(idx)
                            .or_default()
                            .push_str(d["partial_json"].as_str().unwrap_or("")),
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if let Some(b) = blocks.get_mut(&idx) {
                        if b["type"] == "tool_use" {
                            let raw = partial.remove(&idx).unwrap_or_default();
                            let parsed = if raw.trim().is_empty() {
                                Ok(json!({}))
                            } else {
                                serde_json::from_str::<Value>(&raw)
                            };
                            let id = b["id"].clone();
                            let (text, is_err) = match parsed {
                                Ok(input) => {
                                    b["input"] = input.clone();
                                    out.tool(
                                        b["name"].as_str().unwrap_or(""),
                                        input,
                                        exe.as_deref(),
                                    )
                                }
                                Err(e) => {
                                    // Keep the history valid: the block needs an object input.
                                    b["input"] = json!({});
                                    (
                                        format!(
                                            "INVALID_JSON: tool input was not valid JSON ({e}). Call draw again."
                                        ),
                                        true,
                                    )
                                }
                            };
                            tool_results.push(json!({"type": "tool_result", "tool_use_id": id, "content": text, "is_error": is_err}));
                            out.status("thinking…");
                        }
                    }
                }
                "message_delta" => {
                    if let Some(s) = ev["delta"]["stop_reason"].as_str() {
                        stop_reason = s.to_string();
                    }
                }
                "error" => {
                    stream_err = Some(
                        ev["error"]["message"]
                            .as_str()
                            .unwrap_or("stream error")
                            .to_string(),
                    );
                    return Ok(false);
                }
                "message_stop" => return Ok(false),
                _ => {}
            }
            Ok(true)
        })?;

        if turn.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(e) = stream_err {
            anyhow::bail!("{e}");
        }
        conv.anthropic.push(
            json!({"role": "assistant", "content": blocks.into_values().collect::<Vec<_>>()}),
        );
        match stop_reason.as_str() {
            "tool_use" if !tool_results.is_empty() => {
                conv.anthropic
                    .push(json!({"role": "user", "content": tool_results}));
            }
            "pause_turn" => {}
            "refusal" => {
                out.delta("\n\n*The model declined this request.*");
                return Ok(());
            }
            "max_tokens" => {
                out.delta("\n\n*(answer truncated: max_tokens reached)*");
                return Ok(());
            }
            _ => return Ok(()),
        }
    }
    Ok(())
}

fn append(block: &mut Value, field: &str, s: &str) {
    let cur = block[field].as_str().unwrap_or("").to_string();
    block[field] = Value::String(cur + s);
}

// ---------------------------------------------------------------- OpenAI-compatible

fn openai(turn: &Turn, conv: &mut Conversation, out: &Out) -> anyhow::Result<()> {
    let cfg = &turn.cfg.openai;
    let key = if cfg.api_key.trim().is_empty() {
        std::env::var("OPENAI_API_KEY").unwrap_or_default()
    } else {
        cfg.api_key.trim().to_string()
    };
    if conv.openai.is_empty() {
        conv.openai
            .push(json!({"role": "system", "content": system_prompt(&turn.cfg)}));
    }
    let mut content = vec![json!({"type": "text", "text": question_text(turn)})];
    if let Some(img) = &turn.image {
        content.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", b64(&img.jpeg))}}));
    }
    conv.openai
        .push(json!({"role": "user", "content": content}));

    let client = http()?;
    let exe = turn.cfg.desktop_exe();
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let mut wrote_text = false;
    for _round in 0..8 {
        let mut body = json!({
            "model": cfg.model,
            "messages": conv.openai,
            "stream": true,
            "max_tokens": cfg.max_tokens,
        });
        if cfg.tools {
            let mut tools = vec![
                json!({"type": "function", "function": {"name": "draw", "description": DRAW_DESC, "parameters": draw_schema()}}),
            ];
            if exe.is_some() {
                tools.push(json!({"type": "function", "function": {"name": "desktop", "description": crate::desktop::DESC, "parameters": crate::desktop::schema()}}));
            }
            body["tools"] = json!(tools);
        }
        let mut req = client.post(&url).header("content-type", "application/json");
        if !key.is_empty() {
            req = req.bearer_auth(&key);
        }
        out.status("thinking…");
        let resp = check_status(req.body(body.to_string()).send()?)?;

        let mut text = String::new();
        let mut calls: BTreeMap<u64, (String, String, String)> = BTreeMap::new();
        let mut finish = String::new();
        let mut stream_err = None;
        sse_lines(resp, &turn.cancel, |data| {
            if data == "[DONE]" {
                return Ok(false);
            }
            let Ok(ev) = serde_json::from_str::<Value>(data) else {
                return Ok(true);
            };
            if let Some(e) = ev.get("error") {
                stream_err = Some(e["message"].as_str().unwrap_or("stream error").to_string());
                return Ok(false);
            }
            let choice = &ev["choices"][0];
            let delta = &choice["delta"];
            if let Some(t) = delta["content"].as_str() {
                if !t.is_empty() {
                    if text.is_empty() && wrote_text {
                        out.delta("\n\n");
                    }
                    text.push_str(t);
                    wrote_text = true;
                    out.delta(t);
                }
            }
            if let Some(tcs) = delta["tool_calls"].as_array() {
                out.status("drawing…");
                for tc in tcs {
                    let e = calls.entry(tc["index"].as_u64().unwrap_or(0)).or_default();
                    if let Some(id) = tc["id"].as_str() {
                        e.0 = id.to_string();
                    }
                    if let Some(n) = tc["function"]["name"].as_str() {
                        e.1.push_str(n);
                    }
                    if let Some(a) = tc["function"]["arguments"].as_str() {
                        e.2.push_str(a);
                    }
                }
            }
            if let Some(f) = choice["finish_reason"].as_str() {
                finish = f.to_string();
            }
            Ok(true)
        })?;
        if turn.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Some(e) = stream_err {
            anyhow::bail!("{e}");
        }
        if calls.is_empty() {
            conv.openai
                .push(json!({"role": "assistant", "content": text}));
            if finish == "length" {
                out.delta("\n\n*(answer truncated)*");
            }
            return Ok(());
        }
        let tool_calls: Vec<Value> = calls
            .values()
            .map(|(id, name, args)| json!({"id": id, "type": "function", "function": {"name": name, "arguments": args}}))
            .collect();
        conv.openai.push(json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }, "tool_calls": tool_calls}));
        for (id, name, args) in calls.values() {
            let input =
                serde_json::from_str::<Value>(if args.trim().is_empty() { "{}" } else { args });
            let result = match input {
                Ok(input) => out.tool(name, input, exe.as_deref()).0,
                Err(e) => format!("error: arguments were not valid JSON ({e})"),
            };
            conv.openai
                .push(json!({"role": "tool", "tool_call_id": id, "content": result}));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- Claude Code CLI

/// A running `claude -p` in stream-json mode. Started when the palette opens
/// (so Node is warm by the time the user hits Enter) and reused for
/// follow-ups; killed when the palette closes.
pub struct CliProc {
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    lines: std::sync::mpsc::Receiver<String>,
    stderr: Arc<std::sync::Mutex<String>>,
    approver: Arc<std::sync::Mutex<Option<Out>>>,
    done: Arc<AtomicBool>,
}

impl Drop for CliProc {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
    }
}

impl CliProc {
    fn alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stderr_tail(&self) -> String {
        let s = self.stderr.lock().unwrap();
        let lines: Vec<&str> = s.lines().collect();
        lines[lines.len().saturating_sub(6)..].join("\n")
    }
}

/// Shared slot for the warm CLI process, carried inside `Conversation`.
pub type CliSlot = Arc<std::sync::Mutex<Option<CliProc>>>;

/// Starts the Claude CLI in the background so the first answer starts fast.
pub fn prewarm(cfg: &Config, conv: &Conversation) {
    if cfg.provider != "claude-cli" {
        return;
    }
    let cfg = cfg.clone();
    let slot = conv.cli.clone();
    let resume = conv.cli_session.clone();
    std::thread::spawn(move || {
        let mut guard = slot.lock().unwrap();
        if guard.is_some() {
            return;
        }
        // Fast path: a fresh spare kept warm since the last palette closed.
        if resume.is_none() {
            if let Some(p) = take_spare(&cfg) {
                *guard = Some(p);
                return;
            }
        }
        match spawn_cli(&cfg, resume.as_deref()) {
            Ok(p) => *guard = Some(p),
            Err(e) => crate::config::log(&format!("claude prewarm failed: {e:#}")),
        }
    });
}

/// A fresh, unused CLI process kept between palette sessions so the next
/// question starts instantly. Expires after `claude_cli.keep_warm_minutes`.
struct Spare {
    proc: CliProc,
    born: std::time::Instant,
    sig: String,
}

static SPARE: std::sync::Mutex<Option<Spare>> = std::sync::Mutex::new(None);

fn spare_sig(cfg: &Config) -> String {
    format!(
        "{}|{}|{}|{}",
        cfg.claude_cli.command,
        cfg.claude_cli.model,
        system_prompt(cfg),
        cfg.desktop_exe().is_some()
    )
}

fn take_spare(cfg: &Config) -> Option<CliProc> {
    let mut s = SPARE.lock().unwrap().take()?;
    (s.sig == spare_sig(cfg) && s.proc.alive()).then_some(s.proc)
}

/// Called after the palette closes: keeps one fresh process warm for a while.
pub fn refill_spare(cfg: &Config) {
    let minutes = cfg.claude_cli.keep_warm_minutes;
    if cfg.provider != "claude-cli" || minutes == 0 {
        *SPARE.lock().unwrap() = None;
        return;
    }
    let cfg = cfg.clone();
    std::thread::spawn(move || {
        let sig = spare_sig(&cfg);
        {
            let mut slot = SPARE.lock().unwrap();
            if let Some(s) = slot.as_mut() {
                if s.sig == sig && s.proc.alive() {
                    s.born = std::time::Instant::now(); // still unused: extend
                } else {
                    *slot = None;
                }
            }
            if slot.is_none() {
                match spawn_cli(&cfg, None) {
                    Ok(proc) => {
                        *slot = Some(Spare {
                            proc,
                            born: std::time::Instant::now(),
                            sig,
                        })
                    }
                    Err(e) => {
                        crate::config::log(&format!("claude spare failed: {e:#}"));
                        return;
                    }
                }
            }
        }
        let ttl = std::time::Duration::from_secs(minutes as u64 * 60);
        std::thread::sleep(ttl);
        let mut slot = SPARE.lock().unwrap();
        if slot.as_ref().is_some_and(|s| s.born.elapsed() >= ttl) {
            *slot = None; // dropping kills it; idle RAM goes back to ~0
            crate::win::trim_memory();
        }
    });
}

fn spawn_cli(cfg: &Config, resume: Option<&str>) -> anyhow::Result<CliProc> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let full_cfg = cfg;
    let cfg = &full_cfg.claude_cli;
    let exe = std::env::current_exe()?;
    let done = Arc::new(AtomicBool::new(false));
    let approver: Arc<std::sync::Mutex<Option<Out>>> = Default::default();
    // Actions requested through the MCP server are approved here, over a
    // loopback socket, so the user sees the same Allow / Don't Allow card.
    let mut env = serde_json::Map::new();
    let desktop = full_cfg.desktop_exe().is_some();
    if desktop {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        env.insert(
            "SCREENBUDDY_APPROVAL_PORT".into(),
            json!(listener.local_addr()?.port().to_string()),
        );
        spawn_approval_server(listener, approver.clone(), done.clone());
    }
    let mcp = json!({"mcpServers": {"screenbuddy": {"type": "stdio", "command": exe.to_string_lossy(), "args": ["--mcp"], "env": env}}}).to_string();
    let allowed = if desktop {
        "mcp__screenbuddy__draw mcp__screenbuddy__desktop"
    } else {
        "mcp__screenbuddy__draw"
    };
    let system = system_prompt(full_cfg);
    let build = |program: &str| {
        let mut c = Command::new(program);
        c.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
            "--include-partial-messages",
        ])
        .args(["--system-prompt", &system])
        .args(["--tools", ""])
        .args(["--strict-mcp-config", "--mcp-config", &mcp])
        .args(["--allowedTools", allowed]);
        if !cfg.model.trim().is_empty() {
            c.args(["--model", cfg.model.trim()]);
        }
        if let Some(id) = resume {
            c.args(["--resume", id]);
        }
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        c
    };
    let mut child = match build(&cfg.command).spawn() {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !cfg.command.ends_with(".cmd") => {
            build(&format!("{}.cmd", cfg.command))
                .spawn()
                .map_err(|_| {
                    anyhow::anyhow!(
                        "'{}' not found. Install Claude Code or set claude_cli.command.",
                        cfg.command
                    )
                })?
        }
        Err(e) => return Err(e.into()),
    };
    let stdin = child
        .stdin
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let (tx, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let stderr = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(e) = child.stderr.take() {
        let buf = stderr.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(e).lines() {
                let Ok(line) = line else { break };
                let mut b = buf.lock().unwrap();
                if b.len() < 64_000 {
                    b.push_str(&line);
                    b.push('\n');
                }
            }
        });
    }
    Ok(CliProc {
        child,
        stdin,
        lines,
        stderr,
        approver,
        done,
    })
}

fn claude_cli(turn: &Turn, conv: &mut Conversation, out: &Out) -> anyhow::Result<()> {
    // Use the warm process if there is one; otherwise start (or resume) now.
    let mut proc = {
        let mut guard = conv.cli.lock().unwrap();
        match guard.take() {
            Some(mut p) => {
                if p.alive() {
                    p
                } else {
                    spawn_cli(&turn.cfg, conv.cli_session.as_deref())?
                }
            }
            None => spawn_cli(&turn.cfg, conv.cli_session.as_deref())?,
        }
    };
    *proc.approver.lock().unwrap() = Some(out.clone());

    let mut content = Vec::new();
    if let Some(img) = &turn.image {
        content.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64(&img.jpeg)}}));
    }
    content.push(json!({"type": "text", "text": question_text(turn)}));
    let line = json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    proc.stdin.write_all(line.as_bytes())?;
    proc.stdin.write_all(b"\n")?;
    proc.stdin.flush()?;

    let mut streamed_this_msg = false;
    let mut wrote_text = false;
    loop {
        if turn.cancel.load(Ordering::Relaxed) {
            return Ok(()); // dropping `proc` kills the CLI mid-answer
        }
        let line = match proc
            .lines
            .recv_timeout(std::time::Duration::from_millis(100))
        {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if wrote_text {
                    return Ok(());
                }
                let _ = proc.child.wait();
                anyhow::bail!("claude exited unexpectedly: {}", proc.stderr_tail());
            }
        };
        let Ok(ev) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(id) = ev["session_id"].as_str() {
            conv.cli_session = Some(id.to_string());
        }
        match ev["type"].as_str().unwrap_or("") {
            "stream_event" => {
                let e = &ev["event"];
                match e["type"].as_str().unwrap_or("") {
                    "message_start" => streamed_this_msg = false,
                    "content_block_start" if e["content_block"]["type"] == "tool_use" => {
                        let draw = e["content_block"]["name"]
                            .as_str()
                            .is_some_and(|n| n.ends_with("draw"));
                        out.status(if draw { "drawing…" } else { "thinking…" })
                    }
                    "content_block_start" if e["content_block"]["type"] == "thinking" => {
                        out.status("thinking…")
                    }
                    "content_block_start"
                        if e["content_block"]["type"] == "text"
                            && wrote_text
                            && !streamed_this_msg =>
                    {
                        out.delta("\n\n")
                    }
                    "content_block_delta" if e["delta"]["type"] == "text_delta" => {
                        streamed_this_msg = true;
                        wrote_text = true;
                        out.delta(e["delta"]["text"].as_str().unwrap_or(""));
                    }
                    _ => {}
                }
            }
            "assistant" => {
                for b in ev["message"]["content"].as_array().into_iter().flatten() {
                    match b["type"].as_str() {
                        Some("tool_use")
                            if b["name"].as_str().is_some_and(|n| n.ends_with("draw")) =>
                        {
                            let _ = out.draw(b["input"].clone());
                            out.status("thinking…");
                        }
                        // Partial streaming unavailable: fall back to the full message text.
                        Some("text") if !streamed_this_msg => {
                            if wrote_text {
                                out.delta("\n\n");
                            }
                            wrote_text = true;
                            out.delta(b["text"].as_str().unwrap_or(""));
                        }
                        _ => {}
                    }
                }
                streamed_this_msg = false;
            }
            "result" => {
                *proc.approver.lock().unwrap() = None;
                let err = (ev["is_error"].as_bool() == Some(true)).then(|| {
                    ev["result"]
                        .as_str()
                        .unwrap_or("claude reported an error")
                        .to_string()
                });
                // Keep the process warm for the next follow-up.
                *conv.cli.lock().unwrap() = Some(proc);
                return match err {
                    Some(e) => Err(anyhow::anyhow!("{e}")),
                    None => Ok(()),
                };
            }
            _ => {}
        }
    }
}

/// Answers approval requests from `screenbuddy --mcp`: one JSON line
/// {"intent","detail"} in, "allow" or "deny" out. Denies when no turn is active.
fn spawn_approval_server(
    listener: std::net::TcpListener,
    approver: Arc<std::sync::Mutex<Option<Out>>>,
    done: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let mut line = String::new();
                    let _ = BufReader::new(&stream).read_line(&mut line);
                    let req: Value = serde_json::from_str(&line).unwrap_or_default();
                    let out = approver.lock().unwrap().clone();
                    let ok = out.is_some_and(|o| {
                        o.approve(
                            req["intent"].as_str().unwrap_or("perform an action"),
                            req["detail"].as_str().unwrap_or(""),
                        )
                    });
                    let _ = stream.write_all(if ok { b"allow\n" } else { b"deny\n" });
                }
                Err(_) => std::thread::sleep(std::time::Duration::from_millis(40)),
            }
        }
    });
}
