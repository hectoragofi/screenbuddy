//! Mermaid diagrams in answers, rendered by Pele (vendor/pele, MIT) running
//! in an embedded QuickJS, then rasterized with resvg into egui textures.
//!
//! The JS runtime lives on a worker thread that is spawned on the first
//! diagram and exits after a minute idle, so it costs nothing when unused.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use egui::{Color32, ColorImage, RichText, TextureHandle, TextureOptions, Ui, Vec2};
use resvg::{tiny_skia, usvg};

const PELE_JS: &str = include_str!("../vendor/pele/pele.global.js");
const IDLE_EXIT: Duration = Duration::from_secs(60);
const RENDER_BUDGET: Duration = Duration::from_secs(3);
/// Upper bound on the raster edge, in pixels.
const MAX_PX: f32 = 4096.0;

/// A run of Markdown or the body of a closed ```mermaid fence.
#[derive(Debug, PartialEq)]
pub enum Seg<'a> {
    Md(&'a str),
    Mermaid(&'a str),
}

/// Splits out closed ```mermaid / ~~~mermaid blocks. A fence still being
/// streamed stays Markdown (shown as code) until it closes.
pub fn split(md: &str) -> Vec<Seg<'_>> {
    let mut out = Vec::new();
    let mut start = 0; // start of the current Markdown run
    let mut pos = 0;
    // Open fence: (char, len, is_mermaid, fence line start, body start).
    let mut open: Option<(char, usize, bool, usize, usize)> = None;
    while pos < md.len() {
        let end = md[pos..].find('\n').map_or(md.len(), |i| pos + i + 1);
        let line = &md[pos..end];
        let t = line.trim_end_matches(['\n', '\r']);
        let indent = t.len() - t.trim_start_matches(' ').len();
        let body = &t[indent..];
        let fence = body.chars().next().filter(|c| indent <= 3 && (*c == '`' || *c == '~'));
        let run = fence.map_or(0, |c| body.len() - body.trim_start_matches(c).len());
        match open {
            None if run >= 3 => {
                let c = fence.unwrap();
                let info = body[run..].trim();
                let lang = info.split_whitespace().next().unwrap_or("");
                open = Some((c, run, lang.eq_ignore_ascii_case("mermaid"), pos, end));
            }
            Some((c, n, mermaid, fence_at, body_at))
                if fence == Some(c) && run >= n && body[run..].trim().is_empty() =>
            {
                if mermaid {
                    if fence_at > start {
                        out.push(Seg::Md(&md[start..fence_at]));
                    }
                    out.push(Seg::Mermaid(&md[body_at..pos]));
                    start = end;
                }
                open = None;
            }
            _ => {}
        }
        pos = end;
    }
    if start < md.len() {
        out.push(Seg::Md(&md[start..]));
    }
    out
}

/// Colors handed to Pele's CSS variables.
#[derive(Clone, Copy, Hash, PartialEq)]
pub struct Theme {
    pub bg: Color32,
    pub fg: Color32,
    pub muted: Color32,
    pub accent: Color32,
}

impl Theme {
    fn vars(&self) -> HashMap<String, String> {
        let hex = |c: Color32| format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
        let mix = |a: Color32, b: Color32, t: f32| {
            let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
            Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
        };
        [
            ("bg", hex(self.bg)),
            ("fg", hex(self.fg)),
            ("muted", hex(self.muted)),
            ("line", hex(mix(self.bg, self.fg, 0.7))),
            ("surface", hex(mix(self.bg, self.fg, 0.09))),
            ("surface-alt", hex(mix(self.bg, self.fg, 0.16))),
            ("border", hex(mix(self.bg, self.fg, 0.35))),
            ("accent", hex(self.accent)),
            ("radius", "6px".into()),
            ("font", "'Segoe UI'".into()),
            ("font-mono", "Consolas".into()),
        ]
        .into_iter()
        .map(|(k, v)| (format!("--pele-{k}"), v))
        .collect()
    }
}

struct Img {
    size: [usize; 2],
    rgba: Vec<u8>,
    /// Size in points at which the diagram reads naturally.
    native: Vec2,
}

enum Slot {
    Pending,
    Done(Result<Img, String>),
}

type Slots = Arc<Mutex<HashMap<u64, Slot>>>;

/// Rendered diagrams for one egui context (the palette's).
#[derive(Default)]
pub struct DiagramCache {
    slots: Slots,
    tex: HashMap<u64, (TextureHandle, Vec2)>,
    failed: HashMap<u64, String>,
}

impl DiagramCache {
    pub fn clear(&mut self) {
        self.slots.lock().unwrap().clear();
        self.tex.clear();
        self.failed.clear();
    }
}

