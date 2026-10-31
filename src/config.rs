use serde::Deserialize;
use std::path::PathBuf;

pub const DEFAULT_CONFIG: &str = r#"# screenbuddy settings. Saved changes apply on the next hotkey press.

# Which backend answers: "anthropic" | "openai" | "claude-cli"
provider = "anthropic"

# Global hotkeys. Modifiers: Ctrl, Alt, Shift, Win. Keys: A-Z, 0-9, F1-F24, Space, etc.
hotkey = "Ctrl+Shift+Space"
region_hotkey = "Ctrl+Shift+R"

# Save every session (screenshot + transcript) under %APPDATA%\screenbuddy\history
history = false

# Optional extra instructions appended to the built-in system prompt.
extra_instructions = ""

[anthropic]
# Leave empty to use the ANTHROPIC_API_KEY environment variable.
api_key = ""
model = "claude-opus-5-5"
# low | medium | high | xhigh | max
effort = "medium"
max_tokens = 32000
# If the model declines a request, let the API retry it on a fallback model.
fallbacks = true
base_url = "https://api.anthropic.com"

[openai]
# Any OpenAI-compatible endpoint: OpenRouter, DeepSeek, Ollama (http://localhost:11434/v1), ...
base_url = "https://openrouter.ai/api/v1"
# Leave empty to use the OPENAI_API_KEY environment variable (Ollama needs none).
api_key = ""
model = "anthropic/claude-opus-5-5"
# Turn off for models that reject tool calls; disables screen annotations.
tools = true
max_tokens = 8000

[agent_desktop]
# Let screenbuddy operate apps when you ask it to (https://github.com/vercel-labs/agent-desktop).
# Install: npm install -g agent-desktop. Every action asks for your approval first.
enabled = true
# Path to agent-desktop.exe. Empty = find it on PATH or in the global npm folder.
command = ""

[claude_cli]
# Uses your Claude subscription through the Claude Code CLI.
command = "claude"
# Optional model alias or id, e.g. "opus" or "sonnet". Empty = CLI default.
model = ""
"#;

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct Config {
    pub provider: String,
    pub hotkey: String,
    pub region_hotkey: String,
    pub history: bool,
    pub extra_instructions: String,
    pub anthropic: AnthropicCfg,
    pub openai: OpenAiCfg,
    pub claude_cli: ClaudeCliCfg,
    pub agent_desktop: AgentDesktopCfg,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AnthropicCfg {
    pub api_key: String,
    pub model: String,
    pub effort: String,
    pub max_tokens: u32,
    pub fallbacks: bool,
    pub base_url: String,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct OpenAiCfg {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub tools: bool,
    pub max_tokens: u32,
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AgentDesktopCfg {
    pub enabled: bool,
    pub command: String,
}

impl Default for AgentDesktopCfg {
    fn default() -> Self {
        Self {
            enabled: true,
            command: String::new(),
        }
    }
}

#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ClaudeCliCfg {
    pub command: String,
    pub model: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider: "anthropic".into(),
            hotkey: "Ctrl+Shift+Space".into(),
            region_hotkey: "Ctrl+Shift+R".into(),
            history: false,
            extra_instructions: String::new(),
            anthropic: AnthropicCfg::default(),
            openai: OpenAiCfg::default(),
            claude_cli: ClaudeCliCfg::default(),
            agent_desktop: AgentDesktopCfg::default(),
        }
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

impl Default for ClaudeCliCfg {
    fn default() -> Self {
        Self {
            command: "claude".into(),
            model: String::new(),
        }
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

pub fn data_dir() -> PathBuf {
    let base = std::env::var_os("APPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
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

/// Loads the config, writing the default file on first run. Parse errors fall
/// back to defaults and are returned so the UI can surface them.
pub fn load() -> (Config, Option<String>) {
    let path = config_path();
    if !path.exists() {
        let _ = std::fs::write(&path, DEFAULT_CONFIG);
    }
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => (cfg, None),
            Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
        },
        Err(e) => (Config::default(), Some(format!("config.toml: {e}"))),
    }
}

pub fn log(msg: &str) {
    use std::io::Write;
    let path = data_dir().join("screenbuddy.log");
    // Keep the log small: truncate when it grows past 1 MB.
    if std::fs::metadata(&path)
        .map(|m| m.len() > 1_000_000)
        .unwrap_or(false)
    {
        let _ = std::fs::remove_file(&path);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(f, "[{}] {msg}", crate::util::timestamp());
    }
}
