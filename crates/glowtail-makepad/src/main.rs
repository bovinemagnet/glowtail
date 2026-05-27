//! Makepad-based desktop UI for glowtail. Fourth long-term sibling to
//! `glowtail-gui` (egui/eframe), `glowtail-gpui` (GPUI), and
//! `glowtail-iced` (Iced).
//!
//! Layer 1 (viewport rendering): a custom [`LogList`] widget wraps
//! `PortalList`, the engine's [`Engine::viewport`] populates it each
//! time state changes, and a `NextFrame` event drains the live-tail
//! channel so newly appended rows appear without a manual refresh.
//!
//! Layer 2 (keyboard interaction): selection cursor (`j`/`k`/`↑`/`↓`,
//! `PgUp`/`PgDn`/`Home`/`End`), bookmark toggle (`b`), search nav
//! (`n`/`N`), saved-filter cycling (`s`), level hotkeys (`0`-`6`),
//! follow toggle (`f`), stack folding (`z`), filter text input (`/`)
//! and search text input (`?`) with the same Normal/Filter/Search
//! `InputMode` gating that `glowtail-iced` uses.
//!
//! Layer 3 (chrome): JSON detail panel for the selected row, search-
//! match row highlighting. Per-span colouring inside a row remains a
//! known gap — Makepad's retained-widget model doesn't lend itself to
//! the per-`StyledSpan` `<div>` per row the other front-ends use, so
//! the whole row is currently tinted by severity. A source sidebar and
//! command palette also stay queued.

use anyhow::Result;
use clap::Parser;
use glowtail_core::prelude::*;
use glowtail_ui_common::{
    LevelArg, LiveTail, apply_filters, load_session, parser_from_flags, save_session, start_tailers,
};
use makepad_widgets::*;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::mpsc::error::TryRecvError;

/// Window over the filtered set requested from the engine each frame.
/// `PortalList` is internally virtualised — it will only realise widgets
/// for the rows currently on screen — but we still need a bounded upper
/// limit so the populate loop terminates. 1024 rows comfortably exceeds
/// any visible viewport at typical font sizes.
const PAGE_SIZE: usize = 1024;

#[derive(Debug, Parser, Clone)]
#[command(name = "glowtail-makepad")]
#[command(about = "Makepad glowtail desktop UI")]
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

fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
}

/// Parsed CLI args, populated by [`main`] before the Makepad event loop
/// starts. `app_main!` only generates `pub fn app_main()` in
/// `makepad-platform` 1.0, so the binary's `fn main()` is ours to write —
/// we parse args first (clap exits cleanly on `--help`/`--version`), stash
/// them here, then hand off to Makepad.
static ARGS: OnceLock<Args> = OnceLock::new();

fn cli_args() -> &'static Args {
    ARGS.get().expect("CLI args populated before app start")
}

fn main() {
    let args = Args::parse();
    ARGS.set(args).expect("main() runs exactly once");
    app_main();
}

