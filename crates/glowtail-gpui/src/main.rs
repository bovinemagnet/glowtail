use anyhow::{Context as AnyhowContext, Result};
use clap::{Parser, ValueEnum};
use glowtail_core::filter::compose_query_filter;
use glowtail_core::prelude::*;
use gpui::{
    App, Application, Bounds, Context, IntoElement, ListAlignment, ListState, ParentElement,
    Render, SharedString, Styled, Window, WindowBounds, WindowOptions, div, list, prelude::*, px,
    rgb, size,
};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

const ROW_OVERDRAW: f32 = 640.0;
const LIVE_REFRESH_MS: u64 = 100;

#[derive(Debug, Parser)]
#[command(name = "glowtail-gpui")]
#[command(about = "Native GPUI glowtail desktop UI")]
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

#[derive(Debug, Clone, Copy, ValueEnum)]
enum LevelArg {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let parser = parser_from_flags(args.json, args.plain);
    let session = load_session(args.session.as_ref())?;
    let mut engine = Engine::with_session(session);
    // Accumulate per-path load errors so a single unreadable file doesn't
    // prevent the window from opening on the readable ones.
    let mut load_errors: Vec<String> = Vec::new();
    if args.no_follow || !args.from_start {
        for path in &args.paths {
            if let Err(err) = engine.load_file(path, parser.as_ref()) {
                load_errors.push(format!("failed to read {}: {err}", path.display()));
            }
        }
    }
    engine.set_max_rows(normalise_max_rows(args.max_rows));
    apply_filters(
        &mut engine,
        args.filter.clone(),
        args.level,
        args.use_filter,
        args.save_filter,
    )?;

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("glowtail-gpui-tail")
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
    let initial_status = if load_errors.is_empty() {
        None
    } else {
        Some(load_errors.join("; "))
    };
    let app = GlowtailGpui::new(engine, runtime, live_tail, args.session, initial_status);

    let launch_error: Arc<std::sync::Mutex<Option<anyhow::Error>>> =
        Arc::new(std::sync::Mutex::new(None));
    let launch_error_clone = Arc::clone(&launch_error);
    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        let result = cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            move |_, cx| {
                cx.new(move |cx| {
                    app.start_refresh_loop(cx);
                    app
                })
            },
        );
        if let Err(err) = result {
            *launch_error_clone.lock().unwrap() =
                Some(anyhow::anyhow!("failed to open GPUI window: {err}"));
            return;
        }
        cx.activate(true);
    });

    if let Some(err) = launch_error.lock().unwrap().take() {
        return Err(err);
    }
    Ok(())
}

fn start_tailers(
    runtime: &Runtime,
    paths: &[PathBuf],
    parser: Arc<dyn LogParser>,
    from_start: bool,
) -> LiveTail {
    let (tx, rx) = mpsc::channel(DEFAULT_TAILER_CHANNEL_CAPACITY);
    let mut tailers = Vec::new();
    let _guard = runtime.enter();
    for (index, path) in paths.iter().enumerate() {
        tailers.push(FileTailer::start(
            SourceId((index + 1) as u64),
            path.clone(),
            Arc::clone(&parser),
            tx.clone(),
            from_start,
            true,
        ));
    }
    drop(tx);
    LiveTail {
        receiver: rx,
        tailers,
    }
}

/// Treat `--max-rows 0` and an absent flag as "unbounded" so the CLI surface
/// is forgiving — `0` reading as "no rows retained" is a usability trap.
fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
}

fn parser_from_flags(json: bool, plain: bool) -> Arc<dyn LogParser> {
    if json {
        Arc::new(JsonLineParser)
    } else if plain {
        Arc::new(PlainTextParser)
    } else {
        Arc::new(CompositeParser::default())
    }
}

