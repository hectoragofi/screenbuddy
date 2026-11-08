//! Background agents: work the user hands off ("find the cheapest flight and
//! put it in a note") runs on its own while they keep going.
//!
//! # API (what the cards / desk / history slices bind to)
//!
//! ```text
//! agents::spawn(brief: &str) -> AgentId
//! agents::spawn_with(Spawn { name, brief, screenshot, pending }) -> AgentId
//! agents::cancel(id: &str) -> bool        // only while Pending (the 5 s window); it never runs
//! agents::stop(id: &str) -> bool          // Pending or Running -> Stopped (< 10 s after start = didn't count)
//! agents::retry(id: &str) -> bool         // rerun the brief from scratch, same id, fresh session
//! agents::follow_up(id: &str, text: &str) -> bool
//!                                         // resumes the same session (`--resume`); answers a pending
//!                                         // question batch instead if one is open; queued while running
//! agents::answer(id: &str, question: u64, Answer) -> bool
//!                                         // resolve an AgentEvent::Ask (Allow / Deny / Text)
//! agents::subscribe() -> Receiver<(AgentId, AgentEvent)>   // every subscriber gets every event
//! agents::list() -> Vec<Agent>            // newest first, live state merged with the store
//! agents::search(q: &str) -> Vec<Agent>   // all terms must match name, brief, steps or results
//! agents::get(id: &str) -> Option<Agent>
//! agents::dir(id: &str) -> PathBuf        // %APPDATA%\screenbuddy\agents\<id>\ (its cwd + agent.json)
//! ```
//!
//! Events, per agent, in order:
//!
//! ```text
//! Status(Pending) .. Status(Running) .. Plan([..])? Step(..)* Ask(..)? Answered(qid)?
//!   .. Result(text) Files([..]) Done Status(Done)
//!   |  Failed(error) Status(Failed)
//!   |  Status(Stopped)
//! ```
//!
//! A follow-up or retry starts again at `Status(Running)`. `Files` lists only
//! the files written during the latest reply. `Agent::counted` is false when the
//! agent was stopped or cancelled within 10 s of starting (cosmetic).
//!
//! Approvals: desktop actions run immediately; only destructive / outbound /
//! money ones (see `desktop::needs_approval`) raise `Ask` with
//! `AskKind::Approval`. A yes covers the rest of that agent's conversation.
//! `agents.always_approve` skips asking.
//!
//! Runner: with the `claude-cli` provider each turn is its own
//! `claude -p --output-format stream-json` child in a kill-on-close job object,
//! cwd = the agent's folder, with `screenbuddy --mcp` in agent role
//! (`report_plan`, `report_step`, `ask_user`, `desktop`) plus file and web
//! tools. Other providers run an equivalent tool loop over HTTP (`api.rs`).

mod api;
mod cli;
pub mod desk;
mod store;

pub use store::Agent;
#[allow(unused_imports)]
pub use store::TurnRecord;

use crate::config::Config;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

pub type AgentId = String;

/// How long a new agent waits before it starts, so it can be cancelled.
pub const PENDING_WINDOW: Duration = Duration::from_secs(5);
/// Stopping within this long after start means it "didn't count".
pub const GRACE: Duration = Duration::from_secs(10);
/// How long a question or approval waits for the user.
const ASK_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Pending,
    Running,
    Done,
    Failed,
    Stopped,
}