live_design! {
    use link::theme::*;
    use link::widgets::*;

    LogListBase = {{LogList}} {}

    pub LogList = <LogListBase> {
        width: Fill, height: Fill,
        list = <PortalList> {
            width: Fill, height: Fill,
            flow: Down,
            LogRow = <View> {
                width: Fill, height: Fit,
                padding: { top: 2, bottom: 2, left: 6, right: 6 },
                row_label = <Label> {
                    width: Fill,
                    text: "",
                    draw_text: { text_style: { font_size: 11.0 } }
                }
            }
        }
    }

    App = {{App}} {
        ui: <Root> {
            main_window = <Window> {
                window: { title: "glowtail (makepad)" },
                body = <View> {
                    flow: Down,
                    padding: 0,
                    spacing: 0,

                    header = <View> {
                        width: Fill, height: Fit,
                        padding: 8,
                        spacing: 8,
                        flow: Right,
                        title_label = <Label> {
                            text: "glowtail",
                            draw_text: { text_style: { font_size: 13.0 } }
                        }
                        filter_input = <TextInput> {
                            width: 240, height: Fit,
                            empty_text: "filter…",
                        }
                        search_input = <TextInput> {
                            width: 200, height: Fit,
                            empty_text: "search…",
                        }
                        status_label = <Label> {
                            width: Fill,
                            text: "loading…",
                            draw_text: { text_style: { font_size: 12.0 } }
                        }
                    }

                    main_row = <View> {
                        width: Fill, height: Fill,
                        flow: Right,
                        sidebar = <View> {
                            width: 220, height: Fill,
                            padding: 8,
                            flow: Down,
                            spacing: 4,
                            show_bg: true,
                            draw_bg: { color: #14141a },
                            sidebar_title = <Label> {
                                text: "Sources",
                                draw_text: {
                                    text_style: { font_size: 12.0 },
                                    color: #c8a2c8,
                                }
                            }
                            sources_label = <Label> {
                                width: Fill,
                                text: "",
                                draw_text: {
                                    text_style: { font_size: 11.0 },
                                    color: #e6e6e6,
                                }
                            }
                        }
                        log_list = <LogList> {}
                    }

                    detail_panel = <View> {
                        width: Fill, height: Fit,
                        padding: { top: 8, bottom: 8, left: 8, right: 8 },
                        flow: Down,
                        spacing: 4,
                        show_bg: true,
                        draw_bg: { color: #1a1a1a },
                        detail_title = <Label> {
                            text: "",
                            draw_text: {
                                text_style: { font_size: 11.0 },
                                color: #c8a2c8,
                            }
                        }
                        detail_body = <Label> {
                            width: Fill,
                            text: "",
                            draw_text: {
                                text_style: { font_size: 11.0 },
                                color: #fbbc04,
                            }
                        }
                    }

                    footer = <View> {
                        width: Fill, height: Fit,
                        padding: 6,
                        error_label = <Label> {
                            text: "",
                            draw_text: {
                                text_style: { font_size: 12.0 },
                                color: #ff6b6b,
                            }
                        }
                    }

                    palette_view = <View> {
                        width: Fill, height: Fill,
                        padding: 24,
                        spacing: 8,
                        flow: Down,
                        visible: false,
                        show_bg: true,
                        draw_bg: { color: #101014 },
                        palette_title = <Label> {
                            text: "Command palette",
                            draw_text: {
                                text_style: { font_size: 14.0 },
                                color: #c8a2c8,
                            }
                        }
                        palette_input = <TextInput> {
                            width: Fill, height: Fit,
                            empty_text: "type to filter…",
                        }
                        palette_items_label = <Label> {
                            width: Fill,
                            text: "",
                            draw_text: { text_style: { font_size: 12.0 } }
                        }
                        palette_hint = <Label> {
                            text: "↵ run  •  ↑↓/jk navigate  •  esc close",
                            draw_text: {
                                text_style: { font_size: 10.0 },
                                color: #666666,
                            }
                        }
                    }
                }
            }
        }
    }
}

app_main!(App);

#[derive(Live, LiveHook)]
pub struct App {
    #[live]
    ui: WidgetRef,
    #[rust]
    state: AppState,
}

#[derive(Default)]
struct AppState {
    engine: Option<Engine>,
    session_path: Option<PathBuf>,
    runtime: Option<Runtime>,
    live_tail: Option<LiveTail>,
    last_error: Option<String>,
    status_message: Option<String>,
    /// Always-on poll timer. `NextFrame` events fire once per frame and
    /// give us a place to drain the tailer channel without bringing tokio
    /// into Makepad's event loop.
    next_frame: Option<NextFrame>,
    follow: bool,
    /// Row currently under the selection cursor. Anchors bookmark
    /// toggles, search-result jumps, and the JSON detail panel. Mirrors
    /// the iced/gpui semantics.
    selected_row_id: Option<RowId>,
    /// Active level filter (composes with `filter_text`). Set by the
    /// 0-6 hotkeys; persisted as a session filter via `apply_filters`.
    level: Option<LevelArg>,
    /// Free-text filter as last submitted. Currently only mutated by
    /// the initial CLI flag and the saved-filter cycle; will become
    /// editable when the filter text input lands in the next commit.
    filter_text: String,
    /// Position in `engine.session().saved_filters` for the `s`-cycle.
    /// `None` means "no saved filter active".
    saved_filter_index: Option<usize>,
    /// Stack-trace folding toggle (`z` key). When `true`, continuation
    /// lines of a stack trace are hidden behind a folded badge on the
    /// header row.
    fold_stacks: bool,
    /// Which text input (if any) has key focus. `/` enters Filter mode,
    /// `?` enters Search mode, `Cmd+K`/`Ctrl+K` enters Palette mode.
    mode: InputMode,
    /// Substring used to filter command palette items, mirrored from
    /// the `palette_input` TextInput on every `changed` action.
    palette_query: String,
    /// Index of the highlighted palette item. Reset to 0 whenever the
    /// query changes so the cursor never points past the filtered set.
    palette_selected: usize,
}

/// Single-letter shortcuts only fire in `Normal` mode — when the
/// filter/search/palette inputs have key focus all character keys are
/// consumed by the TextInput. Mirrors the gating in `glowtail-iced`.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum InputMode {
    #[default]
    Normal,
    Filter,
    Search,
    Palette,
}

/// A single command palette entry. Mirrors `glowtail-iced::PaletteItem`
/// so muscle memory carries between front-ends.
#[derive(Debug, Clone)]
struct PaletteItem {
    label: String,
    kind: PaletteKind,
}

#[derive(Debug, Clone)]
enum PaletteKind {
    ToggleFollow,
    ToggleStackFolding,
    ClearLevel,
    SetLevel(LevelArg),
    CycleSavedFilter,
    ApplySavedFilter(String),
    ClearFilter,
    ClearSearch,
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        makepad_widgets::live_design(cx);
    }
}

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        match self.bootstrap() {
            Ok(()) => {
                // Seed the filter input with any --filter from the CLI so
                // the user can edit it without retyping.
                let filter_text = self.state.filter_text.clone();
                self.ui
                    .text_input(id!(filter_input))
                    .set_text(cx, &filter_text);
                self.refresh_status(cx);
            }
            Err(err) => {
                self.state.last_error = Some(err.to_string());
                self.refresh_status(cx);
            }
        }
        // Kick off the per-frame polling loop. Each `NextFrame` handler
        // re-requests another tick so the loop runs for the app's
        // lifetime — when there's nothing to do the loop is cheap; when
        // rows are streaming in it keeps the list current.
        self.state.next_frame = Some(cx.new_next_frame());
    }

    fn handle_actions(&mut self, cx: &mut Cx, actions: &Actions) {
        let filter = self.ui.text_input(id!(filter_input));
        let search = self.ui.text_input(id!(search_input));
        let palette = self.ui.text_input(id!(palette_input));

        if let Some(query) = palette.changed(actions) {
            self.state.palette_query = query;
            self.state.palette_selected = 0;
            self.refresh_palette_items(cx);
        }
        if palette.returned(actions).is_some() {
            self.run_palette_item(cx);
            self.push_rows_to_list(cx);
            self.refresh_status(cx);
        }
        if palette.escaped(actions) {
            self.close_palette(cx);
        }

        if let Some((text, _)) = filter.returned(actions) {
            self.state.filter_text = text;
            self.reapply_filters();
            self.state.mode = InputMode::Normal;
            self.push_rows_to_list(cx);
            self.refresh_status(cx);
        }
        if filter.escaped(actions) {
            // Restore the previously-applied filter and leave focus.
            let filter_text = self.state.filter_text.clone();
            filter.set_text(cx, &filter_text);
            self.state.mode = InputMode::Normal;
        }

        if let Some((text, _)) = search.returned(actions) {
            let needle = if text.is_empty() { None } else { Some(text) };
            if let Some(engine) = self.state.engine.as_mut() {
                engine.set_search_text(needle);
            }
            self.state.mode = InputMode::Normal;
            self.push_rows_to_list(cx);
            self.refresh_status(cx);
        }
        if search.escaped(actions) {
            search.set_text(cx, "");
            if let Some(engine) = self.state.engine.as_mut() {
                engine.set_search_text(None);
            }
            self.state.mode = InputMode::Normal;
            self.push_rows_to_list(cx);
            self.refresh_status(cx);
        }
    }
}

