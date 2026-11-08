use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const HEADER: &str = "# screenbuddy settings. Edit here or in Settings (tray menu). Changes apply on the next hotkey press.\n\n";

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct Config {
    /// "anthropic" | "openai" | "claude-cli"
    pub provider: String,
    pub hotkey: String,
    pub region_hotkey: String,
    /// Save every session (screenshot + transcript) to the history folder.
    pub history: bool,
    /// Extra instructions appended to the built-in system prompt.
    pub extra_instructions: String,
    pub appearance: AppearanceCfg,
    pub drawing: DrawingCfg,
    pub vision: VisionCfg,
    pub anthropic: AnthropicCfg,
    pub openai: OpenAiCfg,
    pub claude_cli: ClaudeCliCfg,
    pub agent_desktop: AgentDesktopCfg,
    pub context: ContextCfg,
    pub voice: VoiceCfg,
    pub island: IslandCfg,
    pub input: InputCfg,
    pub agents: AgentsCfg,
    pub walk: WalkCfg,
}

/// Step-by-step walkthroughs ("show me how to ...").
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct WalkCfg {
    /// Let the assistant start a walkthrough when you ask how to do something.
    pub enabled: bool,
}

impl Default for WalkCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct IslandCfg {
    /// The pill with the buddy's face at the top of the screen.
    pub enabled: bool,
}