impl Status {
    pub fn is_active(self) -> bool {
        matches!(self, Status::Pending | Status::Running)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AskKind {
    /// A destructive / outbound / money action waiting for Allow or Deny.
    Approval,
    /// The agent needs information: answer with `Answer::Text`.
    Questions,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Question {
    /// Pass back to `answer`.
    pub id: u64,
    pub kind: AskKind,
    /// Approval: the step's intent ("Send the email in Outlook").
    /// Questions: a one-line lead ("A couple of questions").
    pub text: String,
    /// Approval: the command line. Questions: empty.
    pub detail: String,
    /// Questions: the batch (at most a few). Approval: empty.
    pub questions: Vec<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Answer {
    Allow,
    Deny,
    Text(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum AgentEvent {
    Plan(Vec<String>),
    Step(String),
    Ask(Question),
    Result(String),
    Files(Vec<PathBuf>),
    Failed(String),
    /// The reply finished successfully (after Result and Files).
    Done,
    /// Lifecycle change; the last event of a run is Status(Done|Failed|Stopped).
    Status(Status),
    /// The question with this id was answered, timed out or dropped.
    Answered(u64),
}

/// Options for `spawn_with`.
#[derive(Clone, Default)]
pub struct Spawn {
    /// Short name for cards; derived from the brief when None.
    pub name: Option<String>,
    pub brief: String,
    /// JPEG of the user's screen, sent with the brief.
    pub screenshot: Option<Arc<Vec<u8>>>,
    /// Skip the 5 s cancel window.
    pub immediate: bool,
}

// ---------------------------------------------------------------- global API

fn rt() -> &'static Arc<Runtime> {
    static RT: OnceLock<Arc<Runtime>> = OnceLock::new();
    RT.get_or_init(|| {
        Runtime::new(
            crate::config::data_dir().join("agents"),
            Arc::new(Auto),
            PENDING_WINDOW,
            Arc::new(|| crate::config::load().0),
        )
    })
}

#[allow(dead_code)] // public API for the cards slice
pub fn spawn(brief: &str) -> AgentId {
    rt().spawn(Spawn {
        brief: brief.into(),
        ..Default::default()
    })
}
pub fn spawn_with(opts: Spawn) -> AgentId {
    rt().spawn(opts)
}
pub fn cancel(id: &str) -> bool {
    rt().cancel(id)
}
pub fn stop(id: &str) -> bool {
    rt().stop(id)
}
pub fn retry(id: &str) -> bool {
    rt().retry(id)
}
#[allow(dead_code)]
pub fn follow_up(id: &str, text: &str) -> bool {
    rt().follow_up(id, text)
}
pub fn answer(id: &str, question: u64, answer: Answer) -> bool {
    rt().answer(id, question, answer)
}
pub fn subscribe() -> Receiver<(AgentId, AgentEvent)> {
    rt().subscribe()
}
pub fn list() -> Vec<Agent> {
    rt().list()
}
#[allow(dead_code)]
pub fn search(q: &str) -> Vec<Agent> {
    rt().search(q)
}
pub fn get(id: &str) -> Option<Agent> {
    rt().get(id)
}
#[allow(dead_code)]
pub fn dir(id: &str) -> PathBuf {
    rt().root.join(id)
}

// ---------------------------------------------------------------- runtime

/// One turn for a backend to run.
pub(crate) struct TurnJob {
    pub id: AgentId,
    pub cwd: PathBuf,
    pub cfg: Config,
    pub prompt: String,
    pub image: Option<Arc<Vec<u8>>>,
    /// Session to resume (claude-cli); None on the first turn and on retry.
    pub resume: Option<String>,
    /// First turn of a conversation (spawn or retry), not a follow-up.
    pub first: bool,
}

pub(crate) trait Backend: Send + Sync {
    /// Runs one turn and returns the agent's reply text. Must return soon after
    /// `sink.stopped()` turns true.
    fn run(&self, job: &TurnJob, sink: &Sink) -> Result<String, String>;
}

/// Picks the runner by provider.
struct Auto;

impl Backend for Auto {
    fn run(&self, job: &TurnJob, sink: &Sink) -> Result<String, String> {
        match job.cfg.provider.as_str() {
            "claude-cli" => cli::run(job, sink),
            _ => api::run(job, sink),
        }
    }
}

#[derive(Default)]
struct Ctl {
    stop: AtomicBool,
    /// A worker thread owns this agent right now.
    busy: AtomicBool,
    /// The user said yes once: further approvals in this conversation pass.
    approved: AtomicBool,
    started: Mutex<Option<Instant>>,
    answers: Mutex<HashMap<u64, Sender<Answer>>>,
    followups: Mutex<VecDeque<String>>,
}

struct Slot {
    rec: Agent,
    ctl: Arc<Ctl>,
}

pub(crate) struct Runtime {
    root: PathBuf,
    backend: Arc<dyn Backend>,
    pending: Duration,
    config: Arc<dyn Fn() -> Config + Send + Sync>,
    slots: Mutex<HashMap<AgentId, Slot>>,
    loaded: std::sync::Once,
    subs: Mutex<Vec<Sender<(AgentId, AgentEvent)>>>,
    next_q: AtomicU64,
}

impl Runtime {
    pub(crate) fn new(
        root: PathBuf,
        backend: Arc<dyn Backend>,
        pending: Duration,
        config: Arc<dyn Fn() -> Config + Send + Sync>,
    ) -> Arc<Self> {
        Arc::new(Self {
            root,
            backend,
            pending,
            config,
            slots: Mutex::new(HashMap::new()),
            loaded: std::sync::Once::new(),
            subs: Mutex::new(Vec::new()),
            next_q: AtomicU64::new(1),
        })
    }

    /// Loads the store once; agents that were active when screenbuddy last
    /// closed are marked failed.
    fn ensure_loaded(&self) {
        self.loaded.call_once(|| {
            let mut slots = self.slots.lock().unwrap();
            for mut rec in store::load_all(&self.root) {
                if rec.status.is_active() {
                    rec.status = Status::Failed;
                    rec.error = Some("screenbuddy closed while it was working".into());
                    rec.ask = None;
                    let _ = store::save(&self.root, &rec);
                }
                slots.entry(rec.id.clone()).or_insert(Slot {
                    rec,
                    ctl: Arc::default(),
                });
            }
        });
    }

    fn emit(&self, id: &str, ev: AgentEvent) {
        let mut subs = self.subs.lock().unwrap();
        subs.retain(|s| s.send((id.to_string(), ev.clone())).is_ok());
    }

    pub(crate) fn subscribe(&self) -> Receiver<(AgentId, AgentEvent)> {
        let (tx, rx) = channel();
        self.subs.lock().unwrap().push(tx);
        rx
    }

    /// Changes the record under the lock, saves it, and returns a copy.
    fn update(&self, id: &str, f: impl FnOnce(&mut Agent)) -> Option<Agent> {
        let mut slots = self.slots.lock().unwrap();
        let slot = slots.get_mut(id)?;
        f(&mut slot.rec);
        if let Err(e) = store::save(&self.root, &slot.rec) {
            crate::config::log(&format!("agent {id}: saving failed: {e}"));
        }
        Some(slot.rec.clone())
    }

    fn set_status(&self, id: &str, status: Status) {
        self.update(id, |r| {
            r.status = status;
            if !status.is_active() {
                r.ended = Some(store::now());
            }
        });
        self.emit(id, AgentEvent::Status(status));
    }

    fn ctl(&self, id: &str) -> Option<Arc<Ctl>> {
        self.ensure_loaded();
        self.slots.lock().unwrap().get(id).map(|s| s.ctl.clone())
    }

    pub(crate) fn get(&self, id: &str) -> Option<Agent> {
        self.ensure_loaded();
        self.slots.lock().unwrap().get(id).map(|s| s.rec.clone())
    }

    pub(crate) fn list(&self) -> Vec<Agent> {
        self.ensure_loaded();
        let mut v: Vec<Agent> = self.slots.lock().unwrap().values().map(|s| s.rec.clone()).collect();
        v.sort_by(|a, b| b.created.cmp(&a.created).then_with(|| b.id.cmp(&a.id)));
        v
    }

    pub(crate) fn search(&self, q: &str) -> Vec<Agent> {
        let terms = crate::history::terms(q);
        self.list().into_iter().filter(|a| store::matches(a, &terms)).collect()
    }

    pub(crate) fn spawn(self: &Arc<Self>, opts: Spawn) -> AgentId {
        self.ensure_loaded();
        let id = store::new_id();
        let brief = opts.brief.trim().to_string();
        let name = opts
            .name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| store::name_from(&brief));
        let rec = Agent {
            id: id.clone(),
            name,
            brief: brief.clone(),
            status: Status::Pending,
            created: store::now(),
            counted: true,
            provider: (self.config)().provider,
            ..Default::default()
        };
        let _ = std::fs::create_dir_all(self.root.join(&id));
        let _ = store::save(&self.root, &rec);
        let ctl = Arc::new(Ctl::default());
        ctl.busy.store(true, Ordering::SeqCst);
        self.slots.lock().unwrap().insert(id.clone(), Slot { rec, ctl });
        self.emit(&id, AgentEvent::Status(Status::Pending));
        let wait = if opts.immediate { Duration::ZERO } else { self.pending };
        self.start(&id, brief_prompt(&brief), opts.screenshot, None, wait, true);
        id
    }

    fn start(
        self: &Arc<Self>,
        id: &str,
        prompt: String,
        image: Option<Arc<Vec<u8>>>,
        resume: Option<String>,
        wait: Duration,
        first: bool,
    ) {
        let rt = self.clone();
        let id = id.to_string();
        std::thread::spawn(move || rt.work(id, prompt, image, resume, wait, first));
    }

    fn work(
        self: Arc<Self>,
        id: AgentId,
        mut prompt: String,
        mut image: Option<Arc<Vec<u8>>>,
        mut resume: Option<String>,
        wait: Duration,
        mut first: bool,
    ) {
        let Some(ctl) = self.ctl(&id) else { return };
        let t0 = Instant::now();
        while t0.elapsed() < wait {
            if ctl.stop.load(Ordering::SeqCst) {
                self.update(&id, |r| r.counted = false);
                self.set_status(&id, Status::Stopped);
                ctl.busy.store(false, Ordering::SeqCst);
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        loop {
            let begun = std::time::SystemTime::now();
            *ctl.started.lock().unwrap() = Some(Instant::now());
            self.update(&id, |r| {
                r.status = Status::Running;
                r.started = Some(store::now());
                r.ended = None;
                r.error = None;
            });
            self.emit(&id, AgentEvent::Status(Status::Running));
            let cfg = (self.config)();
            let job = TurnJob {
                id: id.clone(),
                cwd: self.root.join(&id),
                cfg: cfg.clone(),
                prompt: prompt.clone(),
                image: image.take(),
                resume: resume.clone(),
                first,
            };
            let _ = std::fs::create_dir_all(&job.cwd);
            let sink = Sink {
                rt: self.clone(),
                id: id.clone(),
                ctl: ctl.clone(),
                always: cfg.agents.always_approve,
                waiting: Arc::default(),
            };
            let res = if ctl.stop.load(Ordering::SeqCst) {
                Err("stopped".into())
            } else {
                self.backend.run(&job, &sink)
            };
            self.drop_asks(&id, &ctl);
            if ctl.stop.load(Ordering::SeqCst) {
                let quick = ctl.started.lock().unwrap().is_some_and(|s| s.elapsed() < GRACE);
                self.update(&id, |r| {
                    if quick {
                        r.counted = false;
                    }
                });
                ctl.followups.lock().unwrap().clear();
                self.set_status(&id, Status::Stopped);
            } else {
                match res {
                    Ok(text) => {
                        let text = text.trim().to_string();
                        let files = store::files_since(&job.cwd, begun);
                        self.update(&id, |r| {
                            r.result = text.clone();
                            r.files = files.clone();
                            r.turns.push(TurnRecord {
                                prompt: prompt.clone(),
                                result: text.clone(),
                                at: store::now(),
                            });
                        });
                        self.emit(&id, AgentEvent::Result(text));
                        self.emit(&id, AgentEvent::Files(files));
                        self.emit(&id, AgentEvent::Done);
                        self.set_status(&id, Status::Done);
                    }
                    Err(e) => {
                        crate::config::log(&format!("agent {id} failed: {e}"));
                        self.update(&id, |r| r.error = Some(e.clone()));
                        self.emit(&id, AgentEvent::Failed(e));
                        ctl.followups.lock().unwrap().clear();
                        self.set_status(&id, Status::Failed);
                    }
                }
            }
            // A follow-up that arrived meanwhile runs next; checked under the
            // queue lock so one sent right now is never lost.
            let next = {
                let mut q = ctl.followups.lock().unwrap();
                let n = q.pop_front();
                if n.is_none() {
                    ctl.busy.store(false, Ordering::SeqCst);
                }
                n
            };
            match next {
                Some(n) => {
                    prompt = n;
                    resume = self.get(&id).and_then(|r| r.session_id);
                    first = false;
                }
                None => return,
            }
        }
    }

    /// Questions still open when a turn ends are void.
    fn drop_asks(&self, id: &str, ctl: &Ctl) {
        let open: Vec<u64> = ctl.answers.lock().unwrap().drain().map(|(k, _)| k).collect();
        if !open.is_empty() || self.get(id).is_some_and(|r| r.ask.is_some()) {
            self.update(id, |r| r.ask = None);
        }
        for q in open {
            self.emit(id, AgentEvent::Answered(q));
        }
    }

    pub(crate) fn cancel(&self, id: &str) -> bool {
        match (self.get(id), self.ctl(id)) {
            (Some(r), Some(c)) if r.status == Status::Pending => {
                c.stop.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn stop(&self, id: &str) -> bool {
        match (self.get(id), self.ctl(id)) {
            (Some(r), Some(c)) if r.status.is_active() => {
                c.stop.store(true, Ordering::SeqCst);
                c.followups.lock().unwrap().clear();
                c.answers.lock().unwrap().clear(); // waiting asks see a hang-up = deny
                true
            }
            _ => false,
        }
    }

    pub(crate) fn retry(self: &Arc<Self>, id: &str) -> bool {
        let Some(ctl) = self.ctl(id) else { return false };
        if ctl.busy.swap(true, Ordering::SeqCst) {
            return false;
        }
        ctl.stop.store(false, Ordering::SeqCst);
        ctl.approved.store(false, Ordering::SeqCst);
        let Some(rec) = self.update(id, |r| {
            r.plan.clear();
            r.steps.clear();
            r.result.clear();
            r.files.clear();
            r.turns.clear();
            r.error = None;
            r.session_id = None;
            r.counted = true;
        }) else {
            ctl.busy.store(false, Ordering::SeqCst);
            return false;
        };
        self.start(id, brief_prompt(&rec.brief), None, None, Duration::ZERO, true);
        true
    }

    #[allow(dead_code)]
    pub(crate) fn follow_up(self: &Arc<Self>, id: &str, text: &str) -> bool {
        let text = text.trim();
        let (Some(rec), Some(ctl)) = (self.get(id), self.ctl(id)) else {
            return false;
        };
        if text.is_empty() {
            return false;
        }
        if let Some(q) = rec.ask.as_ref().filter(|q| q.kind == AskKind::Questions) {
            return self.answer(id, q.id, Answer::Text(text.into()));
        }
        {
            let mut q = ctl.followups.lock().unwrap();
            if ctl.busy.swap(true, Ordering::SeqCst) {
                // Still working: runs as the next turn when this one finishes.
                q.push_back(text.into());
                return true;
            }
        }
        ctl.stop.store(false, Ordering::SeqCst);
        self.start(id, text.into(), None, rec.session_id.clone(), Duration::ZERO, false);
        true
    }

    pub(crate) fn answer(&self, id: &str, question: u64, answer: Answer) -> bool {
        let Some(ctl) = self.ctl(id) else { return false };
        let tx = ctl.answers.lock().unwrap().remove(&question);
        tx.is_some_and(|tx| tx.send(answer).is_ok())
    }
}

fn brief_prompt(brief: &str) -> String {
    format!("Task from the user:\n\n{brief}")
}

// ---------------------------------------------------------------- sink

/// What a running backend reports into; cheap to clone across its threads.
#[derive(Clone)]
pub(crate) struct Sink {
    rt: Arc<Runtime>,
    id: AgentId,
    ctl: Arc<Ctl>,
    always: bool,
    /// Asks in progress; the stall watchdog ignores silence while > 0.
    waiting: Arc<AtomicU64>,
}

impl Sink {
    pub fn stopped(&self) -> bool {
        self.ctl.stop.load(Ordering::SeqCst)
    }
    pub fn waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst) > 0
    }
    pub fn session(&self, sid: &str) {
        let cur = self.rt.get(&self.id).and_then(|r| r.session_id);
        if cur.as_deref() != Some(sid) {
            self.rt.update(&self.id, |r| r.session_id = Some(sid.to_string()));
        }
    }
    pub fn plan(&self, steps: Vec<String>) {
        let steps: Vec<String> = steps
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if steps.is_empty() {
            return;
        }
        self.rt.update(&self.id, |r| r.plan = steps.clone());
        self.rt.emit(&self.id, AgentEvent::Plan(steps));
    }
    pub fn step(&self, text: &str) {
        let text = text.trim();
        if text.is_empty()
            || self
                .rt
                .get(&self.id)
                .is_some_and(|r| r.steps.last().map(String::as_str) == Some(text))
        {
            return;
        }
        self.rt.update(&self.id, |r| {
            r.steps.push(text.to_string());
            if r.steps.len() > 60 {
                r.steps.remove(0);
            }
        });
        self.rt.emit(&self.id, AgentEvent::Step(text.to_string()));
    }

    /// Blocks until the user answers; Deny / empty text on stop or timeout.
    pub fn ask(&self, kind: AskKind, text: &str, detail: &str, questions: Vec<String>) -> Answer {
        let qid = self.rt.next_q.fetch_add(1, Ordering::SeqCst);
        let q = Question {
            id: qid,
            kind,
            text: text.into(),
            detail: detail.into(),
            questions,
        };
        let (tx, rx) = channel();
        self.ctl.answers.lock().unwrap().insert(qid, tx);
        self.waiting.fetch_add(1, Ordering::SeqCst);
        self.rt.update(&self.id, |r| r.ask = Some(q.clone()));
        self.rt.emit(&self.id, AgentEvent::Ask(q));
        let t0 = Instant::now();
        let ans = loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(a) => break Some(a),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break None,
                Err(_) if self.stopped() || t0.elapsed() > ASK_TIMEOUT => break None,
                Err(_) => {}
            }
        };
        self.ctl.answers.lock().unwrap().remove(&qid);
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        self.rt.update(&self.id, |r| {
            if r.ask.as_ref().is_some_and(|a| a.id == qid) {
                r.ask = None;
            }
        });
        self.rt.emit(&self.id, AgentEvent::Answered(qid));
        ans.unwrap_or(match kind {
            AskKind::Approval => Answer::Deny,
            AskKind::Questions => Answer::Text(String::new()),
        })
    }

    /// The approval policy for this agent: always_approve, an earlier yes in
    /// this conversation, or ask now.
    pub fn approve(&self, intent: &str, detail: &str) -> bool {
        if self.always || self.ctl.approved.load(Ordering::SeqCst) {
            return true;
        }
        let ok = matches!(self.ask(AskKind::Approval, intent, detail, Vec::new()), Answer::Allow);
        if ok {
            self.ctl.approved.store(true, Ordering::SeqCst);
        }
        ok
    }

    /// `ask_user`: returns the text handed back to the model.
    pub fn questions(&self, qs: Vec<String>) -> String {
        let qs: Vec<String> = qs
            .into_iter()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        if qs.is_empty() {
            return "error: pass at least one question".into();
        }
        match self.ask(AskKind::Questions, "A few questions", "", qs) {
            Answer::Text(t) if !t.trim().is_empty() => format!("The user answered: {}", t.trim()),
            _ => "The user didn't answer. Continue with your best judgement, or stop and say what you need.".into(),
        }
    }
}

// ---------------------------------------------------------------- prompts + tools

/// The agent's own system prompt.
pub(crate) fn agent_prompt(cfg: &Config, desktop: bool) -> String {
    let mut p = String::from(
        "You are a screenbuddy agent: you work in the background on one task the user handed you, on their Windows PC, while they keep doing other things. Do the work yourself, end to end; never describe what you would do instead of doing it.\n\n\
- Start by calling `report_plan` with 2 to 6 short steps (a few plain words each). Call `report_step` with a few words when you start a new step or something notable happens.\n\
- Files you make go in the current working directory (relative paths); the user sees the files of your latest reply. Don't write anywhere else.\n\
- Only ask when you truly can't continue without information only the user has: call `ask_user` once with one short batch of at most three questions, then carry on with the answers. Never ask for permission; screenbuddy asks the user itself before anything that deletes, sends, posts or pays. If an action is declined, find another way or stop and say so.\n\
- Do only what the task asks. Don't send, post, buy or delete anything the task didn't ask for.\n\
- When you are done, reply with a short result for the user: one to three sentences, or a short list, saying what you did or found. No preamble, no sign-off. If you couldn't finish, say what blocked you.",
    );
    if desktop {
        p.push_str("\n\nTo read or operate the user's apps use the `desktop` tool. Prefer the accessibility tree: `list-apps`, `snapshot --app <name> -i --compact`, `find`, `get`; take a screenshot only when the tree doesn't show what you need. Act on refs from the latest snapshot and snapshot again to confirm it worked. Keep each step's `intent` short and concrete, like 'Click Save in Notepad'.");
    }
    if !cfg.extra_instructions.trim().is_empty() {
        p.push_str("\n\n");
        p.push_str(cfg.extra_instructions.trim());
    }
    p
}

pub(crate) const PLAN_DESC: &str = "Tell the user your plan: 2 to 6 short steps, a few plain words each. Call once at the start (again only if the plan changes).";
pub(crate) const STEP_DESC: &str = "Tell the user what you're doing now, in a few words (e.g. 'Comparing prices').";
pub(crate) const ASK_DESC: &str = "Ask the user one short batch of questions (at most three) when you can't continue without information only they have. Blocks until they answer; returns their answer.";

pub(crate) fn plan_schema() -> Value {
    json!({"type": "object", "properties": {"steps": {"type": "array", "items": {"type": "string"}}}, "required": ["steps"]})
}
pub(crate) fn step_schema() -> Value {
    json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]})
}
pub(crate) fn ask_schema() -> Value {
    json!({"type": "object", "properties": {"questions": {"type": "array", "items": {"type": "string"}}}, "required": ["questions"]})
}

/// Router rules for the palette / quick-ask brain.
pub const ROUTER_PROMPT: &str = "You can hand work to a background agent with the `spawn_agent` tool. It works on its own (it can operate the user's apps, browse the web and make files) while the user carries on. Spawn one only when the user clearly asks you to do work that takes more than one step (research and compare, fill in or organise several things, make a document, keep at something). If you're not sure they want that, offer it in one short sentence instead of spawning. Never spawn when they say wait, hold on or not yet, and never for questions you can answer right away or a single action you can do yourself. Write a self-contained brief with everything the agent needs from this conversation and the screen; set needs_screen when the screenshot matters. After spawning, say in one short sentence that it's on it; don't do the work yourself.";

pub const SPAWN_DESC: &str =
    "Start a background agent for multi-step work the user clearly asked for. It runs on its own and reports back.";

pub fn spawn_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {"type": "string", "description": "2-4 word name for the task, e.g. 'Flight to Lisbon'"},
            "brief": {"type": "string", "description": "Self-contained instructions: the goal, every detail from the conversation, what to deliver"},
            "needs_screen": {"type": "boolean", "description": "Send the user's current screenshot with the brief"}
        },
        "required": ["name", "brief"]
    })
}