impl App {
    /// Mirror of the bootstrap flow in `glowtail-iced::GlowtailIced::new` and
    /// `glowtail-gui::main`: load session, pick parser, preload paths,
    /// compose filters, start tailers. Per-path read errors are accumulated
    /// into [`AppState::last_error`] so a single bad path doesn't stop the
    /// app launching with whatever else loaded.
    fn bootstrap(&mut self) -> Result<()> {
        let args = cli_args().clone();
        let session = load_session(args.session.as_ref())?;
        let parser = parser_from_flags(args.json, args.plain);

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

        if let Err(err) = apply_filters(
            &mut engine,
            args.filter.clone(),
            args.level,
            args.use_filter.clone(),
            args.save_filter.clone(),
        ) {
            load_errors.push(format!("filter error: {err}"));
        }

        let runtime = Builder::new_multi_thread()
            .enable_all()
            .thread_name("glowtail-makepad-tail")
            .build()?;
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

        self.state.follow = live_tail.is_some();
        self.state.engine = Some(engine);
        self.state.session_path = args.session;
        self.state.runtime = Some(runtime);
        self.state.live_tail = live_tail;
        self.state.level = args.level;
        self.state.filter_text = args.filter.unwrap_or_default();
        if !load_errors.is_empty() {
            self.state.last_error = Some(load_errors.join("; "));
        }
        Ok(())
    }

    /// Re-compose and re-apply the active filter set. Called when the
    /// 0-6 hotkeys mutate the level filter and when the saved-filter
    /// cycle returns to None. Failures go to `last_error` rather than
    /// the status line so the user keeps a record until the next
    /// successful apply.
    fn reapply_filters(&mut self) {
        let Some(engine) = self.state.engine.as_mut() else {
            return;
        };
        let filter_text = if self.state.filter_text.is_empty() {
            None
        } else {
            Some(self.state.filter_text.clone())
        };
        match apply_filters(engine, filter_text, self.state.level, None, None) {
            Ok(_) => self.state.last_error = None,
            Err(err) => self.state.last_error = Some(err.to_string()),
        }
    }

    /// Move the selection cursor by `delta` rows in the filtered set.
    /// Disables follow mode (the user is now driving navigation) and
    /// returns the new position so the caller can scroll the list.
    fn move_selection(&mut self, delta: isize) -> Option<usize> {
        let engine = self.state.engine.as_mut()?;
        let total = engine.matching_rows_count();
        if total == 0 {
            return None;
        }
        let current = self
            .state
            .selected_row_id
            .and_then(|id| engine.filtered_position_for_row(id))
            .unwrap_or(0);
        let max = total.saturating_sub(1) as isize;
        let next = (current as isize + delta).clamp(0, max) as usize;
        let row = engine.present_row_at(next)?;
        self.state.selected_row_id = Some(row.row_id);
        self.state.follow = false;
        Some(next)
    }

