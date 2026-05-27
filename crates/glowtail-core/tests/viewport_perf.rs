//! Per-frame [`Engine::viewport`] cost across varied viewport sizes
//! and filter intensities. Every UI front-end pays this cost on every
//! render that follows a state change, so the numbers reveal what the
//! shared engine baseline is independent of any framework.
//!
//! Run with:
//!
//! ```sh
//! cargo test --release -p glowtail-core --test viewport_perf -- --ignored --nocapture
//! ```

use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{
    ByteRange, LogLevel, LogRow, ParsedFields, RowId, SourceId, ViewportRequest,
};
use glowtail_core::viewport::Engine;
use std::sync::Arc;
use std::time::Instant;

const TOTAL_ROWS: u64 = 100_000;

fn row(id: u64) -> LogRow {
    let level = match id % 20 {
        0 | 1 => LogLevel::Error,
        2 | 3 => LogLevel::Warn,
        _ => LogLevel::Info,
    };
    let raw = format!("{level:?} synthetic row {id} timeout while contacting db");
    LogRow {
        row_id: RowId(id),
        source_id: SourceId((id % 3) + 1),
        byte_range: ByteRange {
            start: id * 120,
            end: id * 120 + 119,
        },
        timestamp: None,
        level: Some(level),
        raw: Arc::from(raw.clone()),
        message: Arc::from(raw),
        fields: ParsedFields::default(),
    }
}

fn build_engine() -> Engine {
    let mut engine = Engine::default();
    for id in 0..TOTAL_ROWS {
        engine.append_row(row(id));
    }
    engine
}

fn parse_filter(query: &str) -> FilterExpr {
    glowtail_core::filter::parse_filter_query(query).expect("known-good filter")
}

#[derive(Debug)]
struct Scenario {
    label: &'static str,
    /// Filter to apply before measuring; `None` keeps `FilterExpr::All`.
    filter: Option<FilterExpr>,
    /// Viewport sizes to sweep over. Each size produces a row.
    sizes: &'static [usize],
    /// How many viewport calls to run per size — keeps the timing
    /// signal above clock-resolution noise on small windows.
    iterations: u32,
}

#[test]
#[ignore = "manual viewport throughput perf bench — run with --release --nocapture"]
fn viewport_throughput_sweep() {
    let scenarios = [
        Scenario {
            label: "no filter",
            filter: None,
            sizes: &[80, 1_024, 10_000, TOTAL_ROWS as usize],
            iterations: 200,
        },
        Scenario {
            label: "level >= warn",
            filter: Some(FilterExpr::LevelAtLeast(LogLevel::Warn)),
            sizes: &[80, 1_024, 10_000],
            iterations: 200,
        },
        Scenario {
            label: "contains 'timeout' (warm cache)",
            filter: Some(parse_filter("message contains \"timeout\"")),
            sizes: &[80, 1_024, 10_000],
            iterations: 200,
        },
    ];

    eprintln!("=== Engine viewport throughput ===");
    eprintln!("total rows: {TOTAL_ROWS}\n");
    for scenario in scenarios {
        eprintln!("scenario: {}", scenario.label);
        let mut engine = build_engine();
        if let Some(filter) = scenario.filter.clone() {
            engine.set_filter(filter).expect("filter compiles");
        }
        // Warm the filter / search caches so the first measured call
        // doesn't pay one-time index population cost. Mirrors the
        // realistic case where a UI has already shown one frame.
        let _ = engine.viewport(ViewportRequest {
            first_row: 0,
            row_count: 80,
        });

        for &row_count in scenario.sizes {
            let started = Instant::now();
            let mut last_total = 0usize;
            for iter in 0..scenario.iterations {
                let first_row = (iter as usize) % 1_024;
                let snapshot = engine.viewport(ViewportRequest {
                    first_row,
                    row_count,
                });
                last_total = snapshot.total_matching_rows;
            }
            let elapsed = started.elapsed();
            let avg = elapsed / scenario.iterations;
            eprintln!(
                "  size {row_count:>7}  iter ×{}  matching {:>6}  avg {:?}  total {:?}",
                scenario.iterations, last_total, avg, elapsed,
            );
        }
        eprintln!();
    }
    assert_eq!(build_engine().total_rows(), TOTAL_ROWS as usize);
}
