# screenbuddy

Native Windows screen assistant in Rust. Press a hotkey, ask about what's on screen, get a streamed answer, and see arrows and boxes drawn directly over the thing it's talking about.

- **Idle:** the process sleeps in a Win32 message loop. Only the hotkey and tray are alive, the working set is trimmed to almost nothing, and CPU stays at ~0%.
- **Active:** the egui palette, the overlay and the renderer are created when you press the hotkey. Closing the palette destroys them and gives the memory back.
- **Single exe:** no webview and no runtime to install. The CRT is linked statically.

## Use

| Action | Default |
|---|---|
| Ask about the monitor under the cursor | `Ctrl+Shift+Space` |
| Drag-select a region, then ask | `Ctrl+Shift+R` (Esc or right-click cancels) |
| Send · newline · cancel/close | `Enter` · `Shift+Enter` · `Esc` |
| Browse / search history | `Ctrl+H` or the clock button in the palette |
| Push to talk (when Voice is on) | hold `Ctrl+Alt+Space`, or tap it / click the mic to start and stop |
| Close the palette | press the hotkey again, `Esc`, or `✕` |

The palette is a Spotlight-style bar, centered in the upper part of the screen. It grows into a card as the answer streams in. Clicking the screenshot thumbnail retakes the screenshot. Clicking elsewhere dismisses the palette, unless something is drawn on screen, so you can still click the thing it pointed at. The model only draws when pointing actually helps you find something. If the palette covers what it drew, the palette moves out of the way. Follow-up questions keep the conversation going.

The tray menu has: edit settings, reload settings, browse history, open the history folder, start with Windows, and quit.

## History

