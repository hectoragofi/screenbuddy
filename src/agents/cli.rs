//! Claude CLI runner: one `claude -p` stream-json child per turn, in a
//! kill-on-close job object, cwd = the agent's folder.

use super::{Sink, TurnJob};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Silence (while nothing waits on the user) after which a turn is dead.
const STALL: Duration = Duration::from_secs(600);

const APPROVAL_PORT_ENV: &str = "SCREENBUDDY_APPROVAL_PORT";

/// What one stream-json line means for the agent.
#[derive(Debug, PartialEq)]
pub(crate) enum Parsed {
    Session(String),
    Plan(Vec<String>),
    Step(String),
    Result { ok: bool, text: String },
}

fn strs(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str())
        .map(str::to_string)
        .collect()
}

/// Parses one line of `claude -p --output-format stream-json --verbose`.
pub(crate) fn parse_line(line: &str) -> Vec<Parsed> {
    let Ok(ev) = serde_json::from_str::<Value>(line) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(id) = ev["session_id"].as_str() {
        out.push(Parsed::Session(id.to_string()));
    }
    match ev["type"].as_str().unwrap_or("") {
        "assistant" => {
            for b in ev["message"]["content"].as_array().into_iter().flatten() {
                if b["type"] != "tool_use" {
                    continue;
                }
                let name = b["name"].as_str().unwrap_or("");
                let input = &b["input"];
                let short = name.rsplit("__").next().unwrap_or(name);
                match short {
                    "report_plan" => out.push(Parsed::Plan(strs(&input["steps"]))),
                    "report_step" => {
                        if let Some(t) = input["text"].as_str() {
                            out.push(Parsed::Step(t.to_string()));
                        }
                    }
                    "desktop" => {
                        // Actions show as steps; reading the UI doesn't.
                        let args = strs(&input["args"]);
                        let intent = input["intent"].as_str().unwrap_or("").trim();
                        if !crate::desktop::is_read_only(&args) && !intent.is_empty() {
                            out.push(Parsed::Step(intent.to_string()));
                        }
                    }
                    "WebSearch" => {
                        if let Some(q) = input["query"].as_str() {
                            out.push(Parsed::Step(format!("Searching: {q}")));
                        }
                    }
                    "WebFetch" => {
                        if let Some(u) = input["url"].as_str() {
                            let host = u.split("://").nth(1).unwrap_or(u).split('/').next().unwrap_or(u);
                            out.push(Parsed::Step(format!("Reading {host}")));
                        }
                    }
                    "Write" => {
                        if let Some(p) = input["file_path"].as_str() {
                            let f = p.rsplit(['/', '\\']).next().unwrap_or(p);
                            out.push(Parsed::Step(format!("Writing {f}")));
                        }
                    }
                    _ => {}
                }
            }
        }
        "result" => {
            let ok = ev["is_error"].as_bool() != Some(true) && ev["subtype"].as_str().is_none_or(|s| s == "success");
            let text = match ev["result"].as_str() {
                Some(t) if !t.trim().is_empty() => t.to_string(),
                _ if ok => String::new(),
                _ => {
                    let errs = strs(&ev["errors"]);
                    if errs.is_empty() {
                        format!("claude stopped ({})", ev["subtype"].as_str().unwrap_or("error"))
                    } else {
                        errs.join("; ")
                    }
                }
            };
            out.push(Parsed::Result { ok, text });
        }
        _ => {}
    }
    out
}

/// Kills the CLI (and, through the job, Node and the MCP server) on drop.
struct Child {
    child: std::process::Child,
    _job: Option<crate::llm::Job>,
    done: Arc<AtomicBool>,
}

impl Drop for Child {
    fn drop(&mut self) {
        self.done.store(true, Ordering::Relaxed);
        let _ = self.child.kill();
    }
}

