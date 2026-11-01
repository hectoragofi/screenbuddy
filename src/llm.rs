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
use std::time::{Duration, Instant};

/// How long a stream (HTTP or the Claude CLI) may stay silent, while nothing is
/// waiting on the user, before the answer is treated as dead.
const STALL: Duration = Duration::from_secs(90);

/// Prefix of errors where the answer was cut off by a dead or stalled stream;
/// the palette offers a retry for these.
pub const INTERRUPTED: &str = "Interrupted: ";

pub const SYSTEM_PROMPT: &str = "You are screenbuddy, a calm, precise assistant built into the user's desktop. With each question you get a screenshot of what the user is looking at. Answer like a thoughtful expert sitting next to them: lead with the answer, keep it short (one to three sentences unless they ask for more), and use Markdown only when it genuinely helps, such as numbered steps or code. Write math in LaTeX: $...$ inline and $$...$$ on its own line for displayed equations; never put math in code blocks. No preamble, no restating the question, no sign-offs.";

pub const DRAW_PROMPT: &str = "You can also point at things on their screen with the `draw` tool, but most questions do not need it. Do not draw for explanations, summaries, translations, opinions, code or writing help, or anything you can answer clearly in words. Draw only when the user wants to find something in the interface (where is X, what do I click, which one, how do I do this here) and pointing will save them from searching. When you do draw, mark exactly one target (at most three for multi-step instructions), prefer a box or circle around it with a 1-3 word label, and keep your text short since the drawing does the work. Labels and text shapes render LaTeX in $...$, so for a math problem on screen you may write the key formula or result right next to it with a text shape. Never mention coordinates or the tool itself.";

pub const DRAW_DESC: &str = "Point at something on the user's screen. Use only when the user needs to locate a UI element and words alone would leave them searching; never for decoration or emphasis. Coordinates are screenshot pixels, origin top-left. Kinds: box (x,y,w,h outline around the target; the default choice), circle (x,y center, r), highlight (x,y,w,h translucent fill for a text span or region), arrow (x,y tail -> x2,y2 head on a small target), text (a label at x,y). One shape is usually right.";

fn anthropic_tools(draw: bool, desktop: bool) -> Value {
    let mut tools = Vec::new();
    if draw {
        tools.push(json!({
            "name": "draw",
            "description": DRAW_DESC,
            "input_schema": draw_schema(),
            "eager_input_streaming": true
        }));
    }
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
                        "label": {"type": "string", "description": "short label, 1-4 words or a short formula; LaTeX in $...$ is rendered"},
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
    /// agent-desktop session id for this conversation.
    pub desktop_session: String,
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
    dctx: Option<crate::desktop::Ctx>,
    watch: Watch,
}

/// Stall detector shared by a turn's worker and its helper threads: the time
/// of the last sign of life, and how many waits on the user are in progress
/// (those never count as a stall).
#[derive(Clone)]
struct Watch(Arc<std::sync::Mutex<(Instant, usize)>>);

impl Default for Watch {
    fn default() -> Self {
        Self(Arc::new(std::sync::Mutex::new((Instant::now(), 0))))
    }
}

impl Watch {
    fn touch(&self) {
        self.0.lock().unwrap().0 = Instant::now();
    }
    /// Pauses stall detection until the guard drops.
    fn hold(&self) -> Hold {
        self.0.lock().unwrap().1 += 1;
        Hold(self.clone())
    }
    fn stalled(&self, limit: Duration) -> bool {
        let w = self.0.lock().unwrap();
        w.1 == 0 && w.0.elapsed() >= limit
    }
}

struct Hold(Watch);

impl Drop for Hold {
    fn drop(&mut self) {
        let mut w = self.0.0.lock().unwrap();
        w.1 -= 1;
        w.0 = Instant::now();
    }
}

fn stall_error(limit: Duration) -> anyhow::Error {
    anyhow::anyhow!(
        "{INTERRUPTED}no response for {} s, the stream stalled.",
        limit.as_secs()
    )
}

impl Out {
    pub fn new(tx: Sender<Ev>, ctx: egui::Context) -> Self {
        Self {
            tx,
            ctx,
            dctx: None,
            watch: Watch::default(),
        }
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
        let _hold = self.watch.hold();
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
        let dctx = self.dctx.clone().unwrap_or_else(|| crate::desktop::Ctx { session: crate::desktop::new_session_id(), cursor: None });
        let out = crate::desktop::exec(exe, &args, &intent, &dctx);
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
        let _hold = self.watch.hold();
        self.send(Ev::Draw(input, tx));
        rx.recv_timeout(std::time::Duration::from_secs(10))
            .unwrap_or_else(|_| Err("overlay unavailable".into()))
    }
}

