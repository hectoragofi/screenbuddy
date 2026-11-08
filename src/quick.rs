//! Quick ask: hold Alt, speak, let go. The screen under the cursor and the
//! transcript go to the same brain as the palette (`llm.rs`, every provider),
//! the answer streams into a caption beside the cursor, and drawings land on
//! the overlay as usual. Clicking the caption continues the conversation in
//! the palette. Double-tapping Alt opens the palette.
//!
//! Runs on its own thread, driven by `keys` and `caption` events; idle it
//! blocks on a channel and costs nothing.

use crate::caption::{self, CaptionEvent, Cmd};
use crate::config::{self, Config};
use crate::keys::{self, KeyEvent};
use crate::{capture, history, llm, overlay, vision, voice, win};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

/// Buddy state for the island (or anyone else listening).
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    Idle,
    /// Recording; mic level 0..=1.
    Listening(f32),
    Thinking,
    Streaming,
    Done,
    Error(String),
}

type StatusSink = Box<dyn Fn(Status) + Send + Sync>;
static STATUS: Mutex<Option<StatusSink>> = Mutex::new(None);
static TX: OnceLock<Sender<Msg>> = OnceLock::new();

/// Receives every buddy-state change (called on the quick-ask thread; keep it short).
pub fn set_status_sink(f: impl Fn(Status) + Send + Sync + 'static) {
    *STATUS.lock().unwrap() = Some(Box::new(f));
}

fn status(s: Status) {
    if let Some(f) = STATUS.lock().unwrap().as_ref() {
        f(s);
    }
}

enum Msg {
    Key(KeyEvent),
    /// The mic on an agent card: start, or finish, a spoken follow-up.
    Mic(String),
    Caption(CaptionEvent),
    Config(Box<Config>),
}

/// Starts the keyboard hook and the quick-ask thread (once).
pub fn start(cfg: &Config) {
    let (tx, rx) = channel();
    if TX.set(tx.clone()).is_err() {
        configure(cfg);
        return;
    }
    configure(cfg);
    let (ctx, crx) = channel();
    caption::subscribe(ctx);
    let tx2 = tx.clone();
    std::thread::Builder::new()
        .name("screenbuddy-caption-events".into())
        .spawn(move || {
            for ev in crx {
                if tx2.send(Msg::Caption(ev)).is_err() {
                    break;
                }
            }
        })
        .ok();
    let cfg = cfg.clone();
    std::thread::Builder::new()
        .name("screenbuddy-quick".into())
        .spawn(move || Quick::new(cfg).run(rx))
        .ok();
    keys::start(move |ev| {
        if let KeyEvent::DoubleTap { .. } = ev {
            win::push(win::AppEvent::AltDoubleTap);
        }
        let _ = tx.send(Msg::Key(ev));
    });
}

/// Applies a reloaded config (hook settings and the brain).
pub fn configure(cfg: &Config) {
    let i = &cfg.input;
    keys::configure(i.alt_hold, i.alt_hold_ms, i.alt_double_tap);
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Config(Box::new(cfg.clone())));
    }
}

/// Toggles a spoken follow-up to agent `id` (the mic button on its card):
/// the first call starts listening, the next sends the transcript to
/// `agents::follow_up`.
pub fn mic_follow_up(id: &str) {
    if let Some(tx) = TX.get() {
        let _ = tx.send(Msg::Mic(id.to_string()));
    }
}

static LISTENING_FOR: Mutex<Option<String>> = Mutex::new(None);

/// The agent a spoken follow-up is being recorded for (its card shows it).
pub fn listening_for() -> Option<String> {
    LISTENING_FOR.lock().unwrap().clone()
}

fn set_listening_for(id: Option<String>) {
    let changed = {
        let mut g = LISTENING_FOR.lock().unwrap();
        let changed = *g != id;
        *g = id;
        changed
    };
    if changed {
        crate::cards::refresh();
    }
}

/// Spoken questions answer in a caption, so keep them short.
const BRIEF: &str = "This question was spoken aloud (hold-to-talk) and your answer appears in a small caption next to the cursor. Answer in one to three short sentences unless the user asks for detail; point at things by drawing rather than describing where they are.";

struct Turn {
    question: String,
    answer: String,
    error: Option<String>,
    rx: Receiver<llm::Ev>,
    cancel: Arc<AtomicBool>,
    cap: capture::Capture,
    streaming: bool,
}

struct Quick {
    cfg: Config,
    rec: Option<voice::Recording>,
    /// Screenshot taken when the hold started, still encoding.
    shot: Option<std::thread::JoinHandle<anyhow::Result<capture::Capture>>>,
    transcript: Option<Receiver<Result<String, String>>>,
    turn: Option<Turn>,
    /// The last finished quick ask, saved so the palette can continue it.
    last: Option<PathBuf>,
    /// Alt was held over this agent's card: the words are a follow-up to it.
    follow: Option<String>,
}

impl Quick {
    fn new(cfg: Config) -> Self {
        Self {
            cfg,
            rec: None,
            shot: None,
            transcript: None,
            turn: None,
            last: None,
            follow: None,
        }
    }

