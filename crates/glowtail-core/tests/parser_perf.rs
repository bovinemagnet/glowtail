//! Parser-throughput benchmarks expressed as `#[ignore]`d integration tests.
//! Run with:
//!
//! ```text
//! cargo test -p glowtail-core --test parser_perf -- --ignored --nocapture
//! ```
//!
//! Quantifies the cost of [`PlainTextParser`] vs [`JsonLineParser`] over a
//! fixed corpus of synthetic lines. The JSON parser is the relevant baseline
//! for finding PH2 in the 2026-05-26 perf review (per-field `Arc<str>`
//! allocation); compare rows/sec before and after applying the interning
//! fix.

use glowtail_core::model::{ByteRange, RowId, SourceId};
use glowtail_core::parser::{JsonLineParser, LogParser, PlainTextParser};
use std::time::{Duration, Instant};

const PARSE_ROWS: usize = 100_000;

fn rows_per_second(rows: usize, elapsed: Duration) -> f64 {
    rows as f64 / elapsed.as_secs_f64()
}

fn plain_line(id: usize) -> String {
    // Mirror the rough shape of a real plain log line — RFC3339 timestamp,
    // level token, service identifier, free text. Enough variability that the
    // string-search hot path inside the parser isn't trivially branch-predicted
    // to the same outcome for every row.
    let level = if id.is_multiple_of(10) { "ERROR" } else { "INFO" };
    format!(
        "2026-05-26T10:15:{:02}.000Z {level} service=billing request_id=req-{id} message=handling request",
        id % 60
    )
}

fn json_line(id: usize) -> String {
    // Ten fields is roughly the median in production JSON logs (timestamp,
    // level, message, service, request_id, trace_id, span_id, host, env,
    // duration_ms). Two of those names are stripped to `log.level` /
    // `message`; the rest hit `parse_fields` and the per-field `Arc<str>`
    // allocation that PH2 flags.
    let level = if id.is_multiple_of(10) { "ERROR" } else { "INFO" };
    format!(
        "{{\"timestamp\":\"2026-05-26T10:15:{:02}.000Z\",\"level\":\"{level}\",\"message\":\"handling request {id}\",\"service\":\"billing\",\"request_id\":\"req-{id}\",\"trace_id\":\"trace-{id:08x}\",\"span_id\":\"span-{id:08x}\",\"host\":\"api-{id_host}\",\"env\":\"prod\",\"duration_ms\":{dur}}}",
        id % 60,
        id_host = id % 32,
        dur = id % 250,
    )
}

fn measure<P: LogParser>(parser: &P, corpus: &[String]) -> Duration {
    let started = Instant::now();
    for (id, line) in corpus.iter().enumerate() {
        let row = parser.parse_line(
            SourceId(1),
            RowId(id as u64),
            ByteRange {
                start: (id * 256) as u64,
                end: (id * 256 + line.len()) as u64,
            },
            line.as_str(),
        );
        // Touch the result so the optimiser can't elide field allocation.
        std::hint::black_box(row.message.len());
        std::hint::black_box(row.fields.0.len());
    }
    started.elapsed()
}

/// `PlainTextParser` baseline. Each row pays one `Arc::from(line)` plus a
/// timestamp + level scan. No JSON, no per-field allocation.
#[test]
#[ignore = "parser perf benchmark"]
fn parser_throughput_plain() {
    let corpus: Vec<String> = (0..PARSE_ROWS).map(plain_line).collect();
    let elapsed = measure(&PlainTextParser, &corpus);
    eprintln!(
        "parser_throughput_plain: rows={} elapsed={:?} rate={:.0} rows/s",
        PARSE_ROWS,
        elapsed,
        rows_per_second(PARSE_ROWS, elapsed),
    );
}

/// `JsonLineParser` over a ten-field corpus. Dominated by `serde_json::from_str`
/// and the per-field `Arc::<str>::from(k.clone())` in `parse_fields` —
/// finding PH2 in the 2026-05-26 review. Use this number as the before/after
/// baseline for the proposed key-interning fix.
#[test]
#[ignore = "parser perf benchmark"]
fn parser_throughput_jsonl() {
    let corpus: Vec<String> = (0..PARSE_ROWS).map(json_line).collect();
    let elapsed = measure(&JsonLineParser, &corpus);
    eprintln!(
        "parser_throughput_jsonl: rows={} elapsed={:?} rate={:.0} rows/s",
        PARSE_ROWS,
        elapsed,
        rows_per_second(PARSE_ROWS, elapsed),
    );
}
