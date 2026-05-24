//! Tailing-perf benchmarks expressed as `#[ignore]`d integration tests so they
//! ship with the test suite but don't run in CI by default. Run with:
//!
//! ```text
//! cargo test -p glowtail-core --test tail_perf -- --ignored --nocapture
//! ```
//!
//! Each test `eprintln!`s a single metric line. Re-run before and after a
//! change to compare numbers — that's the contract.

use glowtail_core::events::LogEvent;
use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{
    ByteRange, LogLevel, LogRow, ParsedFields, RowId, SourceId, ViewportRequest,
};
use glowtail_core::parser::PlainTextParser;
use glowtail_core::source::{DEFAULT_TAILER_CHANNEL_CAPACITY, FileTailer};
use glowtail_core::viewport::Engine;
use std::io::Write;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

const APPEND_THROUGHPUT_ROWS: u64 = 1_000_000;
const PER_APPEND_LATENCY_ROWS: u64 = 50_000;
const FILTER_CHANGE_ROWS: u64 = 200_000;
const VIEWPORT_WINDOW: usize = 80;

fn row(id: u64, level: LogLevel) -> LogRow {
    let raw: Arc<str> = Arc::from(format!("{level:?} synthetic row {id}"));
    LogRow {
        row_id: RowId(id),
        source_id: SourceId(1),
        byte_range: ByteRange {
            start: id * 80,
            end: id * 80 + 79,
        },
        timestamp: None,
        level: Some(level),
        raw: Arc::clone(&raw),
        message: raw,
        fields: ParsedFields::default(),
    }
}

fn level_for(id: u64) -> LogLevel {
    if id.is_multiple_of(10) {
        LogLevel::Error
    } else {
        LogLevel::Info
    }
}

fn rows_per_second(rows: u64, elapsed: Duration) -> f64 {
    rows as f64 / elapsed.as_secs_f64()
}

/// Baseline: how fast can the engine swallow appends when no filter or search
/// is active? Measures the cost of `RowIndex::append` plus the
/// `try_incremental_cache_update` short-circuit when the filter caches haven't
/// been materialised.
#[test]
#[ignore = "tail perf benchmark"]
fn append_throughput_no_filter() {
    let mut engine = Engine::default();
    let started = Instant::now();
    for id in 0..APPEND_THROUGHPUT_ROWS {
        engine.append_row(row(id, level_for(id)));
    }
    let elapsed = started.elapsed();
    eprintln!(
        "append_throughput_no_filter: rows={} elapsed={:?} rate={:.0} rows/s",
        APPEND_THROUGHPUT_ROWS,
        elapsed,
        rows_per_second(APPEND_THROUGHPUT_ROWS, elapsed),
    );
    assert_eq!(engine.total_rows(), APPEND_THROUGHPUT_ROWS as usize);
}

/// With a level filter active *and the cache materialised first*, every append
/// runs the incremental cache-update path. This is the realistic steady-state
/// for a UI that's already drawn one frame with the filter set.
#[test]
#[ignore = "tail perf benchmark"]
fn append_throughput_with_level_filter() {
    let mut engine = Engine::default();
    // Seed one row and force the filter cache to materialise before the
    // measured loop, so we exercise the incremental path, not the lazy build.
    engine.append_row(row(0, LogLevel::Info));
    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Warn))
        .unwrap();
    let _ = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: 1,
    });

    let started = Instant::now();
    for id in 1..APPEND_THROUGHPUT_ROWS {
        engine.append_row(row(id, level_for(id)));
    }
    let elapsed = started.elapsed();
    eprintln!(
        "append_throughput_with_level_filter: rows={} elapsed={:?} rate={:.0} rows/s",
        APPEND_THROUGHPUT_ROWS - 1,
        elapsed,
        rows_per_second(APPEND_THROUGHPUT_ROWS - 1, elapsed),
    );
}

/// Latency a UI actually pays per appended row: append + ask for the trailing
/// viewport (what a `tail -f`-style follow draws). Reports p50/p99/max.
#[test]
#[ignore = "tail perf benchmark"]
fn append_then_viewport_p99_latency() {
    let mut engine = Engine::default();
    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Info))
        .unwrap();

    let mut samples = Vec::with_capacity(PER_APPEND_LATENCY_ROWS as usize);
    for id in 0..PER_APPEND_LATENCY_ROWS {
        let t0 = Instant::now();
        engine.append_row(row(id, level_for(id)));
        let first_row = (id as usize).saturating_sub(VIEWPORT_WINDOW - 1);
        let snap = engine.viewport(ViewportRequest {
            first_row,
            row_count: VIEWPORT_WINDOW,
        });
        samples.push(t0.elapsed());
        // Touch the snapshot so the optimiser can't elide the work.
        std::hint::black_box(snap.rows.len());
    }

    samples.sort_unstable();
    let p50 = samples[samples.len() / 2];
    let p99 = samples[samples.len() * 99 / 100];
    let max = *samples.last().unwrap();
    eprintln!(
        "append_then_viewport_p99_latency: rows={} p50={:?} p99={:?} max={:?}",
        PER_APPEND_LATENCY_ROWS, p50, p99, max
    );
}

