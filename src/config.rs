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
    pub anthropic: AnthropicCfg,
    pub openai: OpenAiCfg,
    pub claude_cli: ClaudeCliCfg,
    pub agent_desktop: AgentDesktopCfg,
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
    /// "top" | "center" | "cursor"
    pub position: String,
    /// Close the palette when you click elsewhere (kept open while drawings show).
    pub dismiss_on_blur: bool,
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
            anthropic: AnthropicCfg::default(),
            openai: OpenAiCfg::default(),
            claude_cli: ClaudeCliCfg::default(),
            agent_desktop: AgentDesktopCfg::default(),
        }
    }
}

impl Default for AppearanceCfg {
    fn default() -> Self {
        Self { accent: "#0A84FF".into(), width: 680.0, text_size: 15.0, position: "top".into(), dismiss_on_blur: true }
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