pub fn spawn(turn: Turn, out: Out) {
    std::thread::spawn(move || {
        let mut conv = turn.conv.clone();
        if conv.desktop_session.is_empty() {
            conv.desktop_session = crate::desktop::new_session_id();
        }
        let mut out = out;
        out.dctx = Some(crate::desktop::Ctx::from_config(&turn.cfg, conv.desktop_session.clone()));
        let out = out;
        let res = match turn.cfg.provider.as_str() {
            "openai" => openai(&turn, &mut conv, &out),
            "claude-cli" => claude_cli(&turn, &mut conv, &out),
            _ => anthropic(&turn, &mut conv, &out),
        };
        let cancelled = turn.cancel.load(Ordering::Relaxed);
        if let Err(e) = &res
            && !cancelled
        {
            crate::config::log(&format!("provider error: {e:#}"));
            out.send(Ev::Error(format!("{e:#}")));
        }
        if res.is_err() || cancelled {
            rollback(&turn, &mut conv);
        }
        out.send(Ev::Done(conv));
    });
}

/// Undoes a failed or cancelled turn so asking again starts from a clean
/// history: the question, partial answer and any tool rounds are dropped,
/// and the screenshot is resent if it never reached the model.
fn rollback(turn: &Turn, conv: &mut Conversation) {
    conv.anthropic.truncate(turn.conv.anthropic.len());
    conv.openai.truncate(turn.conv.openai.len());
    if turn.image.is_some() && conv.cli_session.is_none() {
        conv.image_sent = false;
    }
    let cancelled = turn.cancel.load(Ordering::Relaxed);
    if !cancelled && turn.cfg.provider == "claude-cli" && conv.cli.lock().unwrap().is_none() {
        // The CLI died or stalled: start a replacement now so a retry is fast.
        prewarm(&turn.cfg, conv);
    }
}

