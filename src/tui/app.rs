//! TUI application state: screens, focus, input, scrolling, and data refresh
//! from the event store and the live agent snapshot.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::mpsc;

use crate::error::Result;
use crate::kernel::event_store::{ContinuitySummary, EventStore};
use crate::scheduler::LiveState;

/// The active screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Dashboard,
    Continuities,
    Detail,
}

/// Where keyboard focus lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    /// The task input field at the bottom.
    Input,
    /// The event log / lists.
    Log,
}

/// Everything the UI renders.
#[derive(Debug)]
pub struct App {
    pub screen: Screen,
    pub focus: Focus,
    pub monitor: bool,
    pub input: String,
    pub log_scroll: usize,
    pub summary_scroll: usize,
    pub selected: Option<usize>,
    pub live: LiveState,
    pub event_lines: Vec<String>,
    pub summaries: Vec<ContinuitySummary>,
    pub detail_events: Vec<String>,
    pub task_queued: bool,
    pub error: Option<String>,
    pub quit: bool,
    /// A task the user submitted; consumed by the runner.
    pub submitted: Option<String>,
    last_continuity: String,
}

impl App {
    pub fn new(monitor: bool) -> App {
        App {
            screen: Screen::Dashboard,
            focus: Focus::Log,
            monitor,
            input: String::new(),
            log_scroll: 0,
            summary_scroll: 0,
            selected: None,
            live: LiveState::default(),
            event_lines: Vec::new(),
            summaries: Vec::new(),
            detail_events: Vec::new(),
            task_queued: false,
            error: None,
            quit: false,
            submitted: None,
            last_continuity: String::new(),
        }
    }

    /// Handle one key event. Returns `true` when the app should quit.
    pub fn on_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers.contains(KeyModifiers::CONTROL) && matches!(key.code, KeyCode::Char('c')) {
            self.quit = true;
            return true;
        }
        match self.focus {
            Focus::Input => self.on_input_key(key),
            Focus::Log => self.on_log_key(key),
        }
        false
    }

    fn on_input_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char(c) => self.input.push(c),
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Esc => {
                if !self.input.is_empty() {
                    self.input.clear();
                } else {
                    self.focus = Focus::Log;
                }
            }
            KeyCode::Tab => self.focus = Focus::Log,
            _ => {}
        }
    }

    fn on_log_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Char('c') => {
                self.screen = Screen::Continuities;
                self.selected = None;
            }
            KeyCode::Char('d') => {
                if !self.live.continuity_id.is_empty() {
                    self.screen = Screen::Detail;
                }
            }
            KeyCode::Tab => self.focus = Focus::Input,
            KeyCode::Up => match self.screen {
                Screen::Dashboard | Screen::Detail => {
                    self.log_scroll = self.log_scroll.saturating_sub(1)
                }
                Screen::Continuities => self.move_selection(-1),
            },
            KeyCode::Down => match self.screen {
                Screen::Dashboard | Screen::Detail => self.log_scroll += 1,
                Screen::Continuities => self.move_selection(1),
            },
            KeyCode::PageUp => self.log_scroll = self.log_scroll.saturating_sub(20),
            KeyCode::PageDown => self.log_scroll += 20,
            KeyCode::Enter => {
                if self.screen == Screen::Continuities
                    && let Some(index) = self.selected
                    && self.summaries.get(index).is_some()
                {
                    self.screen = Screen::Detail;
                    self.log_scroll = 0;
                }
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        if self.summaries.is_empty() {
            return;
        }
        let current = self.selected.unwrap_or(0) as isize;
        let next = (current + delta).clamp(0, self.summaries.len() as isize - 1) as usize;
        self.selected = Some(next);
    }

    /// The continuity the Detail screen shows.
    pub fn detail_continuity(&self) -> Option<String> {
        match self.screen {
            Screen::Detail => match self.selected {
                Some(index) => self.summaries.get(index).map(|s| s.continuity_id.clone()),
                None => {
                    if self.live.continuity_id.is_empty() {
                        None
                    } else {
                        Some(self.live.continuity_id.clone())
                    }
                }
            },
            _ => None,
        }
    }

    /// Submit the current input as a task.
    fn submit(&mut self) {
        let task = self.input.trim().to_string();
        self.input.clear();
        if task.is_empty() {
            return;
        }
        if self.monitor {
            self.error = Some("monitor mode: task submission is disabled".to_string());
            return;
        }
        self.task_queued = true;
        self.submitted = Some(task);
        self.focus = Focus::Log;
    }

    /// Pull the latest live snapshot and refresh store-backed views.
    pub fn refresh(
        &mut self,
        store: &EventStore,
        live: &Arc<Mutex<LiveState>>,
        task_tx: &mpsc::Sender<String>,
    ) -> Result<()> {
        if let Some(task) = self.submitted.take()
            && task_tx.try_send(task).is_err()
        {
            self.error = Some("task queue is closed".to_string());
        }
        let live = live.lock().unwrap_or_else(|p| p.into_inner()).clone();
        let continuity_changed = live.continuity_id != self.last_continuity;
        if continuity_changed {
            self.last_continuity = live.continuity_id.clone();
            self.log_scroll = 0;
        }
        if live.idle && self.live.state != live.state {
            self.task_queued = false;
        }
        self.live = live;

        if self.screen == Screen::Dashboard || continuity_changed {
            self.event_lines = dashboard_events(store, &self.live.continuity_id)?;
        }
        if self.screen == Screen::Continuities {
            self.summaries = store.continuity_summaries()?;
        }
        if self.screen == Screen::Detail
            && let Some(id) = self.detail_continuity()
        {
            self.detail_events = detail_events(store, &id)?;
        }
        Ok(())
    }
}