    /// `n` / `N` — jump the cursor to the next or previous search match.
    /// No-ops gracefully when the search needle is empty.
    fn jump_search(&mut self, reverse: bool) {
        let engine = match self.state.engine.as_mut() {
            Some(engine) => engine,
            None => return,
        };
        match engine.next_search_result(self.state.selected_row_id, reverse) {
            Some(next) => {
                self.state.selected_row_id = Some(next);
                self.state.follow = false;
                self.state.status_message = None;
            }
            None => {
                self.state.status_message = Some("no search matches".into());
            }
        }
    }

    /// `s` — cycle through saved filters loaded from `--session`.
    /// Mirrors the order used by `glowtail-iced` and `glowtail-gpui`.
    fn cycle_saved_filter(&mut self) {
        let Some(engine) = self.state.engine.as_mut() else {
            return;
        };
        let count = engine.session().saved_filters.len();
        if count == 0 {
            self.state.status_message = Some("no saved filters in session".into());
            return;
        }
        let next_index = match self.state.saved_filter_index {
            None => Some(0),
            Some(i) if i + 1 < count => Some(i + 1),
            Some(_) => None,
        };
        self.state.saved_filter_index = next_index;
        match next_index {
            Some(index) => {
                let name = engine.session().saved_filters[index].name.clone();
                match engine.apply_saved_filter(&name) {
                    Ok(true) => {
                        self.state.last_error = None;
                        self.state.status_message = Some(format!("saved filter: {name}"));
                    }
                    Ok(false) | Err(_) => {
                        self.state.last_error = Some(format!("could not load saved filter {name}"));
                    }
                }
            }
            None => {
                self.reapply_filters();
                self.state.status_message = Some("saved filter: (none)".into());
            }
        }
    }

    /// `b` — toggle bookmark on the currently selected row. Persisted
    /// to the session via [`Drop`] when `--session` is supplied.
    fn toggle_bookmark_for_selection(&mut self) {
        let Some(row_id) = self.state.selected_row_id else {
            self.state.status_message =
                Some("select a row first (j/k or ↑/↓) before bookmarking".into());
            return;
        };
        if let Some(engine) = self.state.engine.as_mut() {
            engine.toggle_bookmark(row_id, None);
        }
    }

    /// `z` — toggle the engine's stack-trace folding so continuation
    /// lines of a Java/Rust stack trace collapse under the header row.
    fn toggle_stack_folding(&mut self) {
        self.state.fold_stacks = !self.state.fold_stacks;
        if let Some(engine) = self.state.engine.as_mut() {
            engine.set_stack_trace_folding(self.state.fold_stacks);
        }
    }

    /// Open the command palette overlay. Hides the regular body so
    /// only the palette is visible and focuses the palette input so
    /// typing immediately filters.
    fn open_palette(&mut self, cx: &mut Cx) {
        self.state.mode = InputMode::Palette;
        self.state.palette_query.clear();
        self.state.palette_selected = 0;
        self.toggle_palette_visibility(cx, true);
        let ti = self.ui.text_input(id!(palette_input));
        if let Some(inner) = ti.borrow_mut() {
            inner.set_key_focus(cx);
        }
        ti.set_text(cx, "");
        self.refresh_palette_items(cx);
    }

    fn close_palette(&mut self, cx: &mut Cx) {
        self.state.mode = InputMode::Normal;
        self.state.palette_query.clear();
        self.state.palette_selected = 0;
        self.toggle_palette_visibility(cx, false);
    }

    fn toggle_palette_visibility(&mut self, cx: &mut Cx, palette_open: bool) {
        let visible_main = !palette_open;
        for id in [id!(header), id!(main_row), id!(footer)] {
            self.ui
                .view(id)
                .apply_over(cx, live! { visible: (visible_main) });
        }
        // The detail panel hides itself when no JSON fields are
        // present, so only force it back on if the palette is opening.
        if palette_open {
            self.ui
                .view(id!(detail_panel))
                .apply_over(cx, live! { visible: false });
        }
        self.ui
            .view(id!(palette_view))
            .apply_over(cx, live! { visible: (palette_open) });
    }