fn http() -> anyhow::Result<reqwest::blocking::Client> {
    Ok(reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        // Per read/write, not total: a backstop behind the stall watchdog in
        // `sse_lines` so a dead connection can't park its reader thread forever.
        .timeout(STALL + Duration::from_secs(30))
        .build()?)
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn system_prompt(cfg: &Config) -> String {
    let mut p = SYSTEM_PROMPT.to_string();
    if cfg.drawing.enabled {
        p.push_str("\n\n");
        p.push_str(DRAW_PROMPT);
    }
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

/// Iterates SSE `data:` payloads until `f` returns false (Ok(true)), the
/// stream ends (Ok(false)), or it is cancelled (Ok(false)). The body is read on
/// a helper thread so a connection that goes silent for `stall` (while no
/// tool call is waiting on the user) fails the turn instead of hanging it.
fn sse_lines(
    body: impl std::io::Read + Send + 'static,
    cancel: &AtomicBool,
    watch: &Watch,
    stall: Duration,
    mut f: impl FnMut(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<bool> {
    let (tx, rx) = std::sync::mpsc::sync_channel::<std::io::Result<String>>(256);
    std::thread::spawn(move || {
        for line in BufReader::new(body).lines() {
            let failed = line.is_err();
            if tx.send(line).is_err() || failed {
                break;
            }
        }
    });
    watch.touch();
    loop {
        if cancel.load(Ordering::Relaxed) {
            return Ok(false);
        }
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(line) => {
                let line = line.map_err(|e| anyhow::anyhow!("{INTERRUPTED}{e}"))?;
                watch.touch();
                if let Some(data) = line.strip_prefix("data:")
                    && !f(data.trim_start())?
                {
                    return Ok(true);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if watch.stalled(stall) {
                    return Err(stall_error(stall));
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Ok(false),
        }
    }
}

/// Error for a stream that closed before its final event.
fn cut_off() -> anyhow::Error {
    anyhow::anyhow!("{INTERRUPTED}the connection closed before the answer finished.")
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
            "tools": anthropic_tools(turn.cfg.drawing.enabled, exe.is_some()),
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

        let ended = sse_lines(resp, &turn.cancel, &out.watch, STALL, |data| {
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
        if !ended {
            return Err(cut_off());
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
            let mut tools = Vec::new();
            if turn.cfg.drawing.enabled {
                tools.push(json!({"type": "function", "function": {"name": "draw", "description": DRAW_DESC, "parameters": draw_schema()}}));
            }
            if exe.is_some() {
                tools.push(json!({"type": "function", "function": {"name": "desktop", "description": crate::desktop::DESC, "parameters": crate::desktop::schema()}}));
            }
            if !tools.is_empty() {
                body["tools"] = json!(tools);
            }
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
        let ended = sse_lines(resp, &turn.cancel, &out.watch, STALL, |data| {
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
        if !ended && finish.is_empty() {
            return Err(cut_off());
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
    _job: Option<Job>,
}

/// A kill-on-close job object holding the CLI and everything it starts (Node,
/// the `--mcp` server), so dropping a `CliProc` never leaves orphans behind.
struct Job(windows::Win32::Foundation::HANDLE);

// The handle is only used to close the job.
unsafe impl Send for Job {}

impl Job {
    fn wrap(child: &std::process::Child) -> Option<Job> {
        use std::os::windows::io::AsRawHandle;
        use windows::Win32::System::JobObjects::*;
        unsafe {
            let job = Job(CreateJobObjectW(None, windows::core::PCWSTR::null()).ok()?);
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                &info as *const _ as *const core::ffi::c_void,
                std::mem::size_of_val(&info) as u32,
            )
            .ok()?;
            AssignProcessToJobObject(job.0, windows::Win32::Foundation::HANDLE(child.as_raw_handle()))
                .ok()?;
            Some(job)
        }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(self.0);
        }
    }
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
        // Fast path: a fresh spare kept warm since the last palette closed,
        // or the one `warm_on_hotkey` is starting right now.
        let pending = WARMING.lock().unwrap().take();
        if let Some(h) = pending {
            let _ = h.join();
        }
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

/// The spawn started by `warm_on_hotkey`, joined before the spare is taken.
static WARMING: std::sync::Mutex<Option<std::thread::JoinHandle<()>>> =
    std::sync::Mutex::new(None);

/// Called the moment the hotkey fires: starts the CLI in parallel with the
/// screen capture and window creation, so Node has a head start before the
/// palette even exists. The process waits as the spare; the palette takes it.
/// Pair with `refill_spare` when the palette closes (or never opens): with
/// `keep_warm_minutes = 0` that kills it again, otherwise it expires on the
/// usual timer, so an unused early start never outlives the setting.
pub fn warm_on_hotkey(cfg: &Config) {
    if cfg.provider != "claude-cli" {
        return;
    }
    let cfg = cfg.clone();
    let h = std::thread::spawn(move || {
        let sig = spare_sig(&cfg);
        let mut slot = SPARE.lock().unwrap();
        if slot.as_mut().is_some_and(|s| s.sig == sig && s.proc.alive()) {
            return;
        }
        *slot = None;
        match spawn_cli(&cfg, None) {
            Ok(proc) => {
                *slot = Some(Spare {
                    proc,
                    born: std::time::Instant::now(),
                    sig,
                })
            }
            Err(e) => crate::config::log(&format!("claude early start failed: {e:#}")),
        }
    });
    *WARMING.lock().unwrap() = Some(h);
}

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
    let pending = WARMING.lock().unwrap().take();
    if let Some(h) = pending {
        let _ = h.join(); // an early start must land before we decide its fate
    }
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
            "SCREENBUDDY_DESKTOP_SESSION".into(),
            json!(crate::desktop::new_session_id()),
        );
        env.insert(
            "SCREENBUDDY_APPROVAL_PORT".into(),
            json!(listener.local_addr()?.port().to_string()),
        );
        spawn_approval_server(listener, approver.clone(), done.clone());
    }
    let mcp = json!({"mcpServers": {"screenbuddy": {"type": "stdio", "command": exe.to_string_lossy(), "args": ["--mcp"], "env": env}}}).to_string();
    let mut allowed_list = Vec::new();
    if full_cfg.drawing.enabled {
        allowed_list.push("mcp__screenbuddy__draw");
    }
    if desktop {
        allowed_list.push("mcp__screenbuddy__desktop");
    }
    let allowed = if allowed_list.is_empty() { "none".to_string() } else { allowed_list.join(" ") };
    let allowed = allowed.as_str();
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
    let job = Job::wrap(&child);
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
        _job: job,
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
    let sent = proc
        .stdin
        .write_all(format!("{line}\n").as_bytes())
        .and_then(|_| proc.stdin.flush());
    if sent.is_err() {
        return Err(cli_died(&mut proc)); // the CLI exited before reading the question
    }

    let mut streamed_this_msg = false;
    let mut wrote_text = false;
    out.watch.touch();
    loop {
        if turn.cancel.load(Ordering::Relaxed) {
            return Ok(()); // dropping `proc` kills the CLI mid-answer
        }
        let line = match proc.lines.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // A hung CLI (or a hung `--mcp` tool call inside it) goes quiet;
                // dropping `proc` kills the whole process tree.
                if out.watch.stalled(STALL) {
                    return Err(stall_error(STALL));
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                // stdout closed without a `result`: the CLI died mid-answer.
                return Err(cli_died(&mut proc));
            }
        };
        out.watch.touch();
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

fn cli_died(proc: &mut CliProc) -> anyhow::Error {
    let _ = proc.child.wait();
    // Give the stderr reader a moment to collect the last lines.
    std::thread::sleep(Duration::from_millis(50));
    let tail = proc.stderr_tail();
    anyhow::anyhow!(
        "{INTERRUPTED}claude exited before finishing{}{tail}",
        if tail.trim().is_empty() { "." } else { ": " }
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Read};

    /// A body that sends `head`, then either ends, fails, or goes silent.
    struct Body {
        head: Cursor<Vec<u8>>,
        tail: Tail,
    }
    #[derive(Clone, Copy)]
    enum Tail {
        Eof,
        Fail,
        Hang,
    }
    impl Read for Body {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.head.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            match self.tail {
                Tail::Eof => Ok(0),
                Tail::Fail => Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "reset")),
                Tail::Hang => loop {
                    std::thread::sleep(Duration::from_secs(3600));
                },
            }
        }
    }
    fn body(s: &str, tail: Tail) -> Body {
        Body {
            head: Cursor::new(s.as_bytes().to_vec()),
            tail,
        }
    }
    fn run(b: Body, stall: Duration) -> (anyhow::Result<bool>, Vec<String>) {
        let mut seen = Vec::new();
        let r = sse_lines(b, &AtomicBool::new(false), &Watch::default(), stall, |d| {
            seen.push(d.to_string());
            Ok(d != "stop")
        });
        (r, seen)
    }

    #[test]
    fn sse_completes_on_final_event() {
        let (r, seen) = run(body("data: a\n\ndata: stop\ndata: never\n", Tail::Hang), Duration::from_secs(5));
        assert!(r.unwrap());
        assert_eq!(seen, ["a", "stop"]);
    }

    #[test]
    fn sse_reports_early_close() {
        let (r, seen) = run(body("data: a\n", Tail::Eof), Duration::from_secs(5));
        assert!(!r.unwrap(), "closed without the final event");
        assert_eq!(seen, ["a"]);
    }

    #[test]
    fn sse_dropped_connection_is_interrupted() {
        let (r, _) = run(body("data: a\n", Tail::Fail), Duration::from_secs(5));
        assert!(r.unwrap_err().to_string().starts_with(INTERRUPTED));
    }

    #[test]
    fn sse_stall_times_out() {
        let t = Instant::now();
        let (r, seen) = run(body("data: a\n", Tail::Hang), Duration::from_millis(300));
        assert!(r.unwrap_err().to_string().starts_with(INTERRUPTED));
        assert_eq!(seen, ["a"]);
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn sse_cancel_stops_a_silent_stream() {
        let cancel = Arc::new(AtomicBool::new(false));
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(200));
            c2.store(true, Ordering::Relaxed);
        });
        let r = sse_lines(body("", Tail::Hang), &cancel, &Watch::default(), Duration::from_secs(60), |_| Ok(true));
        assert!(!r.unwrap());
    }

    #[test]
    fn waiting_on_the_user_is_not_a_stall() {
        let w = Watch::default();
        let limit = Duration::from_millis(100);
        let hold = w.hold();
        std::thread::sleep(limit * 2);
        assert!(!w.stalled(limit));
        drop(hold); // the wait ending counts as activity
        assert!(!w.stalled(limit));
        std::thread::sleep(limit * 2);
        assert!(w.stalled(limit));
    }

    #[test]
    fn cli_that_dies_is_an_error_not_a_hang() {
        // The test binary itself rejects claude's flags and exits at once,
        // like a CLI that crashes before answering.
        let mut cfg = Config::default();
        cfg.provider = "claude-cli".into();
        cfg.drawing.enabled = false;
        cfg.claude_cli.command = std::env::current_exe().unwrap().to_string_lossy().into();
        let (tx, _rx) = std::sync::mpsc::channel();
        let out = Out::new(tx, egui::Context::default());
        let turn = Turn {
            cfg,
            conv: Default::default(),
            question: "hi".into(),
            image: None,
            cancel: Default::default(),
        };
        let mut conv = Conversation::default();
        let t = Instant::now();
        let e = claude_cli(&turn, &mut conv, &out).unwrap_err();
        assert!(e.to_string().starts_with(INTERRUPTED), "{e}");
        assert!(t.elapsed() < Duration::from_secs(30));
    }

    #[test]
    fn failed_turn_rolls_back_history() {
        let mut cfg = Config::default();
        cfg.provider = "anthropic".into();
        let turn = Turn {
            cfg,
            conv: Conversation {
                image_sent: true,
                ..Default::default()
            },
            question: "q".into(),
            image: Some(Image {
                jpeg: Arc::new(vec![]),
                w: 1,
                h: 1,
            }),
            cancel: Default::default(),
        };
        let mut conv = turn.conv.clone();
        conv.anthropic.push(json!({"role": "user", "content": "q"}));
        conv.anthropic.push(json!({"role": "assistant", "content": "partial"}));
        rollback(&turn, &mut conv);
        assert!(conv.anthropic.is_empty());
        assert!(!conv.image_sent, "the screenshot must be resent on retry");
    }
}
