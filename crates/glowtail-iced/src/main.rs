//! Iced-based desktop UI for glowtail. Third long-term sibling to
//! `glowtail-gui` (egui/eframe) and `glowtail-gpui` (GPUI).
//!
//! The crate stays as thin a translation layer as possible: it parses CLI
//! flags identical to the other front-ends, reuses
//! [`glowtail_ui_common`] for session/filter/tailer plumbing, and maps the
//! semantic [`SpanKind`] values returned by [`Engine::viewport`] to
//! [`iced::Color`] in [`span_colour`]. No engine logic lives here.

use anyhow::{Context, Result};
use clap::Parser;
use glowtail_core::model::SourceSummary;
use glowtail_core::prelude::*;
use glowtail_ui_common::{
    LevelArg, LiveTail, apply_filters, load_session, parser_from_flags, save_session, start_tailers,
};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{
    Row, button, column, container, row, scrollable, scrollable::Direction, scrollable::Scrollbar,
    text, text_input,
};
use iced::{
    Alignment, Background, Border, Color, Element, Length, Subscription, Task, Theme, time,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc::error::TryRecvError;

/// How often the Tick subscription fires to drain new rows from the
/// tailer channel and refresh the cached viewport. 33 ms ≈ 30 Hz, matching
/// the egui front-end's redraw cadence and giving live tail a snappy feel
/// without burning idle CPU.
const POLL_INTERVAL: Duration = Duration::from_millis(33);

/// Number of rows the engine is asked to render per viewport snapshot.
/// Iced has no first-class virtualised list; we ask the engine for a fixed
/// window and the user advances `first_row` with Home/End/PageUp/PageDown.
const PAGE_SIZE: usize = 200;

#[derive(Debug, Parser)]
#[command(name = "glowtail-iced")]
#[command(about = "Iced/wgpu glowtail desktop UI")]
struct Args {
    #[arg(required = true)]
    paths: Vec<PathBuf>,
    #[arg(long)]
    json: bool,
    #[arg(long)]
    plain: bool,
    #[arg(long)]
    filter: Option<String>,
    #[arg(long)]
    level: Option<LevelArg>,
    #[arg(long)]
    no_follow: bool,
    #[arg(long)]
    from_start: bool,
    #[arg(long)]
    session: Option<PathBuf>,
    #[arg(long)]
    use_filter: Option<String>,
    #[arg(long)]
    save_filter: Option<String>,
    /// Retain at most this many rows; older rows are dropped from the front
    /// of the buffer when the cap is exceeded. `0` means unbounded (default).
    #[arg(long)]
    max_rows: Option<usize>,
}

/// Treat `--max-rows 0` and an absent flag as "unbounded" so the surface is
/// forgiving — `0` reading as "no rows retained" is a usability trap.
fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let app = GlowtailIced::new(args)?;
    iced::application("glowtail (iced)", GlowtailIced::update, GlowtailIced::view)
        .subscription(GlowtailIced::subscription)
        .theme(|_| iced::Theme::Dark)
        .run_with(move || (app, Task::none()))
        .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Which text input (if any) currently has focus. Single-letter shortcuts
/// (b, n, f, j/k, 0-6, s, /, ?) only fire in [`InputMode::Normal`] — the
/// keyboard subscription always emits them, but the update handler gates
/// them by mode so typing into a focused filter/search input doesn't
/// accidentally toggle bookmarks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    Normal,
    Filter,
    Search,
}

