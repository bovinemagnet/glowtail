use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use eframe::egui;
use glowtail_core::filter::{compose_query_filter, parse_filter_query};
use glowtail_core::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

const ROW_HEIGHT: f32 = 22.0;

#[derive(Debug, Parser)]
#[command(name = "glowtail-gui")]
#[command(about = "Native GPU-backed glowtail desktop UI")]
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
    let session = load_session(args.session.as_ref())?;
    let parser = parser_from_flags(args.json, args.plain);
    let mut engine = if !args.no_follow && args.from_start {
        Engine::with_session(session)
    } else {
        let mut engine = Engine::with_session(session);
        for path in &args.paths {
            engine
                .load_file(path, parser.as_ref())
                .with_context(|| format!("failed to read {}", path.display()))?;
        }
        engine
    };
    apply_filters(
        &mut engine,
        args.filter.clone(),
        args.level,
        args.use_filter.clone(),
        args.save_filter.clone(),
    )?;

    let runtime = Builder::new_multi_thread()
        .enable_all()
        .thread_name("glowtail-gui-tail")
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("glowtail")
            .with_inner_size([1400.0, 900.0]),
        renderer: eframe::Renderer::Wgpu,
        ..Default::default()
    };

    eframe::run_native(
        "glowtail",
        options,
        Box::new(|_cc| {
            Ok(Box::new(GlowtailGui::new(
                engine,
                args.filter.unwrap_or_default(),
                args.session,
                runtime,
                live_tail,
            )))
        }),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
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

fn start_tailers(
    runtime: &Runtime,
    paths: &[PathBuf],
    parser: Arc<dyn LogParser>,
    from_start: bool,
) -> LiveTail {
    let (tx, rx) = mpsc::channel(1024);
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

struct GlowtailGui {
    engine: Engine,
    filter_text: String,
    search_text: String,
    command_palette_open: bool,
    command_text: String,
    selected_row: Option<RowPresentation>,
    fold_stacks: bool,
    follow: bool,
    session_path: Option<PathBuf>,
    runtime: Runtime,
    live_tail: Option<LiveTail>,
    status_message: Option<String>,
    saved_filter_name: String,
    scroll_to_row: Option<usize>,
}

impl GlowtailGui {
    fn new(
        engine: Engine,
        filter_text: String,
        session_path: Option<PathBuf>,
        runtime: Runtime,
        live_tail: Option<LiveTail>,
    ) -> Self {
        Self {
            engine,
            filter_text,
            search_text: String::new(),
            command_palette_open: false,
            command_text: String::new(),
            selected_row: None,
            fold_stacks: false,
            follow: live_tail.is_some(),
            session_path,
            runtime,
            live_tail,
            status_message: None,
            saved_filter_name: String::new(),
            scroll_to_row: None,
        }
    }

    fn top_bar(&mut self, ctx: &egui::Context) {
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("glowtail");
                ui.separator();

                ui.label("Filter");
                let filter_changed = ui
                    .add_sized(
                        [240.0, 24.0],
                        egui::TextEdit::singleline(&mut self.filter_text)
                            .hint_text("query or contains..."),
                    )
                    .changed();

                ui.label("Search");
                let search_changed = ui
                    .add_sized(
                        [240.0, 24.0],
                        egui::TextEdit::singleline(&mut self.search_text).hint_text("highlight..."),
                    )
                    .changed();

                if filter_changed {
                    let filter = if self.filter_text.trim().is_empty() {
                        Ok(FilterExpr::All)
                    } else {
                        parse_filter_query(&self.filter_text)
                    };
                    if let Err(err) = filter.and_then(|filter| self.engine.set_filter(filter)) {
                        self.status_message = Some(format!("filter error: {err}"));
                    }
                }

                if search_changed {
                    self.engine.set_search_text(Some(self.search_text.clone()));
                }

                if ui.button("Command (Cmd/Ctrl+K)").clicked() {
                    self.command_palette_open = true;
                }

                if ui.button("Prev").clicked() {
                    self.select_search_result(true);
                }
                if ui.button("Next").clicked() {
                    self.select_search_result(false);
                }
                if ui.button("Bookmark").clicked() {
                    self.toggle_selected_bookmark();
                }

                ui.checkbox(&mut self.follow, "Follow");
                ui.checkbox(&mut self.fold_stacks, "Fold stacks");
                self.engine.set_stack_trace_folding(self.fold_stacks);
            });

            ui.horizontal(|ui| {
                ui.label("Saved filter");
                ui.add_sized(
                    [180.0, 24.0],
                    egui::TextEdit::singleline(&mut self.saved_filter_name).hint_text("name"),
                );
                if ui.button("Save").clicked() {
                    self.save_current_filter();
                }
                let names: Vec<String> = self
                    .engine
                    .session()
                    .saved_filters
                    .iter()
                    .map(|filter| filter.name.to_string())
                    .collect();
                for name in names {
                    if ui.button(&name).clicked() {
                        self.apply_saved_filter(&name);
                    }
                }
                if let Some(message) = self.status_message.as_ref() {
                    ui.separator();
                    ui.label(message);
                }
            });
        });
    }

    fn source_sidebar(&self, ctx: &egui::Context, snapshot: &ViewportSnapshot) {
        egui::SidePanel::left("sources")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                ui.heading("Sources");
                ui.separator();
                for source in &snapshot.source_summaries {
                    ui.horizontal(|ui| {
                        ui.monospace(format!("#{}", source.source_id.0));
                        ui.label(source.name.as_ref());
                    });
                    ui.small(format!(
                        "{} rows, {} warn, {} error",
                        source.rows,
                        source.level_counts.warn,
                        source.level_counts.error + source.level_counts.fatal
                    ));
                    ui.add_space(8.0);
                }
                self.session_sidebar(ui);
            });
    }

    fn session_sidebar(&self, ui: &mut egui::Ui) {
        ui.separator();
        ui.heading("Session");
        ui.small(format!(
            "{} saved filters",
            self.engine.session().saved_filters.len()
        ));
        ui.small(format!(
            "{} bookmarks",
            self.engine.session().bookmarks.len()
        ));
        if let Some(path) = self.session_path.as_ref() {
            ui.small(format!("session {}", path.display()));
        } else {
            ui.small("no session file");
        }
    }

    fn detail_panel(&self, ctx: &egui::Context) {
        egui::SidePanel::right("detail")
            .resizable(true)
            .default_width(320.0)
            .show(ctx, |ui| {
                ui.heading("Details");
                ui.separator();
                let Some(row) = &self.selected_row else {
                    ui.label("Select a row to inspect fields.");
                    return;
                };

                ui.label(format!("Row {}", row.row_id.0));
                ui.label(format!("Source {}", row.source_id.0));
                if let Some(level) = row.level {
                    ui.label(format!("Level {level:?}"));
                }
                if row.folded_stack_rows > 0 {
                    ui.label(format!("Folded stack rows {}", row.folded_stack_rows));
                }
                ui.separator();

                let fields = row.json_fields();
                if fields.is_empty() {
                    ui.label("No structured JSON fields on this row.");
                } else {
                    egui::Grid::new("json_fields")
                        .num_columns(2)
                        .striped(true)
                        .show(ui, |ui| {
                            for (key, value) in fields {
                                ui.monospace(key.as_ref());
                                ui.label(value.as_ref());
                                ui.end_row();
                            }
                        });
                }
            });
    }

    fn timeline_panel(&self, ctx: &egui::Context, snapshot: &ViewportSnapshot) {
        egui::TopBottomPanel::bottom("timeline")
            .resizable(false)
            .exact_height(74.0)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{} matching / {} total",
                        snapshot.total_matching_rows, snapshot.total_rows
                    ));
                    ui.separator();
                    ui.label(format!("warn {}", snapshot.level_counts.warn));
                    ui.label(format!(
                        "error {}",
                        snapshot.level_counts.error + snapshot.level_counts.fatal
                    ));
                });

                let available = ui.available_size();
                let (rect, _response) = ui.allocate_exact_size(available, egui::Sense::hover());
                let painter = ui.painter_at(rect);
                painter.rect_filled(rect, 0.0, egui::Color32::from_gray(24));

                if snapshot.timeline.is_empty() {
                    painter.text(
                        rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "no timestamps",
                        egui::FontId::monospace(12.0),
                        egui::Color32::GRAY,
                    );
                    return;
                }

                let max_total = snapshot
                    .timeline
                    .iter()
                    .map(|bucket| bucket.total)
                    .max()
                    .unwrap_or(1) as f32;
                let width = rect.width() / snapshot.timeline.len() as f32;
                for (index, bucket) in snapshot.timeline.iter().enumerate() {
                    let x0 = rect.left() + index as f32 * width;
                    let height = (bucket.total as f32 / max_total) * rect.height();
                    let bar = egui::Rect::from_min_max(
                        egui::pos2(x0, rect.bottom() - height),
                        egui::pos2(x0 + width.max(1.0) - 1.0, rect.bottom()),
                    );
                    let color = if bucket.error > 0 {
                        egui::Color32::from_rgb(220, 70, 70)
                    } else if bucket.warn > 0 {
                        egui::Color32::from_rgb(230, 180, 70)
                    } else {
                        egui::Color32::from_rgb(90, 150, 220)
                    };
                    painter.rect_filled(bar, 0.0, color);
                }
            });
    }

    fn log_viewport(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.style_mut().spacing.item_spacing = egui::vec2(0.0, 0.0);
            let total_matching_rows = self.engine.matching_rows_count();
            let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
            if self.follow && total_matching_rows > 0 {
                scroll = scroll.vertical_scroll_offset(total_matching_rows as f32 * ROW_HEIGHT);
            } else if let Some(row) = self.scroll_to_row.take() {
                scroll = scroll.vertical_scroll_offset(row as f32 * ROW_HEIGHT);
            }
            scroll.show_rows(ui, ROW_HEIGHT, total_matching_rows, |ui, range| {
                let page = self.engine.viewport(ViewportRequest {
                    first_row: range.start,
                    row_count: range.end.saturating_sub(range.start),
                });
                for row in page.rows {
                    self.log_row(ui, row);
                }
            });
        });
    }

    fn log_row(&mut self, ui: &mut egui::Ui, row: RowPresentation) {
        let is_selected = self.selected_row.as_ref().map(|r| r.row_id) == Some(row.row_id);
        let (rect, response) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ROW_HEIGHT),
            egui::Sense::click(),
        );
        if response.clicked() {
            self.selected_row = Some(row.clone());
        }

        let color = severity_color(row.severity_role());
        let painter = ui.painter_at(rect);
        if is_selected {
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(40, 60, 80));
        } else if row.row_id.0.is_multiple_of(2) {
            painter.rect_filled(rect, 0.0, egui::Color32::from_gray(18));
        }
        painter.rect_filled(
            egui::Rect::from_min_max(
                rect.left_top(),
                egui::pos2(rect.left() + 4.0, rect.bottom()),
            ),
            0.0,
            color,
        );

        let mut x = rect.left() + 10.0;
        if row.is_bookmarked {
            x = paint_text(
                ui,
                &painter,
                x,
                rect,
                "*",
                egui::Color32::from_rgb(230, 120, 230),
            );
        }
        if row.folded_stack_rows > 0 {
            x = paint_text(
                ui,
                &painter,
                x,
                rect,
                &format!("+{} ", row.folded_stack_rows),
                egui::Color32::GRAY,
            );
        }
        if let Some(source_name) = row.source_name.as_ref() {
            x = paint_text(
                ui,
                &painter,
                x,
                rect,
                &format!("[{source_name}] "),
                egui::Color32::GRAY,
            );
        }
        for span in &row.spans {
            x = paint_span(ui, &painter, x, rect, span);
        }
    }

    fn command_palette(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.key_pressed(egui::Key::K) && input.modifiers.command) {
            self.command_palette_open = true;
        }

        if !self.command_palette_open {
            return;
        }

        egui::Window::new("Command Palette")
            .collapsible(false)
            .resizable(false)
            .fixed_size([460.0, 260.0])
            .anchor(egui::Align2::CENTER_TOP, [0.0, 120.0])
            .show(ctx, |ui| {
                ui.label("Type a command or choose one below.");
                let response = ui.add(
                    egui::TextEdit::singleline(&mut self.command_text)
                        .hint_text("type a command and press Enter"),
                );
                if response.lost_focus() && ui.input(|input| input.key_pressed(egui::Key::Enter)) {
                    self.run_command_text();
                }
                ui.separator();
                egui::Grid::new("commands").num_columns(2).show(ui, |ui| {
                    if ui.button("Clear filter").clicked() {
                        self.clear_filter();
                    }
                    ui.label("Remove query/level filter");
                    ui.end_row();
                    if ui.button("Clear search").clicked() {
                        self.clear_search();
                    }
                    ui.label("Remove search highlights");
                    ui.end_row();
                    if ui.button("Next search").clicked() {
                        self.select_search_result(false);
                    }
                    ui.label("Jump to next highlighted row");
                    ui.end_row();
                    if ui.button("Previous search").clicked() {
                        self.select_search_result(true);
                    }
                    ui.label("Jump to previous highlighted row");
                    ui.end_row();
                    if ui.button("Toggle bookmark").clicked() {
                        self.toggle_selected_bookmark();
                    }
                    ui.label("Bookmark selected row");
                    ui.end_row();
                    if ui.button("Toggle stack folding").clicked() {
                        self.fold_stacks = !self.fold_stacks;
                        self.engine.set_stack_trace_folding(self.fold_stacks);
                    }
                    ui.label("Collapse/expand stack continuations");
                    ui.end_row();
                });
                if ui.button("Save session").clicked() {
                    self.save_session();
                }
                if ui.button("Close").clicked() {
                    self.command_palette_open = false;
                }
            });
    }

    fn drain_live_events(&mut self) {
        let Some(live_tail) = self.live_tail.as_mut() else {
            return;
        };

        let mut changed = false;
        loop {
            match live_tail.receiver.try_recv() {
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    self.engine
                        .add_source(source_id, path.display().to_string());
                }
                Ok(LogEvent::RowAppended(row)) => {
                    self.engine.append_row(row);
                    changed = true;
                }
                Ok(LogEvent::SourceRotated { source_id }) => {
                    self.status_message = Some(format!("source {} rotated", source_id.0));
                }
                Ok(LogEvent::SourceError { source_id, message }) => {
                    self.status_message = Some(format!("source {} error: {message}", source_id.0));
                }
                Ok(LogEvent::SourceRemoved { .. }) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    self.status_message = Some("live tail disconnected".into());
                    break;
                }
            }
        }

        if changed {
            self.save_session();
        }
    }

    fn keyboard_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input(|input| input.key_pressed(egui::Key::F) && input.modifiers.command) {
            self.follow = !self.follow;
        }
        if ctx.input(|input| input.key_pressed(egui::Key::B) && input.modifiers.command) {
            self.toggle_selected_bookmark();
        }
        if ctx.input(|input| input.key_pressed(egui::Key::N) && input.modifiers.command) {
            self.select_search_result(false);
        }
        if ctx.input(|input| {
            input.key_pressed(egui::Key::N) && input.modifiers.command && input.modifiers.shift
        }) {
            self.select_search_result(true);
        }
    }

    fn run_command_text(&mut self) {
        match self.command_text.trim() {
            "clear filter" => self.clear_filter(),
            "clear search" => self.clear_search(),
            "next search" => self.select_search_result(false),
            "previous search" => self.select_search_result(true),
            "toggle bookmark" => self.toggle_selected_bookmark(),
            "fold stacks" => {
                self.fold_stacks = true;
                self.engine.set_stack_trace_folding(true);
            }
            "unfold stacks" => {
                self.fold_stacks = false;
                self.engine.set_stack_trace_folding(false);
            }
            "toggle follow" => self.follow = !self.follow,
            "save session" => self.save_session(),
            _ => self.status_message = Some("unknown command".into()),
        }
        self.command_text.clear();
        self.command_palette_open = false;
    }

    fn clear_filter(&mut self) {
        self.filter_text.clear();
        self.engine.clear_filter();
    }

    fn clear_search(&mut self) {
        self.search_text.clear();
        self.engine.set_search_text(None);
    }

    fn select_search_result(&mut self, reverse: bool) {
        let current = self.selected_row.as_ref().map(|row| row.row_id);
        let Some(row_id) = self.engine.next_search_result(current, reverse) else {
            self.status_message = Some("no search results".into());
            return;
        };
        self.select_row(row_id);
    }

    fn select_row(&mut self, row_id: RowId) {
        let Some(position) = self.engine.filtered_position_for_row(row_id) else {
            self.status_message = Some(format!("row {} is not visible", row_id.0));
            return;
        };
        let snapshot = self.engine.viewport(ViewportRequest {
            first_row: position,
            row_count: 1,
        });
        self.selected_row = snapshot.rows.into_iter().next();
        self.scroll_to_row = Some(position);
        self.follow = false;
    }

    fn toggle_selected_bookmark(&mut self) {
        let Some(row_id) = self.selected_row.as_ref().map(|row| row.row_id) else {
            self.status_message = Some("select a row before bookmarking".into());
            return;
        };
        let is_bookmarked = self.engine.toggle_bookmark(row_id, None);
        if let Some(row) = self.selected_row.as_mut() {
            row.is_bookmarked = is_bookmarked;
        }
        self.save_session();
    }

    fn save_current_filter(&mut self) {
        let name = self.saved_filter_name.trim();
        if name.is_empty() {
            self.status_message = Some("enter a saved filter name".into());
            return;
        }
        self.engine.save_filter(name.to_owned());
        self.status_message = Some(format!("saved filter {name}"));
        self.save_session();
    }

    fn apply_saved_filter(&mut self, name: &str) {
        match self.engine.apply_saved_filter(name) {
            Ok(true) => {
                self.status_message = Some(format!("applied filter {name}"));
            }
            Ok(false) => self.status_message = Some(format!("saved filter not found: {name}")),
            Err(err) => self.status_message = Some(err.to_string()),
        }
    }

    fn save_session(&mut self) {
        match save_session(self.session_path.as_ref(), self.engine.session()) {
            Ok(()) => {
                if self.session_path.is_some() {
                    self.status_message = Some("session saved".into());
                }
            }
            Err(err) => self.status_message = Some(err.to_string()),
        }
    }
}