    /// Build the list of palette items based on the current engine
    /// state. Items mirror the iced front-end exactly so users keep
    /// the same vocabulary across siblings.
    fn palette_items(&self) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        items.push(PaletteItem {
            label: if self.state.follow {
                "Disable follow".into()
            } else {
                "Enable follow".into()
            },
            kind: PaletteKind::ToggleFollow,
        });
        items.push(PaletteItem {
            label: if self.state.fold_stacks {
                "Show stack traces".into()
            } else {
                "Fold stack traces".into()
            },
            kind: PaletteKind::ToggleStackFolding,
        });
        if self.state.level.is_some() {
            items.push(PaletteItem {
                label: "Clear level filter".into(),
                kind: PaletteKind::ClearLevel,
            });
        }
        for level in [
            LevelArg::Trace,
            LevelArg::Debug,
            LevelArg::Info,
            LevelArg::Warn,
            LevelArg::Error,
            LevelArg::Fatal,
        ] {
            items.push(PaletteItem {
                label: format!("Set level: {}", level_label(Some(level))),
                kind: PaletteKind::SetLevel(level),
            });
        }
        if let Some(engine) = self.state.engine.as_ref()
            && !engine.session().saved_filters.is_empty()
        {
            items.push(PaletteItem {
                label: "Cycle saved filter".into(),
                kind: PaletteKind::CycleSavedFilter,
            });
            for saved in &engine.session().saved_filters {
                items.push(PaletteItem {
                    label: format!("Apply saved filter: {}", saved.name),
                    kind: PaletteKind::ApplySavedFilter(saved.name.to_string()),
                });
            }
        }
        if !self.state.filter_text.is_empty() {
            items.push(PaletteItem {
                label: "Clear filter text".into(),
                kind: PaletteKind::ClearFilter,
            });
        }
        if self.state.engine.is_some() {
            items.push(PaletteItem {
                label: "Clear search".into(),
                kind: PaletteKind::ClearSearch,
            });
        }
        if self.state.palette_query.is_empty() {
            items
        } else {
            let needle = self.state.palette_query.to_ascii_lowercase();
            items
                .into_iter()
                .filter(|item| item.label.to_ascii_lowercase().contains(&needle))
                .collect()
        }
    }

    fn refresh_palette_items(&mut self, cx: &mut Cx) {
        let items = self.palette_items();
        let max = items.len().saturating_sub(1);
        if self.state.palette_selected > max {
            self.state.palette_selected = max;
        }
        let body = if items.is_empty() {
            String::from("(no matching commands)")
        } else {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| {
                    if index == self.state.palette_selected {
                        format!("▶ {}", item.label)
                    } else {
                        format!("  {}", item.label)
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.ui.label(id!(palette_items_label)).set_text(cx, &body);
    }

    /// Execute the currently highlighted palette item and close the
    /// palette. Selected, query, and visibility all reset.
    fn run_palette_item(&mut self, cx: &mut Cx) {
        let items = self.palette_items();
        let Some(item) = items.get(self.state.palette_selected).cloned() else {
            self.close_palette(cx);
            return;
        };
        match item.kind {
            PaletteKind::ToggleFollow => self.state.follow = !self.state.follow,
            PaletteKind::ToggleStackFolding => self.toggle_stack_folding(),
            PaletteKind::ClearLevel => {
                self.state.level = None;
                self.reapply_filters();
            }
            PaletteKind::SetLevel(level) => {
                self.state.level = Some(level);
                self.reapply_filters();
            }
            PaletteKind::CycleSavedFilter => self.cycle_saved_filter(),
            PaletteKind::ApplySavedFilter(name) => {
                if let Some(engine) = self.state.engine.as_mut() {
                    match engine.apply_saved_filter(&name) {
                        Ok(true) => {
                            self.state.status_message = Some(format!("saved filter: {name}"));
                            self.state.last_error = None;
                        }
                        Ok(false) | Err(_) => {
                            self.state.last_error =
                                Some(format!("could not load saved filter {name}"));
                        }
                    }
                }
            }
            PaletteKind::ClearFilter => {
                self.state.filter_text.clear();
                self.ui.text_input(id!(filter_input)).set_text(cx, "");
                self.reapply_filters();
            }
            PaletteKind::ClearSearch => {
                self.ui.text_input(id!(search_input)).set_text(cx, "");
                if let Some(engine) = self.state.engine.as_mut() {
                    engine.set_search_text(None);
                }
            }
        }
        self.close_palette(cx);
    }

    fn refresh_status(&mut self, cx: &mut Cx) {
        let status_text = if let Some(engine) = self.state.engine.as_mut() {
            let snapshot = engine.metadata_snapshot();
            let level = level_label(self.state.level);
            let saved = match self.state.saved_filter_index {
                Some(index) => {
                    let name = engine.session().saved_filters[index].name.clone();
                    format!(" • saved: {name}")
                }
                None => String::new(),
            };
            let msg = self
                .state
                .status_message
                .as_deref()
                .map(|m| format!(" • {m}"))
                .unwrap_or_default();
            format!(
                "rows: {} • matching: {} • warn: {} • error: {} • sources: {} • level: {}{}{}{}{}",
                snapshot.total_rows,
                snapshot.total_matching_rows,
                snapshot.level_counts.warn,
                snapshot.level_counts.error + snapshot.level_counts.fatal,
                snapshot.source_summaries.len(),
                level,
                saved,
                if self.state.follow { " • follow" } else { "" },
                if self.state.fold_stacks {
                    " • fold"
                } else {
                    ""
                },
                msg,
            )
        } else {
            String::from("engine not initialised")
        };
        self.ui.label(id!(status_label)).set_text(cx, &status_text);

        let error_text = self.state.last_error.as_deref().unwrap_or("");
        self.ui.label(id!(error_label)).set_text(cx, error_text);
    }

    /// Drain every pending `LogEvent` from the tailer channel into the
    /// engine. Returns the number of rows appended so the caller can
    /// decide whether to scroll the PortalList to the tail.
    fn drain_events(&mut self) -> usize {
        let Some(live_tail) = self.state.live_tail.as_mut() else {
            return 0;
        };
        let Some(engine) = self.state.engine.as_mut() else {
            return 0;
        };
        let mut appended = 0;
        loop {
            match live_tail.receiver.try_recv() {
                Ok(LogEvent::RowAppended(row)) => {
                    engine.append_row(row);
                    appended += 1;
                }
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    engine.add_source(source_id, path.display().to_string());
                }
                Ok(LogEvent::SourceError { message, .. }) => {
                    self.state.last_error = Some(message);
                }
                Ok(_) => {}
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }
        appended
    }

    /// Snapshot the current viewport rows into the [`LogList`] so its
    /// next `draw_walk` can render them. Called when the engine state
    /// changes (live append, filter change, selection move) — *not*
    /// every frame, so the vector copy is bounded by actual UI mutations.
    fn push_rows_to_list(&mut self, cx: &mut Cx) {
        let Some(engine) = self.state.engine.as_mut() else {
            return;
        };
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: PAGE_SIZE,
        });
        // Pull selection position now so the LogList can scroll to keep
        // the cursor on-screen during navigation. `None` means
        // "selection currently invisible" (e.g. filtered out) — the list
        // won't scroll in that case.
        let selection_position = self
            .state
            .selected_row_id
            .and_then(|id| engine.filtered_position_for_row(id));
        // Capture the selected row's JSON fields (if any) before passing
        // ownership of `snapshot.rows` into the list widget. Avoids a
        // second `present_row_at` lookup.
        let detail_fields = self
            .state
            .selected_row_id
            .and_then(|id| snapshot.rows.iter().find(|row| row.row_id == id))
            .map(|row| row.json_fields())
            .unwrap_or_default();
        // Build the sidebar text from the snapshot's source summaries
        // before we hand ownership of snapshot.rows away. The format
        // mirrors the iced front-end's sidebar so the chrome reads the
        // same across the four siblings.
        let sources_text = if snapshot.source_summaries.is_empty() {
            String::from("(no sources)")
        } else {
            snapshot
                .source_summaries
                .iter()
                .map(|s| {
                    let errors = s.level_counts.error + s.level_counts.fatal;
                    format!(
                        "{}\n  {} rows  {}W  {}E",
                        s.name, s.rows, s.level_counts.warn, errors
                    )
                })
                .collect::<Vec<_>>()
                .join("\n")
        };
        self.ui.log_list(id!(log_list)).set_state(
            cx,
            snapshot.rows,
            self.state.follow,
            self.state.selected_row_id,
            selection_position,
        );
        self.ui
            .label(id!(sources_label))
            .set_text(cx, &sources_text);
        self.refresh_detail_panel(cx, &detail_fields);
    }

    /// Render the JSON detail panel for the currently selected row.
    /// Hidden (empty text + `visible: false`) when there's no selection
    /// or no JSON fields on the selected row, so the layout doesn't
    /// shift around as the cursor moves through plain-text rows.
    fn refresh_detail_panel(&mut self, cx: &mut Cx, fields: &[(Arc<str>, Arc<str>)]) {
        let panel = self.ui.view(id!(detail_panel));
        if fields.is_empty() {
            panel.apply_over(cx, live! { visible: false });
            self.ui.label(id!(detail_title)).set_text(cx, "");
            self.ui.label(id!(detail_body)).set_text(cx, "");
            return;
        }
        panel.apply_over(cx, live! { visible: true });
        self.ui.label(id!(detail_title)).set_text(cx, "JSON detail");
        let body = fields
            .iter()
            .map(|(key, value)| format!("{key} = {value}"))
            .collect::<Vec<_>>()
            .join("\n");
        self.ui.label(id!(detail_body)).set_text(cx, &body);
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());

        // Per-frame poll: drain the tailer channel, refresh the list
        // window, refresh the status, schedule the next tick. Doing this
        // inside `handle_event` (not `handle_actions`) lets us drain on
        // *any* event including the `NextFrame` we manufactured.
        if self
            .state
            .next_frame
            .as_ref()
            .map(|nf| nf.is_event(event).is_some())
            .unwrap_or(false)
        {
            let appended = self.drain_events();
            if appended > 0 {
                self.push_rows_to_list(cx);
                self.refresh_status(cx);
            }
            self.state.next_frame = Some(cx.new_next_frame());
        }

        // On the very first Construct/Draw we don't yet have rows in
        // the list. Push them once the engine is ready.
        if let Event::Startup = event {
            self.push_rows_to_list(cx);
        }

        // Keyboard shortcuts. Until the filter/search text inputs land
        // we don't need mode gating — every KeyDown is interpreted as a
        // Normal-mode shortcut. The mode machine arrives with the text
        // input commit (task #9).
        if let Event::KeyDown(key) = event {
            self.handle_key_down(cx, key);
        }
    }
}