/// Draws a Mermaid block; falls back to its source (plus Pele's error) when
/// it does not parse.
pub fn show(ui: &mut Ui, cache: &mut DiagramCache, md: &mut egui_commonmark::CommonMarkCache, src: &str, theme: Theme) {
    let ppp = ui.ctx().pixels_per_point();
    let avail = ui.available_width().max(80.0);
    // Pele picks a direction for the width it is given; bucket it so small
    // resizes reuse the render.
    let max_w = ((avail / 40.0).floor() * 40.0).max(80.0);
    let key = {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        (src, max_w.to_bits(), ppp.to_bits(), theme).hash(&mut h);
        h.finish()
    };

    if cache.tex.len() + cache.failed.len() > 128 {
        cache.tex.clear();
        cache.failed.clear();
    }
    if !cache.tex.contains_key(&key) && !cache.failed.contains_key(&key) {
        let done = {
            let mut slots = cache.slots.lock().unwrap();
            match slots.remove(&key) {
                None => {
                    slots.insert(key, Slot::Pending);
                    drop(slots);
                    submit(Job {
                        key,
                        src: src.to_string(),
                        theme,
                        max_w,
                        ppp,
                        slots: cache.slots.clone(),
                        ctx: ui.ctx().clone(),
                    });
                    None
                }
                Some(Slot::Pending) => {
                    slots.insert(key, Slot::Pending);
                    None
                }
                Some(Slot::Done(r)) => Some(r),
            }
        };
        match done {
            Some(Ok(img)) => {
                let image = ColorImage::from_rgba_premultiplied(img.size, &img.rgba);
                let h = ui.ctx().load_texture(format!("mermaid:{key:x}"), image, TextureOptions::LINEAR);
                cache.tex.insert(key, (h, img.native));
            }
            Some(Err(e)) => {
                cache.failed.insert(key, e);
            }
            None => {}
        }
    }

    if let Some((handle, native)) = cache.tex.get(&key) {
        let k = (avail / native.x).min(1.0);
        let size = *native * k;
        ui.add_space(4.0);
        let resp = ui
            .add(egui::Image::new(handle).fit_to_exact_size(size).sense(egui::Sense::click()))
            .on_hover_text("Right-click to copy the Mermaid source");
        resp.context_menu(|ui| {
            if ui.button("Copy Mermaid source").clicked() {
                ui.ctx().copy_text(src.to_string());
                ui.close();
            }
        });
        ui.add_space(4.0);
    } else if let Some(err) = cache.failed.get(&key) {
        let block = format!("```mermaid\n{src}```\n");
        egui_commonmark::CommonMarkViewer::new().show(ui, md, &block);
        let first = err.lines().next().unwrap_or(err);
        ui.label(RichText::new(format!("Diagram didn't render: {first}")).color(theme.muted).small());
    } else {
        ui.label(RichText::new("Drawing diagram…").color(theme.muted).small());
    }
}

struct Job {
    key: u64,
    src: String,
    theme: Theme,
    max_w: f32,
    ppp: f32,
    slots: Slots,
    ctx: egui::Context,
}

fn worker() -> &'static Mutex<Option<Sender<Job>>> {
    static W: OnceLock<Mutex<Option<Sender<Job>>>> = OnceLock::new();
    W.get_or_init(|| Mutex::new(None))
}

fn submit(job: Job) {
    let mut w = worker().lock().unwrap();
    let job = match w.as_ref() {
        Some(tx) => match tx.send(job) {
            Ok(()) => return,
            Err(mpsc::SendError(job)) => job,
        },
        None => job,
    };
    let (tx, rx) = mpsc::channel();
    let _ = tx.send(job);
    let spawned = std::thread::Builder::new()
        .name("mermaid".into())
        .stack_size(16 << 20)
        .spawn(move || run(rx));
    if spawned.is_ok() {
        *w = Some(tx);
    }
}

fn run(rx: Receiver<Job>) {
    let mut engine = Engine::new();
    loop {
        let job = match rx.recv_timeout(IDLE_EXIT) {
            Ok(j) => j,
            Err(RecvTimeoutError::Timeout) => {
                // Unregister first so no new jobs reach this receiver, then
                // finish anything that slipped in.
                *worker().lock().unwrap() = None;
                while let Ok(j) = rx.try_recv() {
                    handle(&mut engine, j);
                }
                return;
            }
            Err(RecvTimeoutError::Disconnected) => return,
        };
        handle(&mut engine, job);
    }
}

fn handle(engine: &mut Result<Engine, String>, job: Job) {
    let result = match engine {
        Ok(e) => e
            .svg(&job.src, job.max_w)
            .and_then(|svg| rasterize(&svg, &job.theme, job.ppp, &e.opts)),
        Err(e) => Err(e.clone()),
    };
    job.slots.lock().unwrap().insert(job.key, Slot::Done(result));
    job.ctx.request_repaint();
}

