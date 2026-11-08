//! Minimal MCP stdio server (`screenbuddy --mcp`) exposing the `draw` tool to
//! the Claude Code CLI. The parent screenbuddy process reads the tool call from
//! the CLI's stream-json output and renders it; this server only validates.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub fn run() {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = req.get("id").cloned() else {
            continue;
        }; // notification
        let reply = match handle(&req) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err((code, msg)) => {
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": msg}})
            }
        };
        if writeln!(stdout, "{reply}")
            .and_then(|_| stdout.flush())
            .is_err()
        {
            break;
        }
    }
}

fn handle(req: &Value) -> Result<Value, (i64, String)> {
    match req["method"].as_str().unwrap_or("") {
        "initialize" => Ok(json!({
            "protocolVersion": req["params"]["protocolVersion"].as_str().unwrap_or("2025-06-18"),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "screenbuddy", "version": env!("CARGO_PKG_VERSION")}
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            if crate::agents::is_agent_mcp() {
                return Ok(json!({"tools": crate::agents::mcp_tools(desktop_exe().is_some())}));
            }
            let mut tools = vec![json!({
                "name": "draw",
                "description": crate::llm::DRAW_DESC,
                "inputSchema": crate::llm::draw_schema()
            })];
            if desktop_exe().is_some() {
                tools.push(json!({
                    "name": "desktop",
                    "description": crate::desktop::DESC,
                    "inputSchema": crate::desktop::schema()
                }));
            }
            tools.extend(crate::vision::mcp_tools());
            if crate::agents::router_enabled(&crate::config::load().0) {
                tools.push(crate::agents::mcp_spawn_tool());
            }
            if crate::walk::router_enabled(&crate::config::load().0) {
                tools.push(json!({"name": "start_walkthrough", "description": crate::walk::DESC, "inputSchema": crate::walk::schema()}));
            }
            Ok(json!({"tools": tools}))
        }
        "tools/call" => {
            let p = &req["params"];
            let name = p["name"].as_str().unwrap_or("");
            let agent_tool = crate::agents::mcp_call(name, &p["arguments"]);
            let (text, is_error) = match p["name"].as_str() {
                _ if agent_tool.is_some() => agent_tool.unwrap_or_default(),
                Some("draw") if crate::agents::is_agent_mcp() => return Err((-32602, "unknown tool draw".into())),
                Some("window_screenshot") if crate::agents::is_agent_mcp() => {
                    return Ok(crate::agents::desk::mcp_screenshot(&p["arguments"]));
                }
                Some("draw") => validate(&p["arguments"]),
                // The parent reads the call from the CLI's stream and starts it.
                Some("start_walkthrough") if !crate::agents::is_agent_mcp() => (crate::walk::ACK.to_string(), false),
                Some("desktop") => desktop(&p["arguments"]),
                Some(n) if crate::vision::is_tool(n) => return Ok(crate::vision::mcp_call(n, &p["arguments"])),
                _ => return Err((-32602, format!("unknown tool {}", p["name"]))),
            };
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        m => Err((-32601, format!("method not found: {m}"))),
    }
}

fn desktop_exe() -> Option<std::path::PathBuf> {
    crate::config::load().0.desktop_exe()
}

/// Runs an agent-desktop command; actions are approved by the parent
/// screenbuddy process over the loopback port it passed in the environment.
fn desktop(input: &Value) -> (String, bool) {
    let Some(exe) = desktop_exe() else {
        return ("error: agent-desktop is not installed".into(), true);
    };
    let (args, intent) = match crate::desktop::parse(input) {
        Ok(v) => v,
        Err(e) => return (format!("error: {e}"), true),
    };
    if crate::desktop::needs_approval(&args, &intent) && !ask_parent(&intent, &args.join(" ")) {
        return (
            "The user declined this action. Stop and tell them briefly.".into(),
            true,
        );
    }
    let cfg = crate::config::load().0;
    let session = std::env::var("SCREENBUDDY_DESKTOP_SESSION").unwrap_or_else(|_| crate::desktop::new_session_id());
    let t0 = crate::agents::desk::now_ms();
    let out = crate::desktop::exec(&exe, &args, &intent, &crate::desktop::Ctx::from_config(&cfg, session));
    if crate::agents::is_agent_mcp() && !crate::desktop::is_read_only(&args) {
        // The parent moves windows the agent opened onto its desk.
        let _ = crate::agents::cli_notify(&json!({"kind": "desk", "args": args, "out": out, "t0": t0}));
    }
    let ok = serde_json::from_str::<Value>(&out)
        .ok()
        .and_then(|v| v["ok"].as_bool())
        .unwrap_or(true);
    (out, !ok)
}

fn ask_parent(intent: &str, detail: &str) -> bool {
    use std::io::{BufReader, Write};
    let Some(port) = std::env::var("SCREENBUDDY_APPROVAL_PORT")
        .ok()
        .and_then(|p| p.parse::<u16>().ok())
    else {
        return false;
    };
    let Ok(mut stream) = std::net::TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let req = json!({"intent": intent, "detail": detail}).to_string();
    if writeln!(stream, "{req}").is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = BufReader::new(&stream).read_line(&mut reply);
    reply.trim() == "allow"
}

fn validate(args: &Value) -> (String, bool) {
    let Some(shapes) = args["shapes"].as_array() else {
        return ("error: `shapes` must be an array".into(), true);
    };
    for (i, s) in shapes.iter().enumerate() {
        let placed = s["element"].is_string() || (s["x"].as_f64().is_some() && s["y"].as_f64().is_some());
        if s["kind"].as_str().is_none() || !placed {
            return (format!("error: shape {i} needs kind and x, y (or element)"), true);
        }
        if s["kind"] == "diagram" {
            // Parse it here so the model sees Mermaid errors and can retry.
            let Some(src) = s["mermaid"].as_str() else {
                return (format!("error: shape {i}: diagram needs `mermaid` source"), true);
            };
            let theme = crate::overlay::card_theme([10, 132, 255]);
            if let Err(e) = crate::diagram::render_blocking(src, theme, 1200.0, 800.0, 1.0) {
                return (format!("error: shape {i}: the Mermaid diagram didn't parse ({e}); fix the syntax and draw again"), true);
            }
        }
    }
    (
        format!("Drew {} annotation(s) on the user's screen.", shapes.len()),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lists_and_calls() {
        let l = handle(&json!({"method": "tools/list"})).unwrap();
        assert_eq!(l["tools"][0]["name"], "draw");
        let c = handle(&json!({"method": "tools/call", "params": {"name": "draw", "arguments": {"shapes": [{"kind": "box", "x": 1, "y": 2, "w": 3, "h": 4}]}}})).unwrap();
        assert_eq!(c["isError"], false);
    }
}