impl App {
    fn handle_key_down(&mut self, cx: &mut Cx, key: &KeyEvent) {
        // Cmd+K (or Ctrl+K) toggles the palette regardless of mode.
        // KeyModifiers::is_primary() handles the macOS-vs-others split.
        if matches!(key.key_code, KeyCode::KeyK) && key.modifiers.is_primary() {
            if self.state.mode == InputMode::Palette {
                self.close_palette(cx);
            } else {
                self.open_palette(cx);
            }
            return;
        }

        // While the palette is open, j/k or arrows navigate items.
        if self.state.mode == InputMode::Palette {
            match key.key_code {
                KeyCode::ArrowUp | KeyCode::KeyK => {
                    self.state.palette_selected = self.state.palette_selected.saturating_sub(1);
                    self.refresh_palette_items(cx);
                }
                KeyCode::ArrowDown | KeyCode::KeyJ => {
                    let max = self.palette_items().len().saturating_sub(1);
                    self.state.palette_selected = (self.state.palette_selected + 1).min(max);
                    self.refresh_palette_items(cx);
                }
                KeyCode::ReturnKey => {
                    self.run_palette_item(cx);
                    self.push_rows_to_list(cx);
                    self.refresh_status(cx);
                }
                KeyCode::Escape => self.close_palette(cx),
                _ => {}
            }
            return;
        }

        // `/` and `?` are always-on entry points to focus the inputs.
        // Both rely on the TextInputRef::borrow_mut path because
        // `set_key_focus` is on the inner `TextInput`, not its Ref.
        if matches!(key.key_code, KeyCode::Slash) && !key.modifiers.shift {
            self.focus_filter_input(cx);
            self.state.mode = InputMode::Filter;
            return;
        }
        if matches!(key.key_code, KeyCode::Slash) && key.modifiers.shift {
            self.focus_search_input(cx);
            self.state.mode = InputMode::Search;
            return;
        }

        // Every other key is mode-sensitive. When a text input has
        // focus the TextInput widget consumes character keys and emits
        // `Returned`/`Escaped` actions handled in `handle_actions`.
        if self.state.mode != InputMode::Normal {
            return;
        }

        let mut state_changed = true;
        match key.key_code {
            KeyCode::ArrowUp | KeyCode::KeyK => {
                self.move_selection(-1);
            }
            KeyCode::ArrowDown | KeyCode::KeyJ => {
                self.move_selection(1);
            }
            KeyCode::PageUp => {
                self.move_selection(-(PAGE_STEP as isize));
            }
            KeyCode::PageDown => {
                self.move_selection(PAGE_STEP as isize);
            }
            KeyCode::Home => {
                self.move_selection(isize::MIN / 2);
                self.state.follow = false;
            }
            KeyCode::End => {
                self.move_selection(isize::MAX / 2);
                self.state.follow = true;
            }
            KeyCode::KeyF => {
                self.state.follow = !self.state.follow;
            }
            KeyCode::KeyB => self.toggle_bookmark_for_selection(),
            KeyCode::KeyN if key.modifiers.shift => self.jump_search(true),
            KeyCode::KeyN => self.jump_search(false),
            KeyCode::KeyS => self.cycle_saved_filter(),
            KeyCode::KeyZ => self.toggle_stack_folding(),
            KeyCode::Key0 => {
                self.state.level = None;
                self.reapply_filters();
            }
            KeyCode::Key1 => {
                self.state.level = Some(LevelArg::Trace);
                self.reapply_filters();
            }
            KeyCode::Key2 => {
                self.state.level = Some(LevelArg::Debug);
                self.reapply_filters();
            }
            KeyCode::Key3 => {
                self.state.level = Some(LevelArg::Info);
                self.reapply_filters();
            }
            KeyCode::Key4 => {
                self.state.level = Some(LevelArg::Warn);
                self.reapply_filters();
            }
            KeyCode::Key5 => {
                self.state.level = Some(LevelArg::Error);
                self.reapply_filters();
            }
            KeyCode::Key6 => {
                self.state.level = Some(LevelArg::Fatal);
                self.reapply_filters();
            }
            _ => state_changed = false,
        }
        if state_changed {
            self.push_rows_to_list(cx);
            self.refresh_status(cx);
        }
    }