fn apply_filters(
    engine: &mut Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    use_filter: Option<String>,
    save_filter: Option<String>,
) -> Result<()> {
    let saved = use_filter
        .map(|name| {
            engine
                .session()
                .saved_filter(&name)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("saved filter not found: {name}"))
        })
        .transpose()?;
    let level: Option<LogLevel> = level.map(Into::into);
    let filter = compose_query_filter(saved.as_ref(), level, filter_text.as_deref())?;
    engine.set_filter(filter)?;
    if let Some(name) = save_filter {
        engine.save_filter(name);
    }
    Ok(())
}

fn load_session(path: Option<&PathBuf>) -> Result<InvestigationSession> {
    let Some(path) = path else {
        return Ok(InvestigationSession::default());
    };
    if !path.exists() {
        return Ok(InvestigationSession::default());
    }
    InvestigationSession::load_from_path(path)
        .with_context(|| format!("failed to load session {}", path.display()))
}

fn save_session(path: Option<&PathBuf>, session: &InvestigationSession) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create session directory {}", parent.display()))?;
    }
    session
        .save_to_path(path)
        .with_context(|| format!("failed to save session {}", path.display()))
}

impl From<LevelArg> for LogLevel {
    fn from(value: LevelArg) -> Self {
        match value {
            LevelArg::Trace => LogLevel::Trace,
            LevelArg::Debug => LogLevel::Debug,
            LevelArg::Info => LogLevel::Info,
            LevelArg::Warn => LogLevel::Warn,
            LevelArg::Error => LogLevel::Error,
            LevelArg::Fatal => LogLevel::Fatal,
        }
    }
}

struct LiveTail {
    receiver: mpsc::Receiver<LogEvent>,
    tailers: Vec<FileTailer>,
}

struct GlowtailGpui {
    /// Engine wrapped in `Rc<RefCell<_>>` so the per-row render closure can
    /// borrow it mutably without us materialising every row up-front.
    engine: Rc<RefCell<Engine>>,
    metadata: Arc<ViewportSnapshot>,
    list_state: ListState,
    /// Kept alive to host the spawned `FileTailer` tasks. Not read after
    /// construction — `Drop` order ensures it outlives `live_tail`, so the
    /// runtime drives the tasks to completion after `signal_stop` (M5).
    #[allow(dead_code)]
    runtime: Runtime,
    live_tail: Option<LiveTail>,
    status_message: Option<String>,
    session_path: Option<PathBuf>,
}

impl GlowtailGpui {
    fn new(
        engine: Engine,
        runtime: Runtime,
        live_tail: Option<LiveTail>,
        session_path: Option<PathBuf>,
        status_message: Option<String>,
    ) -> Self {
        let engine = Rc::new(RefCell::new(engine));
        let (metadata, item_count) = {
            let mut engine = engine.borrow_mut();
            let metadata = engine.metadata_snapshot();
            let count = engine.matching_rows_count();
            (metadata, count)
        };
        let list_state = ListState::new(item_count, ListAlignment::Top, px(ROW_OVERDRAW));
        Self {
            engine,
            metadata: Arc::new(metadata),
            list_state,
            runtime,
            live_tail,
            status_message,
            session_path,
        }
    }

    fn start_refresh_loop(&self, cx: &mut Context<Self>) {
        if self.live_tail.is_none() {
            return;
        }

        cx.spawn(
            async move |view: gpui::WeakEntity<GlowtailGpui>, cx: &mut gpui::AsyncApp| {
                loop {
                    cx.background_executor()
                        .timer(Duration::from_millis(LIVE_REFRESH_MS))
                        .await;
                    if view.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            },
        )
        .detach();
    }

    fn drain_live_events(&mut self) -> bool {
        let Some(live_tail) = self.live_tail.as_mut() else {
            return false;
        };

        let mut changed = false;
        loop {
            match live_tail.receiver.try_recv() {
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    self.engine
                        .borrow_mut()
                        .add_source(source_id, path.display().to_string());
                    changed = true;
                }
                Ok(LogEvent::RowAppended(row)) => {
                    self.engine.borrow_mut().append_row(row);
                    changed = true;
                }
                Ok(LogEvent::SourceRotated { source_id }) => {
                    self.status_message = Some(format!("source {} rotated", source_id.0));
                }
                Ok(LogEvent::SourceError { source_id, message }) => {
                    self.status_message = Some(format!("source {} error: {message}", source_id.0));
                }
                Ok(LogEvent::SourceRemoved { .. }) => {}
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.status_message = Some("live tail disconnected".into());
                    break;
                }
            }
        }

