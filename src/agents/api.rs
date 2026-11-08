//! Agent tool loop for the HTTP providers (Anthropic Messages, OpenAI-compatible
//! chat). Simpler than the CLI: no streaming, tools are report_plan,
//! report_step, ask_user, desktop, write_file / read_file / list_files inside
//! the agent's folder, plus server-side web search on Anthropic. The history
//! lives in `.conversation.json` in the agent folder so follow-ups continue it.

use super::{Sink, TurnJob};
use base64::Engine;
use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

const MAX_ROUNDS: usize = 40;
const HISTORY: &str = ".conversation.json";

fn tools(desktop: bool) -> Vec<(&'static str, &'static str, Value)> {
    let mut v = vec![
        ("report_plan", super::PLAN_DESC, super::plan_schema()),
        ("report_step", super::STEP_DESC, super::step_schema()),
        ("ask_user", super::ASK_DESC, super::ask_schema()),
        (
            "write_file",
            "Write a text file in your working folder (relative path). Overwrites.",
            json!({"type": "object", "properties": {"path": {"type": "string"}, "content": {"type": "string"}}, "required": ["path", "content"]}),
        ),
        (
            "read_file",
            "Read a text file from your working folder (relative path).",
            json!({"type": "object", "properties": {"path": {"type": "string"}}, "required": ["path"]}),
        ),
        (
            "list_files",
            "List the files in your working folder.",
            json!({"type": "object", "properties": {}}),
        ),
    ];
    if desktop {
        v.push(("desktop", crate::desktop::DESC, crate::desktop::schema()));
    }
    v.push(("window_screenshot", super::desk::SHOT_DESC, super::desk::shot_schema()));
    if super::drive::enabled(&crate::config::load().0) {
        v.push(("drive", super::drive::DESC, super::drive::schema()));
    }
    v
}

/// A relative path that stays inside `cwd`.
fn inside(cwd: &Path, rel: &str) -> Result<PathBuf, String> {
    let p = Path::new(rel.trim());
    if rel.trim().is_empty()
        || p.is_absolute()
        || p.components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(format!(
            "error: '{rel}' must be a relative path inside your working folder"
        ));
    }
    Ok(cwd.join(p))
}

