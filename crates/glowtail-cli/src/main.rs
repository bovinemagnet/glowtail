mod args;

use anyhow::{Context, Result};
use args::{Args, Command, LevelArg};
use clap::Parser;
use glowtail_core::events::LogEvent;
use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{ByteRange, LogLevel, RowId, SourceId};
use glowtail_core::parser::{CompositeParser, JsonLineParser, LogParser, PlainTextParser};
use glowtail_core::source::FileTailer;
use glowtail_core::viewport::Engine;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    match args.command {
        Command::Tail {
            paths,
            json,
            plain,
            filter,
            level,
            no_follow,
            from_start,
        } => {
            run_tail(
                paths,
                parser_from_flags(json, plain),
                filter,
                level,
                !no_follow,
                from_start,
            )
            .await
        }
        Command::View {
            paths,
            json,
            plain,
            filter,
            level,
            no_follow,
            from_start: _from_start,
        } => {
            let mut engine = load_initial_engine(paths, parser_from_flags(json, plain))
                .await
                .context("failed to load logs")?;
            apply_filters(&mut engine, filter, level)?;
            if no_follow {
                engine.set_search_text(None);
            }
            glowtail_tui::run_tui(engine)
        }
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

async fn load_initial_engine(paths: Vec<PathBuf>, parser: Arc<dyn LogParser>) -> Result<Engine> {
    let mut engine = Engine::default();
    let mut source_counter = 0u64;

    for path in paths {
        source_counter += 1;
        let source_id = SourceId(source_counter);
        let content = tokio::fs::read_to_string(&path)
            .await
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

async fn run_tail(
    paths: Vec<PathBuf>,
    parser: Arc<dyn LogParser>,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    follow: bool,
    from_start: bool,
) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(1024);
    let mut tailers = Vec::new();

    for (idx, path) in paths.into_iter().enumerate() {
        tailers.push(FileTailer::start(
            SourceId((idx + 1) as u64),
            path,
            Arc::clone(&parser),
            tx.clone(),
            from_start,
            follow,
        ));
    }

    let mut engine = Engine::default();
    apply_filters(&mut engine, filter_text, level)?;

    while let Some(event) = rx.recv().await {
        match event {
            LogEvent::RowAppended(row) => {
                let message = row.message.clone();
                engine.append_row(row);
                let snapshot = engine.viewport(glowtail_core::model::ViewportRequest {
                    first_row: engine.matching_rows_count().saturating_sub(1),
                    row_count: 1,
                });
                if !snapshot.rows.is_empty() {
                    println!("{message}");
                }
            }
            LogEvent::SourceRemoved { .. } if !follow => break,
            LogEvent::SourceError { message, .. } => eprintln!("source error: {message}"),
            _ => {}
        }
    }

    for tailer in tailers {
        tailer.stop().await;
    }

    Ok(())
}

fn apply_filters(
    engine: &mut Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
) -> Result<()> {
    if let Some(level) = level {
        let level = match level {
            LevelArg::Trace => LogLevel::Trace,
            LevelArg::Debug => LogLevel::Debug,
            LevelArg::Info => LogLevel::Info,
            LevelArg::Warn => LogLevel::Warn,
            LevelArg::Error => LogLevel::Error,
            LevelArg::Fatal => LogLevel::Fatal,
        };
        engine.set_filter(FilterExpr::LevelAtLeast(level))?;
    }

    if let Some(text) = filter_text {
        engine.set_filter(FilterExpr::Contains(text))?;
    }

    Ok(())
}