    fn busy(&self) -> bool {
        self.rec.is_some() || self.transcript.is_some() || self.turn.is_some()
    }

    fn run(mut self, rx: Receiver<Msg>) {
        loop {
            let msg = if self.busy() {
                match rx.recv_timeout(Duration::from_millis(33)) {
                    Ok(m) => Some(m),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            } else {
                match rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                }
            };
            match msg {
                Some(Msg::Key(k)) => self.key(k),
                Some(Msg::Mic(id)) => {
                    if self.rec.is_some() {
                        self.key(KeyEvent::HoldEnd {
                            cursor: capture::cursor_pos(),
                            target: Some(id),
                        });
                    } else {
                        self.key(KeyEvent::HoldStart {
                            cursor: capture::cursor_pos(),
                            target: Some(id),
                        });
                    }
                }
                Some(Msg::Caption(c)) => self.caption(c),
                Some(Msg::Config(c)) => self.cfg = *c,
                None => {}
            }
            self.poll();
        }
    }

    fn key(&mut self, k: KeyEvent) {
        match k {
            KeyEvent::HoldStart { cursor, target } => {
                if win::ui_open() {
                    return; // the palette has its own voice input
                }
                // A new question replaces whatever was going on.
                self.stop_turn();
                self.rec = None;
                self.transcript = None;
                self.follow = target.filter(|id| crate::agents::get(id).is_some());
                if !self.cfg.voice.enabled || !voice::model_ready() {
                    caption::send(Cmd::Error(
                        "Turn on voice in Settings › Voice to ask by holding Alt.".into(),
                    ));
                    return;
                }
                // The screen as it is when you start talking about it (a
                // follow-up goes to the agent, which has its own context).
                self.shot = if self.follow.is_some() {
                    None
                } else {
                    capture::grab_monitor_under_cursor()
                        .ok()
                        .map(|s| std::thread::spawn(move || capture::encode(&s, None)))
                };
                llm::warm_on_hotkey(&self.cfg);
                caption::send(Cmd::Listen { at: cursor });
                status(Status::Listening(0.0));
                match voice::Recording::start(&self.cfg.voice.language, egui::Context::default()) {
                    Ok(r) => {
                        self.rec = Some(r);
                        set_listening_for(self.follow.clone());
                    }
                    Err(e) => self.fail(e),
                }
            }
            KeyEvent::HoldEnd { .. } => {
                if let Some(r) = self.rec.take() {
                    set_listening_for(None);
                    self.transcript = Some(r.finish());
                    caption::send(Cmd::Think {
                        question: "Transcribing".into(),
                    });
                    status(Status::Thinking);
                }
            }
            KeyEvent::HoldCancel => {
                if self.rec.take().is_some() {
                    set_listening_for(None);
                    self.shot = None;
                    self.follow = None;
                    caption::send(Cmd::Hide);
                    status(Status::Idle);
                }
            }
            KeyEvent::DoubleTap { .. } => {} // the palette opens via AppEvent
        }
    }

    fn caption(&mut self, c: CaptionEvent) {
        match c {
            CaptionEvent::Clicked => {
                self.stop_turn();
                if let Some(dir) = self.last.take() {
                    win::push(win::AppEvent::OpenChat(dir));
                }
                status(Status::Idle);
            }
            CaptionEvent::Dismissed => {
                self.stop_turn();
                status(Status::Idle);
            }
        }
    }