/// Whether the brain gets `spawn_agent`.
pub fn router_enabled(cfg: &Config) -> bool {
    cfg.agents.enabled
}

/// Handles a `spawn_agent` call from the brain; returns (agent id, tool result).
pub fn spawn_from_tool(input: &Value, screen: Option<Arc<Vec<u8>>>) -> Result<(AgentId, String), String> {
    let brief = input["brief"].as_str().unwrap_or("").trim();
    if brief.is_empty() {
        return Err("error: `brief` is empty".into());
    }
    let screenshot = if input["needs_screen"].as_bool() == Some(true) {
        screen
    } else {
        None
    };
    let name = input["name"].as_str().map(str::to_string);
    let id = spawn_with(Spawn {
        name,
        brief: brief.into(),
        screenshot,
        immediate: false,
    });
    Ok((id, "The agent is starting (the user can cancel it within 5 seconds) and will report back on its own. Tell the user in one short sentence; don't do the task yourself.".into()))
}

// ---------------------------------------------------------------- MCP (agent role)

/// Environment variable set on `screenbuddy --mcp` when it serves an agent.
pub(crate) const ROLE_ENV: &str = "SCREENBUDDY_ROLE";

/// True inside `screenbuddy --mcp` started for an agent.
pub fn is_agent_mcp() -> bool {
    std::env::var(ROLE_ENV).is_ok_and(|r| r == "agent")
}

