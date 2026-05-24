use anyhow::{Context, Result};
use clap::Parser;
use eframe::egui;
use glowtail_core::filter::parse_filter_query;
use glowtail_core::prelude::*;
use glowtail_ui_common::{
    LevelArg, LiveTail, apply_filters, load_session, parser_from_flags, save_session, start_tailers,
};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc;

const ROW_HEIGHT: f32 = 22.0;
const HORIZONTAL_STEP_PX: f32 = 8.0;

/// Clamp `current + delta` to `[0, total - 1]`. Returns `0` when `total == 0`.
fn scroll_target(current: usize, delta: isize, total: usize) -> usize {
    if total == 0 {
        return 0;
    }
    let max = (total - 1) as isize;
    (current as isize + delta).clamp(0, max) as usize
}

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
    /// Retain at most this many rows; older rows are dropped from the front
    /// of the buffer when the cap is exceeded. `0` means unbounded (default).
    /// Recommended when tailing high-volume files for a long time.
    #[arg(long)]
    max_rows: Option<usize>,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let session = load_session(args.session.as_ref())?;
    let parser = parser_from_flags(args.json, args.plain);
    // Accumulate per-path errors instead of returning the first failure so a
    // single unreadable path doesn't prevent the GUI from launching with the
    // rows it *could* load. Errors are surfaced as the initial status message.
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

    let initial_status = if load_errors.is_empty() {
        None
    } else {
        Some(load_errors.join("; "))
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
                initial_status,
            )))
        }),
    )
    .map_err(|err| anyhow::anyhow!("{err}"))
}

/// Treat `--max-rows 0` and an absent flag as "unbounded" so the surface is
/// forgiving — `0` reading as "no rows retained" is a usability trap.
fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
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
    /// Kept alive to host the spawned `FileTailer` tasks. Not read after
    /// construction — `Drop` order ensures it outlives `live_tail`, so the
    /// runtime drives the tasks to completion after `signal_stop` (M5).
    #[allow(dead_code)]
    runtime: Runtime,
    live_tail: Option<LiveTail>,
    status_message: Option<String>,
    saved_filter_name: String,
    scroll_to_row: Option<usize>,
    horizontal_offset_px: f32,
    current_first_row: usize,
    current_page_size: usize,
}