impl Default for IslandCfg {
    fn default() -> Self {
        Self { enabled: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ContextCfg {
    /// "Ask screenbuddy" in Explorer's right-click menu for files.
    pub explorer_menu: bool,
    /// Text selected in the app when the hotkey fires: "uia+copy" (UI Automation,
    /// else simulated Ctrl+C with the clipboard restored) | "uia" | "off".
    pub selection: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VoiceCfg {
    /// Push-to-talk with the on-device Whistle model (downloaded on first enable).
    pub enabled: bool,
    /// Hold to talk; a quick tap toggles recording instead.
    pub hotkey: String,
    /// Send the question as soon as the transcript arrives.
    pub auto_send: bool,
    /// "" = detect, or en | de | fr | es | it | nl | pl.
    pub language: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct InputCfg {
    /// Hold Alt alone to ask by voice; the answer appears beside the cursor.
    pub alt_hold: bool,
    /// How long Alt must be held before recording starts.
    pub alt_hold_ms: u32,
    /// Double-tap Alt to open the palette.
    pub alt_double_tap: bool,
}

impl Default for InputCfg {
    fn default() -> Self {
        Self { alt_hold: true, alt_hold_ms: 600, alt_double_tap: true }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AgentsCfg {
    /// Let the assistant hand multi-step work to background agents.
    pub enabled: bool,
    /// Agents act without asking, even to delete, send or pay.
    pub always_approve: bool,
    /// Claude CLI model for agents; empty = same as claude_cli.model.
    pub model: String,
    /// "ask": an agent may ask to use the real mouse and keyboard while the
    /// user watches; "never": agents only work in the background.
    pub drive: String,
}

impl Default for AgentsCfg {
    fn default() -> Self {
        Self { enabled: true, always_approve: false, model: String::new(), drive: "ask".into() }
    }
}

impl Default for ContextCfg {
    fn default() -> Self {
        Self { explorer_menu: false, selection: "uia+copy".into() }
    }
}

impl Default for VoiceCfg {
    fn default() -> Self {
        Self { enabled: false, hotkey: "Ctrl+Alt+Space".into(), auto_send: false, language: String::new() }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AppearanceCfg {
    /// Accent color for the palette (hex).
    pub accent: String,
    /// Palette width in points.
    pub width: f32,
    /// Answer text size in points.
    pub text_size: f32,
    /// "top" | "center" | "cursor" | "last" (where you last dragged it)
    pub position: String,
    /// Top-left of the palette in screen pixels, saved when you drag it.
    pub saved_pos: Option<[i32; 2]>,
    /// Close the palette when you click elsewhere (kept open while drawings show).
    pub dismiss_on_blur: bool,
    /// How much of the screen shows through the palette, 0 (solid) to 100.
    pub blur: f32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct DrawingCfg {
    /// Let the assistant point at things on screen.
    pub enabled: bool,
    /// Default annotation color (hex).
    pub color: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct VisionCfg {
    /// Send element names and bounds from the accessibility tree with the screenshot.
    pub ui_tree: bool,
    /// Let the assistant re-capture a region at full resolution.
    pub zoom: bool,
    /// Step-by-step guide mode: wait for you to act, then look again.
    pub guide: bool,
    /// Let the assistant search the web (Anthropic API and Claude CLI).
    pub web_search: bool,
}

impl Default for VisionCfg {
    fn default() -> Self {
        Self {
            ui_tree: true,
            zoom: true,
            guide: true,
            web_search: true,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AnthropicCfg {
    /// Empty = ANTHROPIC_API_KEY environment variable.
    pub api_key: String,
    pub model: String,
    /// low | medium | high | xhigh | max
    pub effort: String,
    pub max_tokens: u32,
    /// Retry declined requests on a fallback model (server-side).
    pub fallbacks: bool,
    pub base_url: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct OpenAiCfg {
    pub base_url: String,
    /// Empty = OPENAI_API_KEY environment variable (Ollama needs none).
    pub api_key: String,
    pub model: String,
    /// Turn off for models that reject tool calls.
    pub tools: bool,
    pub max_tokens: u32,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct AgentDesktopCfg {
    /// Let the assistant operate apps when you ask (https://github.com/vercel-labs/agent-desktop).
    pub enabled: bool,
    /// Path to agent-desktop.exe; empty = PATH or the global npm folder.
    pub command: String,
    /// "ask" (every action) | "once" (first action per conversation) | "never"
    pub approval: String,
    /// Show the agent's own cursor while it works.
    pub cursor: bool,
    pub cursor_fill: String,
    pub cursor_rim: String,
    pub cursor_accent: String,
    /// Show what the agent is doing next to its cursor.
    pub cursor_label: bool,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct ClaudeCliCfg {
    pub command: String,
    /// Model alias or id ("opus", "sonnet", ...); empty = CLI default.
    pub model: String,
    /// Keep one Claude process ready after closing (~100 MB while warm). 0 = off.
    pub keep_warm_minutes: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "anthropic".into(),
            hotkey: "Ctrl+Shift+Space".into(),
            region_hotkey: "Ctrl+Shift+R".into(),
            history: false,
            extra_instructions: String::new(),
            appearance: AppearanceCfg::default(),
            drawing: DrawingCfg::default(),
            vision: VisionCfg::default(),
            anthropic: AnthropicCfg::default(),
            openai: OpenAiCfg::default(),
            claude_cli: ClaudeCliCfg::default(),
            agent_desktop: AgentDesktopCfg::default(),
            context: ContextCfg::default(),
            voice: VoiceCfg::default(),
            island: IslandCfg::default(),
            input: InputCfg::default(),
            agents: AgentsCfg::default(),
            walk: WalkCfg::default(),
        }
    }
}

impl Default for AppearanceCfg {
    fn default() -> Self {
        Self {
            accent: "#0A84FF".into(),
            width: 560.0,
            text_size: 15.0,
            position: "top".into(),
            saved_pos: None,
            dismiss_on_blur: true,
            blur: 60.0,
        }
    }
}

impl Default for DrawingCfg {
    fn default() -> Self {
        Self { enabled: true, color: "#0A84FF".into() }
    }
}

impl Default for AnthropicCfg {
    fn default() -> Self {
        Self {
            api_key: String::new(),
            model: "claude-opus-5-5".into(),
            effort: "medium".into(),
            max_tokens: 32000,
            fallbacks: true,
            base_url: "https://api.anthropic.com".into(),
        }
    }
}

impl Default for OpenAiCfg {
    fn default() -> Self {
        Self {
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: String::new(),
            model: "anthropic/claude-opus-5-5".into(),
            tools: true,
            max_tokens: 8000,
        }
    }
}

impl Default for AgentDesktopCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
            approval: "once".into(),
            cursor: true,
            cursor_fill: "#FFFFFF".into(),
            cursor_rim: "#111318".into(),
            cursor_accent: "#0A84FF".into(),
            cursor_label: true,
        }
    }
}

impl Default for ClaudeCliCfg {
    fn default() -> Self {
        Self { command: "claude".into(), model: String::new(), keep_warm_minutes: 10 }
    }
}

impl Config {
    /// The agent-desktop binary, if enabled and installed.
    pub fn desktop_exe(&self) -> Option<std::path::PathBuf> {
        if self.agent_desktop.enabled {
            crate::desktop::resolve(&self.agent_desktop.command)
        } else {
            None
        }
    }

    pub fn provider_label(&self) -> String {
        match self.provider.as_str() {
            "openai" => format!("openai · {}", self.openai.model),
            "claude-cli" => {
                if self.claude_cli.model.is_empty() {
                    "claude subscription".into()
                } else {
                    format!("claude subscription · {}", self.claude_cli.model)
                }
            }
            _ => format!("anthropic · {}", self.anthropic.model),
        }
    }
}

/// Parses "#rrggbb" into RGB.
pub fn hex_rgb(s: &str) -> Option<[u8; 3]> {
    let h = s.trim().strip_prefix('#')?;
    if h.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(h, 16).ok()?;
    Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
}

pub fn rgb_hex(c: [u8; 3]) -> String {
    format!("#{:02X}{:02X}{:02X}", c[0], c[1], c[2])
}

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    let dir = base.join("screenbuddy");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn history_dir() -> PathBuf {
    let dir = data_dir().join("history");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// Loads the config, writing defaults on first run. Parse errors fall back to
/// defaults and are returned so the UI can surface them.
pub fn load() -> (Config, Option<String>) {
    let path = config_path();
    if !path.exists() {
        let _ = save(&Config::default());
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
        },
        Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
    }
}

pub fn save(cfg: &Config) -> std::io::Result<()> {
    let body = toml::to_string_pretty(cfg).map_err(std::io::Error::other)?;
    // Write-then-rename so a crash never leaves a half-written file.
    let path = config_path();
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, format!("{HEADER}{body}"))?;
    std::fs::rename(tmp, path)
}

pub fn log(msg: &str) {
    use std::io::Write;
    let path = data_dir().join("screenbuddy.log");
    if std::fs::metadata(&path).map(|m| m.len() > 1_000_000).unwrap_or(false) {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "[{}] {msg}", crate::util::timestamp());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let c = Config::default();
        let s = toml::to_string_pretty(&c).unwrap();
        assert_eq!(toml::from_str::<Config>(&s).unwrap(), c);
        assert_eq!(hex_rgb("#0A84FF"), Some([10, 132, 255]));
        assert_eq!(rgb_hex([10, 132, 255]), "#0A84FF");
    }
}
