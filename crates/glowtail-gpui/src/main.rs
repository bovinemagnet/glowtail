use anyhow::{Context as AnyhowContext, Result};
use clap::{Parser, ValueEnum};
use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{
    ByteRange, LogLevel, RowId, RowPresentation, SourceId, SpanKind, ViewportRequest,
    ViewportSnapshot,
};
use glowtail_core::parser::{CompositeParser, JsonLineParser, LogParser, PlainTextParser};
use glowtail_core::viewport::Engine;
use gpui::{
    App, Application, Bounds, Context, IntoElement, ListAlignment, ListState, ParentElement,
    Render, SharedString, Styled, Window, WindowBounds, WindowOptions, div, list, prelude::*, px,
    rgb, size,
};
use std::path::PathBuf;
use std::sync::Arc;

const ROW_OVERDRAW: f32 = 640.0;

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
    let mut engine = load_engine(&args)?;
    configure_filter(&mut engine, args.filter, args.level)?;

    let snapshot = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: engine.matching_rows_count(),
    });
    let app = GlowtailGpui::new(snapshot);

    Application::new().run(move |cx: &mut App| {
        let bounds = Bounds::centered(None, size(px(1400.), px(900.)), cx);
        cx.open_window(
            WindowOptions {
                focus: true,
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            |_, cx| cx.new(|_| app),
        )
        .expect("failed to open GPUI window");
        cx.activate(true);
    });

    Ok(())
}

fn load_engine(args: &Args) -> Result<Engine> {
    let parser = parser_from_flags(args.json, args.plain);
    let mut engine = Engine::default();
    let mut source_counter = 0u64;

    for path in &args.paths {
        source_counter += 1;
        let source_id = SourceId(source_counter);
        engine.add_source(source_id, path.display().to_string());
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;

        let mut offset = 0u64;
        for line in content.lines() {
            let end = offset + line.len() as u64 + 1;
            let row = parser.parse_line(
                source_id,
                RowId(engine.total_rows() as u64),
                ByteRange { start: offset, end },
                line,
            );
            engine.append_row(row);
            offset = end;
        }
    }

    Ok(engine)
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

fn configure_filter(
    engine: &mut Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
) -> Result<()> {
    let mut filters = Vec::new();
    if let Some(level) = level {
        filters.push(FilterExpr::LevelAtLeast(level.into()));
    }
    if let Some(text) = filter_text {
        filters.push(FilterExpr::Contains(text));
    }
    engine.set_filter(FilterExpr::and_all(filters))?;
    Ok(())
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

#[derive(Clone)]
struct GlowtailGpui {
    snapshot: Arc<ViewportSnapshot>,
    list_state: ListState,
}

impl GlowtailGpui {
    fn new(snapshot: ViewportSnapshot) -> Self {
        let row_count = snapshot.total_matching_rows;
        Self {
            snapshot: Arc::new(snapshot),
            list_state: ListState::new(row_count, ListAlignment::Top, px(ROW_OVERDRAW)),
        }
    }
}

impl Render for GlowtailGpui {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        let snapshot = Arc::clone(&self.snapshot);
        div()
            .size_full()
            .bg(rgb(0x101418))
            .text_color(rgb(0xd8dee9))
            .font_family("monospace")
            .flex()
            .flex_col()
            .child(top_bar(&snapshot))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .overflow_hidden()
                    .child(source_sidebar(&snapshot))
                    .child(log_viewport(snapshot.clone(), self.list_state.clone()))
                    .child(detail_panel(&snapshot)),
            )
            .child(timeline_panel(&snapshot))
    }
}

fn top_bar(snapshot: &ViewportSnapshot) -> impl IntoElement {
    div()
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
        .child(
            div()
                .ml_auto()
                .text_sm()
                .text_color(rgb(0x9aa7b2))
                .child("GPUI components: sources · virtual list · timeline · JSON detail"),
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

fn log_viewport(snapshot: Arc<ViewportSnapshot>, list_state: ListState) -> impl IntoElement {
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
                let row = snapshot.rows.get(index).cloned();
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
                .bg(severity_color(row.level))
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

fn detail_panel(snapshot: &ViewportSnapshot) -> impl IntoElement {
    let selected = snapshot.rows.first();
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
        let fields = json_field_pairs(row);
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
                panel = panel.child(detail_line(key, value));
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
        row = row.child(
            div()
                .flex_1()
                .h(px(height))
                .rounded_sm()
                .bg(if bucket.error > 0 {
                    rgb(0xdc4f4f)
                } else if bucket.warn > 0 {
                    rgb(0xd6a33d)
                } else {
                    rgb(0x4f9ee3)
                }),
        );
    }

    row.into_any()
}

fn json_field_pairs(row: &RowPresentation) -> Vec<(String, String)> {
    let mut fields = Vec::new();
    let mut pending_key = None::<String>;
    for span in &row.spans {
        match span.kind {
            SpanKind::JsonKey => pending_key = Some(span.text.to_string()),
            SpanKind::JsonValue => {
                if let Some(key) = pending_key.take() {
                    fields.push((key, span.text.to_string()));
                }
            }
            _ => {}
        }
    }
    fields
}

fn severity_color(level: Option<LogLevel>) -> gpui::Rgba {
    match level {
        Some(LogLevel::Fatal | LogLevel::Error) => rgb(0xdc4f4f),
        Some(LogLevel::Warn) => rgb(0xd6a33d),
        Some(LogLevel::Info) => rgb(0x4f9ee3),
        Some(LogLevel::Debug | LogLevel::Trace) => rgb(0x7c75d8),
        None => rgb(0x4b5563),
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