impl GlowtailGui {
    fn new(
        engine: Engine,
        filter_text: String,
        session_path: Option<PathBuf>,
        runtime: Runtime,
        live_tail: Option<LiveTail>,
        status_message: Option<String>,
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
            status_message,
            saved_filter_name: String::new(),
            scroll_to_row: None,
            horizontal_offset_px: 0.0,
            current_first_row: 0,
            current_page_size: 1,
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
                    if self.filter_text.trim().is_empty() {
                        self.engine.clear_filter();
                    } else if let Err(err) = parse_filter_query(&self.filter_text)
                        .and_then(|filter| self.engine.set_filter(filter))
                    {
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
                    let evicted = self.engine.evicted_row_count();
                    if evicted > 0 {
                        ui.separator();
                        ui.colored_label(
                            egui::Color32::from_rgb(214, 163, 61),
                            format!("truncated: {evicted} oldest rows dropped"),
                        );
                    }
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
                    let color = if bucket.error_count() > 0 {
                        egui::Color32::from_rgb(220, 70, 70)
                    } else if bucket.warn_count() > 0 {
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
                // Scroll so the last row sits at the bottom of the viewport
                // rather than one row past the bottom edge. The previous
                // `total * ROW_HEIGHT` placed the row *after* the last at
                // the top, leaving an empty band beneath the data.
                let viewport_h = ui.available_height();
                let needed = (total_matching_rows as f32 * ROW_HEIGHT - viewport_h).max(0.0);
                scroll = scroll.vertical_scroll_offset(needed);
            } else if let Some(row) = self.scroll_to_row.take() {
                scroll = scroll.vertical_scroll_offset(row as f32 * ROW_HEIGHT);
            }
            scroll.show_rows(ui, ROW_HEIGHT, total_matching_rows, |ui, range| {
                self.current_first_row = range.start;
                self.current_page_size = range.end.saturating_sub(range.start).max(1);
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

        let mut x = rect.left() + 10.0 - self.horizontal_offset_px;
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
                Ok(_) => {}
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

        // Scroll navigation. `consume_key` prevents egui's ScrollArea from
        // double-handling arrow/page keys.
        let total = self.engine.matching_rows_count();
        let first = self.current_first_row;
        let page = self.current_page_size.max(1) as isize;

        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp)) {
            self.scroll_to_row = Some(scroll_target(first, -1, total));
            self.follow = false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown)) {
            self.scroll_to_row = Some(scroll_target(first, 1, total));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageUp)) {
            self.scroll_to_row = Some(scroll_target(first, -page, total));
            self.follow = false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::PageDown)) {
            self.scroll_to_row = Some(scroll_target(first, page, total));
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Home)) {
            self.scroll_to_row = Some(0);
            self.follow = false;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::End)) {
            self.follow = true;
        }

        // Horizontal scroll.
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowLeft)) {
            self.horizontal_offset_px = (self.horizontal_offset_px - HORIZONTAL_STEP_PX).max(0.0);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::ArrowRight)) {
            self.horizontal_offset_px += HORIZONTAL_STEP_PX;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::COMMAND, egui::Key::ArrowLeft)) {
            self.horizontal_offset_px = 0.0;
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

/// Polling cadence for draining the live-tail channel while a tailer is
/// active. 16ms ≈ 60 Hz — matches typical monitor refresh and, combined with
/// `DEFAULT_TAILER_CHANNEL_CAPACITY`, lifts sustained tail throughput from
/// ~10k rows/s (at 100ms) to ~1M rows/s without per-row repaint cost since
/// `drain_live_events` already coalesces all pending events per frame.
const LIVE_POLL_INTERVAL_MS: u64 = 16;
/// Polling cadence when there's no active live tail. egui already repaints
/// on input; this is just a slow heartbeat so e.g. status TTL has a chance
/// to fire. Slower polling here saves idle CPU.
const IDLE_POLL_INTERVAL_MS: u64 = 1000;

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
        // Coalesce repaints: only run the fast polling loop while there's a
        // live tail to drain. Idle sessions repaint on input plus a slow
        // heartbeat. The previous unconditional 100ms heartbeat burned CPU
        // on static logs even when nothing changed.
        let next_poll_ms = if self.live_tail.is_some() {
            LIVE_POLL_INTERVAL_MS
        } else {
            IDLE_POLL_INTERVAL_MS
        };
        ctx.request_repaint_after(std::time::Duration::from_millis(next_poll_ms));
    }
}

impl Drop for GlowtailGui {
    fn drop(&mut self) {
        self.save_session();
        // Don't `block_on(stop())` here — that blocks the UI thread on
        // shutdown and panics when invoked from inside a Tokio worker. Flip
        // the per-tailer stop flag and let the runtime drive the spawned
        // tasks to completion when the runtime itself is dropped below.
        if let Some(live_tail) = self.live_tail.take() {
            for tailer in &live_tail.tailers {
                tailer.signal_stop();
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

#[cfg(test)]
mod tests {
    use super::scroll_target;

    #[test]
    fn scroll_target_clamps_below_zero() {
        assert_eq!(scroll_target(0, -1, 100), 0);
        assert_eq!(scroll_target(5, -10, 100), 0);
    }

    #[test]
    fn scroll_target_advances_in_range() {
        assert_eq!(scroll_target(50, 10, 100), 60);
    }

    #[test]
    fn scroll_target_clamps_above_last() {
        assert_eq!(scroll_target(99, 10, 100), 99);
        assert_eq!(scroll_target(0, 1_000, 100), 99);
    }

    #[test]
    fn scroll_target_with_empty_list_returns_zero() {
        assert_eq!(scroll_target(0, 0, 0), 0);
        assert_eq!(scroll_target(10, 5, 0), 0);
    }
}
