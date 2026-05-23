mod args;

use anyhow::{Context, Result};
use args::{Args, Command, LevelArg};
use clap::Parser;
use glowtail_core::prelude::*;
use glowtail_ui_common::{apply_filters, load_session, parser_from_flags, save_session};
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
            session,
            use_filter,
            save_filter,
            max_rows,
        } => {
            run_tail(TailRun {
                paths,
                parser: parser_from_flags(json, plain),
                filter_text: filter,
                level,
                follow: !no_follow,
                from_start,
                session,
                use_filter,
                save_filter,
                max_rows: normalise_max_rows(max_rows),
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
            max_rows,
        } => {
            let parser = parser_from_flags(json, plain);
            let follow = !no_follow;
            let investigation = load_session(session.as_ref())?;
            let mut engine = if follow && from_start {
                Engine::with_session(investigation)
            } else {
                load_initial_engine(paths.clone(), Arc::clone(&parser), investigation).await?
            };
            engine.set_max_rows(normalise_max_rows(max_rows));
            apply_filters(&mut engine, filter, level, use_filter, save_filter)?;

            let engine = if follow {
                let (tx, rx) = mpsc::channel(DEFAULT_TAILER_CHANNEL_CAPACITY);
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

            save_session(session.as_ref(), &engine.into_session())?;
            Ok(())
        }
    }
}

async fn load_initial_engine(
    paths: Vec<PathBuf>,
    parser: Arc<dyn LogParser>,
    session: InvestigationSession,
) -> Result<Engine> {
    let mut engine = Engine::with_session(session);
    for path in paths {
        let source_id = engine.next_source_id();
        engine.add_source(source_id, path.display().to_string());
        let bytes = tokio::fs::read(&path)
            .await
            .with_context(|| format!("failed to read {}", path.display()))?;
        engine.ingest_bytes(source_id, parser.as_ref(), &bytes);
    }
    Ok(engine)
}

struct TailRun {
    paths: Vec<PathBuf>,
    parser: Arc<dyn LogParser>,
    filter_text: Option<String>,
    level: Option<LevelArg>,
    follow: bool,
    from_start: bool,
    session: Option<PathBuf>,
    use_filter: Option<String>,
    save_filter: Option<String>,
    max_rows: Option<usize>,
}

/// Treat `--max-rows 0` and a missing flag as "unbounded" so the CLI surface
/// is forgiving: `--max-rows 0` reads as "no cap" rather than "drop every row
/// as soon as it arrives". Engine-side `set_max_rows(None)` is the unbounded
/// path.
fn normalise_max_rows(value: Option<usize>) -> Option<usize> {
    match value {
        Some(0) | None => None,
        other => other,
    }
}

async fn run_tail(options: TailRun) -> Result<()> {
    if !options.follow {
        return run_tail_no_follow(options).await;
    }
    run_tail_follow(options).await
}

async fn run_tail_follow(options: TailRun) -> Result<()> {
    let (tx, mut rx) = mpsc::channel(DEFAULT_TAILER_CHANNEL_CAPACITY);
    let mut tailers = Vec::new();

    for (idx, path) in options.paths.into_iter().enumerate() {
        tailers.push(FileTailer::start(
            SourceId((idx + 1) as u64),
            path,
            Arc::clone(&options.parser),
            tx.clone(),
            options.from_start,
            true,
        ));
    }
    drop(tx);

    let investigation = load_session(options.session.as_ref())?;
    let mut engine = Engine::with_session(investigation);
    engine.set_max_rows(options.max_rows);
    let filter_expr = apply_filters(
        &mut engine,
        options.filter_text,
        options.level,
        options.use_filter,
        options.save_filter,
    )?;
    let compiled_filter = glowtail_core::filter::CompiledFilter::compile(&filter_expr)?;

    // In follow mode the loop exits when every tailer's `tx` clone has been
    // dropped (i.e. every spawned task ended). `SourceRemoved` is informational
    // here — we count on the channel-close semantics for termination.
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
            LogEvent::SourceError { message, .. } => eprintln!("source error: {message}"),
            _ => {}
        }
    }

    for tailer in tailers {
        tailer.stop().await;
    }

    save_session(options.session.as_ref(), &engine.into_session())?;
    Ok(())
}

async fn run_tail_no_follow(options: TailRun) -> Result<()> {
    let investigation = load_session(options.session.as_ref())?;
    let mut engine =
        load_initial_engine(options.paths, Arc::clone(&options.parser), investigation).await?;
    engine.set_max_rows(options.max_rows);
    let filter_expr = apply_filters(
        &mut engine,
        options.filter_text,
        options.level,
        options.use_filter,
        options.save_filter,
    )?;
    let compiled_filter = glowtail_core::filter::CompiledFilter::compile(&filter_expr)?;

    for row in engine.rows_snapshot() {
        if compiled_filter.matches(row) {
            println!("{}", row.raw);
        }
    }

    save_session(options.session.as_ref(), &engine.into_session())?;
    Ok(())
}
