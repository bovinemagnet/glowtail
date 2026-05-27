//! Makepad-based desktop UI for glowtail. Fourth long-term sibling to
//! `glowtail-gui` (egui/eframe), `glowtail-gpui` (GPUI), and
//! `glowtail-iced` (Iced).
//!
//! This commit lifts the crate from scaffold to **viewport rendering**:
//! a custom [`LogList`] widget wraps `PortalList`, the engine's
//! [`Engine::viewport`] populates it each frame, and a `NextFrame` event
//! drains the live-tail channel so newly appended rows appear without a
//! manual refresh. The interactive surface (filter input, selection
//! cursor, search nav, bookmarks, saved-filter cycling, level hotkeys,
//! detail panel) is layer 2 and tracked as an explicit follow-up.

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
                        spacing: 12,
                        flow: Right,
                        title_label = <Label> {
                            text: "glowtail — makepad",
                            draw_text: { text_style: { font_size: 13.0 } }
                        }
                        status_label = <Label> {
                            text: "loading…",
                            draw_text: { text_style: { font_size: 12.0 } }
                        }
                    }

                    log_list = <LogList> {}

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
    /// Always-on poll timer. `NextFrame` events fire once per frame and
    /// give us a place to drain the tailer channel without bringing tokio
    /// into Makepad's event loop.
    next_frame: Option<NextFrame>,
    follow: bool,
}

impl LiveRegister for App {
    fn live_register(cx: &mut Cx) {
        makepad_widgets::live_design(cx);
    }
}

impl MatchEvent for App {
    fn handle_startup(&mut self, cx: &mut Cx) {
        match self.bootstrap() {
            Ok(()) => self.refresh_status(cx),
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
        if !load_errors.is_empty() {
            self.state.last_error = Some(load_errors.join("; "));
        }
        Ok(())
    }

    fn refresh_status(&mut self, cx: &mut Cx) {
        let status_text = if let Some(engine) = self.state.engine.as_mut() {
            let snapshot = engine.metadata_snapshot();
            format!(
                "rows: {} • matching: {} • warn: {} • error: {} • sources: {}{}",
                snapshot.total_rows,
                snapshot.total_matching_rows,
                snapshot.level_counts.warn,
                snapshot.level_counts.error + snapshot.level_counts.fatal,
                snapshot.source_summaries.len(),
                if self.state.follow { " • follow" } else { "" },
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
    /// changes (live append, filter change) — *not* every frame, so the
    /// vector copy is bounded by actual UI mutations.
    fn push_rows_to_list(&mut self, cx: &mut Cx) {
        let Some(engine) = self.state.engine.as_mut() else {
            return;
        };
        let snapshot = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: PAGE_SIZE,
        });
        self.ui
            .log_list(id!(log_list))
            .set_rows(cx, snapshot.rows, self.state.follow);
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
    }
}

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
}

impl Widget for LogList {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event, scope: &mut Scope) {
        self.view.handle_event(cx, event, scope);
    }

    fn draw_walk(&mut self, cx: &mut Cx2d, scope: &mut Scope, walk: Walk) -> DrawStep {
        while let Some(step) = self.view.draw_walk(cx, scope, walk).step() {
            // `set_tail_range` is on `PortalListRef`, not the inner widget,
            // so call it before borrowing the inner for the populate loop.
            let portal_list = step.as_portal_list();
            portal_list.set_tail_range(self.follow);
            if let Some(mut list) = portal_list.borrow_mut() {
                let count = self.rows.len();
                list.set_item_range(cx, 0, count);
                while let Some(item_id) = list.next_visible_item(cx) {
                    if let Some(row) = self.rows.get(item_id) {
                        let item = list.item(cx, item_id, live_id!(LogRow));
                        let text = row_text(row);
                        let colour = severity_vec(row.severity_role());
                        let label = item.label(id!(row_label));
                        label.set_text(cx, &text);
                        label.apply_over(cx, live! { draw_text: { color: (colour) } });
                        item.draw_all(cx, &mut Scope::empty());
                    }
                }
            }
        }
        DrawStep::done()
    }
}

impl LogListRef {
    /// Push a new viewport snapshot into the list. The widget redraws on
    /// the next frame; cheap when called with `follow=true` because
    /// [`PortalList::set_first_id`] just shifts the cursor to the tail.
    fn set_rows(&mut self, cx: &mut Cx, rows: Vec<RowPresentation>, follow: bool) {
        if let Some(mut inner) = self.borrow_mut() {
            inner.rows = rows;
            inner.follow = follow;
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