#[derive(Debug, Clone)]
enum Message {
    FilterInputChanged(String),
    FilterSubmitted,
    SearchInputChanged(String),
    SearchSubmitted,
    /// `/` — focus and select the filter input.
    EnterFilterMode,
    /// `?` — focus and select the search input.
    EnterSearchMode,
    /// Escape — exit any active input mode and return to Normal.
    EscapePressed,
    LevelCycled,
    /// `0`-`6` — set the level filter to a specific severity (None for `0`).
    LevelSetTo(Option<LevelArg>),
    FollowToggled,
    /// `s` — cycle through the saved filters loaded from `--session`.
    /// Order: None → first → … → last → None.
    SavedFilterCycled,
    /// `b` — toggle bookmark on the currently selected row.
    BookmarkToggled,
    /// `n` / `N` — jump the selection cursor to the next/previous search
    /// result and scroll the viewport to keep it visible.
    NextSearchResult,
    PrevSearchResult,
    /// `j` / `↓` — move the row-selection cursor down by one row.
    SelectionMoveDown,
    /// `k` / `↑` — move the row-selection cursor up by one row.
    SelectionMoveUp,
    /// `z` — toggle stack-trace folding (`engine.set_stack_trace_folding`).
    StackFoldingToggled,
    PageUp,
    PageDown,
    HomePressed,
    EndPressed,
    Tick,
}

struct GlowtailIced {
    engine: Engine,
    filter_text: String,
    search_text: String,
    level: Option<LevelArg>,
    follow: bool,
    first_row: usize,
    cached_rows: Vec<RowPresentation>,
    /// Source summaries from the most recent snapshot. Renders into the
    /// left sidebar so the user can see at a glance which files are
    /// contributing rows under the current filter — same shape as the
    /// gpui front-end's sidebar.
    cached_sources: Vec<SourceSummary>,
    total_matching_rows: usize,
    total_rows: usize,
    /// Row currently under the selection cursor. Anchors bookmark toggles
    /// and the detail panel. `None` means "no selection yet" — the first
    /// `j`/`↓` press picks the top of the viewport.
    selected_row_id: Option<RowId>,
    /// Position into `cached_rows` for the saved-filter cycle. `None` means
    /// "no saved filter active". Mirrors the `s`-cycling state in
    /// `glowtail-gpui` so users can move between front-ends without
    /// rebuilding muscle memory.
    saved_filter_index: Option<usize>,
    mode: InputMode,
    /// `z` toggle — when `true` the engine collapses stack-trace
    /// continuation lines into the header row's folded badge.
    fold_stacks: bool,
    filter_input_id: text_input::Id,
    search_input_id: text_input::Id,
    session_path: Option<PathBuf>,
    /// Kept alive to host the `FileTailer` tasks. Dropped *after*
    /// `live_tail` so the runtime drives the tasks to completion when the
    /// channel senders close.
    #[allow(dead_code)]
    runtime: Runtime,
    live_tail: Option<LiveTail>,
    status_message: Option<String>,
    error_message: Option<String>,
}

impl GlowtailIced {
    fn new(args: Args) -> Result<Self> {
        let session = load_session(args.session.as_ref())?;
        let parser = parser_from_flags(args.json, args.plain);

        // Mirror the gui crate: accumulate per-path read errors instead of
        // bailing on the first failure so a single unreadable path doesn't
        // stop the UI from opening with whatever else loaded fine.
        let mut load_errors: Vec<String> = Vec::new();
        let mut engine = if !args.no_follow && args.from_start {
            Engine::with_session(session)
        } else {
            let mut engine = Engine::with_session(session);
            for path in &args.paths {
                if let Err(err) = engine.load_file(path, parser.as_ref()) {
                    load_errors.push(format!("failed to read {}: {err}", path.display()));
                }
            }
            engine
        };
        engine.set_max_rows(normalise_max_rows(args.max_rows));

        let mut error_message = None;
        if let Err(err) = apply_filters(
            &mut engine,
            args.filter.clone(),
            args.level,
            args.use_filter.clone(),
            args.save_filter.clone(),
        ) {
            error_message = Some(err.to_string());
        }

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("glowtail-iced-tail")
            .build()
            .context("failed to create async runtime")?;
        let live_tail = if args.no_follow {
            None
        } else {
            Some(start_tailers(
                &runtime,
                &args.paths,
                Arc::clone(&parser),
                args.from_start,
            ))
        };

        let follow = live_tail.is_some();
        let status_message = if load_errors.is_empty() {
            None
        } else {
            Some(load_errors.join("; "))
        };

        let mut app = Self {
            engine,
            filter_text: args.filter.unwrap_or_default(),
            search_text: String::new(),
            level: args.level,
            follow,
            first_row: 0,
            cached_rows: Vec::new(),
            cached_sources: Vec::new(),
            total_matching_rows: 0,
            total_rows: 0,
            selected_row_id: None,
            saved_filter_index: None,
            mode: InputMode::Normal,
            fold_stacks: false,
            filter_input_id: text_input::Id::unique(),
            search_input_id: text_input::Id::unique(),
            session_path: args.session,
            runtime,
            live_tail,
            status_message,
            error_message,
        };
        app.refresh_snapshot();
        Ok(app)
    }

