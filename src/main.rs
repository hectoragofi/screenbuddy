#![cfg_attr(not(test), windows_subsystem = "windows")]

mod capture;
mod config;
mod context;
mod desktop;
mod history;
mod llm;
mod math;
mod mcp;
mod mention;
mod overlay;
mod palette;
mod settings;
mod tray;
mod uia;
mod util;
mod voice;
mod vision;
mod win;

use win::AppEvent;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, MSG, TranslateMessage,
};

fn main() {
    if std::env::args().any(|a| a == "--mcp") {
        mcp::run();
        return;
    }
    if std::env::args().any(|a| a == "--stt") {
        // Child process for one voice transcription (see voice.rs).
        std::process::exit(voice::stt_child());
    }
    // `screenbuddy --open-file <path>`: Explorer's "Ask screenbuddy" menu.
    let open_file = std::env::args()
        .position(|a| a == "--open-file")
        .and_then(|i| std::env::args().nth(i + 1))
        .map(std::path::PathBuf::from);
    if let Some(i) = std::env::args().position(|a| a == "--ask") {
        // Headless debug mode: capture, ask, print the stream to stdout.
        let q = std::env::args()
            .nth(i + 1)
            .unwrap_or_else(|| "What is on my screen?".into());
        std::process::exit(headless(&q));
    }
    if win::already_running() {
        if let Some(p) = &open_file {
            context::send_file_to_running(p);
            return;
        }
        // `screenbuddy --open` asks the running instance to open its palette.
        if std::env::args().any(|a| a == "--open") {
            win::signal_running(win::HK_ASK);
        }
        return;
    }
    win::set_dpi_awareness();
    if let Some(p) = open_file {
        win::push(AppEvent::OpenFile(p));
    }
    if let Err(e) = run() {
        config::log(&format!("fatal: {e:#}"));
        win::message_box(&format!("screenbuddy failed to start:\n{e:#}"));
    }
}

struct App {
    cfg: config::Config,
    cfg_error: Option<String>,
    hotkeys: (String, String),
    /// Registered push-to-talk hotkey (None = needs registering).
    voice_hk: Option<String>,
    tray: tray::Tray,
}

impl App {
    /// Re-reads config.toml; re-registers hotkeys if they changed.
    fn reload(&mut self) {
        let (cfg, err) = config::load();
        let want = (cfg.hotkey.clone(), cfg.region_hotkey.clone());
        let mut errs: Vec<String> = err.into_iter().collect();
        if want != self.hotkeys {
            errs.extend(win::register_hotkeys(&want.0, &want.1));
            self.hotkeys = want;
        }
        let voice = if cfg.voice.enabled { cfg.voice.hotkey.clone() } else { String::new() };
        if self.voice_hk.as_deref() != Some(voice.as_str()) {
            errs.extend(win::register_voice_hotkey(&voice));
            self.voice_hk = Some(voice);
        }
        self.cfg = cfg;
        self.cfg_error = if errs.is_empty() {
            None
        } else {
            Some(errs.join("\n"))
        };
    }

    /// `grab`: pick up text selected in the foreground app (hotkey presses only).
    fn ask(&mut self, region: bool, grab: bool, mut start: palette::Start) -> palette::Outcome {
        self.reload();
        // Start the Claude CLI now, in parallel with capture + window creation.
        llm::warm_on_hotkey(&self.cfg);
        let mode = self.cfg.context.selection.as_str();
        if grab && mode != "off" {
            if let Some((text, app)) = context::selected_text(mode == "uia+copy") {
                start.chips.insert(0, context::Chip::selection(&text, &app));
            }
        }
        let out = self.open_palette(region, start);
        llm::refill_spare(&self.cfg);
        win::trim_memory();
        out
    }

    fn open_palette(&mut self, region: bool, start: palette::Start) -> palette::Outcome {
        let shot = match capture::grab_monitor_under_cursor() {
            Ok(s) => s,
            Err(e) => {
                win::message_box(&format!("Screen capture failed: {e}"));
                return palette::Outcome::Closed;
            }
        };
        let area = if region {
            match overlay::pick_region(shot.monitor.rect) {
                Some(r) => Some(r),
                None => return palette::Outcome::Closed,
            }
        } else {
            None
        };
        let cap = match capture::encode(&shot, area) {
            Ok(c) => c,
            Err(e) => {
                win::message_box(&format!("Screenshot encoding failed: {e}"));
                return palette::Outcome::Closed;
            }
        };
        drop(shot); // free the raw pixels before the window opens
        vision::set_capture(&cap, &self.cfg);
        palette::run_with(self.cfg.clone(), cap, self.cfg_error.clone(), start)
    }
}

