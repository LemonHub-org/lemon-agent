//! Rendering for the Lemon Agent TUI: dashboard, continuity list, and detail
//! views built with ratatui.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, List, ListItem, Paragraph, Row, Table, Wrap};

use crate::scheduler::AgentState;
use crate::tui::app::{App, Focus, Screen};

const ACCENT: Color = Color::Green;
const DIM: Color = Color::DarkGray;

/// Draw the current screen.
pub fn render(frame: &mut Frame, app: &App) {
    let area = frame.area();
    match app.screen {
        Screen::Dashboard => draw_dashboard(frame, app, area),
        Screen::Continuities => draw_continuities(frame, app, area),
        Screen::Detail => draw_detail(frame, app, area),
    }
}

fn header(frame: &mut Frame, app: &App, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            format!("Lemon Agent v{}", env!("CARGO_PKG_VERSION")),
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            if app.monitor { "monitor" } else { "agent" },
            Style::default().fg(DIM),
        ),
    ]);
    let mode = match app.screen {
        Screen::Dashboard => "dashboard",
        Screen::Continuities => "continuities",
        Screen::Detail => "detail",
    };
    let status = if app.live.idle {
        "idle — waiting for a task".to_string()
    } else {
        format!(
            "{} · step {} · {}",
            state_label(app.live.state),
            app.live.step_num,
            app.live.budget_summary
        )
    };
    let right = Line::from(vec![
        Span::styled(mode, Style::default().fg(ACCENT)),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(DIM)),
    ]);
    let block = Block::bordered()
        .title(title.alignment(Alignment::Left))
        .title(right.alignment(Alignment::Right));
    frame.render_widget(block, area);
}

fn footer(frame: &mut Frame, app: &App, area: Rect) {
    let hints = if app.focus == Focus::Input {
        "Enter submit · Esc clear · Tab to log".to_string()
    } else {
        "Tab input · ↑/↓ scroll · c continuities · d detail · Esc/q quit".to_string()
    };
    let mut lines = vec![Line::from(Span::styled(hints, Style::default().fg(DIM)))];
    if let Some(error) = &app.error {
        lines.push(Line::from(Span::styled(
            format!("error: {error}"),
            Style::default().fg(Color::Red),
        )));
    }
    if app.task_queued {
        lines.push(Line::from(Span::styled(
            "task queued — the agent will pick it up when idle",
            Style::default().fg(Color::Yellow),
        )));
    }
    let input = if app.monitor {
        "monitor mode: read-only".to_string()
    } else {
        format!(
            "> {}",
            if app.input.is_empty() {
                "type a task and press Enter".to_string()
            } else {
                app.input.clone()
            }
        )
    };
    let input_style = if app.focus == Focus::Input {
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(DIM)
    };
    lines.push(Line::from(Span::styled(input, input_style)));

    let block = Block::bordered().title("task");
    let paragraph = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn draw_dashboard(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(8),
            Constraint::Min(1),
            Constraint::Length(4),
        ])
        .split(area);
    header(frame, app, chunks[0]);

    // Live panel.
    let mut live_lines = Vec::new();
    if app.live.continuity_id.is_empty() {
        live_lines.push(Line::from("no continuity yet"));
    } else {
        live_lines.push(Line::from(vec![
            Span::styled("continuity ", Style::default().fg(DIM)),
            Span::raw(app.live.continuity_id.clone()),
        ]));
        live_lines.push(Line::from(vec![
            Span::styled("state      ", Style::default().fg(DIM)),
            Span::styled(state_label(app.live.state), state_style(app.live.state)),
        ]));
        live_lines.push(Line::from(vec![
            Span::styled("steps      ", Style::default().fg(DIM)),
            Span::raw(app.live.step_num.to_string()),
        ]));
        live_lines.push(Line::from(vec![
            Span::styled("budget     ", Style::default().fg(DIM)),
            Span::raw(app.live.budget_summary.clone()),
        ]));
        if let Some(error) = &app.live.last_error {
            live_lines.push(Line::from(vec![
                Span::styled("last error ", Style::default().fg(DIM)),
                Span::styled(truncate(error, 160), Style::default().fg(Color::Red)),
            ]));
        }
        if let Some(report) = &app.live.report {
            live_lines.push(Line::from(vec![
                Span::styled("report     ", Style::default().fg(DIM)),
                Span::styled(report.status.clone(), status_style(&report.status)),
            ]));
            live_lines.push(Line::from(truncate(&report.summary, 200)));
        }
    }
    let live_block = Block::bordered().title("live");
    frame.render_widget(
        Paragraph::new(live_lines)
            .block(live_block)
            .wrap(Wrap { trim: false }),
        chunks[1],
    );

    // Event log.
    let visible = area.height.saturating_sub(15) as usize;
    let log_style = if app.focus == Focus::Log {
        Style::default()
    } else {
        Style::default().fg(DIM)
    };
    let items: Vec<ListItem> = app
        .event_lines
        .iter()
        .skip(
            app.log_scroll
                .min(app.event_lines.len().saturating_sub(visible)),
        )
        .take(visible)
        .map(|line| ListItem::new(line.clone()).style(log_style))
        .collect();
    let log_block = Block::bordered().title(format!("events ({})", app.event_lines.len()));
    frame.render_widget(List::new(items).block(log_block), chunks[2]);

    footer(frame, app, chunks[3]);
}