    /// Drain every pending `LogEvent` from the tailer channel into the
    /// engine. Returns true when at least one row was appended, so the
    /// caller can decide whether to snap the viewport to the tail.
    fn drain_events(&mut self) -> bool {
        let Some(live_tail) = self.live_tail.as_mut() else {
            return false;
        };
        let mut appended = false;
        loop {
            match live_tail.receiver.try_recv() {
                Ok(LogEvent::RowAppended(row)) => {
                    self.engine.append_row(row);
                    appended = true;
                }
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    self.engine
                        .add_source(source_id, path.display().to_string());
                }
                Ok(LogEvent::SourceError { message, .. }) => {
                    self.status_message = Some(message);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        appended
    }

    /// Rebuild [`Self::cached_rows`] from the engine. `view()` only takes
    /// `&self`, so any state mutation that could change what's on screen
    /// must call this so the next render sees the updated rows.
    fn refresh_snapshot(&mut self) {
        let snapshot = self.engine.viewport(ViewportRequest {
            first_row: self.first_row,
            row_count: PAGE_SIZE,
        });
        self.total_matching_rows = snapshot.total_matching_rows;
        self.total_rows = snapshot.total_rows;
        self.cached_rows = snapshot.rows;
        self.cached_sources = snapshot.source_summaries;
    }

    fn snap_to_tail(&mut self) {
        self.first_row = self.total_matching_rows.saturating_sub(PAGE_SIZE);
    }

    /// Scroll the viewport so that the row at `position` (within the
    /// filtered set) is visible. Used by selection navigation and the
    /// n/N search keys so the cursor doesn't disappear off-screen.
    fn scroll_to_position(&mut self, position: usize) {
        if position < self.first_row {
            self.first_row = position;
        } else if position >= self.first_row + PAGE_SIZE {
            self.first_row = position.saturating_sub(PAGE_SIZE - 1);
        }
    }

    fn apply_current_filters(&mut self) {
        match apply_filters(
            &mut self.engine,
            Some(self.filter_text.clone()),
            self.level,
            None,
            None,
        ) {
            Ok(_) => self.error_message = None,
            Err(err) => self.error_message = Some(err.to_string()),
        }
        self.refresh_snapshot();
    }

    fn move_selection(&mut self, delta: isize) -> Task<Message> {
        if self.total_matching_rows == 0 {
            return Task::none();
        }
        self.follow = false;
        let current_position = self
            .selected_row_id
            .and_then(|id| self.engine.filtered_position_for_row(id))
            .unwrap_or(self.first_row);
        let max = self.total_matching_rows.saturating_sub(1) as isize;
        let new_position = (current_position as isize + delta).clamp(0, max) as usize;
        if let Some(row) = self.engine.present_row_at(new_position) {
            self.selected_row_id = Some(row.row_id);
            self.scroll_to_position(new_position);
        }
        self.refresh_snapshot();
        Task::none()
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::FilterInputChanged(value) => {
                self.filter_text = value;
                Task::none()
            }
            Message::FilterSubmitted => {
                self.apply_current_filters();
                self.mode = InputMode::Normal;
                Task::none()
            }
            Message::SearchInputChanged(value) => {
                self.search_text = value;
                Task::none()
            }
            Message::SearchSubmitted => {
                let search = if self.search_text.is_empty() {
                    None
                } else {
                    Some(self.search_text.clone())
                };
                self.engine.set_search_text(search);
                self.refresh_snapshot();
                self.mode = InputMode::Normal;
                Task::none()
            }
            Message::EnterFilterMode => {
                self.mode = InputMode::Filter;
                text_input::focus(self.filter_input_id.clone())
            }
            Message::EnterSearchMode => {
                self.mode = InputMode::Search;
                text_input::focus(self.search_input_id.clone())
            }
            Message::EscapePressed => {
                self.mode = InputMode::Normal;
                Task::none()
            }
            Message::LevelCycled => {
                self.level = cycle_level(self.level);
                self.apply_current_filters();
                Task::none()
            }
            Message::LevelSetTo(level) if self.mode == InputMode::Normal => {
                self.level = level;
                self.apply_current_filters();
                Task::none()
            }
            Message::FollowToggled if self.mode == InputMode::Normal => {
                self.follow = !self.follow;
                if self.follow {
                    self.snap_to_tail();
                    self.refresh_snapshot();
                }
                Task::none()
            }
            Message::SavedFilterCycled if self.mode == InputMode::Normal => {
                self.cycle_saved_filter();
                Task::none()
            }
            Message::BookmarkToggled if self.mode == InputMode::Normal => {
                if let Some(row_id) = self.selected_row_id {
                    self.engine.toggle_bookmark(row_id, None);
                    self.refresh_snapshot();
                } else {
                    self.status_message =
                        Some("select a row first (j/k or ↑/↓) before bookmarking".into());
                }
                Task::none()
            }
            Message::NextSearchResult if self.mode == InputMode::Normal => {
                self.jump_search(false);
                Task::none()
            }
            Message::PrevSearchResult if self.mode == InputMode::Normal => {
                self.jump_search(true);
                Task::none()
            }
            Message::SelectionMoveUp if self.mode == InputMode::Normal => self.move_selection(-1),
            Message::SelectionMoveDown if self.mode == InputMode::Normal => self.move_selection(1),
            Message::StackFoldingToggled if self.mode == InputMode::Normal => {
                self.fold_stacks = !self.fold_stacks;
                self.engine.set_stack_trace_folding(self.fold_stacks);
                self.refresh_snapshot();
                Task::none()
            }
            Message::PageUp if self.mode == InputMode::Normal => {
                self.follow = false;
                self.first_row = self.first_row.saturating_sub(PAGE_SIZE);
                self.refresh_snapshot();
                Task::none()
            }
            Message::PageDown if self.mode == InputMode::Normal => {
                self.follow = false;
                self.first_row = self
                    .first_row
                    .saturating_add(PAGE_SIZE)
                    .min(self.total_matching_rows.saturating_sub(1));
                self.refresh_snapshot();
                Task::none()
            }
            Message::HomePressed if self.mode == InputMode::Normal => {
                self.follow = false;
                self.first_row = 0;
                self.refresh_snapshot();
                Task::none()
            }
            Message::EndPressed if self.mode == InputMode::Normal => {
                self.follow = true;
                self.snap_to_tail();
                self.refresh_snapshot();
                Task::none()
            }
            Message::Tick => {
                let appended = self.drain_events();
                if appended && self.follow {
                    self.snap_to_tail();
                }
                if appended || self.follow {
                    self.refresh_snapshot();
                }
                Task::none()
            }
            // Single-letter and selection-shortcut messages emitted while a
            // text input has focus fall through here. The text input has
            // already consumed the keystroke via its own `on_input` handler;
            // ignoring the shortcut leaves the typed character in the
            // input and prevents accidental UI toggles mid-type.
            _ => Task::none(),
        }
    }

    /// Jump the selection cursor to the next/previous search match and
    /// scroll the viewport so it stays visible. `glowtail-gpui` uses the
    /// same engine method, so n/N feels identical between front-ends.
    fn jump_search(&mut self, reverse: bool) {
        let Some(next) = self
            .engine
            .next_search_result(self.selected_row_id, reverse)
        else {
            self.status_message = Some("no search matches".into());
            return;
        };
        self.selected_row_id = Some(next);
        if let Some(position) = self.engine.filtered_position_for_row(next) {
            self.scroll_to_position(position);
        }
        self.follow = false;
        self.refresh_snapshot();
    }

    /// Cycle through the saved filters loaded from `--session`. Order
    /// matches `glowtail-gpui`: None → 0 → 1 → … → N-1 → None.
    fn cycle_saved_filter(&mut self) {
        let count = self.engine.session().saved_filters.len();
        if count == 0 {
            self.status_message = Some("no saved filters in session".into());
            return;
        }
        let next_index = match self.saved_filter_index {
            None => Some(0),
            Some(i) if i + 1 < count => Some(i + 1),
            Some(_) => None,
        };
        self.saved_filter_index = next_index;
        match next_index {
            Some(index) => {
                let name = self.engine.session().saved_filters[index].name.clone();
                match self.engine.apply_saved_filter(&name) {
                    Ok(true) => {
                        self.error_message = None;
                        self.status_message = Some(format!("saved filter: {name}"));
                    }
                    Ok(false) | Err(_) => {
                        self.error_message = Some(format!("could not load saved filter {name}"));
                    }
                }
            }
            None => {
                // Returning to "no saved filter" — clear and re-apply the
                // typed filter/level state so the user lands where they
                // started, not on an empty FilterExpr::All.
                self.apply_current_filters();
                self.status_message = Some("saved filter: (none)".into());
            }
        }
        self.refresh_snapshot();
    }

    fn view(&self) -> Element<'_, Message> {
        let filter_input = text_input("filter…", &self.filter_text)
            .id(self.filter_input_id.clone())
            .on_input(Message::FilterInputChanged)
            .on_submit(Message::FilterSubmitted)
            .width(Length::FillPortion(3));

        let search_input = text_input("search (?)", &self.search_text)
            .id(self.search_input_id.clone())
            .on_input(Message::SearchInputChanged)
            .on_submit(Message::SearchSubmitted)
            .width(Length::FillPortion(2));

        let level_label = format!("level: {}", level_label(self.level));
        let level_button = button(text(level_label)).on_press(Message::LevelCycled);

        let follow_label = if self.follow {
            "follow: on"
        } else {
            "follow: off"
        };
        let follow_button = button(text(follow_label)).on_press(Message::FollowToggled);

        let saved_label = match self.saved_filter_index {
            Some(index) => format!("saved: {}", self.engine.session().saved_filters[index].name),
            None => String::from("saved: (none)"),
        };
        let saved_button = button(text(saved_label)).on_press(Message::SavedFilterCycled);

        let top_bar = row![
            filter_input,
            search_input,
            level_button,
            follow_button,
            saved_button,
        ]
        .spacing(8)
        .padding(8)
        .align_y(Alignment::Center);

        let mut rows_column = column![].spacing(2);
        let selected_id = self.selected_row_id;
        for row_presentation in &self.cached_rows {
            let is_selected = selected_id == Some(row_presentation.row_id);
            rows_column = rows_column.push(render_row(row_presentation, is_selected));
        }
        let body = scrollable(container(rows_column).padding(8))
            .direction(Direction::Both {
                vertical: Scrollbar::default(),
                horizontal: Scrollbar::default(),
            })
            .height(Length::Fill)
            .width(Length::Fill);

        let sidebar = self.source_sidebar();
        let main = row![sidebar, body]
            .spacing(0)
            .height(Length::Fill)
            .width(Length::Fill);

        let detail = self.detail_panel();

        let status = self.status_line();
        let footer = container(text(status).size(12))
            .padding(6)
            .width(Length::Fill);

        column![top_bar, main, detail, footer].spacing(0).into()
    }

