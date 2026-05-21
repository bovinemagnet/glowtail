use glowtail_core::filter::FilterExpr;
use glowtail_core::model::{
    ByteRange, LogLevel, LogRow, ParsedFields, RowId, SourceId, ViewportRequest,
};
use glowtail_core::viewport::Engine;
use std::sync::Arc;
use std::time::Instant;

fn row(id: u64, level: LogLevel) -> LogRow {
    LogRow {
        row_id: RowId(id),
        source_id: SourceId(1),
        byte_range: ByteRange {
            start: id * 80,
            end: id * 80 + 79,
        },
        timestamp: None,
        level: Some(level),
        raw: Arc::from(format!("{level:?} synthetic row {id}")),
        message: Arc::from(format!("{level:?} synthetic row {id}")),
        fields: ParsedFields::default(),
    }
}

#[test]
#[ignore = "manual phase-2 large viewport benchmark smoke test"]
fn large_viewport_filter_benchmark_smoke() {
    let mut engine = Engine::default();
    let started = Instant::now();
    for id in 0..100_000 {
        let level = if id % 10 == 0 {
            LogLevel::Error
        } else {
            LogLevel::Info
        };
        engine.append_row(row(id, level));
    }

    engine
        .set_filter(FilterExpr::LevelAtLeast(LogLevel::Warn))
        .unwrap();
    let snapshot = engine.viewport(ViewportRequest {
        first_row: 500,
        row_count: 80,
    });

    eprintln!(
        "large viewport smoke: rows={} matching={} elapsed={:?}",
        engine.total_rows(),
        snapshot.total_matching_rows,
        started.elapsed()
    );
    assert_eq!(snapshot.rows.len(), 80);
}
