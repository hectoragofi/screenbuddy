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

pub const SYSTEM_PROMPT: &str = "You are screenbuddy, a desktop assistant that can see the user's screen. \
The user pressed a hotkey and attached a screenshot of what they are looking at. \
Answer the question directly and concisely in Markdown; skip preamble. \
When pointing at something on screen helps (where to click, which element, what is wrong), call the `draw` tool \
to annotate the user's screen. Coordinates are screenshot pixels with the origin at the top-left; the screenshot size \
is given with the image. Use a few precise annotations with short labels (1-4 words). Arrows point from the label \
toward the target, with x2,y2 on the target. Do not mention coordinates in your text.";

pub const DRAW_DESC: &str = "Draw annotations on the user's screen on top of the area shown in the screenshot. \
Coordinates are in screenshot pixels, origin top-left. Kinds: arrow (x,y tail -> x2,y2 head on the target), \
box (x,y,w,h outline), highlight (x,y,w,h translucent fill), circle (x,y center, r radius), text (label at x,y). \
Annotations stay until cleared.";

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
    if cfg.extra_instructions.trim().is_empty() {
        SYSTEM_PROMPT.to_string()
    } else {
        format!("{SYSTEM_PROMPT}\n\n{}", cfg.extra_instructions.trim())
    }
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
            "tools": [{
                "name": "draw",
                "description": DRAW_DESC,
                "input_schema": draw_schema(),
                "eager_input_streaming": true
            }],
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
                        Some("tool_use") => out.status("drawing…"),
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
                                    if b["name"] == "draw" {
                                        match out.draw(input) {
                                            Ok(t) => (t, false),
                                            Err(e) => (e, true),
                                        }
                                    } else {
                                        ("Unknown tool".to_string(), true)
                                    }
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
            body["tools"] = json!([{"type": "function", "function": {"name": "draw", "description": DRAW_DESC, "parameters": draw_schema()}}]);
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
            let result = if name == "draw" {
                match serde_json::from_str::<Value>(if args.trim().is_empty() {
                    "{}"
                } else {
                    args
                }) {
                    Ok(input) => out.draw(input).unwrap_or_else(|e| format!("error: {e}")),
                    Err(e) => format!("error: arguments were not valid JSON ({e})"),
                }
            } else {
                "error: unknown tool".to_string()
            };
            conv.openai
                .push(json!({"role": "tool", "tool_call_id": id, "content": result}));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- Claude Code CLI

fn claude_cli(turn: &Turn, conv: &mut Conversation, out: &Out) -> anyhow::Result<()> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let cfg = &turn.cfg.claude_cli;
    let exe = std::env::current_exe()?;
    let mcp = json!({"mcpServers": {"screenbuddy": {"type": "stdio", "command": exe.to_string_lossy(), "args": ["--mcp"]}}}).to_string();
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
        .args(["--system-prompt", &system_prompt(&turn.cfg)])
        .args(["--tools", ""])
        .args(["--strict-mcp-config", "--mcp-config", &mcp])
        .args(["--allowedTools", "mcp__screenbuddy__draw"]);
        if !cfg.model.trim().is_empty() {
            c.args(["--model", cfg.model.trim()]);
        }
        if let Some(id) = &conv.cli_session {
            c.args(["--resume", id]);
        }
        c.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        c
    };
    out.status("starting claude…");
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

    let mut content = Vec::new();
    if let Some(img) = &turn.image {
        content.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": b64(&img.jpeg)}}));
    }
    content.push(json!({"type": "text", "text": question_text(turn)}));
    let line = json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    {
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow::anyhow!("no stdin"))?;
        stdin.write_all(line.as_bytes())?;
        stdin.write_all(b"\n")?;
    } // dropping stdin closes it → CLI answers this one message then exits

    let stderr = child.stderr.take();
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        if let Some(mut e) = stderr {
            let _ = std::io::Read::read_to_string(&mut e, &mut s);
        }
        s
    });

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout"))?;
    let mut streamed_this_msg = false;
    let mut wrote_text = false;
    let mut result_err = None;
    for line in BufReader::new(stdout).lines() {
        if turn.cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            return Ok(());
        }
        let Ok(line) = line else { break };
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
                        out.status("drawing…")
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
                if ev["is_error"].as_bool() == Some(true) {
                    result_err = Some(
                        ev["result"]
                            .as_str()
                            .unwrap_or("claude reported an error")
                            .to_string(),
                    );
                }
            }
            _ => {}
        }
    }
    let status = child.wait()?;
    let stderr = err_reader.join().unwrap_or_default();
    if let Some(e) = result_err {
        anyhow::bail!("{e}");
    }
    if !status.success() && !wrote_text {
        let tail: String = stderr
            .lines()
            .rev()
            .take(6)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n");
        anyhow::bail!("claude exited with {status}: {tail}");
    }
    Ok(())
}