With **Save conversations** on (Settings → General), every chat is saved to `%APPDATA%\screenbuddy\history\<time>\` as `chat.json`, `chat.md` and `screenshot.jpg`. `Ctrl+H` (or the clock button, the tray's **Browse history…**, or Settings → General → **Browse history**) turns the palette's input into a search box over past chats: every word must appear somewhere in the questions or answers. `↑`/`↓`/`Enter` or a click opens one, so you can read it, and the next question continues it: the earlier turns go to the model as text, together with a fresh screenshot, so this works for every provider. Follow-ups are saved back into the same chat. There's no index or background process: the folder is scanned when the list opens, and everything is dropped when the palette closes.

## Context

- **Selected text:** if text is selected in the app you're in when you press the hotkey, it shows up as a chip under the bar and goes with your question. screenbuddy asks UI Automation for the focused element's selection first. If that element has no text pattern (and Settings → General → Selected text is on "+ Copy fallback"), it sends Ctrl+C, reads the text, and puts back everything that was on your clipboard. The restored copy is kept out of clipboard history. Apps that do expose a text pattern never get a keystroke, so terminals don't receive a stray Ctrl+C.
- **Explorer:** Settings → General → **Explorer menu** adds "Ask screenbuddy" to the right-click menu for files (`HKCU\Software\Classes\*\shell\screenbuddy`; on Windows 11 it's under "Show more options"). Turning it off deletes the key. The file opens in the palette as a chip: images are sent as an image (re-encoded like screenshots), text files as text (first 100 KB), and anything else as its path and size. Right-clicking more files while the palette is open adds them to it. `screenbuddy.exe --open-file <path>` does the same from a script.

Chips can be removed with their ✕. They go to all three providers: text is put in front of the question, images are sent after the screenshot.

## Voice

Settings → **Voice** turns on push-to-talk with [Whistle](https://huggingface.co/Cactus-Compute/whistle), Cactus Compute's 16.9 MB on-device speech-to-text model (Apache-2.0; English, German, French, Spanish, Italian, Dutch, Polish). Hold the shortcut, or click the mic in the palette, then speak. When you let go, the transcript goes into the input, or gets sent right away if **Send automatically** is on.

- **Model:** not part of the exe. It's downloaded the first time you turn voice on, to `%APPDATA%\screenbuddy\models\whistle.cact`, pinned to a Hugging Face revision and checked against its SHA-256. If the official repo fails, the [mirror](https://huggingface.co/machineandfolk/whistle-mirror) is used.
- **Engine:** Whistle runs on Cactus's Needle engine. (Cactus's main `cactus` C++ engine is ARM-NEON only and doesn't build for x64 Windows.) Needle ships a prebuilt `windows-x86_64/libneedle.a` built with clang and libc++. `scripts/build-needle.sh` links it into `.needle/needle.dll` with llvm-mingw (libc++ static, so it only imports KERNEL32 and the UCRT), and `build.rs` embeds that DLL in the exe. Without it the app still builds, and voice says the engine is missing.
- **Memory:** nothing is loaded while idle. Recording opens the default microphone through WASAPI in shared mode, and Windows converts the audio to 16 kHz mono float. At the same moment a child process (`screenbuddy.exe --stt`) loads the engine (with `LoadLibrary`/FFI) and the model while you talk, receives the audio over a pipe, runs `needle_transcribe` once you stop, prints the result and exits. A 5-second clip takes about 0.1–0.2 s.


## Doing things for you (agent-desktop)

If [agent-desktop](https://github.com/vercel-labs/agent-desktop) is installed, just ask screenbuddy to do something: "open Notepad and type hi", "turn on dark mode", "reply 'on my way' in Slack". There's an **Install** button in Settings → Agent (it runs `npm install -g agent-desktop`). It works through the accessibility tree, not pixels. Reading apps (`snapshot`, `list-apps`, …) never asks.

Permission for actions (Settings → Agent):

- **Ask once per chat** (default): approve the first action, and the rest of that conversation runs on its own.
- **Ask every time**: one Allow / Don't Allow card per click or keystroke.
- **Never ask**: acts right away. "Always allow" on the approval card switches to this.

While it acts, the agent has its own cursor (agent-desktop's cursor overlay) with a short label saying what it's doing. You can pick the body, outline and highlight colors, or turn it off. Each conversation runs in its own agent-desktop session.

## Settings

Tray → **Settings…**, or the ⚙ in the palette. Sections: General (shortcuts with press-to-record, open at login, history, context), Model (provider, keys, model, effort, warm-process time), Appearance (accent, position, width, text size, close on blur), Drawing (on/off, color), Voice (push to talk, model), Agent (install, permission, cursor), Advanced (custom instructions, config file, log, reset). Changes save immediately.

Everything is stored in `%APPDATA%\screenbuddy\config.toml`, which you can also edit by hand. It's re-read every time you press the hotkey.

## Providers

All three providers use the same streaming parser shape and the same `draw` tool.

- **Anthropic API:** `POST /v1/messages` with SSE streaming, the screenshot as a base64 JPEG, adaptive thinking, and `effort`. The `draw` tool uses eager input streaming, so shapes appear as soon as each tool call finishes. The thinking blocks the API returns are sent back unchanged in follow-up turns.
- **OpenAI-compatible:** `POST /chat/completions` with SSE streaming, the image as an `image_url` data URL, and `draw` as a function tool.
- **Claude subscription:** runs `claude -p --output-format stream-json --input-format stream-json` for each question and passes the image as a content block in stream-json. Follow-ups use `--resume <session>`. `draw` is offered through `--mcp-config` pointing at this exe in `--mcp` stdio mode. The CLI (Node) only uses memory while it's answering. That's the cost of using a subscription with nothing running while idle.

## Overlay

The overlay is one layered, click-through, topmost tool window (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`). It's sized to the bounding box of the shapes, rasterized with tiny-skia, and pushed with `UpdateLayeredWindow`. There's no GPU and no redraw loop: it only repaints when a new shape arrives.

Labels use Segoe UI through fontdue. The process is per-monitor-DPI-aware v2, so coordinates are physical pixels.

The screenshot is downscaled to ≤1568 px on the long edge and ≤1.15 MP, then encoded as JPEG q80 in memory. Because the image is already within the API's limits, the model sees exactly the pixels that were sent, so the coordinates it returns map back to the screen exactly. Nothing is written to disk unless `history = true`.

## Debug

- `screenbuddy.exe --ask "question" [--provider claude-cli]` runs headless: it captures the screen, streams the answer and prints any draw calls to stdout.
- `SCREENBUDDY_ALLOW_CAPTURE=1` lets screenshot tools capture the palette and overlay. Normally they're hidden from capture with `WDA_EXCLUDEFROMCAPTURE`.
- The log is at `%APPDATA%\screenbuddy\screenbuddy.log`.
- `screenbuddy.exe --stt <needle.dll> <whistle.cact> [lang|-] < audio.f32` transcribes raw 16 kHz mono f32le PCM from stdin and prints the engine's JSON.

## Build / release

```
bash scripts/build-needle.sh   # once: speech engine -> .needle/needle.dll (downloads llvm-mingw; LLVM_MINGW=<dir> reuses one)
cargo build --release      # target/release/screenbuddy.exe
cargo test
git tag v0.1.0 && git push origin v0.1.0   # CI builds and publishes the GitHub release
```

Third-party: Cactus Needle engine and the Whistle model, Apache-2.0 (Cactus Compute).
