//! CPU-only translation perf bench for `glowtail-gpui` (GPUI).
//!
//! Frame-rate and GPU performance can't be measured from a headless
//! `cargo test` — GPUI needs a real renderer. What we *can* measure
//! is the per-frame CPU cost of the **translation seam**: iterating
//! viewport rows and turning each `StyledSpan` into a `gpui::Rgba`,
//! plus the per-row `severity_color()` lookup. That's the work that
//! has to happen for every visible row on every frame, and it's
//! directly comparable between the four front-ends.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p glowtail-gpui --test render_perf -- --ignored --nocapture
//! ```

use glowtail_core::model::{SeverityRole, SpanKind};
use glowtail_ui_common::sample_rows;
use gpui::{Rgba, rgb};
use std::time::Instant;

/// Inlined copy of `glowtail-gpui::main::severity_color`. Kept
/// identical to the production function — drift between the two
/// would mean the bench is measuring something the binary doesn't.
fn severity_color(role: SeverityRole) -> Rgba {
    match role {
        SeverityRole::Fatal | SeverityRole::Error => rgb(0xdc4f4f),
        SeverityRole::Warn => rgb(0xd6a33d),
        SeverityRole::Info => rgb(0x4f9ee3),
        SeverityRole::Debug | SeverityRole::Trace => rgb(0x7c75d8),
        SeverityRole::Unknown => rgb(0x4b5563),
    }
}

/// Inlined copy of `glowtail-gpui::main::span_color`.
fn span_color(kind: SpanKind) -> Rgba {
    match kind {
        SpanKind::Timestamp => rgb(0x8ab4f8),
        SpanKind::Error => rgb(0xff7b72),
        SpanKind::Warning => rgb(0xd6a33d),
        SpanKind::SearchMatch => rgb(0x0d1117),
        SpanKind::JsonKey => rgb(0x7ee7e7),
        SpanKind::JsonValue => rgb(0xa5d6a7),
        _ => rgb(0xe6edf3),
    }
}

fn fingerprint(c: Rgba) -> u64 {
    let bits = c.r.to_bits() ^ c.g.to_bits() ^ c.b.to_bits() ^ c.a.to_bits();
    bits as u64
}

#[test]
#[ignore = "manual UI translation perf bench — run with --nocapture"]
fn translation_seam_perf() {
    let rows = sample_rows(10_000);
    let iterations = 50;
    let total_spans: usize = rows.iter().map(|r| r.spans.len()).sum::<usize>() * iterations;

    let started = Instant::now();
    let mut sink: u64 = 0;
    for _ in 0..iterations {
        for row in &rows {
            let role = row.severity_role();
            sink = sink.wrapping_add(fingerprint(severity_color(role)));
            for span in &row.spans {
                sink = sink.wrapping_add(fingerprint(span_color(span.kind)));
            }
        }
    }
    let elapsed = started.elapsed();
    let ns_per_span = elapsed.as_nanos() as f64 / total_spans as f64;

    eprintln!(
        "[glowtail-gpui] {} rows × {} iterations · {} span lookups · {:?} elapsed · {:.2} ns/span · sink={:#x}",
        rows.len(),
        iterations,
        total_spans,
        elapsed,
        ns_per_span,
        sink,
    );
    assert!(total_spans > 0);
}
