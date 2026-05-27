//! CPU-only translation perf bench for `glowtail-makepad`.
//!
//! Frame-rate and GPU performance can't be measured from a headless
//! `cargo test` — Makepad needs a real shader-backed window. What we
//! *can* measure is the per-frame CPU cost of the **translation
//! seam**: iterating viewport rows and turning each `StyledSpan` into
//! a Makepad `Vec4`, plus the per-row `severity_vec()` lookup. That's
//! the work that has to happen for every visible row on every frame,
//! and it's directly comparable between the four front-ends.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p glowtail-makepad --test render_perf -- --ignored --nocapture
//! ```

use glowtail_core::model::{SeverityRole, SpanKind};
use glowtail_ui_common::sample_rows;
use makepad_widgets::Vec4;
use std::time::Instant;

fn rgb_to_vec4(r: u8, g: u8, b: u8) -> Vec4 {
    Vec4 {
        x: r as f32 / 255.0,
        y: g as f32 / 255.0,
        z: b as f32 / 255.0,
        w: 1.0,
    }
}

/// Inlined copy of `glowtail-makepad::main::severity_vec`. Kept
/// identical to the production function — drift between the two
/// would mean the bench is measuring something the binary doesn't.
fn severity_vec(role: SeverityRole) -> Vec4 {
    match role {
        SeverityRole::Fatal => rgb_to_vec4(0xff, 0x4b, 0x4b),
        SeverityRole::Error => rgb_to_vec4(0xff, 0x6b, 0x6b),
        SeverityRole::Warn => rgb_to_vec4(0xff, 0xc8, 0x6b),
        SeverityRole::Info => rgb_to_vec4(0x88, 0xc8, 0xff),
        SeverityRole::Debug => rgb_to_vec4(0x80, 0x80, 0x80),
        SeverityRole::Trace => rgb_to_vec4(0x60, 0x60, 0x60),
        SeverityRole::Unknown => rgb_to_vec4(0xa0, 0xa0, 0xa0),
    }
}

/// Inlined copy of `glowtail-makepad::main::span_colour`.
fn span_colour(kind: SpanKind, role: SeverityRole) -> Vec4 {
    match kind {
        SpanKind::Timestamp => rgb_to_vec4(0x8a, 0xb4, 0xf8),
        SpanKind::Level => severity_vec(role),
        SpanKind::Source => rgb_to_vec4(0xc8, 0xa2, 0xc8),
        SpanKind::Message => rgb_to_vec4(0xe6, 0xe6, 0xe6),
        SpanKind::JsonKey => rgb_to_vec4(0xc8, 0xa2, 0xc8),
        SpanKind::JsonValue => rgb_to_vec4(0xfb, 0xbc, 0x04),
        SpanKind::SearchMatch => rgb_to_vec4(0xff, 0xeb, 0x3b),
        SpanKind::Error => rgb_to_vec4(0xff, 0x6b, 0x6b),
        SpanKind::Warning => rgb_to_vec4(0xff, 0xc8, 0x6b),
        SpanKind::StackTrace => rgb_to_vec4(0xa0, 0xa0, 0xa0),
        _ => rgb_to_vec4(0xe6, 0xe6, 0xe6),
    }
}

fn fingerprint(c: Vec4) -> u64 {
    let bits = c.x.to_bits() ^ c.y.to_bits() ^ c.z.to_bits() ^ c.w.to_bits();
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
            sink = sink.wrapping_add(fingerprint(severity_vec(role)));
            for span in &row.spans {
                sink = sink.wrapping_add(fingerprint(span_colour(span.kind, role)));
            }
        }
    }
    let elapsed = started.elapsed();
    let ns_per_span = elapsed.as_nanos() as f64 / total_spans as f64;

    eprintln!(
        "[glowtail-makepad] {} rows × {} iterations · {} span lookups · {:?} elapsed · {:.2} ns/span · sink={:#x}",
        rows.len(),
        iterations,
        total_spans,
        elapsed,
        ns_per_span,
        sink,
    );
    assert!(total_spans > 0);
}
