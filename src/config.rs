//! Configuration loading with layered overrides: file < environment < CLI.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

const DEFAULT_CONFIG_PATH: &str = "config.toml";

/// Fully resolved agent configuration.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct Config {
    pub agent: AgentConfig,
    pub llm: LlmConfig,
    pub sandbox: SandboxConfig,
    pub logging: LoggingConfig,
}

impl Config {
    /// Load configuration from `path`. A missing file is only tolerated when
    /// `path` is the built-in default path; an explicitly requested path must
    /// exist.
    pub fn load(path: &Path) -> Result<Config> {
        let mut config = match fs::read_to_string(path) {
            Ok(contents) => {
                let parsed: Config = toml::from_str(&contents).map_err(|e| {
                    Error::InvalidConfig(format!("failed to parse {}: {e}", path.display()))
                })?;
                parsed
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::NotFound
                    && path == Path::new(DEFAULT_CONFIG_PATH) =>
            {
                Config::default()
            }
            Err(e) => {
                return Err(Error::InvalidConfig(format!(
                    "failed to read config {}: {e}",
                    path.display()
                )));
            }
        };
        config.apply_env_overrides();
        Ok(config)
    }

    /// Apply `AGENT_*` environment variable overrides.
    fn apply_env_overrides(&mut self) {
        if let Ok(v) = env::var("AGENT_API_KEY") {
            self.llm.api_key = v;
        }
        if let Ok(v) = env::var("AGENT_LLM_BASE_URL") {
            self.llm.base_url = v;
        }
        if let Ok(v) = env::var("AGENT_MODEL") {
            self.llm.model = v;
        }
        if let Ok(v) = env::var("AGENT_LLM_PROVIDER") {
            self.llm.provider = v;
        }
        if let Ok(v) = env::var("AGENT_WORK_DIR") {
            self.agent.work_dir = PathBuf::from(v);
        }
        if let Ok(v) = env::var("AGENT_SCRIPTS_DIR") {
            self.agent.scripts_dir = PathBuf::from(v);
        }
        if let Ok(v) = env::var("AGENT_DB_PATH") {
            self.agent.db_path = PathBuf::from(v);
        }
        if let Ok(v) = env::var("AGENT_LOG_LEVEL") {
            self.logging.level = v;
        }
    }

    /// Apply CLI overrides on top of environment overrides.
    pub fn apply_cli_overrides(&mut self, cli: &crate::cli::Cli) {
        if let Some(v) = &cli.api_key {
            self.llm.api_key = v.clone();
        }
        if let Some(v) = &cli.llm_base_url {
            self.llm.base_url = v.clone();
        }
        if let Some(v) = &cli.model {
            self.llm.model = v.clone();
        }
        if let Some(v) = &cli.llm_provider {
            self.llm.provider = v.clone();
        }
        if let Some(v) = &cli.work_dir {
            self.agent.work_dir = v.clone();
        }
        if let Some(v) = &cli.db_path {
            self.agent.db_path = v.clone();
        }
    }

    /// Validate all configuration values and report every problem found.
    /// Returns an error listing all actionable validation failures.
    pub fn validate(&self) -> Result<()> {
        let mut problems: Vec<String> = Vec::new();

        if self.agent.max_steps == 0 {
            problems.push("agent.max_steps must be > 0".to_string());
        }
        if self.agent.max_llm_calls == 0 {
            problems.push("agent.max_llm_calls must be > 0".to_string());
        }
        if self.agent.max_tool_calls == 0 {
            problems.push("agent.max_tool_calls must be > 0".to_string());
        }
        if self.agent.max_context_tokens == 0 {
            problems.push("agent.max_context_tokens must be > 0".to_string());
        }
        if self.agent.max_file_size_bytes == 0 {
            problems.push("agent.max_file_size_bytes must be > 0".to_string());
        }
        if self.agent.command_timeout_secs == 0 {
            problems.push("agent.command_timeout_secs must be > 0".to_string());
        }
        if self.agent.max_evolution_attempts == 0 {
            problems.push("agent.max_evolution_attempts must be > 0".to_string());
        }
        // heartbeat_interval_secs and snapshot_interval_steps accept 0, which
        // means "on every step" for tests and tight audit loops.

        if self.llm.base_url.trim().is_empty() {
            problems.push("llm.base_url must be set (or AGENT_LLM_BASE_URL)".to_string());
        }
        if self.llm.model.trim().is_empty() {
            problems.push("llm.model must be set (or AGENT_MODEL)".to_string());
        }
        if !["openai", "anthropic", "gemini", "custom"].contains(&self.llm.provider.as_str()) {
            problems.push(format!(
                "llm.provider must be one of openai|anthropic|gemini|custom, got {:?}",
                self.llm.provider
            ));
        }
        if self.llm.max_output_tokens == 0 {
            problems.push("llm.max_output_tokens must be > 0".to_string());
        }
        if !(0.0..=2.0).contains(&self.llm.temperature) {
            problems.push("llm.temperature must be in [0.0, 2.0]".to_string());
        }
        if self.llm.max_retries > 10 {
            problems.push("llm.max_retries must be <= 10".to_string());
        }
        if self.llm.provider == "custom"
            && let Err(e) = crate::llm::provider::validate_custom(&self.llm.custom)
        {
            problems.push(e.to_string());
        }

        if self.sandbox.root_dir.as_os_str().is_empty() {
            problems.push("sandbox.root_dir must be set".to_string());
        }
        if self.sandbox.allowed_commands.is_empty() {
            problems.push("sandbox.allowed_commands must not be empty".to_string());
        }
        for cmd in &self.sandbox.allowed_commands {
            if cmd.trim().is_empty() || cmd.contains([' ', '/', '\\', ':', '\t']) {
                problems.push(format!(
                    "sandbox.allowed_commands contains invalid executable name {cmd:?}"
                ));
            }
        }

        if !["trace", "debug", "info", "warn", "error"].contains(&self.logging.level.as_str()) {
            problems.push(format!(
                "logging.level must be one of trace|debug|info|warn|error, got {:?}",
                self.logging.level
            ));
        }

        if problems.is_empty() {
            Ok(())
        } else {
            Err(Error::InvalidConfig(problems.join("; ")))
        }
    }
}

/// Scheduler and agent loop settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub work_dir: PathBuf,
    pub scripts_dir: PathBuf,
    pub db_path: PathBuf,
    pub max_steps: usize,
    pub max_input_tokens: usize,
    pub max_llm_calls: usize,
    pub max_tool_calls: usize,
    pub max_wall_clock_secs: u64,
    pub max_evolution_attempts: usize,
    pub max_context_tokens: usize,
    pub heartbeat_interval_secs: u64,
    pub snapshot_interval_steps: usize,
    pub max_file_size_bytes: usize,
    pub command_timeout_secs: u64,
    pub loop_sleep_ms: u64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        AgentConfig {
            work_dir: PathBuf::from("./workspace"),
            scripts_dir: PathBuf::from("./scripts"),
            db_path: PathBuf::from("./agent.db"),
            max_steps: 200,
            max_input_tokens: 100_000,
            max_llm_calls: 50,
            max_tool_calls: 200,
            max_wall_clock_secs: 86_400,
            max_evolution_attempts: 5,
            max_context_tokens: 128_000,
            heartbeat_interval_secs: 60,
            snapshot_interval_steps: 10,
            max_file_size_bytes: 10 * 1024 * 1024,
            command_timeout_secs: 120,
            loop_sleep_ms: 100,
        }
    }
}

