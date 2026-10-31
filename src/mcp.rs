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
        "tools/list" => Ok(json!({"tools": [{
            "name": "draw",
            "description": crate::llm::DRAW_DESC,
            "inputSchema": crate::llm::draw_schema()
        }]})),
        "tools/call" => {
            let p = &req["params"];
            if p["name"] != "draw" {
                return Err((-32602, format!("unknown tool {}", p["name"])));
            }
            let (text, is_error) = validate(&p["arguments"]);
            Ok(json!({"content": [{"type": "text", "text": text}], "isError": is_error}))
        }
        m => Err((-32601, format!("method not found: {m}"))),
    }
}

fn validate(args: &Value) -> (String, bool) {
    let Some(shapes) = args["shapes"].as_array() else {
        return ("error: `shapes` must be an array".into(), true);
    };
    for (i, s) in shapes.iter().enumerate() {
        if s["kind"].as_str().is_none() || s["x"].as_f64().is_none() || s["y"].as_f64().is_none() {
            return (format!("error: shape {i} needs kind, x, y"), true);
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
