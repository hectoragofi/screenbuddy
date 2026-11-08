# screenbuddy v1: the buddy

Direction (8 Nov 2026): screenbuddy becomes an ambient AI buddy in the spirit of heyclicky.com, but
open-source-style (no accounts, billing, limits), private repo for now, and deeply integrated with
Apeiron (the owner's infinite-canvas window manager, `Documents\code\infinite-canvas`).

It should feel like a hardware object from Teenage Engineering / Nothing / Apple: calm, precise,
one accent colour, dot-matrix character, springy interruptible motion. Never copy Clicky's art,
name or wording; this is our own thing.

**Not** full text-to-speech. Replies are text: captions beside the cursor, cards, the palette.

## Pieces

| # | Piece | Module(s) | Wave |
|---|---|---|---|
| 1 | Design tokens + fonts | `src/ui/tokens.rs`, `assets/fonts/` | 1 |
| 2 | Island (top-centre pill, buddy face, peek) | `src/island.rs`, `src/buddy.rs` | 1 |
| 3 | Alt-hold voice + double-tap Alt text, captions | `src/keys.rs`, `src/caption.rs`, `src/quick.rs` | 1 |
| 4 | Agents backend (router, runner, store, approvals policy) | `src/agents/*` | 1 |
| 5 | Apeiron IPC server | Apeiron repo `src/ipc.rs` | 1 |
| 6 | Apeiron client + capture fallback + drawings follow camera | `src/apeiron.rs`, `src/capture.rs`, `src/overlay.rs` | 1 |
| 7 | Agent cards hanging from the island, follow-up, retry, files | `src/cards.rs` | 2 |
| 8 | Agent desk on the canvas, fly-to from cards | `src/agents/desk.rs` | 2 |
| 9 | Approval cards, always-approve, drive mode | `src/cards.rs`, `src/desktop.rs` | 2 |
| 10 | Walkthroughs (step-by-step, waits for click) | `src/walk.rs` | 2 |
| 11 | Searchable agent history in palette/Home | `src/palette.rs` | 2 |

## Shared rules for every slice

- Rust only, no new runtimes. Idle memory matters (target < 50 MB active, ~0 idle extra). New
  always-on windows are **raw Win32 layered windows rendered with tiny-skia + `UpdateLayeredWindow`
  on their own thread** (same technique as `src/overlay.rs`), NOT eframe: the main thread runs the
  palette modally with `eframe::run_native`, so anything persistent must live off it.
- All screenbuddy window classes are named `screenbuddy.<thing>` (e.g. `screenbuddy.island`).
  Apeiron must never adopt them. Use `WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_LAYERED`
  and `WDA_EXCLUDEFROMCAPTURE` (respect `SCREENBUDDY_ALLOW_CAPTURE=1`).
- Per-monitor DPI aware; all sizes below are logical px × monitor scale.
- Touch shared files minimally: `main.rs` (mod line + event arm), `config.rs` (one new section
  struct with `#[serde(default)]`), `settings.rs` (append rows to a page; don't restructure).
  New `AppEvent` variants go at the end of the enum.
- Never run bare `cargo fmt` (repo isn't fmt-clean). Format only files you created
  (`rustfmt --edition 2024 src/x.rs`).
- Commits: normal conventional messages, no Co-Authored-By trailer. Push the branch after every
  commit. Open a PR into `main` when done; no "Generated with Claude Code" footer.
- `cargo build --release` and `cargo test` must pass. Don't kill or replace the installed
  `%LOCALAPPDATA%\Programs\screenbuddy\screenbuddy.exe`.

## 1. Design tokens (`src/ui/tokens.rs`)

```
INK      #0E0E0E   surfaces (dark, default)
INK_2    #1A1A19   raised surface
PAPER    #F2F0EB   text on ink / light surface
MUTE     #8A8780   secondary text, idle dots
LINE     #FFFFFF @ 8%  hairlines
SIGNAL   #FF5A1F   the one accent: listening, working, live
ASK      #4C82FF   approvals / questions (calm blue)
OK       #3DDC84   done dot
ERR      #FF3B30
```
Type: **Geist Mono** (OFL) for all UI text; labels 10.5 px ALL CAPS +6% tracking; body 12.5 px.
**Doto** (OFL, Google Fonts, dot-matrix) for readouts/numbers (`03 AGENTS`, `00:41`). Fonts go in
`assets/fonts/` with their OFL licence files, embedded via `include_bytes!`, rendered with fontdue
(already a dep). Radii: pill = full, card 16, chip 8. Shadow: 0 8 24 rgba(0,0,0,.35).
Motion: one spring helper `Spring { stiffness: 380.0, damping: 30.0 }` (critically damped-ish),
retargetable mid-flight, 120 Hz-friendly tick (dt-based, never frame-count based). Nothing pops.

## 2. Island (`src/island.rs`, `src/buddy.rs`)

Top-centre of the monitor under the cursor, 8 px from the top edge.
- **Rest:** pill 36 px tall, ~120 px wide: buddy face left, Doto readout right (agent count with
  something new, or nothing). Below 40% opacity when idle for 10 s; full on hover.
- **Peek (hover 120 ms):** grows into a 360 px wide panel: running/recent agents (name, one-line
  status, age), each row clickable (emits `IslandEvent::OpenAgent(id)`), plus "Ask…" row.
- **Click on pill:** opens the palette (`AppEvent::IslandClick`).
- **Buddy face:** our own 9×7 dot-matrix glyph. States: `Idle` (slow breathe, blink every 4–7 s),
  `Listening(level)` (dots become a level meter, SIGNAL), `Thinking` (dot orbit), `Streaming`
  (ripple), `Done` (happy, OK flash), `Error`, `Working(n)` (small SIGNAL dot badge).
- API: `island::start() -> IslandHandle` (spawns its thread), `IslandHandle::set_state(BuddyState)`,
  `IslandHandle::set_agents(Vec<AgentRow>)`, `IslandHandle::anchor() -> RECT` (screen rect of the
  pill, so cards can hang from it). Thread-safe (channel + `PostThreadMessage` wake).
- Idle CPU ~0: no timers while nothing animates.

## 3. Input + captions (`src/keys.rs`, `src/caption.rs`, `src/quick.rs`)

- `WH_KEYBOARD_LL` hook thread. **Hold Alt alone** ≥ `input.alt_hold_ms` (default 600) → start
  push-to-talk (reuse `voice.rs`); release → transcribe → quick ask. Cancel (do nothing) if any other
  key goes down while Alt is held, if a mouse button is down (Apeiron uses Alt for free-drag), or if
  Apeiron reports a gesture. On release after a hold, inject a dummy key (VK 0xE8, tagged with our
  `dwExtraInfo`) so the target app doesn't open its menu bar. Ignore injected events.
- **Double-tap Alt** (two lone taps < 300 ms) → open the palette with text focus.
- Setting `input.alt_hold` (bool, default true) and `input.alt_hold_ms`; existing hotkeys stay.
- **Quick ask** (`quick.rs`): screenshot + transcript → same brain as the palette (reuse `vision.rs`
  / `llm.rs`) → the streamed answer appears as a **caption** beside the cursor (`caption.rs`:
  layered window, max 380 px wide, markdown-lite, fades 6 s after done, hover keeps it), drawings go
  to the overlay as today. Buddy state follows (Listening → Thinking → Streaming → Done).
  "Open in palette" affordance on the caption (click) continues the conversation there.
- Holding Alt while the cursor is over an agent card = follow-up to that agent (wave 2 wires it;
  keys.rs exposes `keys::cursor_target()` hook point / event with cursor position).

## 4. Agents backend (`src/agents/`)

- **Router:** the brain (palette + quick ask) gets a tool `spawn_agent { name, brief, needs_screen }`.
  System prompt: spawn only when the user clearly asks for work that takes more than one step;
  if unsure, offer instead of spawning; never spawn when the user says wait.
- **Runner:** each agent = its own `claude -p --output-format stream-json --verbose` child (inside
  the job object), with the screenbuddy MCP server (`screenbuddy --mcp`: draw + desktop tools) and
  web tools. Working dir `%APPDATA%\screenbuddy\agents\<id>\` (files it makes land there). Parse the
  stream into `AgentEvent { Plan(Vec<String>), Step(String), Ask(Question), Result(String), Files(Vec<PathBuf>), Failed(String), Done }`.
  For non-CLI providers, run an equivalent tool loop via `llm.rs`.
- **Lifecycle:** `Pending` (5 s cancel window) → `Running` → `Done | Failed | Stopped`. Stopping
  within 10 s of start marks it "didn't count" (cosmetic; no limits exist). Retry = rerun brief.
  Follow-up = resume the same session (`--resume <session_id>`), only latest reply's files shown.
  Several agents in parallel.
- **Store:** `agents/<id>/agent.json` (name, brief, status, timestamps, session id, transcript
  summary, files). `agents::list()`, `agents::search(q)`.
- **API:** `agents::spawn(brief) -> AgentId`, `cancel`, `stop`, `retry`, `follow_up(id, text)`,
  `subscribe() -> Receiver<(AgentId, AgentEvent)>` (multi-subscriber), `list()`.
- **Approval policy** (`desktop.rs`): actions run immediately; only **destructive / outbound /
  money** (delete, close-without-save, send, post, submit payment, purchase) need approval. The
  approval request is an `AgentEvent::Ask` with kind `Approval`; settings `agents.always_approve`.
  A yes covers that one agent's conversation only.

## 5. Apeiron IPC (Apeiron repo)

Named pipe `\\.\pipe\apeiron-v1`, newline-delimited JSON, multiple clients, never blocks the UI
thread. Coordinates: physical screen px; canvas units = physical px at zoom 1.

Requests `{"id":1,"op":"...", ...}` → replies `{"id":1,"ok":true,"data":{...}}` or `{"id":1,"ok":false,"error":"..."}`:
- `hello` → `{version:1, canvas:bool}` (`canvas:false` in tabs-only mode)
- `get_layout` → `{camera:{x,y,zoom}, stage:bool, moving:bool, windows:[{hwnd, title, exe, canvas_rect:[x,y,w,h], screen_rect:[x,y,w,h]|null, on_screen:bool, tab_hidden:bool, away:bool}]}`
- `focus_window {hwnd}` → fly camera, land at 1:1, activate; replies once settled
- `leave_stage` → drop overview to 1:1; replies once settled
- `fly_to {canvas_rect}` / `back` (return camera to where it was before the last fly)
- `reserve_region {name, w, h}` → `{canvas_rect}`; stable per name (persisted with layout), placed
  in free canvas space away from the user's windows
- `place_window {hwnd, region}` → put that window inside the region without moving the camera
- `subscribe` → server then pushes events on this connection:
  `{"event":"camera","x","y","zoom","moving"}`, `{"event":"stage","open"}`,
  `{"event":"window_moved","hwnd","canvas_rect","screen_rect"}`, `{"event":"window_closed","hwnd"}`,
  `{"event":"focus","hwnd"}`, `{"event":"tab_switch","group","hwnd"}`, `{"event":"gesture","active"}`.
  Camera events are coalesced to ≤ 60/s.
- Apeiron never adopts windows whose class name starts with `screenbuddy.`.

## 6. Apeiron client (`src/apeiron.rs`)

- Background thread connects (retry every 5 s if Apeiron absent; everything works without it).
  Holds latest layout/camera/stage, exposes `apeiron::get()` snapshot + request helpers
  (`focus_window`, `leave_stage`, `fly_to`, `back`, `reserve_region`, `place_window`).
- **Capture:** before a screenshot, if stage is open → `leave_stage` first (or capture target
  window directly). For a window that is off-screen / tab-hidden, capture it with Windows Graphics
  Capture per-window (fallback `PrintWindow(PW_RENDERFULLCONTENT)`).
- **Drawings follow the camera:** overlay shapes anchored to a window (by hwnd at draw time) are
  hidden while `moving`, and re-projected from the window's new `screen_rect` when it settles; shapes
  on windows that went off-screen are dropped.
- **Pointing at off-screen windows:** if a draw targets a window not on screen, `focus_window` first.
- Alt-hold suppression when Apeiron reports `gesture active`.
