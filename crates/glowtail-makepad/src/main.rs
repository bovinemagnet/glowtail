//! Makepad-based desktop UI for glowtail. Fourth long-term sibling to
//! `glowtail-gui` (egui/eframe), `glowtail-gpui` (GPUI), and
//! `glowtail-iced` (Iced).
//!
//! This file is the **scaffold** stage of the plan: it parses CLI flags
//! identical to the other front-ends, loads the engine via
//! `glowtail-ui-common` (so session, filter, and tailer plumbing stays in
//! one place), brings up a Makepad window, and renders a status line
//! sourced from [`Engine::metadata_snapshot`]. The PortalList-virtualised
//! row view, `SpanKind`→native colour mapping, live-tail channel bridging,
//! and feature parity with `glowtail-gpui` are explicit follow-ups.

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

    App = {{App}} {
        ui: <Root> {
            main_window = <Window> {
                window: { title: "glowtail (makepad)" },
                body = <View> {
                    flow: Down,
                    padding: 12,
                    spacing: 8,

                    title_label = <Label> {
                        text: "glowtail — makepad scaffold",
                        draw_text: { text_style: { font_size: 14.0 } }
                    }

                    status_label = <Label> {
                        text: "loading…",
                        draw_text: { text_style: { font_size: 12.0 } }
                    }

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
    /// Held so the `FileTailer` tasks keep running for the app's lifetime.
    /// Not polled yet — that lands with the live-tail integration step.
    #[allow(dead_code)]
    live_tail: Option<LiveTail>,
    last_error: Option<String>,
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
                "rows: {} • matching: {} • warn: {} • error: {} • sources: {}",
                snapshot.total_rows,
                snapshot.total_matching_rows,
                snapshot.level_counts.warn,
                snapshot.level_counts.error + snapshot.level_counts.fatal,
                snapshot.source_summaries.len(),
            )
        } else {
            String::from("engine not initialised")
        };
        self.ui.label(id!(status_label)).set_text(cx, &status_text);

        let error_text = self.state.last_error.as_deref().unwrap_or("");
        self.ui.label(id!(error_label)).set_text(cx, error_text);
    }
}

impl AppMain for App {
    fn handle_event(&mut self, cx: &mut Cx, event: &Event) {
        self.match_event(cx, event);
        self.ui.handle_event(cx, event, &mut Scope::empty());
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