    /// Render the source sidebar listing each `SourceSummary` from the
    /// most recent viewport snapshot. Mirrors the gpui front-end: the
    /// row counts and severity totals come from
    /// `Engine::viewport().source_summaries`, so they reflect whatever
    /// filter/level is active.
    fn source_sidebar(&self) -> Element<'_, Message> {
        let mut col = column![
            text("Sources")
                .size(12)
                .color(Color::from_rgb8(0xc8, 0xa2, 0xc8))
        ]
        .spacing(4);
        if self.cached_sources.is_empty() {
            col = col.push(
                text("(no sources)")
                    .size(11)
                    .color(Color::from_rgb8(0x88, 0x88, 0x88)),
            );
        } else {
            for summary in &self.cached_sources {
                let counts = &summary.level_counts;
                let total = counts.error + counts.fatal;
                let warn = counts.warn;
                col = col.push(
                    column![
                        text(summary.name.to_string())
                            .size(12)
                            .color(Color::from_rgb8(0xe6, 0xe6, 0xe6)),
                        row![
                            text(format!("{} rows", summary.rows))
                                .size(10)
                                .color(Color::from_rgb8(0x88, 0x88, 0x88)),
                            text(format!("{warn}W"))
                                .size(10)
                                .color(Color::from_rgb8(0xff, 0xc8, 0x6b)),
                            text(format!("{total}E"))
                                .size(10)
                                .color(Color::from_rgb8(0xff, 0x6b, 0x6b)),
                        ]
                        .spacing(8),
                    ]
                    .spacing(1),
                );
            }
        }
        container(scrollable(col))
            .padding(8)
            .width(Length::Fixed(220.0))
            .height(Length::Fill)
            .style(|_| container::Style {
                background: Some(Background::Color(Color::from_rgba8(0x14, 0x14, 0x18, 1.0))),
                border: Border {
                    width: 1.0,
                    color: Color::from_rgb8(0x33, 0x33, 0x33),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    /// Render the JSON detail panel for the currently selected row. Empty
    /// when no selection (or no JSON fields on the selected row) so the
    /// layout doesn't shift around as the user navigates.
    fn detail_panel(&self) -> Element<'_, Message> {
        let Some(row) = self
            .selected_row_id
            .and_then(|id| self.cached_rows.iter().find(|row| row.row_id == id))
        else {
            return container(text("")).height(Length::Fixed(0.0)).into();
        };
        let fields = row.json_fields();
        if fields.is_empty() {
            return container(text("")).height(Length::Fixed(0.0)).into();
        }
        let mut col = column![
            text("JSON detail")
                .size(13)
                .color(Color::from_rgb8(0xc8, 0xa2, 0xc8))
        ]
        .spacing(2);
        for (key, value) in fields {
            col = col.push(
                row![
                    text(key.to_string())
                        .size(12)
                        .color(Color::from_rgb8(0xc8, 0xa2, 0xc8))
                        .width(Length::Fixed(160.0)),
                    text(value.to_string())
                        .size(12)
                        .color(Color::from_rgb8(0xfb, 0xbc, 0x04)),
                ]
                .spacing(8),
            );
        }
        container(col)
            .padding(8)
            .width(Length::Fill)
            .height(Length::Shrink)
            .style(|_| container::Style {
                background: Some(Background::Color(Color::from_rgba8(0x1a, 0x1a, 0x1a, 1.0))),
                border: Border {
                    width: 1.0,
                    color: Color::from_rgb8(0x33, 0x33, 0x33),
                    ..Default::default()
                },
                ..Default::default()
            })
            .into()
    }

    fn status_line(&self) -> String {
        let end = (self.first_row + self.cached_rows.len()).min(self.total_matching_rows);
        let mut parts = vec![format!(
            "{}–{} of {} matching / {} total",
            self.first_row, end, self.total_matching_rows, self.total_rows,
        )];
        if let Some(error) = self.error_message.as_ref() {
            parts.push(format!("filter error: {error}"));
        } else if let Some(status) = self.status_message.as_ref() {
            parts.push(status.clone());
        }
        match self.mode {
            InputMode::Filter => parts.push("[filter mode: ↵ apply, esc cancel]".into()),
            InputMode::Search => parts.push("[search mode: ↵ apply, esc cancel]".into()),
            InputMode::Normal => {}
        }
        parts.join("  •  ")
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = time::every(POLL_INTERVAL).map(|_| Message::Tick);
        let keys = keyboard::on_key_press(|key, _modifiers| match key {
            // Always-on keys — work in any mode.
            Key::Named(Named::Escape) => Some(Message::EscapePressed),
            Key::Named(Named::ArrowUp) => Some(Message::SelectionMoveUp),
            Key::Named(Named::ArrowDown) => Some(Message::SelectionMoveDown),
            Key::Named(Named::PageUp) => Some(Message::PageUp),
            Key::Named(Named::PageDown) => Some(Message::PageDown),
            Key::Named(Named::Home) => Some(Message::HomePressed),
            Key::Named(Named::End) => Some(Message::EndPressed),
            // Letter shortcuts — emitted unconditionally; `update` no-ops
            // them while a text input has focus. See [`InputMode`].
            Key::Character(ref c) => match c.as_str() {
                "/" => Some(Message::EnterFilterMode),
                "?" => Some(Message::EnterSearchMode),
                "b" => Some(Message::BookmarkToggled),
                "f" => Some(Message::FollowToggled),
                "s" => Some(Message::SavedFilterCycled),
                "n" => Some(Message::NextSearchResult),
                "N" => Some(Message::PrevSearchResult),
                "j" => Some(Message::SelectionMoveDown),
                "k" => Some(Message::SelectionMoveUp),
                "z" => Some(Message::StackFoldingToggled),
                "0" => Some(Message::LevelSetTo(None)),
                "1" => Some(Message::LevelSetTo(Some(LevelArg::Trace))),
                "2" => Some(Message::LevelSetTo(Some(LevelArg::Debug))),
                "3" => Some(Message::LevelSetTo(Some(LevelArg::Info))),
                "4" => Some(Message::LevelSetTo(Some(LevelArg::Warn))),
                "5" => Some(Message::LevelSetTo(Some(LevelArg::Error))),
                "6" => Some(Message::LevelSetTo(Some(LevelArg::Fatal))),
                _ => None,
            },
            _ => None,
        });
        Subscription::batch([tick, keys])
    }
}

impl Drop for GlowtailIced {
    fn drop(&mut self) {
        if let Some(path) = self.session_path.as_ref()
            && let Err(err) = save_session(Some(path), self.engine.session())
        {
            eprintln!("warning: failed to save session: {err}");
        }
    }
}

fn render_row(presentation: &RowPresentation, is_selected: bool) -> Element<'_, Message> {
    let role = presentation.severity_role();
    let mut row_builder: Row<'_, Message> = row![
        text(severity_glyph(role))
            .color(severity_colour(role))
            .size(14)
    ]
    .spacing(4)
    .align_y(Alignment::Center);

