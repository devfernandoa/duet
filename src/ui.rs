use crate::agent::Agent;
use crate::app::App;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use tui_term::widget::PseudoTerminal;

pub fn draw(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    draw_tab_bar(frame, app, chunks[0]);
    draw_focused_pane(frame, app, chunks[1]);
    draw_status_line(frame, app, chunks[2]);
}

fn draw_tab_bar(frame: &mut Frame, app: &App, area: Rect) {
    let mut spans = Vec::new();
    for (i, record) in app.records.iter().enumerate() {
        let label = format!(" {}:{} ", record.name, agent_label(record.agent));
        let style = if i == app.focused {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default()
        };
        spans.push(Span::styled(label, style));
    }
    if spans.is_empty() {
        spans.push(Span::raw(" no tabs — Ctrl+T to create one "));
    }
    frame.render_widget(Line::from(spans), area);
}

fn draw_focused_pane(frame: &mut Frame, app: &App, area: Rect) {
    match app.tabs.get(app.focused) {
        Some(tab) => {
            if let Some(status) = tab.exit_status() {
                let msg = format!("process exited ({status})\n\nPress Ctrl+R to restart this tab.");
                frame.render_widget(Paragraph::new(msg), area);
            } else {
                let pseudo_term = PseudoTerminal::new(tab.screen());
                frame.render_widget(pseudo_term, area);
            }
        }
        None => {
            frame.render_widget(
                Paragraph::new("No tabs open. Press Ctrl+T to create one, Ctrl+Q to quit."),
                area,
            );
        }
    }
}

fn draw_status_line(frame: &mut Frame, app: &App, area: Rect) {
    if let Some(err) = &app.last_error {
        frame.render_widget(
            Paragraph::new(format!("error: {err}")).style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }

    let text = match app.records.get(app.focused) {
        Some(record) => format!(
            "{} [{}{}]  Ctrl+T new  Ctrl+W close  Ctrl+\u{2190}/\u{2192} switch tab  Ctrl+A switch agent  Ctrl+G switch account  Ctrl+R restart  Ctrl+Q quit",
            record.name,
            agent_label(record.agent),
            record
                .claude_account
                .as_ref()
                .map(|a| format!(":{a}"))
                .unwrap_or_default(),
        ),
        None => "Ctrl+T new tab  Ctrl+Q quit".to_string(),
    };
    frame.render_widget(
        Paragraph::new(text).style(Style::default().fg(Color::DarkGray)),
        area,
    );
}

fn agent_label(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}
