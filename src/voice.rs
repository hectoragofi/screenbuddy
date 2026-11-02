//! Push-to-talk voice input with Whistle, Cactus Compute's on-device
//! speech-to-text model, running on Cactus's Needle engine.
//!
//! - The engine (`needle.dll`, linked from Cactus's prebuilt `libneedle.a` by
//!   `scripts/build-needle.sh`) is embedded in the exe at build time and written
//!   to `%APPDATA%\screenbuddy\models` on first use.
//! - The model (`whistle.cact`, 16.9 MB) is never bundled: it is downloaded on
//!   first enable, pinned by revision and SHA-256.
//! - Nothing is loaded while idle. Recording opens the default microphone via
//!   WASAPI, and a short-lived child process (`screenbuddy --stt`) loads the
//!   engine and model while you speak, transcribes when you stop, then exits,
//!   so all of its memory goes back to the OS.

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const SAMPLE_RATE: u32 = 16_000;
const MODEL_FILE: &str = "whistle.cact";
const MODEL_SIZE: u64 = 16_919_407;
const MODEL_SHA256: &str = "b6e02f048568ac5d01a2042556c658061e699acbc0aa2a1439f52f3d461dffeb";
/// Official repo first, then the mirror; both pinned to a revision.
const MODEL_URLS: [&str; 2] = [
    "https://huggingface.co/Cactus-Compute/whistle/resolve/741118687e1f008f06d1351a402a731274789790/whistle.cact",
    "https://huggingface.co/machineandfolk/whistle-mirror/resolve/3aea78bae19f1e76501fa1f992672d0cbacd87d3/whistle.cact",
];
/// Languages Whistle knows; "" lets it detect.
pub const LANGUAGES: &[(&str, &str)] = &[
    ("", "Detect"),
    ("en", "English"),
    ("de", "German"),
    ("fr", "French"),
    ("es", "Spanish"),
    ("it", "Italian"),
    ("nl", "Dutch"),
    ("pl", "Polish"),
];

#[cfg(needle)]
static ENGINE: Option<&[u8]> = Some(include_bytes!(concat!(env!("OUT_DIR"), "/needle.dll")));
#[cfg(not(needle))]
static ENGINE: Option<&[u8]> = None;

/// True if this build carries the speech engine.
pub fn engine_included() -> bool {
    ENGINE.is_some()
}