/// Tools `screenbuddy --mcp` lists for an agent.
pub fn mcp_tools(desktop: bool) -> Vec<Value> {
    let mut v = vec![
        json!({"name": "report_plan", "description": PLAN_DESC, "inputSchema": plan_schema()}),
        json!({"name": "report_step", "description": STEP_DESC, "inputSchema": step_schema()}),
        json!({"name": "ask_user", "description": ASK_DESC, "inputSchema": ask_schema()}),
    ];
    if desktop {
        v.push(
            json!({"name": "desktop", "description": crate::desktop::DESC, "inputSchema": crate::desktop::schema()}),
        );
    }
    v
}

/// Agent-role MCP tools handled here (desktop stays in mcp.rs). None = not ours.
pub fn mcp_call(name: &str, args: &Value) -> Option<(String, bool)> {
    match name {
        // The parent reads these straight from the CLI's stream.
        "report_plan" | "report_step" => Some(("ok".into(), false)),
        "ask_user" => {
            let qs: Vec<Value> = args["questions"].as_array().cloned().unwrap_or_default();
            let req = json!({"kind": "ask", "questions": qs});
            Some(match cli::ask_parent_line(&req) {
                Some(line) => {
                    let v: Value = serde_json::from_str(&line).unwrap_or_default();
                    (
                        v["answer"].as_str().unwrap_or("The user didn't answer.").to_string(),
                        false,
                    )
                }
                None => ("error: screenbuddy is not reachable".into(), true),
            })
        }
        "spawn_agent" if !is_agent_mcp() => Some(("ok".into(), false)),
        _ => None,
    }
}