/// Runs one tool call; returns (text, is_error).
fn call(name: &str, input: &Value, job: &TurnJob, sink: &Sink) -> (String, bool) {
    match name {
        "report_plan" => {
            sink.plan(
                input["steps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str())
                    .map(str::to_string)
                    .collect(),
            );
            ("ok".into(), false)
        }
        "report_step" => {
            sink.step(input["text"].as_str().unwrap_or(""));
            ("ok".into(), false)
        }
        "ask_user" => (
            sink.questions(
                input["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|s| s.as_str())
                    .map(str::to_string)
                    .collect(),
            ),
            false,
        ),
        "write_file" => match inside(&job.cwd, input["path"].as_str().unwrap_or("")) {
            Ok(p) => {
                if let Some(d) = p.parent() {
                    let _ = std::fs::create_dir_all(d);
                }
                sink.step(&format!(
                    "Writing {}",
                    p.file_name()
                        .map(|f| f.to_string_lossy().to_string())
                        .unwrap_or_default()
                ));
                match std::fs::write(&p, input["content"].as_str().unwrap_or("")) {
                    Ok(()) => (format!("Wrote {}", p.display()), false),
                    Err(e) => (format!("error: {e}"), true),
                }
            }
            Err(e) => (e, true),
        },
        "read_file" => match inside(&job.cwd, input["path"].as_str().unwrap_or("")) {
            Ok(p) => match std::fs::read_to_string(&p) {
                Ok(t) => (t.chars().take(60_000).collect(), false),
                Err(e) => (format!("error: {e}"), true),
            },
            Err(e) => (e, true),
        },
        "list_files" => {
            let mut names: Vec<String> = std::fs::read_dir(&job.cwd)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| e.file_name().to_string_lossy().to_string())
                        .filter(|n| !n.starts_with('.') && !n.starts_with("agent.json"))
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            (
                if names.is_empty() {
                    "(empty)".into()
                } else {
                    names.join("\n")
                },
                false,
            )
        }
        "desktop" => {
            let Some(exe) = job.cfg.desktop_exe() else {
                return ("error: agent-desktop is not installed".into(), true);
            };
            let (args, intent) = match crate::desktop::parse(input) {
                Ok(v) => v,
                Err(e) => return (format!("error: {e}"), true),
            };
            if crate::desktop::needs_approval(&args, &intent) && !sink.approve(&intent, &args.join(" ")) {
                return (
                    "The user declined this action. Don't retry it; find another way or stop and say so.".into(),
                    true,
                );
            }
            if !crate::desktop::is_read_only(&args) {
                sink.step(&intent);
            }
            let ctx = crate::desktop::Ctx::from_config(&job.cfg, format!("screenbuddy-agent-{}", job.id));
            let t0 = super::desk::now_ms();
            let out = crate::desktop::exec(&exe, &args, &intent, &ctx);
            sink.desk_action(&args, &out, t0);
            let ok = serde_json::from_str::<Value>(&out)
                .ok()
                .and_then(|v| v["ok"].as_bool())
                .unwrap_or(true);
            (out, !ok)
        }
        "drive" => super::drive::call(sink, input),
        "window_screenshot" => match shot(input, sink) {
            Ok((_, text)) => (format!("{text} (this provider can't see images; use the accessibility tree)"), false),
            Err(e) => (e, true),
        },
        _ => ("error: unknown tool".into(), true),
    }
}

fn shot(input: &Value, sink: &Sink) -> Result<(Vec<u8>, String), String> {
    super::desk::shot(input["hwnd"].as_i64().map(|h| h as isize), &sink.desk_windows())
}

fn client() -> Result<reqwest::blocking::Client, String> {
    reqwest::blocking::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .build()
        .map_err(|e| e.to_string())
}

fn load_history(cwd: &Path, first: bool) -> Vec<Value> {
    if first {
        return Vec::new();
    }
    std::fs::read_to_string(cwd.join(HISTORY))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn save_history(cwd: &Path, msgs: &[Value]) {
    let _ = std::fs::write(cwd.join(HISTORY), serde_json::to_string(msgs).unwrap_or_default());
}

/// Sends a request, polling `sink.stopped()` so stop works mid-request.
fn send(sink: &Sink, req: reqwest::blocking::RequestBuilder) -> Result<Value, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let r = req.send().map_err(|e| e.to_string()).and_then(|resp| {
            let status = resp.status();
            let body = resp.text().map_err(|e| e.to_string())?;
            if !status.is_success() {
                let msg = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|v| v["error"]["message"].as_str().map(str::to_string))
                    .unwrap_or(body);
                return Err(format!("{status}: {}", msg.chars().take(300).collect::<String>()));
            }
            serde_json::from_str::<Value>(&body).map_err(|e| e.to_string())
        });
        let _ = tx.send(r);
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(r) => return r,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if sink.stopped() => return Err("stopped".into()),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(_) => return Err("request thread died".into()),
        }
    }
}

pub(crate) fn run(job: &TurnJob, sink: &Sink) -> Result<String, String> {
    match job.cfg.provider.as_str() {
        "openai" => openai(job, sink),
        _ => anthropic(job, sink),
    }
}

