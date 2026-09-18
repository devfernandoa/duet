use crate::agent::Agent;
use crate::app::{App, Overlay};
use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use tui_term::widget::PseudoTerminal;

const INK: Color = Color::Rgb(220, 226, 232);
const MUTED: Color = Color::Rgb(125, 139, 154);
const PANEL: Color = Color::Rgb(20, 27, 36);
const ACCENT: Color = Color::Rgb(91, 179, 255);
const VIOLET: Color = Color::Rgb(177, 136, 255);

pub fn draw(frame: &mut Frame, app: &App) {
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(13, 18, 25))),
        frame.area(),
    );
    let columns = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Length(31), Constraint::Min(20)])
        .split(frame.area());
    draw_sidebar(frame, app, columns[0]);
    draw_workspace(frame, app, columns[1]);
    if let Some(overlay) = &app.overlay {
        draw_overlay(frame, app, overlay);
    }
}

fn draw_sidebar(frame: &mut Frame, app: &App, area: Rect) {
    frame.render_widget(
        Block::default()
            .borders(Borders::RIGHT)
            .border_style(Style::default().fg(Color::Rgb(47, 61, 77)))
            .style(Style::default().bg(PANEL)),
        area,
    );
    let inner = Rect {
        x: area.x + 2,
        y: area.y + 1,
        width: area.width.saturating_sub(4),
        height: area.height.saturating_sub(2),
    };
    let mut lines = vec![
        Line::from(Span::styled(
            "DUET",
            Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "YOUR AGENT WORKSPACE",
            Style::default().fg(MUTED),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "SESSIONS",
            Style::default().fg(MUTED).add_modifier(Modifier::BOLD),
        )),
    ];
    if app.records.is_empty() {
        lines.push(Line::from(Span::styled(
            "  No sessions yet",
            Style::default().fg(MUTED),
        )));
    }
    for (index, record) in app.records.iter().enumerate() {
        let selected = index == app.focused;
        let marker = if selected { "▌" } else { " " };
        let title_style = if selected {
            Style::default().fg(INK).add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(MUTED)
        };
        lines.push(Line::from(vec![
            Span::styled(marker, Style::default().fg(ACCENT)),
            Span::styled(format!(" {}", record.name), title_style),
        ]));
        let agent = match record.agent {
            Agent::Claude => "CLAUDE",
            Agent::Codex => "CODEX",
        };
        let detail = match (&record.agent, &record.claude_account) {
            (Agent::Claude, Some(account)) => format!("    {agent} · {account}"),
            _ => format!("    {agent}"),
        };
        lines.push(Line::from(Span::styled(
            detail,
            Style::default().fg(if selected {
                VIOLET
            } else {
                Color::Rgb(83, 97, 113)
            }),
        )));
    }
    let available = inner.height.saturating_sub(lines.len() as u16 + 4);
    for _ in 0..available {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        "+  New session",
        Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(Span::styled(
        "Ctrl+T",
        Style::default().fg(MUTED),
    )));
    frame.render_widget(Paragraph::new(lines), inner);
}

fn draw_workspace(frame: &mut Frame, app: &App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(area);
    let title = app
        .records
        .get(app.focused)
        .map(|r| {
            format!(
                "  {}  ·  {} ",
                r.name,
                match r.agent {
                    Agent::Claude => "Claude",
                    Agent::Codex => "Codex",
                }
            )
        })
        .unwrap_or_else(|| "  Welcome to Duet ".to_string());
    frame.render_widget(
        Paragraph::new(title)
            .style(Style::default().fg(INK).bg(Color::Rgb(17, 24, 33)))
            .block(
                Block::default()
                    .borders(Borders::BOTTOM)
                    .border_style(Style::default().fg(Color::Rgb(47, 61, 77))),
            ),
        rows[0],
    );
    match app.tabs.get(app.focused) {
        Some(tab) if tab.exit_status().is_none() => frame.render_widget(PseudoTerminal::new(tab.screen()), rows[1]),
        Some(tab) => frame.render_widget(Paragraph::new(format!("This session has stopped ({:?}).\n\nPress Ctrl+R to reconnect it.", tab.exit_status())).style(Style::default().fg(MUTED)), rows[1]),
        None => frame.render_widget(Paragraph::new("Start a session to open Claude or Codex here.\n\nYour sessions and working folders return when you launch Duet again.").style(Style::default().fg(MUTED)).alignment(Alignment::Center).wrap(Wrap { trim: true }), rows[1]),
    };
    let footer = if let Some(error) = &app.last_error {
        format!("  {error}")
    } else {
        "  Ctrl+T new  ·  Ctrl+←/→ switch  ·  Ctrl+O add account  ·  Ctrl+A handoff  ·  Ctrl+W close  ·  Ctrl+Q quit".to_string()
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(MUTED).bg(Color::Rgb(17, 24, 33))),
        rows[2],
    );
}

fn draw_overlay(frame: &mut Frame, app: &App, overlay: &Overlay) {
    let box_area = centered_rect(58, 42, frame.area());
    frame.render_widget(Clear, box_area);
    let (title, lines) = match overlay {
        Overlay::AccountSetup { name } => (
            "Set up your first Claude account",
            vec![
                Line::from("Duet keeps each Claude login in its own private profile."),
                Line::from("Use a label you will recognize later."),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Account name  ", Style::default().fg(MUTED)),
                    Span::styled(format!("{}▏", name), Style::default().fg(INK)),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    "Enter to continue",
                    Style::default().fg(ACCENT),
                )),
            ],
        ),
        Overlay::NewSession {
            agent,
            account_index,
        } => {
            let agent_text = match agent {
                Agent::Claude => "Claude",
                Agent::Codex => "Codex",
            };
            let names = app.account_names();
            let account = names
                .get(*account_index)
                .map(String::as_str)
                .unwrap_or("no account");
            let account_line = if *agent == Agent::Claude {
                format!("Account     ◀  {account}  ▶")
            } else {
                "Account     Managed by your Codex CLI".to_string()
            };
            (
                "New session",
                vec![
                    Line::from(Span::styled(
                        "Choose what you want to open.",
                        Style::default().fg(MUTED),
                    )),
                    Line::from(""),
                    Line::from(vec![
                        Span::styled("Agent       ", Style::default().fg(MUTED)),
                        Span::styled(
                            agent_text,
                            Style::default().fg(INK).add_modifier(Modifier::BOLD),
                        ),
                    ]),
                    Line::from(Span::styled(account_line, Style::default().fg(INK))),
                    Line::from(""),
                    Line::from(Span::styled(
                        "Tab / ↑↓ agent  ·  ←→ account  ·  Enter open  ·  Esc cancel",
                        Style::default().fg(ACCENT),
                    )),
                ],
            )
        }
    };
    let block = Block::default()
        .title(format!(" {title} "))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(ACCENT))
        .style(Style::default().bg(Color::Rgb(25, 34, 46)));
    frame.render_widget(
        Paragraph::new(Text::from(lines))
            .block(block)
            .wrap(Wrap { trim: true }),
        box_area,
    );
}

fn centered_rect(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(vertical[1])[1]
}