fn run() -> anyhow::Result<()> {
    win::create_main_window()?;
    let (cfg, err) = config::load();
    let mut errs: Vec<String> = err.into_iter().collect();
    errs.extend(win::register_hotkeys(&cfg.hotkey, &cfg.region_hotkey));
    let tray = tray::create(&cfg.hotkey, &cfg.region_hotkey)?;
    context::sync_explorer_menu(cfg.context.explorer_menu);
    let mut app = App {
        hotkeys: (cfg.hotkey.clone(), cfg.region_hotkey.clone()),
        voice_hk: None,
        cfg,
        cfg_error: if errs.is_empty() {
            None
        } else {
            Some(errs.join("\n"))
        },
        tray,
    };
    app.reload(); // registers the push-to-talk hotkey
    if let Some(e) = &app.cfg_error {
        config::log(e);
    }
    config::log("started");
    win::trim_memory();

    let mut msg = MSG::default();
    loop {
        unsafe {
            if !GetMessageW(&mut msg, None, 0, 0).as_bool() {
                break;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        while let Some(ev) = win::pop() {
            let outcome = match ev {
                AppEvent::Hotkey(win::HK_ASK) => app.ask(false, true, Default::default()),
                AppEvent::TrayClick => app.ask(false, false, Default::default()),
                AppEvent::Hotkey(win::HK_REGION) => app.ask(true, true, Default::default()),
                AppEvent::Hotkey(win::HK_VOICE) => app.ask(
                    false,
                    true,
                    palette::Start { voice: true, ..Default::default() },
                ),
                AppEvent::Hotkey(_) => palette::Outcome::Closed,
                AppEvent::OpenFile(p) => app.ask(
                    false,
                    false,
                    palette::Start { chips: vec![context::file_chip(&p)], ..Default::default() },
                ),
                AppEvent::Menu(id) => match id.as_str() {
                    tray::ASK => app.ask(false, false, Default::default()),
                    tray::REGION => app.ask(true, false, Default::default()),
                    tray::BROWSE => app.ask(
                        false,
                        false,
                        palette::Start { history: true, ..Default::default() },
                    ),
                    tray::SETTINGS => {
                        let o = settings::run();
                        app.hotkeys = Default::default(); // Settings unregistered them
                        app.voice_hk = None;
                        app.reload();
                        win::trim_memory();
                        o
                    }
                    tray::RELOAD => {
                        app.reload();
                        if let Some(e) = &app.cfg_error {
                            win::message_box(e);
                        }
                        palette::Outcome::Closed
                    }
                    other => match tray::handle_passive(other) {
                        tray::Passive::Quit => palette::Outcome::Quit,
                        _ => palette::Outcome::Closed,
                    },
                },
            };
            app.tray.autostart.set_checked(win::autostart_enabled());
            if let palette::Outcome::Quit = outcome {
                config::log("quit");
                return Ok(());
            }
        }
    }
    Ok(())
}

fn headless(q: &str) -> i32 {
    use std::io::Write;
    win::set_dpi_awareness();
    let (mut cfg, _) = config::load();
    if let Some(i) = std::env::args().position(|a| a == "--provider") {
        cfg.provider = std::env::args().nth(i + 1).unwrap_or_default();
    }
    let cap = match capture::grab_monitor_under_cursor().and_then(|s| capture::encode(&s, None)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("capture: {e}");
            return 1;
        }
    };
    vision::set_capture(&cap, &cfg);
    if std::env::args().any(|a| a == "--uia") {
        let t = std::time::Instant::now();
        let list = vision::context(&cfg);
        println!("[uia {} ms]{list}", t.elapsed().as_millis());
    }
    println!(
        "[{} | {}x{} jpeg {} KB]",
        cfg.provider_label(),
        cap.img_w,
        cap.img_h,
        cap.jpeg.len() / 1024
    );
    let (tx, rx) = std::sync::mpsc::channel();
    let turn = llm::Turn {
        cfg,
        conv: Default::default(),
        question: q.to_string(),
        image: Some(llm::Image {
            jpeg: std::sync::Arc::new(cap.jpeg.clone()),
            w: cap.img_w,
            h: cap.img_h,
        }),
        attachments: Vec::new(),
        cancel: Default::default(),
    };
    llm::spawn(turn, llm::Out::new(tx, egui::Context::default()));
    let mut code = 0;
    for ev in rx {
        match ev {
            llm::Ev::Delta(t) => print!("{t}"),
            llm::Ev::Status(s) => eprintln!("[{s}]"),
            llm::Ev::Draw(input, reply) => {
                let r = overlay::parse_draw(&input, &cap, [10, 132, 255])
                    .map(|c| format!("Drew {} annotation(s).", c.shapes.len()));
                println!(
                    "
[draw {input} -> {r:?}]"
                );
                let _ = reply.send(r);
            }
            llm::Ev::Approve(intent, detail, reply) => {
                let yes = std::env::args().any(|a| a == "--yes");
                println!(
                    "
[approve? {intent} :: {detail} -> {}]",
                    if yes {
                        "allowed"
                    } else {
                        "denied (pass --yes)"
                    }
                );
                let _ = reply.send(yes);
            }
            llm::Ev::Error(e) => {
                println!(
                    "
[error] {e}"
                );
                code = 2;
            }
            llm::Ev::Done(_) => break,
        }
        let _ = std::io::stdout().flush();
    }
    println!();
    code
}
