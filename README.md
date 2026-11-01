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

The palette is a Spotlight-style bar, centered in the upper part of the screen. It grows into a card as the answer streams in. Clicking the screenshot thumbnail retakes the screenshot. Clicking elsewhere dismisses the palette, unless something is drawn on screen, so you can still click the thing it pointed at. The model only draws when pointing actually helps you find something. If the palette covers what it drew, the palette moves out of the way. Follow-up questions keep the conversation going.

The tray menu has: edit settings, reload settings, open the history folder, start with Windows, and quit.

## Seeing more than one screenshot

- **UI elements:** with the screenshot, screenbuddy sends a compact list of the elements in the foreground window and the window under the cursor, read from UI Automation: `e7 button "Save" 412,88,64,24` (bounds in screenshot pixels, `*` marks keyboard focus). The walk is breadth-first under a hard 150 ms budget, capped in depth and count, and only named or focusable elements are listed (at most 120). A `draw` shape can say `"element": "e7"` instead of coordinates, and the box snaps to the element's real bounds. Pixel coordinates still work.
- **Zoom:** the model can call `zoom(x, y, w, h)` on a region of the screenshot. screenbuddy re-captures that area at native screen resolution (downscaled only if it exceeds the image limits) and returns it as an image tool result with an id such as `z1`. Shapes drawn with `"zoom": "z1"` use that image's pixels and are mapped back to the screen. Shapes without it keep using full-screenshot pixels.
- **Guide mode:** ask "walk me through …". The model points at one step and calls `watch`. That waits until the screen changes (a 256 px grayscale thumbnail of the work area, compared every 500 ms and debounced until it settles) or keyboard focus moves (UIA). Then it clears the old pointer, re-captures the same area (same coordinate space) with a fresh element list, and the model continues. The palette shows "◉ Watching for your next step · Esc stops" in the accent color. Esc stops it, even while your app has focus, and so does closing the palette. While waiting, the thread sleeps in 100 ms ticks and the palette repaints at 4 fps.

All three are in Settings → Drawing → Seeing your screen (`[vision]` `ui_tree`, `zoom`, `guide` in config.toml, all on by default). For the Claude subscription provider, `screenbuddy --mcp` lists `zoom` and `watch` and forwards them to the parent process over a loopback socket with a per-process token, so captures and coordinate bookkeeping live in one place.

## Doing things for you (agent-desktop)

If [agent-desktop](https://github.com/vercel-labs/agent-desktop) is installed, just ask screenbuddy to do something: "open Notepad and type hi", "turn on dark mode", "reply 'on my way' in Slack". There's an **Install** button in Settings → Agent (it runs `npm install -g agent-desktop`). It works through the accessibility tree, not pixels. Reading apps (`snapshot`, `list-apps`, …) never asks.

Permission for actions (Settings → Agent):

- **Ask once per chat** (default): approve the first action, and the rest of that conversation runs on its own.
- **Ask every time**: one Allow / Don't Allow card per click or keystroke.
- **Never ask**: acts right away. "Always allow" on the approval card switches to this.

While it acts, the agent has its own cursor (agent-desktop's cursor overlay) with a short label saying what it's doing. You can pick the body, outline and highlight colors, or turn it off. Each conversation runs in its own agent-desktop session.

## Settings

Tray → **Settings…**, or the ⚙ in the palette. Sections: General (shortcuts with press-to-record, open at login, history), Model (provider, keys, model, effort, warm-process time), Appearance (accent, position, width, text size, close on blur), Drawing (on/off, color), Agent (install, permission, cursor), Advanced (custom instructions, config file, log, reset). Changes save immediately.

Everything is stored in `%APPDATA%\screenbuddy\config.toml`, which you can also edit by hand. It's re-read every time you press the hotkey.

## Providers

All three providers use the same streaming parser shape and the same `draw`, `zoom` and `watch` tools. Image tool results go to Anthropic as `tool_result` image blocks, to OpenAI-compatible servers as a user message with the image right after the tool messages, and to the Claude CLI as MCP image content.

- **Anthropic API:** `POST /v1/messages` with SSE streaming, the screenshot as a base64 JPEG, adaptive thinking, and `effort`. The `draw` tool uses eager input streaming, so shapes appear as soon as each tool call finishes. The thinking blocks the API returns are sent back unchanged in follow-up turns.
- **OpenAI-compatible:** `POST /chat/completions` with SSE streaming, the image as an `image_url` data URL, and `draw` as a function tool.
- **Claude subscription:** runs `claude -p --output-format stream-json --input-format stream-json` for each question and passes the image as a content block in stream-json. Follow-ups use `--resume <session>`. `draw` is offered through `--mcp-config` pointing at this exe in `--mcp` stdio mode. The CLI (Node) only uses memory while it's answering. That's the cost of using a subscription with nothing running while idle.

## Overlay

The overlay is one layered, click-through, topmost tool window (`WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW`). It's sized to the bounding box of the shapes, rasterized with tiny-skia, and pushed with `UpdateLayeredWindow`. There's no GPU and no redraw loop: it only repaints when a new shape arrives.

Labels use Segoe UI through fontdue. The process is per-monitor-DPI-aware v2, so coordinates are physical pixels.

The screenshot is downscaled to ≤1568 px on the long edge and ≤1.15 MP, then encoded as JPEG q80 in memory. Because the image is already within the API's limits, the model sees exactly the pixels that were sent, so the coordinates it returns map back to the screen exactly. Nothing is written to disk unless `history = true`.

## Debug

- `screenbuddy.exe --ask "question" [--provider claude-cli]` runs headless: it captures the screen, streams the answer and prints any draw calls to stdout.
- Add `--uia` to print the UI element list that would be sent. `SCREENBUDDY_UIA_MS=<ms>` overrides the snapshot budget and logs the walk to stderr.
- `SCREENBUDDY_ALLOW_CAPTURE=1` lets screenshot tools capture the palette and overlay. Normally they're hidden from capture with `WDA_EXCLUDEFROMCAPTURE`.
- The log is at `%APPDATA%\screenbuddy\screenbuddy.log`.

## Build / release

```
cargo build --release      # target/release/screenbuddy.exe
cargo test
git tag v0.1.0 && git push origin v0.1.0   # CI builds and publishes the GitHub release
```
