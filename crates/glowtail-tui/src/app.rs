use crate::terminal::TerminalState;
use crate::widgets::render;
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use glowtail_core::viewport::Engine;
use std::time::Duration;

pub fn run_tui(mut engine: Engine) -> Result<()> {
    let mut terminal_state = TerminalState::new()?;
    let mut first_row = 0usize;
    let mut follow = true;
    let mut input_mode = InputMode::Normal;
    let mut input = String::new();

    loop {
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
            render(f, &snapshot, follow, &input_mode, &input);
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

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum InputMode {
    Normal,
    Search,
    Filter,
}
