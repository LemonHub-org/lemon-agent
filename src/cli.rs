//! Command-line interface definitions.

use std::path::PathBuf;

use clap::Parser;

/// An unattended autonomous programming agent.
#[derive(Debug, Parser)]
#[command(name = "lemon-agent", version, about)]
pub struct Cli {
    /// Path to the TOML configuration file.
    #[arg(short, long, value_name = "FILE", default_value = "config.toml")]
    pub config: PathBuf,

    /// Initial task prompt. When omitted, the agent starts in the Idle state
    /// and waits for a task to be supplied externally.
    #[arg(short, long, value_name = "TASK")]
    pub task: Option<String>,

    /// Override the SQLite database path.
    #[arg(long, value_name = "PATH")]
    pub db_path: Option<PathBuf>,

    /// Override the sandbox working directory.
    #[arg(long, value_name = "DIR")]
    pub work_dir: Option<PathBuf>,

    /// Override the LLM API key (also via AGENT_API_KEY).
    #[arg(long, value_name = "KEY")]
    pub api_key: Option<String>,

    /// Override the LLM base URL (also via AGENT_LLM_BASE_URL).
    #[arg(long, value_name = "URL")]
    pub llm_base_url: Option<String>,

    /// Override the LLM model name (also via AGENT_MODEL).
    #[arg(long, value_name = "MODEL")]
    pub model: Option<String>,
}