impl eframe::App for GlowtailGui {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Drain incoming events first so the follow-mode scroll target below
        // is computed against the up-to-date row count, not the previous
        // frame's stale total.
        self.drain_live_events();
        self.keyboard_shortcuts(ctx);
        let metadata = self.engine.metadata_snapshot();
        self.top_bar(ctx);
        self.source_sidebar(ctx, &metadata);
        self.detail_panel(ctx);
        self.timeline_panel(ctx, &metadata);
        self.log_viewport(ctx);
        self.command_palette(ctx);
        ctx.request_repaint_after(std::time::Duration::from_millis(100));
    }
}

impl Drop for GlowtailGui {
    fn drop(&mut self) {
        self.save_session();
        if let Some(mut live_tail) = self.live_tail.take() {
            for tailer in live_tail.tailers.drain(..) {
                self.runtime.block_on(tailer.stop());
            }
        }
    }
}

fn paint_text(
    _ui: &egui::Ui,
    painter: &egui::Painter,
    x: f32,
    rect: egui::Rect,
    text: &str,
    color: egui::Color32,
) -> f32 {
    let galley = painter.layout_no_wrap(text.to_owned(), egui::FontId::monospace(13.0), color);
    let width = galley.size().x;
    painter.galley(
        egui::pos2(x, rect.center().y - galley.size().y / 2.0),
        galley,
        color,
    );
    x + width
}

