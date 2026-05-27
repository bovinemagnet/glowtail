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

/// Build a synthetic viewport snapshot of `count` rows with realistic
/// span distributions — a mix of plain-text rows (timestamp + level +
/// message), JSON rows with a handful of fields, and `Warn`/`Error`
/// rows that exercise the per-severity colour path. Used by each UI
/// crate's `tests/render_perf.rs` to feed identical data into their
/// per-span colour translation benches; lives here so the four bench
/// files don't drift on data shape.
pub fn sample_rows(count: u64) -> Vec<RowPresentation> {
    use chrono::{DateTime, TimeZone, Utc};
    use glowtail_core::model::ParsedFields;

    let mut engine = Engine::default();
    let base: DateTime<Utc> = Utc.with_ymd_and_hms(2026, 5, 27, 9, 0, 0).unwrap();
    for id in 0..count {
        let modulus = id % 6;
        let level = match modulus {
            0 => Some(LogLevel::Error),
            1 => Some(LogLevel::Warn),
            2..=4 => Some(LogLevel::Info),
            _ => Some(LogLevel::Debug),
        };
        let mut fields = ParsedFields::default();
        if modulus % 2 == 0 {
            // Add JSON fields on every other row — exercises the
            // JsonKey/JsonValue span paths.
            fields.insert("service", "billing");
            fields.insert("request_id", format!("req-{id}"));
            fields.insert("duration_ms", format!("{}", id % 1000));
        }
        let message_text = format!("synthetic event #{id} timeout while contacting db");
        let row = LogRow {
            row_id: RowId(id),
            source_id: SourceId((id % 3) + 1),
            byte_range: ByteRange {
                start: id * 120,
                end: id * 120 + 119,
            },
            timestamp: Some(base + chrono::Duration::milliseconds(id as i64 * 50)),
            level,
            raw: Arc::from(message_text.clone()),
            message: Arc::from(message_text),
            fields,
        };
        engine.append_row(row);
    }
    let snapshot = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: count as usize,
    });
    snapshot.rows
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for review M3: empty / whitespace-only `--filter`
    /// strings used to be forwarded to the engine, leaving it up to
    /// `compose_query_filter` to decide whether `Some("")` produced an
    /// always-match filter or an error. The fix lives one layer down in
    /// `compose_query_filter` itself (which now trims and skips), but the
    /// behavioural guarantee is what every front-end depends on — so the
    /// test belongs at the `apply_filters` boundary.
    #[test]
    fn apply_filters_treats_whitespace_only_text_as_no_filter() {
        let baseline = {
            let mut engine = Engine::default();
            apply_filters(&mut engine, None, None, None, None).unwrap()
        };
        assert_eq!(baseline, FilterExpr::All);

        for text in ["", "   ", "\t\n  "] {
            let mut engine = Engine::default();
            let actual = apply_filters(&mut engine, Some(text.into()), None, None, None).unwrap();
            assert_eq!(
                actual, baseline,
                "filter text {text:?} should be treated as no filter"
            );
        }
    }

    #[test]
    fn apply_filters_with_non_blank_text_returns_non_trivial_filter() {
        let mut engine = Engine::default();
        let filter = apply_filters(&mut engine, Some("timeout".into()), None, None, None).unwrap();
        assert_ne!(filter, FilterExpr::All);
    }

    #[test]
    fn apply_filters_with_level_only_returns_level_filter() {
        let mut engine = Engine::default();
        let filter = apply_filters(&mut engine, None, Some(LevelArg::Warn), None, None).unwrap();
        assert_eq!(filter, FilterExpr::LevelAtLeast(LogLevel::Warn));
    }
}