/// User-perceived cost of typing into the filter box while tailing is hot:
/// pre-load N rows, then time `set_filter` (which drops the filter cache and
/// forces a rebuild on the next viewport call). This is the worst case the
/// incremental cache *cannot* help with — proposed fixes should target this.
#[test]
#[ignore = "tail perf benchmark"]
fn filter_change_latency_under_load() {
    let mut engine = Engine::default();
    for id in 0..FILTER_CHANGE_ROWS {
        engine.append_row(row(id, level_for(id)));
    }
    // Materialise a baseline filter so the "change" measurement reflects a
    // realistic re-filter, not a first-time build.
    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Info))
        .unwrap();
    let _ = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: VIEWPORT_WINDOW,
    });

    let t0 = Instant::now();
    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Error))
        .unwrap();
    // Force the rebuild — `set_filter` invalidates lazily; the cost lands on
    // the next viewport query.
    let snap = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: VIEWPORT_WINDOW,
    });
    let elapsed = t0.elapsed();
    eprintln!(
        "filter_change_latency_under_load: rows={} matching={} elapsed={:?}",
        FILTER_CHANGE_ROWS, snap.total_matching_rows, elapsed
    );
}

// ---- end-to-end pipeline benchmarks (FileTailer → mpsc → Engine) ------------
//
// These prove where the *plumbing* ceiling actually lies: with the channel
// bounded at `DEFAULT_TAILER_CHANNEL_CAPACITY` and the UI draining every
// `poll_ms`, the tailer's `send().await` backpressures the producer once the
// channel fills. Sustained rate ≈ `capacity ÷ poll_interval`.

const PIPELINE_ROWS: u64 = 100_000;

/// Drive a `FileTailer` against a hot-appending temp file and measure how long
/// it takes for `rows_to_produce` rows to arrive at the `Engine` when the
/// simulated UI drains the channel every `poll_ms`. Returns the elapsed wall
/// time (writer-spawn to engine-saturated).
async fn pipeline_bench(poll_ms: u64, rows_to_produce: u64) -> Duration {
    let temp = tempfile::NamedTempFile::new().expect("tempfile");
    let path = temp.path().to_path_buf();

    let (tx, mut rx) = mpsc::channel::<LogEvent>(DEFAULT_TAILER_CHANNEL_CAPACITY);
    let tailer = FileTailer::start(
        SourceId(1),
        path.clone(),
        Arc::new(PlainTextParser),
        tx,
        true,
        true,
    );

    // Hot writer: append `rows_to_produce` lines as fast as possible, then
    // hold the handle open until the engine is saturated so the file isn't
    // truncated mid-read.
    let writer_path = path.clone();
    let writer = tokio::task::spawn_blocking(move || {
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&writer_path)
            .expect("open temp for append");
        for i in 0..rows_to_produce {
            writeln!(file, "synthetic line {i} payload payload payload").expect("write");
        }
        file.flush().expect("flush");
    });

    let started = Instant::now();
    let mut engine = Engine::default();
    let mut ticker = tokio::time::interval(Duration::from_millis(poll_ms));
    // First tick fires immediately; consume it so the loop's first wait
    // actually waits `poll_ms`.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        loop {
            match rx.try_recv() {
                Ok(LogEvent::RowAppended(row)) => engine.append_row(row),
                Ok(LogEvent::SourceAdded { source_id, path }) => {
                    engine.add_source(source_id, path.display().to_string());
                }
                Ok(_) => {}
                Err(mpsc::error::TryRecvError::Empty) => break,
                Err(mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
        if engine.total_rows() as u64 >= rows_to_produce {
            break;
        }
    }
    let elapsed = started.elapsed();

    tailer.signal_stop();
    writer.await.expect("writer task");
    drop(temp);
    elapsed
}

fn report_pipeline(name: &str, poll_ms: u64, rows: u64, elapsed: Duration) {
    let rate = rows as f64 / elapsed.as_secs_f64();
    let cap = DEFAULT_TAILER_CHANNEL_CAPACITY as f64;
    let theoretical = cap * (1000.0 / poll_ms as f64);
    eprintln!(
        "{name}: rows={} poll={}ms cap={} elapsed={:?} rate={:.0} rows/s (theoretical ceiling ≈{:.0})",
        rows, poll_ms, DEFAULT_TAILER_CHANNEL_CAPACITY, elapsed, rate, theoretical,
    );
}

/// Pipeline throughput with the current GUI/GPUI poll cadence (100ms).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "tail perf benchmark"]
async fn pipeline_throughput_100ms_poll() {
    let elapsed = pipeline_bench(100, PIPELINE_ROWS).await;
    report_pipeline(
        "pipeline_throughput_100ms_poll",
        100,
        PIPELINE_ROWS,
        elapsed,
    );
}

/// Pipeline throughput with a 60Hz drain cadence — the rate we'd expect from
/// the UIs if `LIVE_POLL_INTERVAL_MS`/`LIVE_REFRESH_MS` were dropped to ~16ms.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "tail perf benchmark"]
async fn pipeline_throughput_16ms_poll() {
    let elapsed = pipeline_bench(16, PIPELINE_ROWS).await;
    report_pipeline("pipeline_throughput_16ms_poll", 16, PIPELINE_ROWS, elapsed);
}
