use crate::terminal::TerminalState;
use crate::widgets::{CHROME_HEIGHT, render};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use glowtail_core::events::LogEvent;
use glowtail_core::viewport::Engine;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;

/// How long a status message remains visible before auto-clearing. The
/// previous policy cleared on any keypress, which could erase a filter-error
/// before the user finished reading it.
const STATUS_MESSAGE_TTL: Duration = Duration::from_secs(4);

pub fn run_tui(engine: Engine) -> Result<Engine> {
    run_tui_with_events(engine, None)
}

pub fn run_tui_with_events(
    mut engine: Engine,
    mut events: Option<mpsc::Receiver<LogEvent>>,
) -> Result<Engine> {
    let mut terminal_state = TerminalState::new()?;
    let mut state = TuiState::default();

    loop {
        drain_events(&mut engine, &mut events);
        state.expire_status_if_due();

        let size = terminal_state.terminal.size()?;
        let visible_rows = (size.height as usize).saturating_sub(CHROME_HEIGHT);
        let total = engine.matching_rows_count();
        if state.follow {
            state.first_row = total.saturating_sub(visible_rows);
            state.selected_offset = visible_rows.saturating_sub(1);
        }
        clamp_view(&mut state, visible_rows, total);

        let snapshot = engine.viewport(glowtail_core::model::ViewportRequest {
            first_row: state.first_row,
            row_count: visible_rows,
        });

        terminal_state.terminal.draw(|f| {
            render(
                f,
                &snapshot,
                state.follow,
                state.fold_stacks,
                &state.input_mode,
                &state.input,
                state
                    .selected_offset
                    .min(snapshot.rows.len().saturating_sub(1)),
                state.status_message.as_ref().map(|(text, _)| text.as_str()),
            );
        })?;

        if event::poll(Duration::from_millis(50))?
            && let Event::Key(key) = event::read()?
        {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            match state.input_mode {
                InputMode::Normal => match key.code {
                    KeyCode::Char('q') => break,
                    KeyCode::Char('j') | KeyCode::Down => {
                        move_selection_down(&mut state, visible_rows, total)
                    }
                    KeyCode::Char('k') | KeyCode::Up => move_selection_up(&mut state),
                    KeyCode::Char('g') => {
                        state.follow = false;
                        state.first_row = 0;
                        state.selected_offset = 0;
                    }
                    KeyCode::Char('G') => {
                        state.follow = false;
                        state.first_row = total.saturating_sub(visible_rows);
                        state.selected_offset = visible_rows.saturating_sub(1);
                    }
                    KeyCode::Char('f') => state.follow = !state.follow,
                    KeyCode::Char('b') => toggle_bookmark(&mut engine, &snapshot, &mut state),
                    KeyCode::Char('z') => {
                        state.fold_stacks = !state.fold_stacks;
                        engine.set_stack_trace_folding(state.fold_stacks);
                    }
                    KeyCode::Char('n') => jump_to_search(&mut engine, &snapshot, &mut state, false),
                    KeyCode::Char('N') => jump_to_search(&mut engine, &snapshot, &mut state, true),
                    KeyCode::Char('/') => {
                        state.input_mode = InputMode::Search;
                        state.input.clear();
                    }
                    KeyCode::Char('F') => {
                        state.input_mode = InputMode::Filter;
                        state.input.clear();
                    }
                    _ => {}
                },
                InputMode::Search | InputMode::Filter => match key.code {
                    KeyCode::Esc => {
                        state.input_mode = InputMode::Normal;
                        state.input.clear();
                    }
                    KeyCode::Enter => {
                        apply_input(&mut engine, &mut state);
                        state.input_mode = InputMode::Normal;
                        state.input.clear();
                    }
                    KeyCode::Backspace => {
                        state.input.pop();
                    }
                    KeyCode::Char(c) => state.input.push(c),
                    _ => {}
                },
            }
        }
    }

    Ok(engine)
}

fn apply_input(engine: &mut Engine, state: &mut TuiState) {
    match state.input_mode {
        InputMode::Search => engine.set_search_text(Some(state.input.clone())),
        InputMode::Filter => {
            if state.input.trim().is_empty() {
                engine.clear_filter();
            } else if let Err(err) = glowtail_core::filter::parse_filter_query(&state.input)
                .and_then(|filter| engine.set_filter(filter))
            {
                state.set_status(format!("filter error: {err}"));
            }
        }
        InputMode::Normal => {}
    }
}

