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
| Close the palette | press the hotkey again, `Esc`, or `✕` |

In the palette header, `⟳` takes a new screenshot (the palette and the drawings are left out of it) and `⌫` clears the drawings. If the palette covers what the model drew, it moves to a corner out of the way. Follow-up questions keep the conversation going.

The tray menu has: edit settings, reload settings, open the history folder, start with Windows, and quit.

## Settings

`%APPDATA%\screenbuddy\config.toml` is created on first run. It's re-read every time you press the hotkey, so there's nothing to restart.

```toml
provider = "anthropic"            # anthropic | openai | claude-cli
hotkey = "Ctrl+Shift+Space"
region_hotkey = "Ctrl+Shift+R"
history = false                   # save screenshot + chat.md per session

[anthropic]                       # api_key or ANTHROPIC_API_KEY
model = "claude-opus-5-5"
effort = "medium"
fallbacks = true                  # server-side refusal fallback (beta)

[openai]                          # any OpenAI-compatible endpoint
base_url = "https://openrouter.ai/api/v1"   # or DeepSeek, Ollama http://localhost:11434/v1
model = "anthropic/claude-opus-5-5"
tools = true                      # set false for models without tool calls

[claude_cli]                      # Claude subscription via Claude Code
command = "claude"
model = ""                        # e.g. "opus"
```

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

## Build / release

```
cargo build --release      # target/release/screenbuddy.exe
cargo test
git tag v0.1.0 && git push origin v0.1.0   # CI builds and publishes the GitHub release
```
