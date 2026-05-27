//! CPU-only translation perf bench for `glowtail-gui` (egui/wgpu).
//!
//! Frame-rate and GPU performance can't be measured from a headless
//! `cargo test` — egui needs a real wgpu surface. What we *can*
//! measure is the per-frame CPU cost of the **translation seam**:
//! iterating viewport rows and turning each `StyledSpan` into an
//! `egui::Color32` plus the per-row `severity_color()` lookup. That's
//! the work that has to happen for every visible row on every frame,
//! and it's directly comparable between the four front-ends.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p glowtail-gui --test render_perf -- --ignored --nocapture
//! ```

use eframe::egui::Color32;
use glowtail_core::model::{SeverityRole, SpanKind};
use glowtail_ui_common::sample_rows;
use std::time::Instant;

/// Inlined copy of `glowtail-gui::main::severity_color`. Kept identical
/// to the production function — drift between the two would mean the
/// bench is measuring something the binary doesn't actually do.
fn severity_color(role: SeverityRole) -> Color32 {
    match role {
        SeverityRole::Fatal => Color32::from_rgb(0xff, 0x4b, 0x4b),
        SeverityRole::Error => Color32::from_rgb(0xff, 0x6b, 0x6b),
        SeverityRole::Warn => Color32::from_rgb(0xff, 0xc8, 0x6b),
        SeverityRole::Info => Color32::from_rgb(0x88, 0xc8, 0xff),
        SeverityRole::Debug => Color32::from_rgb(0x80, 0x80, 0x80),
        SeverityRole::Trace => Color32::from_rgb(0x60, 0x60, 0x60),
        SeverityRole::Unknown => Color32::from_rgb(0xa0, 0xa0, 0xa0),
    }
}

/// Inlined copy of `glowtail-gui::main::span_color`.
fn span_color(kind: SpanKind, role: SeverityRole) -> Color32 {
    match kind {
        SpanKind::Timestamp => Color32::from_rgb(0x8a, 0xb4, 0xf8),
        SpanKind::Level => severity_color(role),
        SpanKind::Source => Color32::from_rgb(0xc8, 0xa2, 0xc8),
        SpanKind::Message => Color32::from_rgb(0xe6, 0xe6, 0xe6),
        SpanKind::JsonKey => Color32::from_rgb(0xc8, 0xa2, 0xc8),
        SpanKind::JsonValue => Color32::from_rgb(0xfb, 0xbc, 0x04),
        SpanKind::SearchMatch => Color32::from_rgb(0xff, 0xeb, 0x3b),
        SpanKind::Error => Color32::from_rgb(0xff, 0x6b, 0x6b),
        SpanKind::Warning => Color32::from_rgb(0xff, 0xc8, 0x6b),
        SpanKind::StackTrace => Color32::from_rgb(0xa0, 0xa0, 0xa0),
        _ => Color32::from_rgb(0xe6, 0xe6, 0xe6),
    }
}

fn fingerprint(c: Color32) -> u64 {
    let [r, g, b, a] = c.to_array();
    u32::from_be_bytes([r, g, b, a]) as u64
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
                sink = sink.wrapping_add(fingerprint(span_color(span.kind, role)));
            }
        }
    }
    let elapsed = started.elapsed();
    let ns_per_span = elapsed.as_nanos() as f64 / total_spans as f64;

    eprintln!(
        "[glowtail-gui] {} rows × {} iterations · {} span lookups · {:?} elapsed · {:.2} ns/span · sink={:#x}",
        rows.len(),
        iterations,
        total_spans,
        elapsed,
        ns_per_span,
        sink,
    );
    assert!(total_spans > 0);
}
