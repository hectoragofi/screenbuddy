use super::cli::{Parsed, parse_line};
use super::*;
use std::sync::mpsc::Receiver;

fn tmp_root(tag: &str) -> PathBuf {
    let p = std::env::temp_dir().join(format!("sb-agents-test-{tag}-{}", store::new_id()));
    let _ = std::fs::create_dir_all(&p);
    p
}

/// Scripted backend: each turn is a closure run on the worker thread.
type Script = Box<dyn Fn(&TurnJob, &Sink) -> Result<String, String> + Send + Sync>;
struct Fake(Script, Mutex<Vec<(String, Option<String>, bool)>>);

impl Backend for Fake {
    fn run(&self, job: &TurnJob, sink: &Sink) -> Result<String, String> {
        self.1
            .lock()
            .unwrap()
            .push((job.prompt.clone(), job.resume.clone(), job.first));
        (self.0)(job, sink)
    }
}

fn runtime(tag: &str, pending: Duration, script: Script) -> (Arc<Runtime>, Arc<Fake>) {
    let fake = Arc::new(Fake(script, Mutex::new(Vec::new())));
    let rt = Runtime::new(tmp_root(tag), fake.clone(), pending, Arc::new(Config::default));
    (rt, fake)
}

/// Collects this agent's events until `stop` returns true (or 10 s pass).
fn until(rx: &Receiver<(AgentId, AgentEvent)>, id: &str, stop: impl Fn(&AgentEvent) -> bool) -> Vec<AgentEvent> {
    let mut v = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if let Ok((aid, ev)) = rx.recv_timeout(Duration::from_millis(50)) {
            if aid != id {
                continue;
            }
            let end = stop(&ev);
            v.push(ev);
            if end {
                return v;
            }
        }
    }
    panic!("timed out; got {v:?}");
}

fn finished(ev: &AgentEvent) -> bool {
    matches!(ev, AgentEvent::Status(s) if !s.is_active())
}

#[test]
fn runs_to_done_with_plan_steps_files() {
    let (rt, _) = runtime(
        "done",
        Duration::from_millis(30),
        Box::new(|job, sink| {
            sink.session("sess-1");
            sink.plan(vec!["Look".into(), "Write".into()]);
            sink.step("Looking");
            sink.step("Looking"); // duplicate dropped
            std::fs::write(job.cwd.join("notes.md"), "hi").unwrap();
            Ok("  Wrote notes.md  ".into())
        }),
    );
    let rx = rt.subscribe();
    let rx2 = rt.subscribe(); // multi-subscriber
    let id = rt.spawn(Spawn {
        brief: "write some notes please".into(),
        ..Default::default()
    });
    let evs = until(&rx, &id, finished);
    assert_eq!(evs[0], AgentEvent::Status(Status::Pending));
    assert_eq!(evs[1], AgentEvent::Status(Status::Running));
    assert!(evs.contains(&AgentEvent::Plan(vec!["Look".into(), "Write".into()])));
    assert_eq!(evs.iter().filter(|e| matches!(e, AgentEvent::Step(_))).count(), 1);
    assert!(evs.contains(&AgentEvent::Result("Wrote notes.md".into())));
    assert!(evs.contains(&AgentEvent::Done));
    assert_eq!(evs.last(), Some(&AgentEvent::Status(Status::Done)));
    assert_eq!(until(&rx2, &id, finished).len(), evs.len());
    let a = rt.get(&id).unwrap();
    assert_eq!(a.status, Status::Done);
    assert_eq!(a.session_id.as_deref(), Some("sess-1"));
    assert_eq!(a.name, "write some notes please");
    assert_eq!(a.files.len(), 1);
    assert!(a.files[0].ends_with("notes.md"));
    assert!(a.counted);
    // persisted
    assert_eq!(store::load(&rt.root, &id).unwrap().result, "Wrote notes.md");
}

