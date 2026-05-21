use crate::terminal::TerminalState;
use crate::widgets::render;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use glowtail_core::events::LogEvent;
use glowtail_core::viewport::Engine;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

pub fn run_tui(engine: Engine) -> Result<Engine> {
    run_tui_with_events(engine, None)
}

pub fn run_tui_with_events(
    mut engine: Engine,
    mut events: Option<mpsc::Receiver<LogEvent>>,
) -> Result<Engine> {
    let mut terminal_state = TerminalState::new()?;
    let mut first_row = 0usize;
    let mut follow = true;
    let mut fold_stacks = false;
    let mut input_mode = InputMode::Normal;
    let mut input = String::new();

    loop {
        drain_events(&mut engine, &mut events);

        let size = terminal_state.terminal.size()?;
        let visible_rows = size.height.saturating_sub(2) as usize;
        if follow {
            let total = engine.matching_rows_count();
            first_row = total.saturating_sub(visible_rows);
        }

        let snapshot = engine.viewport(glowtail_core::model::ViewportRequest {
            first_row,
            row_count: visible_rows,
        });

        terminal_state.terminal.draw(|f| {
            render(f, &snapshot, follow, fold_stacks, &input_mode, &input);
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match input_mode {
                InputMode::Normal => match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        follow = false;
                        first_row = first_row.saturating_add(1)
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        follow = false;
                        first_row = first_row.saturating_sub(1)
                    }
                    KeyCode::Char('g') => {
                        follow = false;
                        first_row = 0;
                    }
                    KeyCode::Char('G') => {
                        follow = false;
                        first_row = engine.matching_rows_count().saturating_sub(visible_rows);
                    }
                    KeyCode::Char('f') => follow = !follow,
                    KeyCode::Char('b') => {
                        if let Some(row) = snapshot.rows.first() {
                            engine.toggle_bookmark(row.row_id, None);
                        }
                    }
                    KeyCode::Char('z') => {
                        fold_stacks = !fold_stacks;
                        engine.set_stack_trace_folding(fold_stacks);
                    }
                    KeyCode::Char('n') => {
                        if let Some(row_id) = engine
                            .next_search_result(snapshot.rows.first().map(|row| row.row_id), false)
                            && let Some(position) = engine.filtered_position_for_row(row_id)
                        {
                            follow = false;
                            first_row = position;
                        }
                    }
                    KeyCode::Char('N') => {
                        if let Some(row_id) = engine
                            .next_search_result(snapshot.rows.first().map(|row| row.row_id), true)
                            && let Some(position) = engine.filtered_position_for_row(row_id)
                        {
                            follow = false;
                            first_row = position;
                        }
                    }
                    KeyCode::Char('/') => {
                        input_mode = InputMode::Search;
                        input.clear();
                    }
                    KeyCode::Char('F') => {
                        input_mode = InputMode::Filter;
                        input.clear();
                    }
                    _ => {}
                },
                InputMode::Search | InputMode::Filter => match key.code {
                    KeyCode::Esc => {
                        input_mode = InputMode::Normal;
                        input.clear();
                    }
                    KeyCode::Enter => {
                        match input_mode {
                            InputMode::Search => engine.set_search_text(Some(input.clone())),
                            InputMode::Filter => {
                                if input.trim().is_empty() {
                                    engine.clear_filter();
                                } else {
                                    let _ = engine.set_filter(
                                        glowtail_core::filter::FilterExpr::Contains(input.clone()),
                                    );
                                }
                            }
                            InputMode::Normal => {}
                        }
                        input_mode = InputMode::Normal;
                        input.clear();
                    }
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char(c) => input.push(c),
                    _ => {}
                },
            }
        }
    }

    Ok(engine)
}

fn drain_events(engine: &mut Engine, events: &mut Option<mpsc::Receiver<LogEvent>>) {
    let Some(rx) = events.as_mut() else {
        return;
    };

    let mut disconnected = false;
    loop {
        match rx.try_recv() {
            Ok(LogEvent::RowAppended(row)) => engine.append_row(row),
            Ok(LogEvent::SourceAdded { source_id, path }) => {
                engine.add_source(source_id, path.display().to_string());
            }
            Ok(_) => {}
            Err(TryRecvError::Empty) => break,
            Err(TryRecvError::Disconnected) => {
                disconnected = true;
                break;
            }
        }
    }

    if disconnected {
        *events = None;
    }
}

#[derive(Debug, Clone, Copy)]
enum InputMode {
    Normal,
    Search,
    Filter,
}