pub(crate) fn run(job: &TurnJob, sink: &Sink) -> Result<String, String> {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    let cfg = &job.cfg;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let desktop = cfg.desktop_exe().is_some();
    let done = Arc::new(AtomicBool::new(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let port = listener.local_addr().map_err(|e| e.to_string())?.port();
    serve(listener, sink.clone(), done.clone());
    let mut env = serde_json::Map::new();
    env.insert(super::ROLE_ENV.into(), json!("agent"));
    env.insert(APPROVAL_PORT_ENV.into(), json!(port.to_string()));
    env.insert(
        "SCREENBUDDY_DESKTOP_SESSION".into(),
        json!(format!("screenbuddy-agent-{}", job.id)),
    );
    let mcp = json!({"mcpServers": {"screenbuddy": {"type": "stdio", "command": exe.to_string_lossy(), "args": ["--mcp"], "env": env}}}).to_string();
    let builtin = "Read,Write,Edit,Glob,Grep,WebSearch,WebFetch";
    let mut allowed = vec![
        "mcp__screenbuddy__report_plan",
        "mcp__screenbuddy__report_step",
        "mcp__screenbuddy__ask_user",
        "Read",
        "Write",
        "Edit",
        "Glob",
        "Grep",
        "WebSearch",
        "WebFetch",
    ];
    if desktop {
        allowed.push("mcp__screenbuddy__desktop");
    }
    let allowed = allowed.join(" ");
    let system = super::agent_prompt(cfg, desktop);
    let model = if cfg.agents.model.trim().is_empty() {
        cfg.claude_cli.model.trim()
    } else {
        cfg.agents.model.trim()
    };
    let build = |program: &str| {
        let mut c = Command::new(program);
        c.args([
            "-p",
            "--output-format",
            "stream-json",
            "--input-format",
            "stream-json",
            "--verbose",
        ])
        .args(["--system-prompt", &system])
        .args(["--tools", builtin])
        .args(["--strict-mcp-config", "--mcp-config", &mcp])
        .args(["--allowedTools", &allowed]);
        if !model.is_empty() {
            c.args(["--model", model]);
        }
        if let Some(id) = &job.resume {
            c.args(["--resume", id]);
        }
        // `ask_user` may wait on the user for a long time.
        c.env("MCP_TOOL_TIMEOUT", "3600000")
            .current_dir(&job.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .creation_flags(CREATE_NO_WINDOW);
        c
    };
    let command = cfg.claude_cli.command.as_str();
    let spawned = match build(command).spawn() {
        Ok(c) => Ok(c),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !command.ends_with(".cmd") => {
            build(&format!("{command}.cmd")).spawn()
        }
        Err(e) => Err(e),
    };
    let mut child = spawned.map_err(|e| {
        done.store(true, Ordering::Relaxed);
        if e.kind() == std::io::ErrorKind::NotFound {
            format!("'{command}' not found. Install Claude Code or set claude_cli.command.")
        } else {
            format!("couldn't start claude: {e}")
        }
    })?;
    let job_obj = crate::llm::Job::wrap(&child);
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take();
    let mut proc = Child {
        child,
        _job: job_obj,
        done,
    };
    let (tx, lines) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let err_buf = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(e) = stderr {
        let buf = err_buf.clone();
        std::thread::spawn(move || {
            for line in BufReader::new(e).lines().map_while(Result::ok) {
                let mut b = buf.lock().unwrap();
                if b.len() < 32_000 {
                    b.push_str(&line);
                    b.push('\n');
                }
            }
        });
    }

    let mut content = Vec::new();
    if let Some(img) = &job.image {
        use base64::Engine;
        let data = base64::engine::general_purpose::STANDARD.encode(img.as_slice());
        content.push(json!({"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": data}}));
        content.push(json!({"type": "text", "text": "[Screenshot of the user's screen when they asked]"}));
    }
    content.push(json!({"type": "text", "text": job.prompt}));
    let msg = json!({"type": "user", "message": {"role": "user", "content": content}}).to_string();
    if stdin
        .write_all(format!("{msg}\n").as_bytes())
        .and_then(|_| stdin.flush())
        .is_err()
    {
        return Err(died(&mut proc, &err_buf));
    }

    let mut last = Instant::now();
    let mut session = String::new();
    loop {
        if sink.stopped() {
            return Err("stopped".into()); // dropping `proc` kills the tree
        }
        let line = match lines.recv_timeout(Duration::from_millis(100)) {
            Ok(l) => l,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if sink.waiting() {
                    last = Instant::now();
                } else if last.elapsed() > STALL {
                    return Err(format!("no response for {} s, gave up", STALL.as_secs()));
                }
                continue;
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return Err(died(&mut proc, &err_buf)),
        };
        last = Instant::now();
        for p in parse_line(&line) {
            match p {
                Parsed::Session(s) => {
                    if s != session {
                        sink.session(&s);
                        session = s;
                    }
                }
                Parsed::Plan(steps) => sink.plan(steps),
                Parsed::Step(s) => sink.step(&s),
                Parsed::Result { ok: true, text } => return Ok(text),
                Parsed::Result { ok: false, text } => return Err(text),
            }
        }
    }
}

fn died(proc: &mut Child, err: &std::sync::Mutex<String>) -> String {
    let _ = proc.child.wait();
    std::thread::sleep(Duration::from_millis(50));
    let s = err.lock().unwrap();
    let lines: Vec<&str> = s.lines().collect();
    let tail = lines[lines.len().saturating_sub(6)..].join("\n");
    if tail.trim().is_empty() {
        "claude exited before finishing".into()
    } else {
        format!("claude exited before finishing: {tail}")
    }
}

/// Answers the agent's `screenbuddy --mcp`: one JSON line in, one line out.
/// `{"intent","detail"}` (approval) -> "allow" | "deny";
/// `{"kind":"ask","questions":[..]}` -> `{"answer": "..."}`.
fn serve(listener: std::net::TcpListener, sink: Sink, done: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    std::thread::spawn(move || {
        while !done.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((stream, _)) => {
                    let sink = sink.clone();
                    std::thread::spawn(move || {
                        let _ = stream.set_nonblocking(false);
                        let mut line = String::new();
                        let _ = BufReader::new(&stream).read_line(&mut line);
                        let req: Value = serde_json::from_str(&line).unwrap_or_default();
                        let reply = if req["kind"] == "ask" {
                            json!({"answer": sink.questions(strs(&req["questions"]))}).to_string()
                        } else {
                            let ok = sink.approve(
                                req["intent"].as_str().unwrap_or("perform an action"),
                                req["detail"].as_str().unwrap_or(""),
                            );
                            (if ok { "allow" } else { "deny" }).to_string()
                        };
                        let mut w = &stream;
                        let _ = writeln!(w, "{reply}");
                    });
                }
                Err(_) => std::thread::sleep(Duration::from_millis(40)),
            }
        }
    });
}

/// Inside `screenbuddy --mcp`: sends one request line to the parent agent and
/// returns its reply line.
pub(crate) fn ask_parent_line(req: &Value) -> Option<String> {
    let port: u16 = std::env::var(APPROVAL_PORT_ENV).ok()?.parse().ok()?;
    let mut s = std::net::TcpStream::connect(("127.0.0.1", port)).ok()?;
    writeln!(s, "{req}").ok()?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line).ok()?;
    Some(line.trim().to_string())
}
