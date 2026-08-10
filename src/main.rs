//! Lemon Agent entry point: parse CLI arguments, load configuration, and run
//! the agent loop.

use std::process::ExitCode;

use clap::Parser;
use lemon_agent::cli::Cli;
use lemon_agent::config::Config;
use lemon_agent::error::{Error, Result};
use tracing::{error, info};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, code = %e.code(), "agent terminated");
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    let mut config = Config::load(&cli.config)?;
    config.apply_cli_overrides(&cli);
    config.validate()?;

    lemon_agent::logging::init(&config.logging.level, config.logging.file.as_deref())?;

    ensure_dirs(&config)?;

    info!(
        work_dir = %config.agent.work_dir.display(),
        scripts_dir = %config.agent.scripts_dir.display(),
        db_path = %config.agent.db_path.display(),
        model = %config.llm.model,
        "agent initialized"
    );

    if let Some(task) = &cli.task {
        info!(task_preview = %preview(task, 200), "task received");
    } else {
        info!("no task provided; agent remains in idle state");
    }

    Ok(())
}

/// Create the directories the agent operates on, reporting any failure.
fn ensure_dirs(config: &Config) -> Result<()> {
    for path in [&config.agent.work_dir, &config.agent.scripts_dir] {
        if !path.exists() {
            std::fs::create_dir_all(path).map_err(|e| Error::io(Some(path.clone()), e))?;
            info!(path = %path.display(), "created directory");
        }
    }
    Ok(())
}

/// Truncate a string for safe logging without leaking secrets.
fn preview(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}