/// `spawn_agent` as an MCP tool for the palette's Claude CLI (the parent reads
/// the call from the stream and spawns; the server only acknowledges).
pub fn mcp_spawn_tool() -> Value {
    json!({"name": "spawn_agent", "description": SPAWN_DESC, "inputSchema": spawn_schema()})
}

// ---------------------------------------------------------------- headless

/// `screenbuddy --agent "brief" [--follow <id>] [--yes] [--provider p]`: runs
/// one agent (or a follow-up to agent `id`, resuming its session) and prints its
/// event stream. Approvals: --yes allows, otherwise y/n on stdin; questions are
/// answered from stdin.
pub fn headless(brief: &str, follow: Option<String>, yes: bool, provider: Option<String>) -> i32 {
    match provider {
        Some(p) => {
            // A runtime with the provider overridden for this process only.
            let rt = Runtime::new(
                crate::config::data_dir().join("agents"),
                Arc::new(Auto),
                PENDING_WINDOW,
                Arc::new(move || {
                    let mut c = crate::config::load().0;
                    c.provider = p.clone();
                    c
                }),
            );
            let rx = rt.subscribe();
            run_headless(&rt, rx, brief, follow, yes)
        }
        None => run_headless(rt(), rt().subscribe(), brief, follow, yes),
    }
}