    if let Some(name) = presentation.source_name.as_ref() {
        row_builder = row_builder.push(
            text(format!("[{name}]"))
                .size(13)
                .color(Color::from_rgb8(0x88, 0x88, 0x88)),
        );
    }

    for span in &presentation.spans {
        row_builder = row_builder.push(
            text(span.text.to_string())
                .size(13)
                .color(span_colour(span.kind, role)),
        );
    }

    if presentation.is_bookmarked {
        row_builder = row_builder.push(text("★").color(Color::from_rgb8(0xff, 0xc8, 0x6b)));
    }

    let mut wrapper = container(row_builder).padding(2).width(Length::Fill);
    if is_selected {
        wrapper = wrapper.style(|_: &Theme| container::Style {
            background: Some(Background::Color(Color::from_rgba8(0x22, 0x55, 0x88, 0.45))),
            border: Border {
                width: 1.0,
                color: Color::from_rgb8(0x55, 0x99, 0xff),
                ..Default::default()
            },
            ..Default::default()
        });
    } else if presentation.is_bookmarked {
        wrapper = wrapper.style(|_: &Theme| container::Style {
            border: Border {
                width: 1.0,
                color: Color::from_rgba8(0xff, 0xc8, 0x6b, 0.5),
                ..Default::default()
            },
            ..Default::default()
        });
    }
    wrapper.into()
}