fn anthropic(job: &TurnJob, sink: &Sink) -> Result<String, String> {
    let c = &job.cfg.anthropic;
    let key = if c.api_key.trim().is_empty() {
        std::env::var("ANTHROPIC_API_KEY").unwrap_or_default()
    } else {
        c.api_key.trim().to_string()
    };
    if key.is_empty() {
        return Err(
            "No Anthropic API key. Set anthropic.api_key in config.toml or the ANTHROPIC_API_KEY env var.".into(),
        );
    }
    let desktop = job.cfg.desktop_exe().is_some();
    let mut tool_defs: Vec<Value> = tools(desktop)
        .into_iter()
        .map(|(n, d, s)| json!({"name": n, "description": d, "input_schema": s}))
        .collect();
    tool_defs.push(json!({"type": "web_search_20250305", "name": "web_search", "max_uses": 8}));
    let mut msgs = load_history(&job.cwd, job.first);
    let mut content = Vec::new();
    if let Some(img) = &job.image {
        content.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(img.as_slice())}}));
    }
    content.push(json!({"type": "text", "text": job.prompt}));
    msgs.push(json!({"role": "user", "content": content}));
    let http = client()?;
    let url = format!("{}/v1/messages", c.base_url.trim_end_matches('/'));
    let system = super::agent_prompt(&job.cfg, desktop);
    for _ in 0..MAX_ROUNDS {
        let body = json!({"model": c.model, "max_tokens": c.max_tokens.min(16000), "system": system, "messages": msgs, "tools": tool_defs});
        let req = http
            .post(&url)
            .header("x-api-key", &key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .body(body.to_string());
        let resp = send(sink, req)?;
        let blocks = resp["content"].as_array().cloned().unwrap_or_default();
        msgs.push(json!({"role": "assistant", "content": blocks}));
        let mut results = Vec::new();
        for b in &blocks {
            if b["type"] == "tool_use" && b["name"] == "window_screenshot" {
                let content = match shot(&b["input"], sink) {
                    Ok((jpeg, text)) => json!([
                        {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": base64::engine::general_purpose::STANDARD.encode(jpeg)}},
                        {"type": "text", "text": text}
                    ]),
                    Err(e) => json!(e),
                };
                results.push(json!({"type": "tool_result", "tool_use_id": b["id"], "content": content}));
            } else if b["type"] == "tool_use" {
                let (text, is_error) = call(b["name"].as_str().unwrap_or(""), &b["input"], job, sink);
                if sink.stopped() {
                    return Err("stopped".into());
                }
                results.push(
                    json!({"type": "tool_result", "tool_use_id": b["id"], "content": text, "is_error": is_error}),
                );
            } else if b["type"] == "server_tool_use" {
                if let Some(q) = b["input"]["query"].as_str() {
                    sink.step(&format!("Searching: {q}"));
                }
            }
        }
        match resp["stop_reason"].as_str().unwrap_or("") {
            "tool_use" if !results.is_empty() => msgs.push(json!({"role": "user", "content": results})),
            "pause_turn" => {}
            _ => {
                save_history(&job.cwd, &msgs);
                let text: Vec<&str> = blocks
                    .iter()
                    .filter(|b| b["type"] == "text")
                    .filter_map(|b| b["text"].as_str())
                    .collect();
                return Ok(text.join("\n\n"));
            }
        }
        save_history(&job.cwd, &msgs);
    }
    Err("the agent used too many steps without finishing".into())
}

fn openai(job: &TurnJob, sink: &Sink) -> Result<String, String> {
    let c = &job.cfg.openai;
    let key = if c.api_key.trim().is_empty() {
        std::env::var("OPENAI_API_KEY").unwrap_or_default()
    } else {
        c.api_key.trim().to_string()
    };
    let desktop = job.cfg.desktop_exe().is_some();
    let tool_defs: Vec<Value> = tools(desktop)
        .into_iter()
        .map(|(n, d, s)| json!({"type": "function", "function": {"name": n, "description": d, "parameters": s}}))
        .collect();
    let mut msgs = load_history(&job.cwd, job.first);
    if msgs.is_empty() {
        msgs.push(json!({"role": "system", "content": super::agent_prompt(&job.cfg, desktop)}));
    }
    let mut content = vec![json!({"type": "text", "text": job.prompt})];
    if let Some(img) = &job.image {
        content.push(json!({"type": "image_url", "image_url": {"url": format!("data:image/jpeg;base64,{}", base64::engine::general_purpose::STANDARD.encode(img.as_slice()))}}));
    }
    msgs.push(json!({"role": "user", "content": content}));
    let http = client()?;
    let url = format!("{}/chat/completions", c.base_url.trim_end_matches('/'));
    for _ in 0..MAX_ROUNDS {
        let mut body = json!({"model": c.model, "messages": msgs, "max_tokens": c.max_tokens});
        if c.tools {
            body["tools"] = json!(tool_defs);
        }
        let mut req = http
            .post(&url)
            .header("content-type", "application/json")
            .body(body.to_string());
        if !key.is_empty() {
            req = req.bearer_auth(&key);
        }
        let resp = send(sink, req)?;
        let msg = resp["choices"][0]["message"].clone();
        let calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();
        let mut said = json!({"role": "assistant", "content": msg["content"]});
        if !calls.is_empty() {
            said["tool_calls"] = json!(calls);
        }
        msgs.push(said);
        if calls.is_empty() {
            save_history(&job.cwd, &msgs);
            return Ok(msg["content"].as_str().unwrap_or("").to_string());
        }
        for tc in &calls {
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            let input: Value =
                serde_json::from_str(if args.trim().is_empty() { "{}" } else { args }).unwrap_or_default();
            let (text, _) = call(tc["function"]["name"].as_str().unwrap_or(""), &input, job, sink);
            if sink.stopped() {
                return Err("stopped".into());
            }
            msgs.push(json!({"role": "tool", "tool_call_id": tc["id"], "content": text}));
        }
        save_history(&job.cwd, &msgs);
    }
    Err("the agent used too many steps without finishing".into())
}