        if changed {
            self.refresh_metadata();
            self.save_session();
        }
        changed
    }

    fn refresh_metadata(&mut self) {
        let (metadata, new_count) = {
            let mut engine = self.engine.borrow_mut();
            (engine.metadata_snapshot(), engine.matching_rows_count())
        };
        self.metadata = Arc::new(metadata);
        // Only rebuild the ListState if the row count actually changed —
        // otherwise we'd snap the scroll position to the top on every frame.
        if new_count != self.list_state.item_count() {
            // Capture the current logical offset so we can restore it after
            // the rebuild and avoid jumping to row 0 on every appended line.
            let logical_offset = self.list_state.logical_scroll_top();
            self.list_state = ListState::new(new_count, ListAlignment::Top, px(ROW_OVERDRAW));
            self.list_state.scroll_to(logical_offset);
        }
    }

    fn save_session(&self) {
        let _ = save_session(self.session_path.as_ref(), self.engine.borrow().session());
    }
}

impl Drop for GlowtailGpui {
    fn drop(&mut self) {
        self.save_session();
        // Non-blocking shutdown: signal each tailer and let the runtime drop
        // drive the spawned tasks to completion. The previous `block_on`
        // blocked the UI thread and panics when called from a Tokio worker.
        if let Some(live_tail) = self.live_tail.take() {
            for tailer in &live_tail.tailers {
                tailer.signal_stop();
            }
        }
    }
}

impl Render for GlowtailGpui {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.drain_live_events();
        let metadata = Arc::clone(&self.metadata);
        let engine = Rc::clone(&self.engine);
        // Snapshot the detail row up front so `detail_panel` doesn't borrow
        // the engine concurrently with the lazy list-row render closures
        // below — overlapping `borrow_mut()` calls would panic with
        // `RefCell already borrowed` (review M7).
        let detail_row = engine.borrow_mut().present_row_at(0);
        div()
            .size_full()
            .bg(rgb(0x101418))
            .text_color(rgb(0xd8dee9))
            .font_family("monospace")
            .flex()
            .flex_col()
            .child(top_bar(
                &metadata,
                self.live_tail.is_some(),
                self.status_message.as_deref(),
                self.engine.borrow().evicted_row_count(),
            ))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(source_sidebar(&metadata))
                    .child(log_viewport(engine, self.list_state.clone()))
                    .child(detail_panel(detail_row)),
            )
            .child(timeline_panel(&metadata))
    }
}

fn top_bar(
    snapshot: &ViewportSnapshot,
    live_tail_enabled: bool,
    status_message: Option<&str>,
    evicted_count: u64,
) -> impl IntoElement {
    let mode = if live_tail_enabled { "live" } else { "static" };
    let status = status_message.unwrap_or(if live_tail_enabled {
        "following appended lines"
    } else {
        "loaded once"
    });

    let mut bar = div()
        .h(px(52.))
        .w_full()
        .flex()
        .items_center()
        .gap_4()
        .px_4()
        .border_b_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x151b21))
        .child(
            div()
                .text_xl()
                .font_weight(gpui::FontWeight::BOLD)
                .child("glowtail-gpui"),
        )
        .child(metric("matching", snapshot.total_matching_rows))
        .child(metric("total", snapshot.total_rows))
        .child(metric("warn", snapshot.level_counts.warn))
        .child(metric(
            "error",
            snapshot.level_counts.error + snapshot.level_counts.fatal,
        ))
        .child(metric_text("mode", mode));

    if evicted_count > 0 {
        bar = bar.child(
            div()
                .text_sm()
                .text_color(rgb(0xd6a33d))
                .child(format!("truncated: -{evicted_count}")),
        );
    }

    bar.child(
        div()
            .ml_auto()
            .text_sm()
            .text_color(rgb(0x9aa7b2))
            .child(status.to_string()),
    )
}