/// LLM gateway settings with pluggable providers.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    /// "openai" (default), "anthropic", "gemini", or "custom".
    pub provider: String,
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub temperature: f64,
    /// Cap on generated tokens; required by providers such as Anthropic.
    pub max_output_tokens: u64,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub retry_base_delay_secs: u64,
    /// Endpoint details for `provider = "custom"`.
    pub custom: CustomLlmConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        LlmConfig {
            provider: "openai".to_string(),
            api_key: String::new(),
            base_url: "https://api.openai.com/v1".to_string(),
            model: "gpt-4".to_string(),
            temperature: 0.7,
            max_output_tokens: 4096,
            request_timeout_secs: 60,
            max_retries: 3,
            retry_base_delay_secs: 1,
            custom: CustomLlmConfig::default(),
        }
    }
}

/// Definition of a custom OpenAI-compatible endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CustomLlmConfig {
    /// URL path relative to `llm.base_url`, e.g. "/v1/chat/completions".
    pub chat_path: String,
    /// Header carrying the API key, e.g. "Authorization" or "X-Api-Key".
    pub api_key_header: String,
    /// Prefix prepended to the key value, e.g. "Bearer ".
    pub api_key_scheme: String,
    /// Extra static headers sent with every request.
    pub headers: std::collections::BTreeMap<String, String>,
}

impl Default for CustomLlmConfig {
    fn default() -> Self {
        CustomLlmConfig {
            chat_path: "/chat/completions".to_string(),
            api_key_header: "Authorization".to_string(),
            api_key_scheme: "Bearer ".to_string(),
            headers: std::collections::BTreeMap::new(),
        }
    }
}

/// Sandbox and capability settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct SandboxConfig {
    pub root_dir: PathBuf,
    pub allowed_commands: Vec<String>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        SandboxConfig {
            root_dir: PathBuf::from("./workspace"),
            allowed_commands: vec![
                "git".to_string(),
                "cargo".to_string(),
                "rustc".to_string(),
                "python3".to_string(),
                "ls".to_string(),
            ],
        }
    }
}