fn paint_span(
    _ui: &egui::Ui,
    painter: &egui::Painter,
    x: f32,
    rect: egui::Rect,
    span: &StyledSpan,
) -> f32 {
    let color = span_color(span);
    let galley =
        painter.layout_no_wrap(span.text.to_string(), egui::FontId::monospace(13.0), color);
    if span.kind == SpanKind::SearchMatch {
        painter.rect_filled(
            egui::Rect::from_min_size(
                egui::pos2(x, rect.center().y - galley.size().y / 2.0),
                galley.size(),
            ),
            2.0,
            egui::Color32::from_rgb(170, 220, 80),
        );
    }
    painter.galley(
        egui::pos2(x, rect.center().y - galley.size().y / 2.0),
        galley.clone(),
        color,
    );
    x + galley.size().x
}

fn span_color(span: &StyledSpan) -> egui::Color32 {
    match span.kind {
        SpanKind::Timestamp => egui::Color32::from_rgb(120, 170, 230),
        SpanKind::Error => egui::Color32::from_rgb(255, 90, 90),
        SpanKind::Warning => egui::Color32::from_rgb(240, 190, 80),
        SpanKind::SearchMatch => egui::Color32::BLACK,
        SpanKind::JsonKey => egui::Color32::from_rgb(90, 210, 220),
        SpanKind::JsonValue => egui::Color32::from_rgb(140, 220, 150),
        _ => egui::Color32::from_gray(220),
    }
}

fn severity_color(role: SeverityRole) -> egui::Color32 {
    match role {
        SeverityRole::Fatal | SeverityRole::Error => egui::Color32::from_rgb(220, 70, 70),
        SeverityRole::Warn => egui::Color32::from_rgb(230, 180, 70),
        SeverityRole::Info => egui::Color32::from_rgb(80, 150, 220),
        SeverityRole::Debug | SeverityRole::Trace => egui::Color32::from_rgb(120, 120, 160),
        SeverityRole::Unknown => egui::Color32::from_gray(70),
    }
}
