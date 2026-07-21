//! End-to-end CLI behaviour tests for `glowtail tail --no-follow`. The
//! no-follow path is the CLI's core "print matching rows and exit" contract;
//! it was previously exercised only by an `#[ignore]`d perf bench. These
//! spawn the compiled binary against a tiny temp log and assert exactly which
//! rows reach stdout, in file order.

use std::io::Write;
use std::process::Command;

fn cli_binary() -> &'static str {
    env!("CARGO_BIN_EXE_glowtail-cli")
}

fn write_log(contents: &str) -> tempfile::NamedTempFile {
    let mut file = tempfile::Builder::new()
        .prefix("glowtail-cli-test-")
        .suffix(".log")
        .tempfile()
        .expect("tempfile");
    file.write_all(contents.as_bytes()).expect("write log");
    file.flush().expect("flush log");
    file
}

#[test]
fn tail_no_follow_prints_only_substring_matching_rows() {
    let file = write_log(
        "INFO starting up\nWARN disk almost full\nERROR timeout contacting db\nINFO done\n",
    );

    let output = Command::new(cli_binary())
        .arg("tail")
        .arg(file.path())
        .arg("--no-follow")
        .arg("--filter")
        .arg("timeout")
        .output()
        .expect("spawn glowtail-cli");

    assert!(
        output.status.success(),
        "glowtail-cli tail exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec!["ERROR timeout contacting db"],
        "only the row matching --filter timeout should print"
    );
}

#[test]
fn tail_no_follow_with_level_filter_keeps_rows_at_or_above() {
    let file = write_log("INFO starting up\nWARN disk almost full\nERROR timeout\nDEBUG noisy\n");

    let output = Command::new(cli_binary())
        .arg("tail")
        .arg(file.path())
        .arg("--no-follow")
        .arg("--level")
        .arg("warn")
        .output()
        .expect("spawn glowtail-cli");

    assert!(
        output.status.success(),
        "glowtail-cli tail exited non-zero: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines,
        vec!["WARN disk almost full", "ERROR timeout"],
        "only rows at or above WARN should print, in file order"
    );
}