/// Structured logging settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LoggingConfig {
    pub level: String,
    pub file: Option<PathBuf>,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig {
            level: "info".to_string(),
            file: Some(PathBuf::from("agent.log")),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use tempfile::tempdir;

    fn write_config(dir: &Path, contents: &str) -> PathBuf {
        let path = dir.join("config.toml");
        fs::write(&path, contents).unwrap();
        path
    }

    /// Set or remove an environment variable. Safe only because tests that
    /// touch the environment are serialized with `serial_test` and no other
    /// code runs concurrently in the test process.
    unsafe fn set_env_var(key: &str, value: Option<&str>) {
        match value {
            Some(v) => unsafe { env::set_var(key, v) },
            None => unsafe { env::remove_var(key) },
        }
    }
    #[test]
    #[serial_test::serial]
    fn explicit_missing_config_errors() {
        let dir = tempdir().unwrap();
        let config = Config::load(&dir.path().join("config.toml")).unwrap_err();
        assert_eq!(config.code(), crate::error::ErrorCode::InvalidConfig);
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial]
    fn default_config_loads_without_file() {
        let original = env::var("AGENT_API_KEY").ok();
        // SAFETY: serialized test; see `set_env_var`.
        unsafe { set_env_var("AGENT_API_KEY", None) };
        let config =
            Config::load(Path::new(DEFAULT_CONFIG_PATH)).expect("default config must load");
        assert_eq!(config.agent.max_steps, 200);
        assert_eq!(config.llm.model, "gpt-4");
        assert!(config.validate().is_ok());
        // SAFETY: serialized test; see `set_env_var`.
        unsafe { set_env_var("AGENT_API_KEY", original.as_deref()) };
    }

    #[test]
    #[serial_test::serial]
    fn parses_valid_config_file() {
        let dir = tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"
[agent]
max_steps = 42
work_dir = "./work"

[llm]
model = "deepseek-chat"

[sandbox]
allowed_commands = ["git", "cargo"]
"#,
        );
        let config = Config::load(&path).unwrap();
        assert_eq!(config.agent.max_steps, 42);
        assert_eq!(config.agent.work_dir, PathBuf::from("./work"));
        assert_eq!(config.llm.model, "deepseek-chat");
        assert_eq!(config.sandbox.allowed_commands, vec!["git", "cargo"]);
        assert_eq!(config.agent.db_path, PathBuf::from("./agent.db"));
    }

    #[test]
    #[serial_test::serial]
    fn rejects_invalid_config_with_all_problems() {
        let dir = tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"
[agent]
max_steps = 0
max_llm_calls = 0

[llm]
base_url = ""
model = ""
temperature = 5.0

[sandbox]
allowed_commands = []

[logging]
level = "verbose"
"#,
        );
        let err = Config::load(&path)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("max_steps"),
            "missing max_steps problem: {err}"
        );
        assert!(err.contains("base_url"), "missing base_url problem: {err}");
        assert!(
            err.contains("temperature"),
            "missing temperature problem: {err}"
        );
        assert!(
            err.contains("allowed_commands"),
            "missing commands problem: {err}"
        );
        assert!(err.contains("level"), "missing level problem: {err}");
    }

    #[test]
    #[serial_test::serial]
    fn rejects_invalid_executable_names() {
        let dir = tempdir().unwrap();
        let path = write_config(
            dir.path(),
            r#"
[sandbox]
allowed_commands = ["git", "rm -rf", "/usr/bin/cargo"]
"#,
        );
        let err = Config::load(&path)
            .unwrap()
            .validate()
            .unwrap_err()
            .to_string();
        assert!(err.contains("rm -rf"), "unexpected: {err}");
        assert!(err.contains("/usr/bin/cargo"), "unexpected: {err}");
    }

    #[test]
    #[serial_test::serial]
    #[serial_test::serial]
    fn env_overrides_apply() {
        let original = [
            env::var("AGENT_API_KEY").ok(),
            env::var("AGENT_LLM_BASE_URL").ok(),
        ];
        // SAFETY: serialized test; see `set_env_var`.
        unsafe {
            set_env_var("AGENT_API_KEY", Some("secret-key"));
            set_env_var("AGENT_LLM_BASE_URL", Some("http://localhost:9999/v1"));
        }

        let dir = tempdir().unwrap();
        let path = write_config(dir.path(), "[llm]\napi_key = \"file-key\"\n");
        let config = Config::load(&path).unwrap();
        assert_eq!(config.llm.api_key, "secret-key");
        assert_eq!(config.llm.base_url, "http://localhost:9999/v1");

        // SAFETY: serialized test; see `set_env_var`.
        unsafe {
            set_env_var("AGENT_API_KEY", original[0].as_deref());
            set_env_var("AGENT_LLM_BASE_URL", original[1].as_deref());
        }
    }
}
