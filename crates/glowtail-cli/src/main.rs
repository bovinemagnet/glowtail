mod args;

use anyhow::{Context, Result, bail};
use args::{Args, Command, LevelArg};
use clap::Parser;
use glowtail_core::events::LogEvent;
use glowtail_core::filter::{CompiledFilter, FilterExpr};
use glowtail_core::model::{ByteRange, LogLevel, RowId, SourceId};
use glowtail_core::parser::{CompositeParser, JsonLineParser, LogParser, PlainTextParser};
use glowtail_core::session::InvestigationSession;
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
            from_start: _from_start,
            session,
            use_filter,
            save_filter,
        } => {
            run_tail(TailRun {
                paths,
                parser: parser_from_flags(json, plain),
                filter_text: filter,
                level,
                follow: !no_follow,
                session,
                use_filter,
                save_filter,
            })
            .await
        }
        Command::View {
            paths,
            json,
            plain,
            filter,
            level,
            no_follow,
            from_start,
            session,
            use_filter,
            save_filter,
        } => {
            let parser = parser_from_flags(json, plain);
            let follow = !no_follow;
            let investigation = load_session(session.as_ref())?;
            let mut engine = if follow && from_start {
                Engine::with_session(investigation)
            } else {
                load_initial_engine(paths.clone(), Arc::clone(&parser), investigation)
                    .await
                    .context("failed to load logs")?
            };
            configure_filters(&mut engine, filter, level, use_filter, save_filter)?;

            let engine = if follow {
                let (tx, rx) = mpsc::channel(1024);
                let mut tailers = Vec::new();
                for (idx, path) in paths.into_iter().enumerate() {
                    let source_id = SourceId((idx + 1) as u64);
                    engine.add_source(source_id, path.display().to_string());
                    tailers.push(FileTailer::start(
                        source_id,
                        path,
                        Arc::clone(&parser),
                        tx.clone(),
                        from_start,
                        true,
                    ));
                }
                drop(tx);

                let result = glowtail_tui::run_tui_with_events(engine, Some(rx));
                for tailer in tailers {
                    tailer.stop().await;
                }
                result?
            } else {
                glowtail_tui::run_tui(engine)?
            };

            save_session(session.as_ref(), engine.into_session())?;
            Ok(())
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

async fn load_initial_engine(
    paths: Vec<PathBuf>,
    parser: Arc<dyn LogParser>,
    session: InvestigationSession,
) -> Result<Engine> {
    let mut engine = Engine::with_session(session);
    let mut source_counter = 0u64;

    for path in paths {
        source_counter += 1;
        let source_id = SourceId(source_counter);
        engine.add_source(source_id, path.display().to_string());
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

struct TailRun {
    paths: Vec<PathBuf>,
    parser: Arc<dyn LogParser>,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    follow: bool,
    session: Option<PathBuf>,
    use_filter: Option<String>,
    save_filter: Option<String>,
}

async fn run_tail(options: TailRun) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(1024);
    let mut tailers = Vec::new();
    let source_count = options.paths.len();

    for (idx, path) in options.paths.into_iter().enumerate() {
        tailers.push(FileTailer::start(
            SourceId((idx + 1) as u64),
            path,
            Arc::clone(&options.parser),
            tx.clone(),
            true,
            options.follow,
        ));
    }
    drop(tx);

    let investigation = load_session(options.session.as_ref())?;
    let mut engine = Engine::with_session(investigation);
    let filter_expr = configure_filters(
        &mut engine,
        options.filter_text,
        options.level,
        options.use_filter,
        options.save_filter,
    )?;
    let compiled_filter = CompiledFilter::compile(&filter_expr)?;
    let mut removed_sources = 0usize;

    while let Some(event) = rx.recv().await {
        match event {
            LogEvent::RowAppended(row) => {
                let should_print = compiled_filter.matches(&row);
                let raw = row.raw.clone();
                engine.append_row(row);
                if should_print {
                    println!("{raw}");
                }
            }
            LogEvent::SourceRemoved { .. } if !options.follow => {
                removed_sources += 1;
                if removed_sources >= source_count {
                    break;
                }
            }
            LogEvent::SourceError { message, .. } => eprintln!("source error: {message}"),
            _ => {}
        }
    }

    for tailer in tailers {
        tailer.stop().await;
    }

    save_session(options.session.as_ref(), engine.into_session())?;
    Ok(())
}

fn configure_filters(
    engine: &mut Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    use_filter: Option<String>,
    save_filter: Option<String>,
) -> Result<FilterExpr> {
    let filter = filter_expr_from_args(engine, filter_text, level, use_filter)?;
    engine.set_filter(filter.clone())?;
    if let Some(name) = save_filter {
        engine.save_filter(name);
    }
    Ok(filter)
}

fn filter_expr_from_args(
    engine: &Engine,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    use_filter: Option<String>,
) -> Result<FilterExpr> {
    let mut filters = Vec::new();

    if let Some(name) = use_filter {
        let Some(filter) = engine.session().saved_filter(&name).cloned() else {
            bail!("saved filter not found: {name}");
        };
        filters.push(filter);
    }

    if let Some(level) = level {
        filters.push(FilterExpr::LevelAtLeast(level.into()));
    }

    if let Some(text) = filter_text {
        filters.push(FilterExpr::Contains(text));
    }

    Ok(FilterExpr::and_all(filters))
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

fn save_session(path: Option<&PathBuf>, session: InvestigationSession) -> Result<()> {
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
