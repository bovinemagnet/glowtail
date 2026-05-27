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
use glowtail_core::prelude::*;
use glowtail_ui_common::{
    LevelArg, LiveTail, apply_filters, load_session, parser_from_flags, save_session, start_tailers,
};
use iced::keyboard::{self, Key, key::Named};
use iced::widget::{Row, button, column, container, row, scrollable, text, text_input};
use iced::{Alignment, Color, Element, Length, Subscription, Task, time};
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

#[derive(Debug, Clone)]
enum Message {
    FilterInputChanged(String),
    FilterSubmitted,
    SearchInputChanged(String),
    SearchSubmitted,
    LevelCycled,
    FollowToggled,
    LineUp,
    LineDown,
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
    total_matching_rows: usize,
    total_rows: usize,
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
            total_matching_rows: 0,
            total_rows: 0,
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
    }

    fn snap_to_tail(&mut self) {
        self.first_row = self.total_matching_rows.saturating_sub(PAGE_SIZE);
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

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::FilterInputChanged(value) => {
                self.filter_text = value;
            }
            Message::FilterSubmitted => {
                self.apply_current_filters();
            }
            Message::SearchInputChanged(value) => {
                self.search_text = value;
            }
            Message::SearchSubmitted => {
                let search = if self.search_text.is_empty() {
                    None
                } else {
                    Some(self.search_text.clone())
                };
                self.engine.set_search_text(search);
                self.refresh_snapshot();
            }
            Message::LevelCycled => {
                self.level = cycle_level(self.level);
                self.apply_current_filters();
            }
            Message::FollowToggled => {
                self.follow = !self.follow;
                if self.follow {
                    self.snap_to_tail();
                    self.refresh_snapshot();
                }
            }
            Message::LineUp => {
                self.follow = false;
                self.first_row = self.first_row.saturating_sub(1);
                self.refresh_snapshot();
            }
            Message::LineDown => {
                self.follow = false;
                self.first_row =
                    (self.first_row + 1).min(self.total_matching_rows.saturating_sub(1).max(0));
                self.refresh_snapshot();
            }
            Message::PageUp => {
                self.follow = false;
                self.first_row = self.first_row.saturating_sub(PAGE_SIZE);
                self.refresh_snapshot();
            }
            Message::PageDown => {
                self.follow = false;
                self.first_row = self
                    .first_row
                    .saturating_add(PAGE_SIZE)
                    .min(self.total_matching_rows.saturating_sub(1).max(0));
                self.refresh_snapshot();
            }
            Message::HomePressed => {
                self.follow = false;
                self.first_row = 0;
                self.refresh_snapshot();
            }
            Message::EndPressed => {
                self.follow = true;
                self.snap_to_tail();
                self.refresh_snapshot();
            }
            Message::Tick => {
                let appended = self.drain_events();
                if appended && self.follow {
                    self.snap_to_tail();
                }
                if appended || self.follow {
                    self.refresh_snapshot();
                }
            }
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let filter_input = text_input("filter…", &self.filter_text)
            .on_input(Message::FilterInputChanged)
            .on_submit(Message::FilterSubmitted)
            .width(Length::FillPortion(3));

        let search_input = text_input("search…", &self.search_text)
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

        let top_bar = row![filter_input, search_input, level_button, follow_button,]
            .spacing(8)
            .padding(8)
            .align_y(Alignment::Center);

        let mut rows_column = column![].spacing(2);
        for row_presentation in &self.cached_rows {
            rows_column = rows_column.push(render_row(row_presentation));
        }
        let body = scrollable(container(rows_column).padding(8)).height(Length::Fill);

        let status = self.status_line();
        let footer = container(text(status).size(12))
            .padding(6)
            .width(Length::Fill);

        column![top_bar, body, footer].spacing(0).into()
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
        parts.join("  •  ")
    }

    fn subscription(&self) -> Subscription<Message> {
        let tick = time::every(POLL_INTERVAL).map(|_| Message::Tick);
        let keys = keyboard::on_key_press(|key, _modifiers| match key {
            Key::Named(Named::ArrowUp) => Some(Message::LineUp),
            Key::Named(Named::ArrowDown) => Some(Message::LineDown),
            Key::Named(Named::PageUp) => Some(Message::PageUp),
            Key::Named(Named::PageDown) => Some(Message::PageDown),
            Key::Named(Named::Home) => Some(Message::HomePressed),
            Key::Named(Named::End) => Some(Message::EndPressed),
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

fn render_row(presentation: &RowPresentation) -> Row<'_, Message> {
    let role = presentation.severity_role();
    let mut row_builder = row![
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

    row_builder
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
