use glowtail_core::model::{SpanKind, ViewportSnapshot};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph},
};

pub fn render(
    frame: &mut Frame,
    snapshot: &ViewportSnapshot,
    follow: bool,
    input_mode: &impl std::fmt::Debug,
    input: &str,
) {
    let layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(frame.area());

    let status = Paragraph::new(format!(
        "rows={} follow={} mode={input_mode:?}",
        snapshot.total_matching_rows, follow
    ));
    frame.render_widget(status, layout[0]);

    let lines: Vec<Line> = snapshot
        .rows
        .iter()
        .map(|row| {
            let spans = row
                .spans
                .iter()
                .map(|span| {
                    let style = match span.kind {
                        SpanKind::Error => Style::default().fg(Color::Red),
                        SpanKind::Warning => Style::default().fg(Color::Yellow),
                        SpanKind::SearchMatch => Style::default().fg(Color::Black).bg(Color::Green),
                        SpanKind::Timestamp => Style::default().fg(Color::Blue),
                        _ => Style::default(),
                    };
                    Span::styled(span.text.to_string(), style)
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect();

    let viewport =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title("glowtail"));
    frame.render_widget(viewport, layout[1]);

    let help = Paragraph::new(format!(
        "q quit | j/k scroll | f follow | / search | F filter | Esc clear | input={input}"
    ));
    frame.render_widget(help, layout[2]);
}