fn metric(label: &'static str, value: usize) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .text_sm()
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(value.to_string()))
}

fn metric_text(label: &'static str, value: &'static str) -> impl IntoElement {
    div()
        .flex()
        .gap_1()
        .text_sm()
        .child(div().text_color(rgb(0x7f8b96)).child(label))
        .child(div().text_color(rgb(0xe6edf3)).child(value))
}

fn source_sidebar(snapshot: &ViewportSnapshot) -> impl IntoElement {
    let mut panel = div()
        .w(px(240.))
        .h_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .border_r_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x111820))
        .child(div().text_lg().child("Sources"));

    for source in &snapshot.source_summaries {
        panel = panel.child(
            div()
                .rounded_md()
                .p_2()
                .bg(rgb(0x17212b))
                .child(
                    div()
                        .text_sm()
                        .text_color(rgb(0xe6edf3))
                        .child(source.name.to_string()),
                )
                .child(div().text_xs().text_color(rgb(0x9aa7b2)).child(format!(
                    "{} rows · {} warn · {} error",
                    source.rows,
                    source.level_counts.warn,
                    source.level_counts.error + source.level_counts.fatal
                ))),
        );
    }

    panel
}

fn log_viewport(engine: Rc<RefCell<Engine>>, list_state: ListState) -> impl IntoElement {
    div()
        .flex_1()
        .h_full()
        .flex()
        .flex_col()
        .bg(rgb(0x0d1117))
        .child(
            div()
                .h(px(28.))
                .flex()
                .items_center()
                .px_3()
                .text_xs()
                .text_color(rgb(0x9aa7b2))
                .border_b_1()
                .border_color(rgb(0x26313b))
                .child("Log viewport"),
        )
        .child(
            list(list_state, move |index, _window, _cx| {
                let row = engine.borrow_mut().present_row_at(index);
                row_element(row, index)
            })
            .flex_1(),
        )
}

fn row_element(row: Option<RowPresentation>, index: usize) -> gpui::AnyElement {
    let Some(row) = row else {
        return div().h(px(24.)).into_any();
    };

    let mut line = div()
        .h(px(24.))
        .w_full()
        .flex()
        .items_center()
        .gap_1()
        .px_2()
        .border_b_1()
        .border_color(rgb(0x1c2530))
        .bg(if index.is_multiple_of(2) {
            rgb(0x10161d)
        } else {
            rgb(0x0d1117)
        })
        .child(
            div()
                .w(px(4.))
                .h_full()
                .bg(severity_color(row.severity_role()))
                .mr_2(),
        );

    if row.is_bookmarked {
        line = line.child(div().text_color(rgb(0xdc8cff)).child("*"));
    }
    if row.folded_stack_rows > 0 {
        line = line.child(
            div()
                .text_color(rgb(0x8b949e))
                .child(format!("+{} ", row.folded_stack_rows)),
        );
    }
    if let Some(source) = row.source_name.as_ref() {
        line = line.child(
            div()
                .text_color(rgb(0x8b949e))
                .child(format!("[{source}] ")),
        );
    }

    for span in &row.spans {
        let mut span_div = div()
            .text_color(span_color(span.kind))
            .child(SharedString::from(span.text.to_string()));
        if span.kind == SpanKind::SearchMatch {
            span_div = span_div.bg(rgb(0xc9d96f));
        }
        line = line.child(span_div);
    }

    line.into_any()
}

