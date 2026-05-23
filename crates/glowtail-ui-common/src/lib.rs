//! Plumbing shared by every UI front-end (`glowtail-cli`, `glowtail-gui`,
//! `glowtail-gpui`). Keeps CLI flag → engine wiring in one place so the
//! front-ends can't silently diverge on filter composition, session I/O, or
//! tailer startup.
//!
//! Depends only on `glowtail-core` plus `clap`, `tokio`, and `anyhow` —
//! the architecture test in `tests/architecture.rs` enforces that no UI
//! framework crate (egui/eframe, gpui, ratatui/crossterm, wgpu) ever creeps
//! in here.

use anyhow::{Context, Result};
use clap::ValueEnum;
use glowtail_core::prelude::*;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::mpsc;

/// Severity argument shared by every front-end's `--level` flag. Mirrors
/// [`LogLevel`] but lives here so the front-ends can `derive(ValueEnum)`
/// without each redeclaring the same six variants.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LevelArg {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
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

/// Pick a parser from `--json` / `--plain` flags. With neither flag the
/// composite parser is returned, which auto-detects per line.
pub fn parser_from_flags(json: bool, plain: bool) -> Arc<dyn LogParser> {
    if json {
        Arc::new(JsonLineParser)
    } else if plain {
        Arc::new(PlainTextParser)
    } else {
        Arc::new(CompositeParser::default())
    }
}

/// Compose `--use-filter`, `--level`, and `--filter` into a single
/// [`FilterExpr`], install it on the engine, and optionally save it back to
/// the session under `--save-filter`. Returns the composed expression so
/// callers (e.g. the CLI's tail mode) can display or re-use it.
pub fn apply_filters(
    engine: &mut Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    use_filter: Option<String>,
    save_filter: Option<String>,
) -> Result<FilterExpr> {
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
    engine.set_filter(filter.clone())?;
    if let Some(name) = save_filter {
        engine.save_filter(name);
    }
    Ok(filter)
}

/// Load an [`InvestigationSession`] from `path`. Returns the default
/// session if `path` is `None` or refers to a file that does not yet exist
/// — `--session` is a "use it if it's there, create it on save" flag.
pub fn load_session(path: Option<&PathBuf>) -> Result<InvestigationSession> {
    let Some(path) = path else {
        return Ok(InvestigationSession::default());
    };
    if !path.exists() {
        return Ok(InvestigationSession::default());
    }
    InvestigationSession::load_from_path(path)
        .with_context(|| format!("failed to load session {}", path.display()))
}

/// Persist `session` to `path`, creating parent directories as needed.
/// No-op when `path` is `None`.
pub fn save_session(path: Option<&PathBuf>, session: &InvestigationSession) -> Result<()> {
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

/// Handle returned by [`start_tailers`]. Owns the channel receiver the UI
/// drains for new rows and the `FileTailer` task handles that produce them.
/// Drop order matters: dropping `tailers` first signals the tasks to stop,
/// then the receiver naturally closes when the last sender goes away.
pub struct LiveTail {
    pub receiver: mpsc::Receiver<LogEvent>,
    pub tailers: Vec<FileTailer>,
}

/// Start one [`FileTailer`] per `paths` entry on the given Tokio runtime,
/// all writing into a shared MPSC channel sized by
/// [`DEFAULT_TAILER_CHANNEL_CAPACITY`]. When `from_start` is true each
/// tailer replays current file contents through the live channel before
/// streaming appended lines.
pub fn start_tailers(
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