fn toggle_bookmark(
    engine: &mut Engine,
    snapshot: &glowtail_core::model::ViewportSnapshot,
    state: &mut TuiState,
) {
    if snapshot.rows.is_empty() {
        state.set_status("no row to bookmark");
        return;
    }
    let index = state
        .selected_offset
        .min(snapshot.rows.len().saturating_sub(1));
    if let Some(row) = snapshot.rows.get(index) {
        engine.toggle_bookmark(row.row_id, None);
    }
}

fn jump_to_search(
    engine: &mut Engine,
    snapshot: &glowtail_core::model::ViewportSnapshot,
    state: &mut TuiState,
    reverse: bool,
) {
    let current = snapshot
        .rows
        .get(state.selected_offset)
        .map(|row| row.row_id);
    let Some(row_id) = engine.next_search_result(current, reverse) else {
        state.set_status("no search results");
        return;
    };
    let Some(position) = engine.filtered_position_for_row(row_id) else {
        return;
    };
    state.follow = false;
    state.first_row = position.saturating_sub(state.selected_offset);
}

fn move_selection_down(state: &mut TuiState, visible_rows: usize, total: usize) {
    state.follow = false;
    if state.selected_offset + 1 < visible_rows {
        state.selected_offset += 1;
    } else if state.first_row + visible_rows < total {
        state.first_row += 1;
    }
}

fn move_selection_up(state: &mut TuiState) {
    state.follow = false;
    if state.selected_offset > 0 {
        state.selected_offset -= 1;
    } else if state.first_row > 0 {
        state.first_row -= 1;
    }
}

fn clamp_view(state: &mut TuiState, visible_rows: usize, total: usize) {
    let max_first_row = total.saturating_sub(visible_rows);
    if state.first_row > max_first_row {
        state.first_row = max_first_row;
    }
    let visible_in_window = total.saturating_sub(state.first_row).min(visible_rows);
    if visible_in_window == 0 {
        state.selected_offset = 0;
    } else if state.selected_offset >= visible_in_window {
        state.selected_offset = visible_in_window - 1;
    }
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

struct TuiState {
    first_row: usize,
    selected_offset: usize,
    follow: bool,
    fold_stacks: bool,
    input_mode: InputMode,
    input: String,
    /// Active status message and the instant after which it should auto-clear.
    /// Replaces the previous "clear on any keypress" policy that erased
    /// filter-error feedback before the user could read it (review L4).
    status_message: Option<(String, Instant)>,
}

impl Default for TuiState {
    fn default() -> Self {
        Self {
            first_row: 0,
            selected_offset: 0,
            follow: true,
            fold_stacks: false,
            input_mode: InputMode::Normal,
            input: String::new(),
            status_message: None,
        }
    }
}

impl TuiState {
    fn set_status(&mut self, message: impl Into<String>) {
        self.status_message = Some((message.into(), Instant::now() + STATUS_MESSAGE_TTL));
    }

    fn expire_status_if_due(&mut self) {
        if let Some((_, expires_at)) = self.status_message.as_ref()
            && Instant::now() >= *expires_at
        {
            self.status_message = None;
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub enum InputMode {
    Normal,
    Search,
    Filter,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state_at(first_row: usize, selected_offset: usize) -> TuiState {
        TuiState {
            first_row,
            selected_offset,
            ..TuiState::default()
        }
    }

    #[test]
    fn clamp_view_handles_empty_total() {
        let mut state = state_at(7, 4);
        clamp_view(&mut state, 10, 0);
        assert_eq!(state.first_row, 0);
        assert_eq!(state.selected_offset, 0);
    }

    #[test]
    fn clamp_view_caps_selection_to_visible_window() {
        let mut state = state_at(0, 30);
        clamp_view(&mut state, 5, 10);
        assert_eq!(state.first_row, 0);
        assert_eq!(state.selected_offset, 4);
    }

    #[test]
    fn clamp_view_walks_first_row_back_when_past_end() {
        let mut state = state_at(20, 0);
        clamp_view(&mut state, 5, 8);
        assert_eq!(state.first_row, 3);
        assert_eq!(state.selected_offset, 0);
    }

    #[test]
    fn set_status_then_expire_after_ttl_clears_message() {
        let mut state = TuiState::default();
        state.set_status("filter error: bad");
        assert!(state.status_message.is_some());

        // Force expiration by rewinding the deadline into the past.
        if let Some((_, expires_at)) = state.status_message.as_mut() {
            *expires_at = Instant::now() - Duration::from_secs(1);
        }
        state.expire_status_if_due();
        assert!(state.status_message.is_none());
    }
}