#[test]
fn cancel_in_pending_window_never_runs() {
    let (rt, fake) = runtime("cancel", Duration::from_millis(400), Box::new(|_, _| Ok("x".into())));
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "b".into(),
        ..Default::default()
    });
    assert!(rt.cancel(&id));
    let evs = until(&rx, &id, finished);
    assert_eq!(evs.last(), Some(&AgentEvent::Status(Status::Stopped)));
    assert!(fake.1.lock().unwrap().is_empty());
    let a = rt.get(&id).unwrap();
    assert!(!a.counted);
    assert!(!rt.cancel(&id));
}

#[test]
fn stop_soon_after_start_did_not_count() {
    let (rt, _) = runtime(
        "stop",
        Duration::ZERO,
        Box::new(|_, sink| {
            while !sink.stopped() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err("stopped".into())
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "long".into(),
        ..Default::default()
    });
    until(&rx, &id, |e| *e == AgentEvent::Status(Status::Running));
    assert!(!rt.cancel(&id)); // past pending
    let t0 = Instant::now();
    assert!(rt.stop(&id));
    let evs = until(&rx, &id, finished);
    assert!(t0.elapsed() < Duration::from_secs(10));
    assert_eq!(evs.last(), Some(&AgentEvent::Status(Status::Stopped)));
    assert!(!evs.iter().any(|e| matches!(e, AgentEvent::Failed(_))));
    assert!(!rt.get(&id).unwrap().counted);
}

#[test]
fn follow_up_resumes_session_and_resets_files() {
    let (rt, fake) = runtime(
        "follow",
        Duration::ZERO,
        Box::new(|job, sink| {
            sink.session("s-42");
            let name = if job.first { "a.txt" } else { "b.txt" };
            std::fs::write(job.cwd.join(name), "x").unwrap();
            Ok(format!("made {name}"))
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "make a".into(),
        ..Default::default()
    });
    until(&rx, &id, finished);
    std::thread::sleep(Duration::from_millis(1100)); // mtime granularity vs the 1 s slack
    assert!(rt.follow_up(&id, "now make b"));
    let evs = until(&rx, &id, finished);
    let files = evs
        .iter()
        .find_map(|e| {
            if let AgentEvent::Files(f) = e {
                Some(f.clone())
            } else {
                None
            }
        })
        .unwrap();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("b.txt"));
    let calls = fake.1.lock().unwrap().clone();
    assert_eq!(calls[1], ("now make b".into(), Some("s-42".into()), false));
    let a = rt.get(&id).unwrap();
    assert_eq!(a.turns.len(), 2);
    assert_eq!(a.result, "made b.txt");
}

#[test]
fn follow_up_while_running_is_queued() {
    let (rt, fake) = runtime(
        "queue",
        Duration::ZERO,
        Box::new(|job, _| {
            if job.first {
                std::thread::sleep(Duration::from_millis(200));
            }
            Ok("ok".into())
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "x".into(),
        ..Default::default()
    });
    until(&rx, &id, |e| *e == AgentEvent::Status(Status::Running));
    assert!(rt.follow_up(&id, "and then y"));
    until(&rx, &id, |e| *e == AgentEvent::Status(Status::Done));
    until(&rx, &id, |e| *e == AgentEvent::Status(Status::Done));
    assert_eq!(fake.1.lock().unwrap().len(), 2);
}

#[test]
fn approval_ask_and_conversation_scope() {
    let (rt, _) = runtime(
        "approve",
        Duration::ZERO,
        Box::new(|_, sink| {
            let first = sink.approve("Send the email", "click @s1:e4");
            let second = sink.approve("Send another", "click @s1:e5"); // covered by the first yes
            let ans = sink.questions(vec!["Which account?".into()]);
            Ok(format!("{first} {second} {ans}"))
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "email".into(),
        ..Default::default()
    });
    let evs = until(&rx, &id, |e| matches!(e, AgentEvent::Ask(_)));
    let AgentEvent::Ask(q) = evs.last().unwrap().clone() else {
        unreachable!()
    };
    assert_eq!(q.kind, AskKind::Approval);
    assert_eq!(q.text, "Send the email");
    assert_eq!(rt.get(&id).unwrap().ask.as_ref().map(|a| a.id), Some(q.id));
    assert!(rt.answer(&id, q.id, Answer::Allow));
    let evs = until(&rx, &id, |e| matches!(e, AgentEvent::Ask(_)));
    let AgentEvent::Ask(q2) = evs.last().unwrap().clone() else {
        unreachable!()
    };
    assert_eq!(q2.kind, AskKind::Questions);
    assert_eq!(q2.questions, vec!["Which account?".to_string()]);
    assert!(rt.follow_up(&id, "work")); // a follow-up answers open questions
    let evs = until(&rx, &id, finished);
    assert!(evs.contains(&AgentEvent::Result("true true The user answered: work".into())));
    assert!(rt.get(&id).unwrap().ask.is_none());
}

#[test]
fn deny_and_retry_after_failure() {
    let (rt, fake) = runtime(
        "retry",
        Duration::ZERO,
        Box::new(|job, sink| {
            if job.first && !sink.approve("Delete files", "click") {
                return Err("declined".into());
            }
            Ok("fine".into())
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "clean".into(),
        ..Default::default()
    });
    let evs = until(&rx, &id, |e| matches!(e, AgentEvent::Ask(_)));
    let AgentEvent::Ask(q) = evs.last().unwrap().clone() else {
        unreachable!()
    };
    rt.answer(&id, q.id, Answer::Deny);
    let evs = until(&rx, &id, finished);
    assert!(evs.contains(&AgentEvent::Failed("declined".into())));
    assert_eq!(rt.get(&id).unwrap().status, Status::Failed);
    assert!(rt.retry(&id));
    let evs = until(&rx, &id, |e| matches!(e, AgentEvent::Ask(_)));
    let AgentEvent::Ask(q) = evs.last().unwrap().clone() else {
        unreachable!()
    };
    rt.answer(&id, q.id, Answer::Allow);
    until(&rx, &id, finished);
    let a = rt.get(&id).unwrap();
    assert_eq!(a.status, Status::Done);
    assert_eq!(a.turns.len(), 1); // retry starts the conversation over
    assert_eq!(fake.1.lock().unwrap().len(), 2);
}

#[test]
fn always_approve_skips_asking() {
    let fake = Arc::new(Fake(
        Box::new(|_, sink| Ok(sink.approve("Pay", "click").to_string())),
        Mutex::new(Vec::new()),
    ));
    let rt = Runtime::new(
        tmp_root("always"),
        fake,
        Duration::ZERO,
        Arc::new(|| {
            let mut c = Config::default();
            c.agents.always_approve = true;
            c
        }),
    );
    let rx = rt.subscribe();
    let id = rt.spawn(Spawn {
        brief: "pay".into(),
        ..Default::default()
    });
    let evs = until(&rx, &id, finished);
    assert!(!evs.iter().any(|e| matches!(e, AgentEvent::Ask(_))));
    assert!(evs.contains(&AgentEvent::Result("true".into())));
}

#[test]
fn store_roundtrip_list_search_and_stale() {
    let root = tmp_root("store");
    let mut a = Agent {
        id: "20260101-000000-0001".into(),
        name: "Flight to Lisbon".into(),
        brief: "find flights".into(),
        status: Status::Done,
        created: 1,
        counted: true,
        ..Default::default()
    };
    a.turns.push(TurnRecord {
        prompt: "find flights".into(),
        result: "TAP at 9:05 for 120 EUR".into(),
        at: 2,
    });
    store::save(&root, &a).unwrap();
    let b = Agent {
        id: "20260102-000000-0002".into(),
        name: "Tidy desktop".into(),
        status: Status::Running,
        created: 2,
        ..Default::default()
    };
    store::save(&root, &b).unwrap();
    assert_eq!(store::load(&root, &a.id).unwrap(), a);
    let rt = Runtime::new(
        root.clone(),
        Arc::new(Fake(Box::new(|_, _| Ok(String::new())), Mutex::new(Vec::new()))),
        Duration::ZERO,
        Arc::new(Config::default),
    );
    let all = rt.list();
    assert_eq!(
        all.iter().map(|x| x.id.as_str()).collect::<Vec<_>>(),
        vec![b.id.as_str(), a.id.as_str()]
    );
    // active when screenbuddy closed -> failed, and persisted that way
    assert_eq!(all[0].status, Status::Failed);
    assert_eq!(store::load(&root, &b.id).unwrap().status, Status::Failed);
    assert_eq!(rt.search("tap eur").len(), 1);
    assert_eq!(rt.search("lisbon")[0].id, a.id);
    assert!(rt.search("nothing-like-this").is_empty());
    assert_eq!(rt.search("").len(), 2);
}

#[test]
fn names_and_ids() {
    assert_eq!(
        store::name_from("Find the cheapest flight to Lisbon next week."),
        "Find the cheapest flight"
    );
    assert_eq!(store::name_from("   "), "Agent");
    assert_ne!(store::new_id(), store::new_id());
}

#[test]
fn parses_cli_stream() {
    let init = r#"{"type":"system","subtype":"init","session_id":"abc","tools":[]}"#;
    assert_eq!(parse_line(init), vec![Parsed::Session("abc".into())]);
    let plan = r#"{"type":"assistant","session_id":"abc","message":{"content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"t1","name":"mcp__screenbuddy__report_plan","input":{"steps":["List apps","Summarise"]}}]}}"#;
    assert_eq!(
        parse_line(plan),
        vec![
            Parsed::Session("abc".into()),
            Parsed::Plan(vec!["List apps".into(), "Summarise".into()])
        ]
    );
    let step = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__screenbuddy__report_step","input":{"text":"Reading windows"}}]}}"#;
    assert_eq!(parse_line(step), vec![Parsed::Step("Reading windows".into())]);
    let read = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__screenbuddy__desktop","input":{"args":["list-apps"],"intent":"List apps"}}]}}"#;
    assert!(parse_line(read).is_empty());
    let act = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"mcp__screenbuddy__desktop","input":{"args":["click","@s1:e2"],"intent":"Click Save"}}]}}"#;
    assert_eq!(parse_line(act), vec![Parsed::Step("Click Save".into())]);
    let web = r#"{"type":"assistant","message":{"content":[{"type":"tool_use","name":"WebFetch","input":{"url":"https://example.com/a/b"}},{"type":"tool_use","name":"Write","input":{"file_path":"C:\\x\\notes.md","content":""}}]}}"#;
    assert_eq!(
        parse_line(web),
        vec![
            Parsed::Step("Reading example.com".into()),
            Parsed::Step("Writing notes.md".into())
        ]
    );
    let done =
        r#"{"type":"result","subtype":"success","is_error":false,"result":"You have 3 apps open.","session_id":"abc"}"#;
    assert_eq!(
        parse_line(done),
        vec![
            Parsed::Session("abc".into()),
            Parsed::Result {
                ok: true,
                text: "You have 3 apps open.".into()
            }
        ]
    );
    let err = r#"{"type":"result","subtype":"error_max_turns","is_error":true}"#;
    assert_eq!(
        parse_line(err),
        vec![Parsed::Result {
            ok: false,
            text: "claude stopped (error_max_turns)".into()
        }]
    );
    assert!(parse_line("not json").is_empty());
    assert!(parse_line(r#"{"type":"user","message":{}}"#).is_empty());
}

#[test]
fn router_tool_input() {
    assert!(spawn_from_tool(&json!({"name": "x", "brief": "  "}), None).is_err());
}
