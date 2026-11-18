//! Minimal MCP stdio server (`screenbuddy --mcp`) exposing the `draw` tool to
//! the Claude Code CLI. The parent screenbuddy process reads the tool call from
//! the CLI's stream-json output and renders it; this server only validates.

use serde_json::{Value, json};
use std::io::{BufRead, Write};

pub fn run() {
    // Actions may be routed through Apeiron (focus, cursor placement); connect
    // now so the layout is known by the first tool call.
    crate::apeiron::start();
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
            if crate::brain::enabled(&crate::config::load().0) {
                tools.push(json!({"name": "remember", "description": crate::brain::REMEMBER_DESC, "inputSchema": crate::brain::remember_schema()}));
                tools.push(json!({"name": "recall", "description": crate::brain::RECALL_DESC, "inputSchema": crate::brain::recall_schema()}));
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
            // A running agent learns facts saved after its turn began.
            let text = text + &crate::agents::mcp_brain_updates();
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
    let session = std::env::var("SCREENBUDDY_DESKTOP_SESSION").unwrap_or_else(|_| crate::desktop::app_session().to_string());
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
    let Some(mut stream) = loopback::connect(loopback::APPROVAL_PORT_ENV, loopback::APPROVAL_TOKEN_ENV) else {
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

/// Loopback sockets between the app and its `screenbuddy --mcp` child. Each
/// server has a random token, handed to the child through the environment of
/// the CLI it spawns (never on a command line); a client sends it as the first
/// line, then one request line. Anything else is dropped.
pub(crate) mod loopback {
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpStream;
    use std::time::{Duration, Instant};

    pub(crate) const APPROVAL_PORT_ENV: &str = "SCREENBUDDY_APPROVAL_PORT";
    pub(crate) const APPROVAL_TOKEN_ENV: &str = "SCREENBUDDY_APPROVAL_TOKEN";
    /// Time a client gets to send its token and request.
    pub(crate) const TIMEOUT: Duration = Duration::from_secs(10);
    /// Largest request line (a desktop snapshot can be a few MB).
    pub(crate) const MAX_LINE: usize = 8 << 20;
    const MAX_TOKEN: usize = 128;

    /// 32 bytes from the system RNG, hex.
    pub(crate) fn token() -> Option<String> {
        use windows::Win32::Security::Cryptography::{BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom};
        let mut b = [0u8; 32];
        unsafe { BCryptGenRandom(None, &mut b, BCRYPT_USE_SYSTEM_PREFERRED_RNG) }.ok().ok()?;
        Some(b.iter().map(|x| format!("{x:02x}")).collect())
    }

    /// Server side: the request line of an accepted connection, or None (drop
    /// it) on a wrong token, a timeout or an oversized line.
    pub(crate) fn read_request(stream: &TcpStream, token: &str) -> Option<String> {
        read_request_with(stream, token, TIMEOUT, MAX_LINE)
    }

    pub(crate) fn read_request_with(stream: &TcpStream, token: &str, timeout: Duration, cap: usize) -> Option<String> {
        stream.set_nonblocking(false).ok()?;
        stream.set_write_timeout(Some(timeout)).ok()?;
        let deadline = Instant::now() + timeout;
        let mut r = BufReader::new(stream);
        let got = read_line(&mut r, MAX_TOKEN, deadline)?;
        if !same(got.as_bytes(), token.as_bytes()) {
            return None;
        }
        read_line(&mut r, cap, deadline)
    }

    /// One `\n`-terminated line of at most `cap` bytes, read before `deadline`.
    fn read_line(r: &mut BufReader<&TcpStream>, cap: usize, deadline: Instant) -> Option<String> {
        let mut buf = Vec::new();
        loop {
            let left = deadline.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())?;
            r.get_ref().set_read_timeout(Some(left)).ok()?;
            let avail = r.fill_buf().ok()?;
            if avail.is_empty() {
                return None;
            }
            let (n, end) = match avail.iter().position(|&b| b == b'\n') {
                Some(i) => (i + 1, true),
                None => (avail.len(), false),
            };
            buf.extend_from_slice(&avail[..n]);
            r.consume(n);
            if buf.len() > cap + 1 {
                return None;
            }
            if end {
                break;
            }
        }
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
        if buf.len() > cap {
            return None;
        }
        String::from_utf8(buf).ok()
    }

    /// Constant-time comparison (the length is not secret).
    fn same(a: &[u8], b: &[u8]) -> bool {
        a.len() == b.len() && std::hint::black_box(a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y))) == 0
    }

    /// Client side (inside `screenbuddy --mcp`): connects to the parent's
    /// server and sends the token line.
    pub(crate) fn connect(port_env: &str, token_env: &str) -> Option<TcpStream> {
        let port: u16 = std::env::var(port_env).ok()?.parse().ok()?;
        let token = std::env::var(token_env).ok()?;
        let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
        writeln!(s, "{token}").ok()?;
        Some(s)
    }
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

    /// An echo server on the real loopback helper; returns its port.
    fn echo(token: &str, timeout: std::time::Duration, cap: usize) -> u16 {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let token = token.to_string();
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let token = token.clone();
                std::thread::spawn(move || {
                    if let Some(line) = loopback::read_request_with(&s, &token, timeout, cap) {
                        let mut w = &s;
                        let _ = writeln!(w, "echo {line}");
                    }
                });
            }
        });
        port
    }

    /// Sends `msg` raw and returns the reply line ("" when dropped).
    fn send(port: u16, msg: &[u8]) -> String {
        use std::io::{BufRead, Read};
        let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
        s.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let _ = s.write_all(msg);
        let mut r = String::new();
        let _ = std::io::BufReader::new(&s).take(1 << 16).read_line(&mut r);
        r.trim().to_string()
    }

    #[test]
    fn loopback_requires_token() {
        let t = loopback::token().unwrap();
        assert_eq!(t.len(), 64);
        assert_ne!(t, loopback::token().unwrap());
        let port = echo(&t, std::time::Duration::from_secs(5), 1024);
        assert_eq!(send(port, b"{\"intent\":\"x\"}\n"), "");
        assert_eq!(send(port, format!("{}\nhi\n", "0".repeat(64)).as_bytes()), "");
        assert_eq!(send(port, format!("{t}x\nhi\n").as_bytes()), "");
        assert_eq!(send(port, format!("{t}\nhi\n").as_bytes()), "echo hi");
        assert_eq!(send(port, format!("{t}\r\nhi\r\n").as_bytes()), "echo hi");
    }

    #[test]
    fn loopback_drops_idle_and_oversized() {
        let t = loopback::token().unwrap();
        let port = echo(&t, std::time::Duration::from_millis(300), 1024);
        // Connects, sends nothing: dropped after the timeout, not held open.
        let t0 = std::time::Instant::now();
        assert_eq!(send(port, b""), "");
        assert!(t0.elapsed() < std::time::Duration::from_secs(3), "{:?}", t0.elapsed());
        // Token but no request line.
        assert_eq!(send(port, format!("{t}\n").as_bytes()), "");
        // A line over the cap.
        assert_eq!(send(port, format!("{t}\n{}\n", "a".repeat(1025)).as_bytes()), "");
        assert_eq!(send(port, format!("{t}\n{}\n", "a".repeat(1024)).as_bytes()), format!("echo {}", "a".repeat(1024)));
        // An endless token line.
        assert_eq!(send(port, "b".repeat(4096).as_bytes()), "");
    }

    #[test]
    fn loopback_client_sends_token() {
        let t = loopback::token().unwrap();
        let port = echo(&t, std::time::Duration::from_secs(5), 1024);
        // SAFETY: test-only env vars, not read by other tests.
        unsafe {
            std::env::set_var("SB_TEST_LOOP_PORT", port.to_string());
            std::env::set_var("SB_TEST_LOOP_TOKEN", &t);
        }
        let mut s = loopback::connect("SB_TEST_LOOP_PORT", "SB_TEST_LOOP_TOKEN").unwrap();
        writeln!(s, "ok").unwrap();
        let mut r = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(&s), &mut r).unwrap();
        assert_eq!(r.trim(), "echo ok");
    }
}
