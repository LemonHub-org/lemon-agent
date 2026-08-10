//! Lemon Agent entry point: parse CLI arguments, load configuration, and run
//! the agent loop or the terminal UI.

use std::process::ExitCode;

use clap::Parser;
use lemon_agent::cli::{Cli, Command};
use lemon_agent::config::Config;
use lemon_agent::error::Result;
use lemon_agent::scheduler::Agent;
use tracing::{error, info};

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            error!(error = %e, code = %e.code(), "agent terminated");
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> Result<()> {
    if let Some(Command::Tui {
        monitor,
        config,
        task,
    }) = &cli.command
    {
        let config = Config::load(config)?;
        config.validate()?;
        return lemon_agent::tui::run_tui(&config, task.clone(), *monitor).await;
    }

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

    let mut agent = Agent::new(&config, cli.task)?;
    let report = agent.run().await?;
    info!(
        continuity_id = %report.continuity_id,
        status = %report.status,
        steps = report.steps_used,
        summary = %report.summary,
        "continuity finished"
    );
    println!(
        "status: {}\ncontinuity: {}\nsteps: {}\nsummary: {}",
        report.status, report.continuity_id, report.steps_used, report.summary
    );
    Ok(())
}

/// Create the directories the agent operates on, reporting any failure.
fn ensure_dirs(config: &Config) -> Result<()> {
    for path in [&config.agent.work_dir, &config.agent.scripts_dir] {
        if !path.exists() {
            std::fs::create_dir_all(path)
                .map_err(|e| lemon_agent::error::Error::io(Some(path.clone()), e))?;
            info!(path = %path.display(), "created directory");
        }
    }
    Ok(())
}
