//! End-to-end CLI throughput bench. Spawns the compiled `glowtail-cli` binary
//! against a synthetic log file in `--no-follow` mode and times wall-clock to
//! drain. Exercises BufReader, FileTailer, parser, filter, and stdout
//! together — the only bench that does.
//!
//! Run with:
//!
//! ```text
//! cargo test -p glowtail-cli --test cli_tail_perf -- --ignored --nocapture
//! ```

use std::io::{BufWriter, Write};
use std::process::{Command, Stdio};
use std::time::Instant;

const TAIL_ROWS: usize = 1_000_000;

fn write_synthetic_log(path: &std::path::Path) {
    let file = std::fs::File::create(path).expect("create synthetic log");
    let mut writer = BufWriter::with_capacity(1 << 20, file);
    for id in 0..TAIL_ROWS {
        // Short lines to keep the file size predictable (~25 MB). One in
        // every 50 rows is `WARN` so a level filter has measurable work to
        // do but the no-filter path stays representative of the common case.
        let level = if id % 50 == 0 { "WARN" } else { "INFO" };
        writeln!(writer, "{level} row {id:010} payload").expect("write");
    }
    writer.flush().expect("flush");
}

fn cli_binary() -> &'static str {
    env!("CARGO_BIN_EXE_glowtail-cli")
}

/// `glowtail-cli tail <path> --no-follow` end-to-end. Reads file → parses →
/// filters (none) → prints every row to stdout. Stdout is piped to /dev/null
/// equivalent so the terminal isn't the bottleneck.
#[test]
#[ignore = "cli tail perf benchmark"]
fn cli_tail_no_follow_throughput() {
    let temp = tempfile::Builder::new()
        .prefix("glowtail-bench-")
        .suffix(".log")
        .tempfile()
        .expect("tempfile");
    write_synthetic_log(temp.path());

    let started = Instant::now();
    let status = Command::new(cli_binary())
        .arg("tail")
        .arg(temp.path())
        .arg("--no-follow")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("spawn glowtail-cli");
    let elapsed = started.elapsed();
    assert!(status.success(), "glowtail-cli tail exited non-zero");

    let rate = TAIL_ROWS as f64 / elapsed.as_secs_f64();
    eprintln!(
        "cli_tail_no_follow_throughput: rows={} elapsed={:?} rate={:.0} rows/s",
        TAIL_ROWS, elapsed, rate,
    );
}

/// Same fixture, but with a `--level warn` filter so only ~2% of rows print.
/// Isolates the cost of `compiled_filter.matches()` from the cost of writing
/// to stdout — comparing the two numbers tells you where the no-follow loop's
/// time goes.
#[test]
#[ignore = "cli tail perf benchmark"]
fn cli_tail_no_follow_throughput_filtered() {
    let temp = tempfile::Builder::new()
        .prefix("glowtail-bench-")
        .suffix(".log")
        .tempfile()
        .expect("tempfile");
    write_synthetic_log(temp.path());

    let started = Instant::now();
    let status = Command::new(cli_binary())
        .arg("tail")
        .arg(temp.path())
        .arg("--no-follow")
        .arg("--level")
        .arg("warn")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .status()
        .expect("spawn glowtail-cli");
    let elapsed = started.elapsed();
    assert!(status.success(), "glowtail-cli tail exited non-zero");

    let rate = TAIL_ROWS as f64 / elapsed.as_secs_f64();
    eprintln!(
        "cli_tail_no_follow_throughput_filtered: rows={} elapsed={:?} rate={:.0} rows/s",
        TAIL_ROWS, elapsed, rate,
    );
}