fn run_headless(rt: &Arc<Runtime>, rx: Receiver<(AgentId, AgentEvent)>, brief: &str, follow: Option<String>, yes: bool) -> i32 {
    use std::io::{BufRead, Write};
    let cfg = (rt.config)();
    let id = match follow {
        Some(id) => {
            if !rt.follow_up(&id, brief) {
                println!("[no agent {id}, or it is busy]");
                return 1;
            }
            id
        }
        None => rt.spawn(Spawn {
            brief: brief.into(),
            immediate: true,
            ..Default::default()
        }),
    };
    println!(
        "[agent {id} | {} | {}]",
        cfg.provider_label(),
        rt.root.join(&id).display()
    );
    let mut code = 0;
    for (aid, ev) in rx {
        if aid != id {
            continue;
        }
        match ev {
            AgentEvent::Status(s) => {
                println!("[status] {s:?}");
                if !s.is_active() {
                    break;
                }
            }
            AgentEvent::Plan(p) => println!("[plan] {}", p.join(" / ")),
            AgentEvent::Step(s) => println!("[step] {s}"),
            AgentEvent::Ask(q) => {
                let ans = match q.kind {
                    AskKind::Approval => {
                        println!("[approve?] {} :: {}", q.text, q.detail);
                        let ok = yes || {
                            print!("allow? [y/N] ");
                            let _ = std::io::stdout().flush();
                            let mut l = String::new();
                            let _ = std::io::stdin().lock().read_line(&mut l);
                            l.trim().eq_ignore_ascii_case("y")
                        };
                        println!("[{}]", if ok { "allowed" } else { "denied" });
                        if ok { Answer::Allow } else { Answer::Deny }
                    }
                    AskKind::Questions => {
                        println!("[questions] {}", q.questions.join(" | "));
                        print!("answer> ");
                        let _ = std::io::stdout().flush();
                        let mut l = String::new();
                        let _ = std::io::stdin().lock().read_line(&mut l);
                        Answer::Text(l.trim().to_string())
                    }
                };
                rt.answer(&id, q.id, ans);
            }
            AgentEvent::Answered(_) => {}
            AgentEvent::Result(t) => println!("[result]\n{t}"),
            AgentEvent::Files(f) => {
                if !f.is_empty() {
                    println!(
                        "[files] {}",
                        f.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
                    );
                }
            }
            AgentEvent::Failed(e) => {
                println!("[failed] {e}");
                code = 2;
            }
            AgentEvent::Done => println!("[done]"),
        }
        let _ = std::io::stdout().flush();
    }
    if let Some(r) = rt.get(&id) {
        println!("[session {}]", r.session_id.unwrap_or_default());
    }
    code
}

#[cfg(test)]
mod tests;
