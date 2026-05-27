//! CPU-only translation perf bench for `glowtail-iced` (Iced/wgpu).
//!
//! Frame-rate and GPU performance can't be measured from a headless
//! `cargo test` — Iced needs a real wgpu surface. What we *can*
//! measure is the per-frame CPU cost of the **translation seam**:
//! iterating viewport rows and turning each `StyledSpan` into an
//! `iced::Color`, plus the per-row `severity_colour()` lookup. That's
//! the work that has to happen for every visible row on every frame,
//! and it's directly comparable between the four front-ends.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p glowtail-iced --test render_perf -- --ignored --nocapture
//! ```

use glowtail_core::model::{SeverityRole, SpanKind};
use glowtail_ui_common::sample_rows;
use iced::Color;
use std::time::Instant;

/// Inlined copy of `glowtail-iced::main::severity_colour`. Kept
/// identical to the production function — drift between the two
/// would mean the bench is measuring something the binary doesn't.
fn severity_colour(role: SeverityRole) -> Color {
    match role {
        SeverityRole::Fatal => Color::from_rgb8(0xff, 0x4b, 0x4b),
        SeverityRole::Error => Color::from_rgb8(0xff, 0x6b, 0x6b),
        SeverityRole::Warn => Color::from_rgb8(0xff, 0xc8, 0x6b),
        SeverityRole::Info => Color::from_rgb8(0x88, 0xc8, 0xff),
        SeverityRole::Debug => Color::from_rgb8(0x80, 0x80, 0x80),
        SeverityRole::Trace => Color::from_rgb8(0x60, 0x60, 0x60),
        SeverityRole::Unknown => Color::from_rgb8(0xa0, 0xa0, 0xa0),
    }
}

/// Inlined copy of `glowtail-iced::main::span_colour`.
fn span_colour(kind: SpanKind, role: SeverityRole) -> Color {
    match kind {
        SpanKind::Timestamp => Color::from_rgb8(0x8a, 0xb4, 0xf8),
        SpanKind::Level => severity_colour(role),
        SpanKind::Source => Color::from_rgb8(0xc8, 0xa2, 0xc8),
        SpanKind::Message => Color::from_rgb8(0xe6, 0xe6, 0xe6),
        SpanKind::JsonKey => Color::from_rgb8(0xc8, 0xa2, 0xc8),
        SpanKind::JsonValue => Color::from_rgb8(0xfb, 0xbc, 0x04),
        SpanKind::SearchMatch => Color::from_rgb8(0xff, 0xeb, 0x3b),
        SpanKind::Error => Color::from_rgb8(0xff, 0x6b, 0x6b),
        SpanKind::Warning => Color::from_rgb8(0xff, 0xc8, 0x6b),
        SpanKind::StackTrace => Color::from_rgb8(0xa0, 0xa0, 0xa0),
        _ => Color::from_rgb8(0xe6, 0xe6, 0xe6),
    }
}

fn fingerprint(c: Color) -> u64 {
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
            sink = sink.wrapping_add(fingerprint(severity_colour(role)));
            for span in &row.spans {
                sink = sink.wrapping_add(fingerprint(span_colour(span.kind, role)));
            }
        }
    }
    let elapsed = started.elapsed();
    let ns_per_span = elapsed.as_nanos() as f64 / total_spans as f64;

    eprintln!(
        "[glowtail-iced] {} rows × {} iterations · {} span lookups · {:?} elapsed · {:.2} ns/span · sink={:#x}",
        rows.len(),
        iterations,
        total_spans,
        elapsed,
        ns_per_span,
        sink,
    );
    assert!(total_spans > 0);
}