    fn poll(&mut self) {
        if let Some(r) = &self.rec {
            let l = r.level();
            caption::send(Cmd::Level(l));
            status(Status::Listening(l));
        }
        if let Some(rx) = &self.transcript {
            match rx.try_recv() {
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    self.transcript = None;
                    self.fail("The transcriber stopped.".into());
                }
                Ok(r) => {
                    self.transcript = None;
                    match r {
                        Ok(t) if t.trim().is_empty() => {
                            self.follow = None;
                            caption::send(Cmd::Hide);
                            status(Status::Idle);
                        }
                        Ok(t) if self.follow.is_some() => {
                            // The card shows the agent picking it up.
                            let id = self.follow.take().unwrap_or_default();
                            crate::agents::follow_up(&id, t.trim());
                            caption::send(Cmd::Hide);
                            status(Status::Idle);
                        }
                        Ok(t) => self.ask(t.trim().to_string()),
                        Err(e) if e == "discarded" => {}
                        Err(e) => self.fail(e),
                    }
                }
            }
        }
        self.pump_turn();
    }

    fn ask(&mut self, question: String) {
        let cap = match self.shot.take().map(|h| h.join()) {
            Some(Ok(Ok(c))) => c,
            _ => match capture::grab_monitor_under_cursor().and_then(|s| capture::encode(&s, None))
            {
                Ok(c) => c,
                Err(e) => return self.fail(format!("Screen capture failed: {e}")),
            },
        };
        caption::send(Cmd::Think {
            question: question.clone(),
        });
        status(Status::Thinking);
        let mut cfg = self.cfg.clone();
        cfg.extra_instructions = if cfg.extra_instructions.trim().is_empty() {
            BRIEF.to_string()
        } else {
            format!("{}\n\n{BRIEF}", cfg.extra_instructions)
        };
        vision::set_capture(&cap, &cfg);
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        let turn = llm::Turn {
            cfg,
            conv: Default::default(),
            question: question.clone(),
            image: Some(llm::Image {
                jpeg: Arc::new(cap.jpeg.clone()),
                w: cap.img_w,
                h: cap.img_h,
            }),
            attachments: Vec::new(),
            cancel: cancel.clone(),
        };
        llm::spawn(turn, llm::Out::new(tx, egui::Context::default()));
        self.turn = Some(Turn {
            question,
            answer: String::new(),
            error: None,
            rx,
            cancel,
            cap,
            streaming: false,
        });
    }

    fn pump_turn(&mut self) {
        let Some(t) = self.turn.as_mut() else { return };
        let mut finished = false;
        loop {
            match t.rx.try_recv() {
                Ok(llm::Ev::Delta(d)) => {
                    if !t.streaming {
                        t.streaming = true;
                        status(Status::Streaming);
                    }
                    t.answer.push_str(&d);
                    caption::send(Cmd::Append(d));
                }
                Ok(llm::Ev::Status(_)) => {}
                Ok(llm::Ev::Spawned(id)) => {
                    // The router handed the work off; say so on the caption.
                    let name = crate::agents::get(&id).map(|a| a.name).unwrap_or(id);
                    let sep = if t.answer.trim().is_empty() { "" } else { "\n\n" };
                    let note = format!("{sep}Handed to agent \u{2039}{name}\u{203a}");
                    if !t.streaming {
                        t.streaming = true;
                        status(Status::Streaming);
                    }
                    t.answer.push_str(&note);
                    caption::send(Cmd::Append(note));
                }
                Ok(llm::Ev::Draw(input, reply)) => {
                    let color = config::hex_rgb(&self.cfg.drawing.color).unwrap_or([10, 132, 255]);
                    let r = overlay::parse_draw(&input, &t.cap, color).map(|call| {
                        let n = call.shapes.len();
                        caption::send(Cmd::Draw(call, t.cap.monitor.rect, t.cap.monitor.scale));
                        format!("Drew {n} annotation(s) on the user's screen.")
                    });
                    let _ = reply.send(r);
                }
                Ok(llm::Ev::Approve(_, _, reply)) => {
                    // No approval UI on a caption yet; the palette has one.
                    let _ = reply.send(false);
                    let note = "\n\n_Needs your OK: click to continue in the palette._";
                    t.answer.push_str(note);
                    caption::send(Cmd::Append(note.into()));
                }
                Ok(llm::Ev::Error(e)) => {
                    t.error = Some(e);
                }
                Ok(llm::Ev::Done(_)) => {
                    finished = true;
                    break;
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    finished = true;
                    break;
                }
            }
        }
        if finished {
            let t = self.turn.take().unwrap();
            self.last = save(&self.cfg, &t);
            match &t.error {
                Some(e) if t.answer.trim().is_empty() => {
                    caption::send(Cmd::Error(short_error(e)));
                    status(Status::Error(e.clone()));
                }
                _ => {
                    caption::send(Cmd::Done);
                    status(Status::Done);
                }
            }
        }
    }

    /// Cancels a running answer, keeping what came so far for the palette.
    fn stop_turn(&mut self) {
        if let Some(mut t) = self.turn.take() {
            t.cancel.store(true, Ordering::Relaxed);
            if t.error.is_none() && !t.answer.is_empty() {
                t.error = Some(format!("{}stopped.", llm::INTERRUPTED));
            }
            self.last = save(&self.cfg, &t);
        }
    }

    fn fail(&mut self, e: String) {
        self.rec = None;
        self.shot = None;
        self.follow = None;
        set_listening_for(None);
        config::log(&format!("quick ask: {e}"));
        caption::send(Cmd::Error(short_error(&e)));
        status(Status::Error(e));
    }
}

fn short_error(e: &str) -> String {
    let e = e.strip_prefix(llm::INTERRUPTED).unwrap_or(e);
    let first = e.lines().next().unwrap_or(e);
    first.chars().take(160).collect()
}

/// Writes the exchange like the palette's history, so the palette can open it.
/// With history off it goes to a temp folder instead.
fn save(cfg: &Config, t: &Turn) -> Option<PathBuf> {
    let root = if cfg.history {
        config::history_dir()
    } else {
        std::env::temp_dir().join("screenbuddy-quick")
    };
    let dir = root.join(format!("{}-quick", crate::util::file_stamp()));
    std::fs::create_dir_all(&dir).ok()?;
    let _ = std::fs::write(dir.join("screenshot.jpg"), &t.cap.jpeg);
    let chat = history::Chat {
        created: crate::util::timestamp(),
        provider: cfg.provider_label(),
        exchanges: vec![history::SavedExchange {
            question: t.question.clone(),
            answer: t.answer.clone(),
            error: t.error.clone(),
            attached: Vec::new(),
        }],
    };
    match history::write(&dir, &chat) {
        Ok(()) => Some(dir),
        Err(e) => {
            config::log(&format!("saving quick ask failed: {e}"));
            None
        }
    }
}
