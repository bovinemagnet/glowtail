//! Search-cache regression benchmarks. The 2026-05-23 review's H1 finding
//! resolved a per-keystroke `to_ascii_lowercase` + linear scan by caching
//! `search_results` alongside `filtered_positions`. This bench grades the
//! cache so a future refactor can't silently regress it.
//!
//! Run with:
//!
//! ```text
//! cargo test -p glowtail-core --test search_perf -- --ignored --nocapture
//! ```

use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{
    ByteRange, LogLevel, LogRow, ParsedFields, RowId, SourceId, ViewportRequest,
};
use glowtail_core::viewport::Engine;
use std::sync::Arc;
use std::time::Instant;

const SEARCH_ROWS: u64 = 200_000;
const HOT_ITERATIONS: u32 = 1_000;

fn row(id: u64) -> LogRow {
    // One in every 50 rows contains the needle ("needle"); the rest don't.
    // Keeps the matching set non-trivial (~4k rows) without dominating it.
    let raw: Arc<str> = if id.is_multiple_of(50) {
        Arc::from(format!("INFO synthetic row {id} contains needle payload"))
    } else {
        Arc::from(format!("INFO synthetic row {id} payload payload payload"))
    };
    LogRow {
        row_id: RowId(id),
        source_id: SourceId(1),
        byte_range: ByteRange {
            start: id * 80,
            end: id * 80 + 79,
        },
        timestamp: None,
        level: Some(LogLevel::Info),
        raw: Arc::clone(&raw),
        message: raw,
        fields: ParsedFields::default(),
    }
}

/// Cold path: setting the search text and asking once. Pays the full scan.
/// Hot path: 1,000 subsequent `search_results()` calls with the same needle.
/// With the search cache active, each hot call should be O(matches) (a clone
/// of the cached `Vec<RowId>`); without it, each would be O(rows).
#[test]
#[ignore = "search perf benchmark"]
fn search_results_cold_then_hot() {
    let mut engine = Engine::default();
    for id in 0..SEARCH_ROWS {
        engine.append_row(row(id));
    }
    // Realistic shape: filter active, search applied on top.
    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Info))
        .unwrap();
    let _ = engine.viewport(ViewportRequest {
        first_row: 0,
        row_count: 1,
    });

    let cold_started = Instant::now();
    engine.set_search_text(Some("needle".into()));
    let cold_results = engine.search_results();
    let cold_elapsed = cold_started.elapsed();
    let matches = cold_results.len();

    let hot_started = Instant::now();
    for _ in 0..HOT_ITERATIONS {
        let results = engine.search_results();
        std::hint::black_box(results.len());
    }
    let hot_elapsed = hot_started.elapsed();
    let hot_per_call_ns = hot_elapsed.as_nanos() as f64 / HOT_ITERATIONS as f64;

    eprintln!(
        "search_results_cold_then_hot: rows={} matches={} cold={:?} hot_iters={} hot_total={:?} hot_per_call={:.0}ns",
        SEARCH_ROWS, matches, cold_elapsed, HOT_ITERATIONS, hot_elapsed, hot_per_call_ns,
    );

    // Trip the cache (search text change) and re-measure a second cold call.
    // Surfaces invalidation cost separately from the original cold build.
    let invalidate_started = Instant::now();
    engine.set_search_text(Some("payload".into()));
    let alt_results = engine.search_results();
    let invalidate_elapsed = invalidate_started.elapsed();
    eprintln!(
        "search_results_cold_then_hot: invalidate_then_rebuild={:?} matches_after={}",
        invalidate_elapsed,
        alt_results.len(),
    );
}