pub fn models_dir() -> PathBuf {
    let d = crate::config::data_dir().join("models");
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn model_path() -> PathBuf {
    models_dir().join(MODEL_FILE)
}

/// The model is verified by hash when downloaded; afterwards its size is checked.
pub fn model_ready() -> bool {
    std::fs::metadata(model_path())
        .map(|m| m.len() == MODEL_SIZE)
        .unwrap_or(false)
}

pub fn remove_model() -> std::io::Result<()> {
    let p = model_path();
    if p.exists() {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Writes the embedded engine next to the model (once per engine version).
fn engine_path() -> Result<PathBuf, String> {
    let bytes = ENGINE.ok_or(
        "This build doesn't include the speech engine (see README: scripts/build-needle.sh).",
    )?;
    let tag = &hex(&Sha256::digest(bytes))[..12];
    let p = models_dir().join(format!("needle-{tag}.dll"));
    if std::fs::metadata(&p)
        .map(|m| m.len() != bytes.len() as u64)
        .unwrap_or(true)
    {
        let tmp = p.with_extension("tmp");
        std::fs::write(&tmp, bytes)
            .and_then(|_| std::fs::rename(&tmp, &p))
            .map_err(|e| format!("Couldn't write the speech engine: {e}"))?;
        // Old engine versions are no longer needed.
        if let Ok(rd) = std::fs::read_dir(models_dir()) {
            for e in rd.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.starts_with("needle-") && n.ends_with(".dll") && e.path() != p {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    Ok(p)
}

// ---------------------------------------------------------------- download

#[derive(Default, Clone)]
pub struct Download {
    /// 0..=1
    pub progress: f32,
    pub done: bool,
    pub error: Option<String>,
}

/// Downloads the model in the background; poll the returned state.
pub fn start_download(ctx: Option<egui::Context>) -> Arc<Mutex<Download>> {
    let st = Arc::new(Mutex::new(Download::default()));
    let st2 = st.clone();
    std::thread::spawn(move || {
        let mut last = String::new();
        for url in MODEL_URLS {
            match download(url, &model_path(), &st2, ctx.as_ref()) {
                Ok(()) => {
                    st2.lock().unwrap().done = true;
                    last.clear();
                    break;
                }
                Err(e) => {
                    crate::config::log(&format!("voice model download from {url}: {e}"));
                    last = e;
                }
            }
        }
        if !last.is_empty() {
            let mut s = st2.lock().unwrap();
            s.error = Some(last);
            s.done = true;
        }
        if let Some(c) = ctx {
            c.request_repaint();
        }
    });
    st
}

fn download(
    url: &str,
    dest: &Path,
    st: &Mutex<Download>,
    ctx: Option<&egui::Context>,
) -> Result<(), String> {
    let client = reqwest::blocking::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(600))
        .build()
        .map_err(|e| e.to_string())?;
    let mut resp = client
        .get(url)
        .send()
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?;
    let total = resp.content_length().unwrap_or(MODEL_SIZE).max(1);
    let part = dest.with_extension("part");
    let mut f = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    let mut last_paint = Instant::now();
    loop {
        let n = resp.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        got += n as u64;
        if got > MODEL_SIZE * 2 {
            return Err("download is larger than expected".into());
        }
        h.update(&buf[..n]);
        f.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        st.lock().unwrap().progress = (got as f32 / total as f32).min(1.0);
        if let Some(c) = ctx {
            if last_paint.elapsed().as_millis() > 100 {
                c.request_repaint();
                last_paint = Instant::now();
            }
        }
    }
    drop(f);
    let sum = hex(&h.finalize());
    if sum != MODEL_SHA256 {
        let _ = std::fs::remove_file(&part);
        return Err(format!("checksum mismatch (got {sum})"));
    }
    std::fs::rename(&part, dest).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- recording

/// A recording in progress. Dropping it discards the audio.
pub struct Recording {
    stop: Arc<AtomicBool>,
    discard: Arc<AtomicBool>,
    level: Arc<AtomicU32>,
    result: Receiver<Result<String, String>>,
    keep: bool,
}

impl Recording {
    /// Opens the microphone and starts the transcriber process.
    pub fn start(language: &str, ctx: egui::Context) -> Result<Self, String> {
        if !model_ready() {
            return Err("The voice model isn't downloaded yet.".into());
        }
        let engine = engine_path()?;
        let mut child = spawn_transcriber(&engine, &model_path(), language)?;
        let stop = Arc::new(AtomicBool::new(false));
        let discard = Arc::new(AtomicBool::new(false));
        let level = Arc::new(AtomicU32::new(0));
        let (tx, rx) = channel();
        let (ready_tx, ready_rx) = channel();
        let (s2, d2, l2) = (stop.clone(), discard.clone(), level.clone());
        std::thread::spawn(move || {
            let mut stdin = child.stdin.take();
            let mut sink = move |pcm: &[f32]| {
                if let Some(w) = stdin.as_mut() {
                    let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                    if w.write_all(&bytes).is_err() {
                        stdin = None;
                    }
                }
            };
            let cap = wasapi::capture(&s2, &l2, &mut sink, &ready_tx);
            drop(sink);
            let r = match cap {
                Err(e) => {
                    let _ = child.kill();
                    Err(e)
                }
                Ok(()) if d2.load(Ordering::Relaxed) => {
                    let _ = child.kill();
                    Err("discarded".into())
                }
                Ok(()) => finish_transcriber(child),
            };
            let _ = tx.send(r);
            ctx.request_repaint();
        });
        // Surface "no microphone" right away instead of after release.
        match ready_rx.recv_timeout(std::time::Duration::from_secs(3)) {
            Ok(Err(e)) => return Err(e),
            Ok(Ok(())) | Err(_) => {}
        }
        Ok(Self {
            stop,
            discard,
            level,
            result: rx,
            keep: false,
        })
    }

    /// Recent input level, 0..=1, for the meter.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    /// Stops recording; the transcript arrives on the returned channel.
    pub fn finish(mut self) -> Receiver<Result<String, String>> {
        self.stop.store(true, Ordering::Relaxed);
        self.keep = true;
        std::mem::replace(&mut self.result, channel().1)
    }
}

impl Drop for Recording {
    /// Dropping without `finish` (Esc, palette closed) throws the audio away.
    fn drop(&mut self) {
        if !self.keep {
            self.discard.store(true, Ordering::Relaxed);
        }
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn spawn_transcriber(
    engine: &Path,
    model: &Path,
    language: &str,
) -> Result<std::process::Child, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg("--stt")
        .arg(engine)
        .arg(model)
        .arg(if language.is_empty() { "-" } else { language })
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map_err(|e| format!("Couldn't start the transcriber: {e}"))
}

fn finish_transcriber(child: std::process::Child) -> Result<String, String> {
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(if err.is_empty() {
            format!("transcriber exited with {}", out.status)
        } else {
            err
        });
    }
    parse_transcript(&stdout)
}

/// Pulls `text` out of the engine's JSON result.
pub fn parse_transcript(json: &str) -> Result<String, String> {
    let v: serde_json::Value =
        serde_json::from_str(json.trim()).map_err(|e| format!("bad transcriber output: {e}"))?;
    Ok(v["text"].as_str().unwrap_or("").trim().to_string())
}

// ---------------------------------------------------------------- child process

/// `screenbuddy --stt <engine.dll> <model.cact> <lang|->`: loads the engine
/// and model, reads 16 kHz mono f32le PCM from stdin until EOF, prints the
/// engine's JSON result. Returns the process exit code.
pub fn stt_child() -> i32 {
    let args: Vec<String> = std::env::args().collect();
    let Some(i) = args.iter().position(|a| a == "--stt") else {
        return 2;
    };
    let (Some(dll), Some(model)) = (args.get(i + 1), args.get(i + 2)) else {
        eprintln!("usage: --stt <engine.dll> <model.cact> [lang]");
        return 2;
    };
    let lang = args.get(i + 3).filter(|l| l.as_str() != "-").cloned();
    match stt_run(
        Path::new(dll),
        Path::new(model),
        lang.as_deref(),
        &mut std::io::stdin().lock(),
    ) {
        Ok(json) => {
            println!("{json}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

fn stt_run(
    dll: &Path,
    model: &Path,
    lang: Option<&str>,
    input: &mut dyn Read,
) -> Result<String, String> {
    let engine = ffi::Needle::load(dll)?;
    let weights = std::fs::read(model).map_err(|e| format!("reading model: {e}"))?;
    engine.load_model(&weights)?;
    drop(weights);
    let mut raw = Vec::new();
    input.read_to_end(&mut raw).map_err(|e| e.to_string())?;
    let pcm: Vec<f32> = raw
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if pcm.len() < SAMPLE_RATE as usize / 5 {
        return Ok(r#"{"text":""}"#.into());
    }
    engine.transcribe(&pcm, lang)
}

mod ffi {
    //! The Needle C API, loaded at runtime with LoadLibrary (only in the
    //! `--stt` child process).
    use crate::util::wide;
    use std::ffi::{CStr, CString, c_char, c_int, c_ulonglong};
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
    use windows::core::{PCSTR, PCWSTR};

    type Load = unsafe extern "C" fn(*const u8, c_ulonglong) -> c_int;
    type LastError = unsafe extern "C" fn() -> *const c_char;
    type Transcribe = unsafe extern "C" fn(
        *const f32,
        c_int,
        *const c_char,
        *const c_char,
        c_int,
        *mut c_char,
        c_int,
    ) -> c_int;

    pub struct Needle {
        load: Load,
        last_error: LastError,
        transcribe: Transcribe,
        _lib: HMODULE,
    }

    impl Needle {
        pub fn load(dll: &std::path::Path) -> Result<Self, String> {
            let w = wide(&dll.to_string_lossy());
            unsafe {
                let lib = LoadLibraryW(PCWSTR(w.as_ptr()))
                    .map_err(|e| format!("loading speech engine: {e}"))?;
                let sym = |name: &[u8]| {
                    GetProcAddress(lib, PCSTR(name.as_ptr())).ok_or_else(|| {
                        format!(
                            "speech engine is missing {}",
                            String::from_utf8_lossy(&name[..name.len() - 1])
                        )
                    })
                };
                Ok(Self {
                    load: std::mem::transmute::<unsafe extern "system" fn() -> isize, Load>(sym(
                        b"needle_load\0",
                    )?),
                    last_error: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        LastError,
                    >(sym(b"needle_last_error\0")?),
                    transcribe: std::mem::transmute::<
                        unsafe extern "system" fn() -> isize,
                        Transcribe,
                    >(sym(b"needle_transcribe\0")?),
                    _lib: lib,
                })
            }
        }

        fn error(&self) -> String {
            unsafe {
                let p = (self.last_error)();
                if p.is_null() {
                    "unknown error".into()
                } else {
                    CStr::from_ptr(p).to_string_lossy().into_owned()
                }
            }
        }

        pub fn load_model(&self, bytes: &[u8]) -> Result<(), String> {
            let r = unsafe { (self.load)(bytes.as_ptr(), bytes.len() as c_ulonglong) };
            if r < 0 {
                Err(format!("loading voice model: {}", self.error()))
            } else {
                Ok(())
            }
        }

        pub fn transcribe(&self, pcm: &[f32], lang: Option<&str>) -> Result<String, String> {
            let lang = lang.and_then(|l| CString::new(l).ok());
            let mut out = vec![0 as c_char; 256 * 1024];
            let r = unsafe {
                (self.transcribe)(
                    pcm.as_ptr(),
                    pcm.len() as c_int,
                    lang.as_ref().map_or(std::ptr::null(), |l| l.as_ptr()),
                    std::ptr::null(),
                    0,
                    out.as_mut_ptr(),
                    out.len() as c_int,
                )
            };
            if r < 0 {
                return Err(format!("transcription failed: {}", self.error()));
            }
            Ok(unsafe { CStr::from_ptr(out.as_ptr()) }
                .to_string_lossy()
                .into_owned())
        }
    }
}

mod wasapi {
    //! Microphone capture through WASAPI shared mode, asking the audio engine
    //! to convert to 16 kHz mono float so no resampler is needed here.
    use super::SAMPLE_RATE;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::mpsc::Sender;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::System::Com::*;

    const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
    const MAX_SECONDS: usize = 120;

    pub fn capture(
        stop: &AtomicBool,
        level: &AtomicU32,
        sink: &mut dyn FnMut(&[f32]),
        ready: &Sender<Result<(), String>>,
    ) -> Result<(), String> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let r = run(stop, level, sink, ready);
            if let Err(e) = &r {
                let _ = ready.send(Err(e.clone()));
            }
            CoUninitialize();
            r
        }
    }

    unsafe fn run(
        stop: &AtomicBool,
        level: &AtomicU32,
        sink: &mut dyn FnMut(&[f32]),
        ready: &Sender<Result<(), String>>,
    ) -> Result<(), String> {
        let mic = |e: windows::core::Error| format!("Microphone unavailable ({})", e.message());
        unsafe {
            let en: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(mic)?;
            let dev = en
                .GetDefaultAudioEndpoint(eCapture, eConsole)
                .map_err(|_| "No microphone found.".to_string())?;
            let client: IAudioClient = dev.Activate(CLSCTX_ALL, None).map_err(mic)?;
            let fmt = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
                nChannels: 1,
                nSamplesPerSec: SAMPLE_RATE,
                nAvgBytesPerSec: SAMPLE_RATE * 4,
                nBlockAlign: 4,
                wBitsPerSample: 32,
                cbSize: 0,
            };
            client
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
                    10_000_000,
                    0,
                    &fmt,
                    None,
                )
                .map_err(mic)?;
            let cap: IAudioCaptureClient = client.GetService().map_err(mic)?;
            client.Start().map_err(mic)?;
            let _ = ready.send(Ok(()));
            let mut total = 0usize;
            let mut buf: Vec<f32> = Vec::new();
            while !stop.load(Ordering::Relaxed) && total < MAX_SECONDS * SAMPLE_RATE as usize {
                std::thread::sleep(std::time::Duration::from_millis(20));
                loop {
                    let n = cap.GetNextPacketSize().map_err(mic)?;
                    if n == 0 {
                        break;
                    }
                    let mut data = std::ptr::null_mut();
                    let mut frames = 0u32;
                    let mut flags = 0u32;
                    cap.GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                        .map_err(mic)?;
                    buf.clear();
                    if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                        buf.resize(frames as usize, 0.0);
                    } else {
                        buf.extend_from_slice(std::slice::from_raw_parts(
                            data as *const f32,
                            frames as usize,
                        ));
                    }
                    cap.ReleaseBuffer(frames).map_err(mic)?;
                    total += buf.len();
                    let rms =
                        (buf.iter().map(|s| s * s).sum::<f32>() / buf.len().max(1) as f32).sqrt();
                    level.store((rms * 4.0).min(1.0).to_bits(), Ordering::Relaxed);
                    sink(&buf);
                }
            }
            let _ = client.Stop();
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcript_json() {
        assert_eq!(
            parse_transcript(r#"{"text":" Turn off the lights. ","language":"en"}"#).unwrap(),
            "Turn off the lights."
        );
        assert_eq!(parse_transcript(r#"{"text":""}"#).unwrap(), "");
        assert!(parse_transcript("garbage").is_err());
    }

    #[test]
    fn languages_are_whistles() {
        assert_eq!(LANGUAGES.len(), 8);
        assert!(MODEL_URLS.iter().all(|u| u.ends_with(MODEL_FILE)));
        assert_eq!(MODEL_SHA256.len(), 64);
    }

    /// Network + disk: `cargo test -- --ignored download`.
    #[test]
    #[ignore]
    fn download_verifies_checksum() {
        let dest = std::env::temp_dir().join("sb-whistle-test.cact");
        let st = Mutex::new(Download::default());
        download(MODEL_URLS[1], &dest, &st, None).unwrap();
        assert_eq!(std::fs::metadata(&dest).unwrap().len(), MODEL_SIZE);
        assert!(st.lock().unwrap().progress > 0.99);
        let _ = std::fs::remove_file(dest);
    }

    /// Needs a microphone: `cargo test -- --ignored mic`.
    #[test]
    #[ignore]
    fn mic_captures_16k() {
        let stop = AtomicBool::new(false);
        let level = AtomicU32::new(0);
        let (tx, rx) = channel();
        let mut n = 0usize;
        let t0 = Instant::now();
        let mut sink = |pcm: &[f32]| {
            n += pcm.len();
            if t0.elapsed().as_millis() > 1000 {
                stop.store(true, Ordering::Relaxed);
            }
        };
        wasapi::capture(&stop, &level, &mut sink, &tx).unwrap();
        assert!(rx.recv().unwrap().is_ok());
        // About one second at 16 kHz.
        assert!((12_000..24_000).contains(&n), "{n} samples");
    }
}