/// Render recent events of a continuity as display lines.
fn dashboard_events(store: &EventStore, continuity_id: &str) -> Result<Vec<String>> {
    if continuity_id.is_empty() {
        return Ok(Vec::new());
    }
    let events = store.events_after(continuity_id, 0)?;
    Ok(events
        .iter()
        .map(|e| format!("{:>4} {}", e.seq, preview_payload(&e.event)))
        .collect())
}

fn detail_events(store: &EventStore, continuity_id: &str) -> Result<Vec<String>> {
    dashboard_events(store, continuity_id)
}

fn preview_payload(event: &crate::kernel::event_store::EventType) -> String {
    use crate::kernel::event_store::EventType;
    match event {
        EventType::ContinuityStarted { initial_prompt } => {
            format!("ContinuityStarted {}", truncate(initial_prompt, 120))
        }
        EventType::StepStarted { step_num } => format!("StepStarted step {step_num}"),
        EventType::LlmRequest { prompt_preview, .. } => {
            format!("LlmRequest {prompt_preview}")
        }
        EventType::LlmResponse { content, .. } => format!("LlmResponse {content}"),
        EventType::ToolCall {
            tool_name, result, ..
        } => {
            let outcome = match result {
                crate::kernel::event_store::ToolOutcome::Ok(_) => "ok",
                crate::kernel::event_store::ToolOutcome::Err(_) => "error",
            };
            format!("ToolCall {tool_name} [{outcome}]")
        }
        EventType::Error { error, .. } => format!("Error {error}"),
        EventType::Heartbeat { .. } => "Heartbeat".to_string(),
        EventType::EvolutionAttempt { .. } => "EvolutionAttempt".to_string(),
        EventType::EvolutionResult { success, .. } => {
            format!("EvolutionResult success={success}")
        }
        EventType::StepFinished { step_num } => format!("StepFinished step {step_num}"),
        EventType::ContinuityFinished { status, .. } => {
            format!("ContinuityFinished {status}")
        }
    }
}

fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max_chars).collect();
        out.push_str("...");
        out
    }
}

/// Keep the event poll cadence in one place.
pub const POLL_TIMEOUT: Duration = Duration::from_millis(200);

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyCode;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, ratatui::crossterm::event::KeyModifiers::NONE)
    }

    #[test]
    fn ctrl_c_quits() {
        let mut app = App::new(false);
        let key = KeyEvent::new(
            KeyCode::Char('c'),
            ratatui::crossterm::event::KeyModifiers::CONTROL,
        );
        assert!(app.on_key(key));
        assert!(app.quit);
    }

    #[test]
    fn input_editing_and_submit() {
        let mut app = App::new(false);
        app.focus = Focus::Input;
        app.on_key(key(KeyCode::Char('h')));
        app.on_key(key(KeyCode::Char('i')));
        assert_eq!(app.input, "hi");
        app.on_key(key(KeyCode::Backspace));
        assert_eq!(app.input, "h");
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.submitted.as_deref(), Some("h"));
        assert!(app.task_queued);
        assert_eq!(app.focus, Focus::Log);
    }

    #[test]
    fn empty_submit_is_ignored() {
        let mut app = App::new(false);
        app.focus = Focus::Input;
        app.on_key(key(KeyCode::Enter));
        assert!(app.submitted.is_none());
    }

    #[test]
    fn monitor_mode_rejects_submission() {
        let mut app = App::new(true);
        app.focus = Focus::Input;
        app.input = "task".to_string();
        app.on_key(key(KeyCode::Enter));
        assert!(app.submitted.is_none());
        assert!(app.error.is_some());
    }

    #[test]
    fn tab_toggles_focus() {
        let mut app = App::new(false);
        app.focus = Focus::Log;
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Input);
        app.on_key(key(KeyCode::Tab));
        assert_eq!(app.focus, Focus::Log);
    }

    #[test]
    fn screen_navigation() {
        let mut app = App::new(false);
        app.live.continuity_id = "c1".to_string();
        app.on_key(key(KeyCode::Char('c')));
        assert_eq!(app.screen, Screen::Continuities);
        app.summaries.push(ContinuitySummary {
            continuity_id: "c1".to_string(),
            steps: 3,
            started_at_ms: 0,
            finished: false,
        });
        app.on_key(key(KeyCode::Down));
        app.on_key(key(KeyCode::Enter));
        assert_eq!(app.screen, Screen::Detail);
        assert_eq!(app.detail_continuity().as_deref(), Some("c1"));
        app.on_key(key(KeyCode::Char('q')));
        assert!(app.quit);
    }

    #[test]
    fn scrolling_is_bounded() {
        let mut app = App::new(false);
        app.screen = Screen::Dashboard;
        app.on_key(key(KeyCode::Up));
        assert_eq!(app.log_scroll, 0);
        app.on_key(key(KeyCode::Down));
        assert_eq!(app.log_scroll, 1);
        app.on_key(key(KeyCode::PageUp));
        assert_eq!(app.log_scroll, 0);
    }
}
