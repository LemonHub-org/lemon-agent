//! Terminal user interface: run the agent as a daemon with a live dashboard,
//! task submission, and a continuity browser — or watch an existing event
//! store in monitor mode.

pub mod app;
pub mod ui;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{self, Event};
use tokio::sync::{mpsc, watch};

use crate::config::Config;
use crate::error::Result;
use crate::kernel::event_store::EventStore;
use crate::scheduler::{Agent, LiveState};
use crate::tui::app::{App, POLL_TIMEOUT};

/// Run the TUI until the user quits.
pub async fn run_tui(config: &Config, initial_task: Option<String>, monitor: bool) -> Result<()> {
    crate::logging::init(&config.logging.level, config.logging.file.as_deref())?;
    let store = Arc::new(EventStore::open(&config.agent.db_path)?);

    let terminal = ratatui::init();
    let result = run_tui_inner(terminal, config, initial_task, monitor, store).await;
    ratatui::restore();
    result
}

async fn run_tui_inner(
    mut terminal: ratatui::DefaultTerminal,
    config: &Config,
    initial_task: Option<String>,
    monitor: bool,
    store: Arc<EventStore>,
) -> Result<()> {
    let live: Arc<Mutex<LiveState>> = Arc::new(Mutex::new(LiveState::default()));
    let (task_tx, task_rx) = mpsc::channel::<String>(16);
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    let mut agent_handle = None;
    if !monitor {
        let mut agent = Agent::new(config, initial_task.clone())?;
        let observer = live.clone();
        let observer_for_closure = live.clone();
        let shutdown = shutdown_rx.clone();
        agent_handle = Some(tokio::spawn(async move {
            let result = agent
                .run_daemon(task_rx, shutdown, move |snapshot| {
                    *observer_for_closure
                        .lock()
                        .unwrap_or_else(|p| p.into_inner()) = snapshot.clone();
                })
                .await;
            if let Err(e) = result {
                tracing::error!(error = %e, "agent daemon stopped with an error");
                let mut live = observer.lock().unwrap_or_else(|p| p.into_inner());
                live.last_error = Some(format!("daemon stopped: {e}"));
            }
        }));
    }

    let mut app = App::new(monitor);
    if initial_task.is_some() {
        app.task_queued = true;
    }

    loop {
        terminal.draw(|frame| ui::render(frame, &app))?;

        if event::poll(POLL_TIMEOUT)? {
            match event::read()? {
                Event::Key(key) => {
                    let quit = app.on_key(key);
                    if quit {
                        break;
                    }
                }
                _ => {}
            }
        }
        app.refresh(&store, &live, &task_tx)?;
        if app.quit {
            break;
        }
    }

    shutdown_tx.send(true).ok();
    if let Some(handle) = agent_handle {
        let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
    }
    Ok(())
}