struct Engine {
    _rt: rquickjs::Runtime,
    cx: rquickjs::Context,
    deadline: Arc<Mutex<Instant>>,
    opts: usvg::Options<'static>,
}

impl Engine {
    fn new() -> Result<Self, String> {
        let err = |e: rquickjs::Error| format!("diagram engine: {e}");
        let rt = rquickjs::Runtime::new().map_err(err)?;
        rt.set_memory_limit(96 << 20);
        rt.set_max_stack_size(8 << 20);
        let deadline = Arc::new(Mutex::new(Instant::now() + Duration::from_secs(30)));
        let d = deadline.clone();
        rt.set_interrupt_handler(Some(Box::new(move || Instant::now() > *d.lock().unwrap())));
        let cx = rquickjs::Context::full(&rt).map_err(err)?;
        cx.with(|c| {
            c.eval::<(), _>(PELE_JS)?;
            c.eval::<(), _>(
                "globalThis.__render = (s, w) => pele.render(s, {fontFamily: \"'Segoe UI'\", \
                 fontFamilyMono: 'Consolas', maxWidth: w, links: false, images: false}).svg;",
            )
        })
        .map_err(err)?;
        Ok(Self { _rt: rt, cx, deadline, opts: options() })
    }

    fn svg(&mut self, src: &str, max_w: f32) -> Result<String, String> {
        *self.deadline.lock().unwrap() = Instant::now() + RENDER_BUDGET;
        self.cx.with(|c| {
            let f: rquickjs::Function = c.globals().get("__render").map_err(|e| e.to_string())?;
            f.call::<_, String>((src, max_w as f64)).map_err(|e| match e {
                rquickjs::Error::Exception => {
                    let v = c.catch();
                    v.as_exception()
                        .and_then(|x| x.message())
                        .unwrap_or_else(|| "invalid diagram".into())
                }
                e => e.to_string(),
            })
        })
    }
}

/// Substitutes `var(--x, fallback)` with theme values or the fallback, since
/// usvg has no CSS custom properties. Pele defines its private `--_*` vars in
/// the root style attribute.
fn resolve_vars(svg: &str, theme: &Theme) -> String {
    let mut vars = theme.vars();
    if let Some(i) = svg.find("style=\"") {
        let rest = &svg[i + 7..];
        let style = &rest[..rest.find('"').unwrap_or(rest.len())];
        for decl in split_top(style, ';') {
            if let Some((k, v)) = decl.split_once(':') {
                let k = k.trim();
                if k.starts_with("--") && !vars.contains_key(k) {
                    vars.insert(k.to_string(), v.trim().to_string());
                }
            }
        }
    }
    substitute(svg, &vars, 0)
}

fn substitute(s: &str, vars: &HashMap<String, String>, depth: u8) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find("var(") {
        out.push_str(&rest[..i]);
        let inner_start = i + 4;
        let Some(close) = matching_paren(&rest[inner_start..]) else {
            out.push_str(&rest[i..]);
            return out;
        };
        let inner = &rest[inner_start..inner_start + close];
        let (name, fallback) = match inner.find(',') {
            Some(c) => (inner[..c].trim(), Some(inner[c + 1..].trim())),
            None => (inner.trim(), None),
        };
        let value = vars.get(name).map(String::as_str).or(fallback).unwrap_or("");
        if depth < 8 {
            out.push_str(&substitute(value, vars, depth + 1));
        } else {
            out.push_str(value);
        }
        rest = &rest[inner_start + close + 1..];
    }
    out.push_str(rest);
    out
}

/// Byte index of the `)` closing an already-opened paren.
fn matching_paren(s: &str) -> Option<usize> {
    let mut depth = 0usize;
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'(' => depth += 1,
            b')' if depth == 0 => return Some(i),
            b')' => depth -= 1,
            _ => {}
        }
    }
    None
}

fn split_top(s: &str, sep: char) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut depth, mut from) = (0i32, 0);
    for (i, c) in s.char_indices() {
        match c {
            '(' => depth += 1,
            ')' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(&s[from..i]);
                from = i + 1;
            }
            _ => {}
        }
    }
    out.push(&s[from..]);
    out
}

