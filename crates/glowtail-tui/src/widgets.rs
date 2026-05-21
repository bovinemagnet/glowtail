use glowtail_core::model::{SpanKind, ViewportSnapshot};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

/// Total number of terminal rows the chrome (status bar + help line + the two
/// border rows of the viewport block) consumes. Exposed so `app.rs` can
/// derive `visible_rows` from the terminal height without hard-coding the
/// layout in two places.
pub const CHROME_HEIGHT: usize = 4;

#[allow(clippy::too_many_arguments)]
pub fn render(
    frame: &mut Frame,
    snapshot: &ViewportSnapshot,
    follow: bool,
    fold_stacks: bool,
    input_mode: &impl std::fmt::Debug,
    input: &str,
    selected_offset: usize,
    status_message: Option<&str>,
) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let sources = snapshot
        .source_summaries
        .iter()
        .map(|source| format!("{}:{}", source.name, source.rows))
        .collect::<Vec<_>>()
        .join(" ");
    let status_extra = status_message
        .map(|message| format!(" | {message}"))
        .unwrap_or_default();
    let status = Paragraph::new(format!(
        "rows={}/{} warn={} err={} follow={} fold={} timeline={} mode={input_mode:?} {sources}{status_extra}",
        snapshot.total_matching_rows,
        snapshot.total_rows,
        snapshot.level_counts.warn,
        snapshot.level_counts.error + snapshot.level_counts.fatal,
        follow,
        fold_stacks,
        snapshot.timeline.len()
    ));
    frame.render_widget(status, layout[0]);

    let lines: Vec<Line> = snapshot
        .rows
        .iter()
        .enumerate()
        .map(|(index, row)| {
            let mut spans = Vec::new();
            let cursor = if index == selected_offset { '>' } else { ' ' };
            spans.push(Span::styled(
                format!("{cursor} "),
                Style::default().fg(Color::Yellow),
            ));
            if row.is_bookmarked {
                spans.push(Span::styled("* ", Style::default().fg(Color::Magenta)));
            }
            if row.folded_stack_rows > 0 {
                spans.push(Span::styled(
                    format!("+{} ", row.folded_stack_rows),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            if let Some(source_name) = row.source_name.as_ref() {
                spans.push(Span::styled(
                    format!("[{source_name}] "),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            spans.extend(
                row.spans
                    .iter()
                    .map(|span| {
                        let style = match span.kind {
                            SpanKind::Error => Style::default().fg(Color::Red),
                            SpanKind::Warning => Style::default().fg(Color::Yellow),
                            SpanKind::SearchMatch => {
                                Style::default().fg(Color::Black).bg(Color::Green)
                            }
                            SpanKind::Timestamp => Style::default().fg(Color::Blue),
                            SpanKind::JsonKey => Style::default().fg(Color::Cyan),
                            SpanKind::JsonValue => Style::default().fg(Color::Green),
                            _ => Style::default(),
                        };
                        Span::styled(span.text.to_string(), style)
                    })
                    .collect::<Vec<_>>(),
            );
            let mut line = Line::from(spans);
            if index == selected_offset {
                line = line.style(Style::default().bg(Color::Rgb(40, 60, 80)));
            }
            line
        })
        .collect();

    let viewport =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("glowtail"));
    frame.render_widget(viewport, layout[1]);

    let help = Paragraph::new(format!(
        "q quit | j/k select | f follow | b bookmark | z fold | / search | n/N next | F query | Esc clear | input={input}"
    ));
    frame.render_widget(help, layout[2]);
}