fn severity_glyph(role: SeverityRole) -> &'static str {
    match role {
        SeverityRole::Fatal | SeverityRole::Error => "■",
        SeverityRole::Warn => "▲",
        SeverityRole::Info => "·",
        SeverityRole::Debug => "·",
        SeverityRole::Trace => "·",
        SeverityRole::Unknown => " ",
    }
}

fn severity_colour(role: SeverityRole) -> Color {
    match role {
        SeverityRole::Fatal => Color::from_rgb8(0xff, 0x4b, 0x4b),
        SeverityRole::Error => Color::from_rgb8(0xff, 0x6b, 0x6b),
        SeverityRole::Warn => Color::from_rgb8(0xff, 0xc8, 0x6b),
        SeverityRole::Info => Color::from_rgb8(0x88, 0xc8, 0xff),
        SeverityRole::Debug => Color::from_rgb8(0x80, 0x80, 0x80),
        SeverityRole::Trace => Color::from_rgb8(0x60, 0x60, 0x60),
        SeverityRole::Unknown => Color::from_rgb8(0xa0, 0xa0, 0xa0),
    }
}

/// Single translation seam: semantic `SpanKind` to native `iced::Color`.
/// Mirrors `crates/glowtail-gui/src/main.rs::span_color` and
/// `crates/glowtail-gpui/src/main.rs::span_color` so all three front-ends
/// stay visually close at first glance.
fn span_colour(kind: SpanKind, role: SeverityRole) -> Color {
    match kind {
        SpanKind::Timestamp => Color::from_rgb8(0x8a, 0xb4, 0xf8),
        SpanKind::Level => severity_colour(role),
        SpanKind::Source => Color::from_rgb8(0xc8, 0xa2, 0xc8),
        SpanKind::Message => Color::from_rgb8(0xe6, 0xe6, 0xe6),
        SpanKind::JsonKey => Color::from_rgb8(0xc8, 0xa2, 0xc8),
        SpanKind::JsonValue => Color::from_rgb8(0xfb, 0xbc, 0x04),
        SpanKind::SearchMatch => Color::from_rgb8(0xff, 0xeb, 0x3b),
        SpanKind::Error => Color::from_rgb8(0xff, 0x6b, 0x6b),
        SpanKind::Warning => Color::from_rgb8(0xff, 0xc8, 0x6b),
        SpanKind::StackTrace => Color::from_rgb8(0xa0, 0xa0, 0xa0),
        _ => Color::from_rgb8(0xe6, 0xe6, 0xe6),
    }
}

fn level_label(level: Option<LevelArg>) -> &'static str {
    match level {
        None => "all",
        Some(LevelArg::Trace) => "trace",
        Some(LevelArg::Debug) => "debug",
        Some(LevelArg::Info) => "info",
        Some(LevelArg::Warn) => "warn",
        Some(LevelArg::Error) => "error",
        Some(LevelArg::Fatal) => "fatal",
    }
}

fn cycle_level(current: Option<LevelArg>) -> Option<LevelArg> {
    match current {
        None => Some(LevelArg::Trace),
        Some(LevelArg::Trace) => Some(LevelArg::Debug),
        Some(LevelArg::Debug) => Some(LevelArg::Info),
        Some(LevelArg::Info) => Some(LevelArg::Warn),
        Some(LevelArg::Warn) => Some(LevelArg::Error),
        Some(LevelArg::Error) => Some(LevelArg::Fatal),
        Some(LevelArg::Fatal) => None,
    }
}