/// resvg options with Segoe UI / Consolas only (not the whole system font
/// set). Owned by the worker, so the font data goes when it exits.
fn options() -> usvg::Options<'static> {
    let mut db = usvg::fontdb::Database::new();
    let dir = std::env::var_os("WINDIR").map_or_else(|| "C:\\Windows".into(), std::path::PathBuf::from).join("Fonts");
    for f in ["segoeui.ttf", "segoeuib.ttf", "segoeuii.ttf", "segoeuiz.ttf", "seguisb.ttf", "consola.ttf", "consolab.ttf"] {
        if let Ok(data) = std::fs::read(dir.join(f)) {
            db.load_font_data(data);
        }
    }
    db.set_sans_serif_family("Segoe UI");
    db.set_monospace_family("Consolas");
    usvg::Options { font_family: "Segoe UI".into(), fontdb: Arc::new(db), ..Default::default() }
}

fn rasterize(svg: &str, theme: &Theme, ppp: f32, opts: &usvg::Options) -> Result<Img, String> {
    // usvg ignores CSS `rx`, which Pele uses for themeable corner radii.
    let svg = resolve_vars(svg, theme).replace("style=\"rx:6px\"", "rx=\"6\"");
    let tree = usvg::Tree::from_str(&svg, opts).map_err(|e| format!("svg: {e}"))?;
    let native = tree.size();
    let scale = ppp.min(MAX_PX / native.width().max(native.height()).max(1.0));
    let w = (native.width() * scale).ceil().max(1.0) as u32;
    let h = (native.height() * scale).ceil().max(1.0) as u32;
    let mut pm = tiny_skia::Pixmap::new(w, h).ok_or("empty diagram")?;
    resvg::render(&tree, tiny_skia::Transform::from_scale(scale, scale), &mut pm.as_mut());
    Ok(Img {
        size: [w as usize, h as usize],
        rgba: pm.take(),
        native: Vec2::new(native.width(), native.height()),
    })
}

/// Renders a diagram to PNG bytes for the offline render check.
#[cfg(test)]
fn png(src: &str, theme: &Theme) -> Result<Vec<u8>, String> {
    let mut e = Engine::new()?;
    let svg = e.svg(src, 560.0)?;
    let img = rasterize(&svg, theme, 2.0, &e.opts)?;
    let pm = tiny_skia::Pixmap::from_vec(
        img.rgba,
        tiny_skia::IntSize::from_wh(img.size[0] as u32, img.size[1] as u32).unwrap(),
    )
    .unwrap();
    pm.encode_png().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DARK: Theme = Theme {
        bg: Color32::from_rgb(30, 30, 32),
        fg: Color32::from_rgb(236, 236, 240),
        muted: Color32::from_rgb(152, 152, 160),
        accent: Color32::from_rgb(120, 150, 255),
    };

    #[test]
    fn splits_closed_mermaid_fences_only() {
        let md = "Hi\n```mermaid\ngraph LR\nA-->B\n```\nafter\n```rust\nlet x = 1;\n```\n```mermaid\ngraph TD\n";
        assert_eq!(
            split(md),
            vec![
                Seg::Md("Hi\n"),
                Seg::Mermaid("graph LR\nA-->B\n"),
                Seg::Md("after\n```rust\nlet x = 1;\n```\n```mermaid\ngraph TD\n"),
            ]
        );
        // A mermaid fence inside another code block is just code.
        let nested = "````md\n```mermaid\nA-->B\n```\n````\n";
        assert_eq!(split(nested), vec![Seg::Md(nested)]);
    }

    #[test]
    fn resolves_nested_vars() {
        let svg = r#"<svg style="--_bg:var(--pele-bg,#fff);--_x:var(--nope,#123)" fill="var(--_bg)" stroke="var(--_x)"/>"#;
        let out = resolve_vars(svg, &DARK);
        assert!(out.contains(r##"fill="#1e1e20""##), "{out}");
        assert!(out.contains(r##"stroke="#123""##), "{out}");
    }

    #[test]
    fn renders_diagrams() {
        let samples = [
            ("flow", "flowchart LR\n A[Start] --> B{Is it?}\n B -->|Yes| C[OK]\n B -->|No| D[Nope]"),
            ("seq", "sequenceDiagram\n Alice->>Bob: Hello\n Bob-->>Alice: Hi"),
            ("pie", "pie title Pets\n \"Dogs\" : 386\n \"Cats\" : 85"),
            ("xy", "xychart-beta\n x-axis [jan, feb, mar]\n y-axis \"Rev\" 0 --> 100\n bar [10, 50, 80]"),
        ];
        let dir = std::env::var_os("SCREENBUDDY_MERMAID_PNG").map(std::path::PathBuf::from);
        for (name, src) in samples {
            let png = png(src, &DARK).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert!(png.len() > 1000, "{name}");
            if let Some(d) = &dir {
                std::fs::write(d.join(format!("{name}.png")), png).unwrap();
            }
        }
        let mut e = Engine::new().unwrap();
        assert!(e.svg("flowchart LR\n A -->", 560.0).unwrap_err().contains("Parse error"));
    }
}