    fn focus_filter_input(&mut self, cx: &mut Cx) {
        let ti = self.ui.text_input(id!(filter_input));
        if let Some(inner) = ti.borrow_mut() {
            inner.set_key_focus(cx);
        }
    }

    fn focus_search_input(&mut self, cx: &mut Cx) {
        let ti = self.ui.text_input(id!(search_input));
        if let Some(inner) = ti.borrow_mut() {
            inner.set_key_focus(cx);
        }
    }
}

/// Number of rows skipped per PgUp/PgDn keypress. Roughly half the
/// realistic visible viewport so a page move leaves some context above
/// and below the new cursor position.
const PAGE_STEP: usize = 24;

impl Drop for App {
    fn drop(&mut self) {
        if let (Some(path), Some(engine)) =
            (self.state.session_path.as_ref(), self.state.engine.as_ref())
            && let Err(err) = save_session(Some(path), engine.session())
        {
            eprintln!("warning: failed to save session: {err}");
        }
    }
}

/// Custom widget wrapping a `PortalList`. Owns the latest viewport
/// snapshot and renders one [`LogRow`] template per row. The single
/// translation seam from `SpanKind`/`SeverityRole` to Makepad colours
/// lives in [`severity_colour`] / [`row_text`] so the rest of the file
/// stays engine-agnostic.
#[derive(Live, LiveHook, Widget)]
pub struct LogList {
    #[deref]
    view: View,
    #[rust]
    rows: Vec<RowPresentation>,
    #[rust]
    follow: bool,
    #[rust]
    selected_row_id: Option<RowId>,
    /// Position of the selected row in the filtered set, used to
    /// nudge the `PortalList` so the cursor stays on-screen during
    /// keyboard navigation.
    #[rust]
    selection_position: Option<usize>,
}