fn draw_continuities(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    header(frame, app, chunks[0]);

    let selected_style = Style::default()
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);
    let rows: Vec<Row> = app
        .summaries
        .iter()
        .enumerate()
        .map(|(index, summary)| {
            let style = if app.selected == Some(index) {
                selected_style
            } else {
                Style::default()
            };
            Row::new(vec![
                Cell::from(summary.continuity_id.clone()),
                Cell::from(summary.steps.to_string()),
                Cell::from(if summary.finished { "yes" } else { "no" }),
                Cell::from(format_ts(summary.started_at_ms)),
            ])
            .style(style)
        })
        .collect();
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(55),
            Constraint::Percentage(10),
            Constraint::Percentage(10),
            Constraint::Percentage(25),
        ],
    )
    .header(
        Row::new(vec![
            Cell::from("continuity").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("steps").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("finished").style(Style::default().add_modifier(Modifier::BOLD)),
            Cell::from("started").style(Style::default().add_modifier(Modifier::BOLD)),
        ])
        .style(Style::default().fg(ACCENT)),
    )
    .block(Block::bordered().title("continuities"));
    frame.render_widget(table, chunks[1]);

    let hints = Paragraph::new(Line::from(Span::styled(
        "↑/↓ select · Enter detail · Esc back · q quit",
        Style::default().fg(DIM),
    )));
    frame.render_widget(hints, chunks[2]);
}

fn draw_detail(frame: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
    header(frame, app, chunks[0]);

    let id = app.detail_continuity().unwrap_or_else(|| "—".to_string());
    let visible = area.height.saturating_sub(6) as usize;
    let start = app
        .log_scroll
        .min(app.detail_events.len().saturating_sub(visible));
    let text: Vec<Line> = app
        .detail_events
        .iter()
        .skip(start)
        .take(visible)
        .map(|line| Line::from(line.clone()))
        .collect();
    let block = Block::bordered().title(format!("events — {id}"));
    frame.render_widget(Paragraph::new(text).block(block), chunks[1]);

    let hints = Paragraph::new(Line::from(Span::styled(
        "↑/↓ scroll · Esc back · q quit",
        Style::default().fg(DIM),
    )));
    frame.render_widget(hints, chunks[2]);
}

fn state_label(state: AgentState) -> &'static str {
    match state {
        AgentState::Idle => "idle",
        AgentState::Planning => "planning",
        AgentState::Executing => "executing",
        AgentState::Evaluating => "evaluating",
        AgentState::Evolving => "evolving",
        AgentState::Terminated => "terminated",
    }
}

fn state_style(state: AgentState) -> Style {
    match state {
        AgentState::Planning
        | AgentState::Executing
        | AgentState::Evaluating
        | AgentState::Evolving => Style::default().fg(Color::Yellow),
        AgentState::Terminated => Style::default().fg(Color::Red),
        AgentState::Idle => Style::default().fg(ACCENT),
    }
}

fn status_style(status: &str) -> Style {
    match status {
        "completed" => Style::default().fg(ACCENT),
        "idle" => Style::default().fg(DIM),
        _ => Style::default().fg(Color::Red),
    }
}

fn format_ts(ms: u64) -> String {
    let secs = ms / 1000;
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        secs / 31_536_000,
        (secs % 31_536_000) / 2_628_000,
        (secs % 2_628_000) / 86_400,
        (secs % 86_400) / 3_600,
        (secs % 3_600) / 60,
        secs % 60
    )
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    #[test]
    fn dashboard_renders_without_panic() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let app = App::new(false);
        terminal.draw(|frame| render(frame, &app)).unwrap();
        let buffer = terminal.backend().buffer();
        let content = buffer
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect::<String>();
        assert!(content.contains("Lemon Agent"));
        assert!(content.contains("task"));
    }

    #[test]
    fn all_screens_render() {
        let backend = TestBackend::new(100, 30);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut app = App::new(true);
        for screen in [Screen::Dashboard, Screen::Continuities, Screen::Detail] {
            app.screen = screen;
            terminal.draw(|frame| render(frame, &app)).unwrap();
        }
    }
}