fn detail_panel(selected: Option<RowPresentation>) -> impl IntoElement {
    // Show the first visible row's details. Without a click handler in
    // gpui 0.2.2, the prototype settles for "first row" semantics that match
    // the previous behaviour. The row is snapshotted in the parent `render`
    // so this function doesn't borrow the engine — see review M7.
    let mut panel = div()
        .w(px(320.))
        .h_full()
        .flex()
        .flex_col()
        .gap_2()
        .p_3()
        .border_l_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x111820))
        .child(div().text_lg().child("Details"));

    if let Some(row) = selected {
        panel = panel
            .child(detail_line("row", row.row_id.0.to_string()))
            .child(detail_line("source", row.source_id.0.to_string()));
        if let Some(level) = row.level {
            panel = panel.child(detail_line("level", format!("{level:?}")));
        }
        let fields = row.json_fields();
        if fields.is_empty() {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x8b949e))
                    .child("No structured JSON fields on the first visible row."),
            );
        } else {
            panel = panel.child(
                div()
                    .text_sm()
                    .text_color(rgb(0x9aa7b2))
                    .child("JSON fields"),
            );
            for (key, value) in fields {
                panel = panel.child(detail_line(key.to_string(), value.to_string()));
            }
        }
    } else {
        panel = panel.child(
            div()
                .text_sm()
                .text_color(rgb(0x8b949e))
                .child("No rows match the current input."),
        );
    }

    panel
}

fn detail_line(label: impl Into<SharedString>, value: impl Into<SharedString>) -> impl IntoElement {
    div()
        .flex()
        .flex_col()
        .gap_1()
        .rounded_md()
        .p_2()
        .bg(rgb(0x17212b))
        .child(
            div()
                .text_xs()
                .text_color(rgb(0x8b949e))
                .child(label.into()),
        )
        .child(
            div()
                .text_sm()
                .text_color(rgb(0xe6edf3))
                .child(value.into()),
        )
}

fn timeline_panel(snapshot: &ViewportSnapshot) -> impl IntoElement {
    let mut row = div()
        .h(px(82.))
        .w_full()
        .flex()
        .items_end()
        .gap_1()
        .p_3()
        .border_t_1()
        .border_color(rgb(0x26313b))
        .bg(rgb(0x151b21));

    if snapshot.timeline.is_empty() {
        return row
            .items_center()
            .justify_center()
            .text_color(rgb(0x8b949e))
            .child("No timestamps available for timeline")
            .into_any();
    }

    let max_total = snapshot
        .timeline
        .iter()
        .map(|bucket| bucket.total)
        .max()
        .unwrap_or(1) as f32;

    for bucket in &snapshot.timeline {
        let height = 12.0 + (bucket.total as f32 / max_total) * 52.0;
        row = row.child(div().flex_1().h(px(height)).rounded_sm().bg(
            if bucket.error_count() > 0 {
                rgb(0xdc4f4f)
            } else if bucket.warn_count() > 0 {
                rgb(0xd6a33d)
            } else {
                rgb(0x4f9ee3)
            },
        ));
    }

    row.into_any()
}

fn severity_color(role: SeverityRole) -> gpui::Rgba {
    match role {
        SeverityRole::Fatal | SeverityRole::Error => rgb(0xdc4f4f),
        SeverityRole::Warn => rgb(0xd6a33d),
        SeverityRole::Info => rgb(0x4f9ee3),
        SeverityRole::Debug | SeverityRole::Trace => rgb(0x7c75d8),
        SeverityRole::Unknown => rgb(0x4b5563),
    }
}

fn span_color(kind: SpanKind) -> gpui::Rgba {
    match kind {
        SpanKind::Timestamp => rgb(0x8ab4f8),
        SpanKind::Error => rgb(0xff7b72),
        SpanKind::Warning => rgb(0xd6a33d),
        SpanKind::SearchMatch => rgb(0x0d1117),
        SpanKind::JsonKey => rgb(0x7ee7e7),
        SpanKind::JsonValue => rgb(0xa5d6a7),
        _ => rgb(0xe6edf3),
    }
}