impl Widget for LogList {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            // `set_tail_range`/`set_first_id_and_scroll` are on
            // `PortalListRef`, not the inner widget, so call them before
            // borrowing the inner for the populate loop.
            let portal_list = step.as_portal_list();
            portal_list.set_tail_range(self.follow);
            if let Some(position) = self.selection_position {
                // Anchor the visible window a few rows above the
                // selection so the cursor isn't pinned to the very top
                // edge after a navigation step.
                let anchor = position.saturating_sub(4);
                portal_list.set_first_id(anchor);
            }
            if let Some(mut list) = portal_list.borrow_mut() {
                let count = self.rows.len();
                list.set_item_range(cx, 0, count);
                while let Some(item_id) = list.next_visible_item(cx) {
                    if let Some(row) = self.rows.get(item_id) {
                        let is_selected = self.selected_row_id == Some(row.row_id);
                        let item = list.item(cx, item_id, live_id!(LogRow));
                        let text = row_text(row);
                        let text_colour = severity_vec(row.severity_role());
                        // Selection wins over match wins over bookmark in
                        // terms of background. Matches use a translucent
                        // yellow so the search hit stands out even when
                        // it's many rows offscreen; without per-span
                        // colour this is the only way to surface where
                        // the search needle lives.
                        let bg_colour = if is_selected {
                            Vec4 {
                                x: 0x22 as f32 / 255.0,
                                y: 0x55 as f32 / 255.0,
                                z: 0x88 as f32 / 255.0,
                                w: 0.6,
                            }
                        } else if row.is_match {
                            Vec4 {
                                x: 0xc9 as f32 / 255.0,
                                y: 0xd9 as f32 / 255.0,
                                z: 0x6f as f32 / 255.0,
                                w: 0.25,
                            }
                        } else if row.is_bookmarked {
                            Vec4 {
                                x: 0xff as f32 / 255.0,
                                y: 0xc8 as f32 / 255.0,
                                z: 0x6b as f32 / 255.0,
                                w: 0.15,
                            }
                        } else {
                            Vec4 {
                                x: 0.0,
                                y: 0.0,
                                z: 0.0,
                                w: 0.0,
                            }
                        };
                        item.apply_over(
                            cx,
                            live! {
                                show_bg: true,
                                draw_bg: { color: (bg_colour) },
                            },
                        );
                        let label = item.label(id!(row_label));
                        label.set_text(cx, &text);
                        label.apply_over(cx, live! { draw_text: { color: (text_colour) } });
                        item.draw_all(cx, &mut Scope::empty());
                    }
                }
            }
        }
        DrawStep::done()
    }
}

impl LogListRef {
    /// Push a new viewport snapshot into the list along with the
    /// selection cursor it should highlight and scroll to. Cheap to
    /// call after every state mutation — the widget just stores the
    /// values and requests a redraw, the actual work happens in
    /// [`LogList::draw_walk`].
    fn set_state(
        &self,
        cx: &mut Cx,
        rows: Vec<RowPresentation>,
        follow: bool,
        selected_row_id: Option<RowId>,
        selection_position: Option<usize>,
    ) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.rows = rows;
            inner.follow = follow;
            inner.selected_row_id = selected_row_id;
            inner.selection_position = selection_position;
            inner.redraw(cx);
        }
    }
}

/// Flatten a `RowPresentation` into a single line of plain text. The
/// engine returns rich `StyledSpan`s; for this MVP we drop per-span colour
/// (the whole row is tinted by severity) and concatenate the text. Full
/// span-by-span colouring would need one Label per span, which is doable
/// but explodes widget count — deferred to a follow-up.
fn row_text(row: &RowPresentation) -> String {
    let mut out = String::with_capacity(128);
    if let Some(name) = row.source_name.as_ref() {
        out.push('[');
        out.push_str(name);
        out.push_str("] ");
    }
    for span in &row.spans {
        out.push_str(span.text.as_ref());
    }
    if row.is_bookmarked {
        out.push_str(" ★");
    }
    out
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

/// Severity → makepad colour, as the four-component vec4 the
/// `draw_text` shader expects. Mirrors the colour palette in
/// `glowtail-gui::severity_color` / `glowtail-iced::severity_colour`.
fn severity_vec(role: SeverityRole) -> Vec4 {
    let (r, g, b) = match role {
        SeverityRole::Fatal => (0xff, 0x4b, 0x4b),
        SeverityRole::Error => (0xff, 0x6b, 0x6b),
        SeverityRole::Warn => (0xff, 0xc8, 0x6b),
        SeverityRole::Info => (0x88, 0xc8, 0xff),
        SeverityRole::Debug => (0x80, 0x80, 0x80),
        SeverityRole::Trace => (0x60, 0x60, 0x60),
        SeverityRole::Unknown => (0xa0, 0xa0, 0xa0),
    };
    Vec4 {
        x: r as f32 / 255.0,
        y: g as f32 / 255.0,
        z: b as f32 / 255.0,
        w: 1.0,
    }
}
