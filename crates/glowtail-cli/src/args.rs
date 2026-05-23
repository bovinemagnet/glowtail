use clap::{Parser, Subcommand};
pub use glowtail_ui_common::LevelArg;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(name = "glowtail")]
#[command(about = "A Rust-first, UI-neutral log viewer")]
pub struct Args {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    View {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long, conflicts_with = "plain")]
        json: bool,
        #[arg(long)]
        plain: bool,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        level: Option<LevelArg>,
        #[arg(long)]
        no_follow: bool,
        #[arg(long)]
        from_start: bool,
        #[arg(long)]
        session: Option<PathBuf>,
        #[arg(long)]
        use_filter: Option<String>,
        #[arg(long)]
        save_filter: Option<String>,
        /// Retain at most this many rows in memory; older rows are dropped
        /// from the front of the buffer when the cap is exceeded. `0` means
        /// unbounded (the default). Suitable for long-running live tails;
        /// for investigations you usually want the default.
        #[arg(long)]
        max_rows: Option<usize>,
    },
    Tail {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long, conflicts_with = "plain")]
        json: bool,
        #[arg(long)]
        plain: bool,
        #[arg(long)]
        filter: Option<String>,
        #[arg(long)]
        level: Option<LevelArg>,
        #[arg(long)]
        no_follow: bool,
        #[arg(long)]
        from_start: bool,
        #[arg(long)]
        session: Option<PathBuf>,
        #[arg(long)]
        use_filter: Option<String>,
        #[arg(long)]
        save_filter: Option<String>,
        /// Retain at most this many rows in memory; older rows are dropped
        /// from the front of the buffer when the cap is exceeded. `0` means
        /// unbounded (the default).
        #[arg(long)]
        max_rows: Option<usize>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn json_and_plain_are_mutually_exclusive() {
        // Review M2: previously both flags accepted silently with --json winning.
        let result =
            Args::try_parse_from(["glowtail", "view", "samples/mixed.log", "--json", "--plain"]);
        assert!(
            result.is_err(),
            "--json --plain should error, got {result:?}"
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("cannot be used with") || err.contains("conflict"),
            "expected conflict error, got: {err}"
        );
    }

    #[test]
    fn tail_accepts_either_json_or_plain_on_its_own() {
        Args::try_parse_from(["glowtail", "tail", "samples/x.log", "--json"]).unwrap();
        Args::try_parse_from(["glowtail", "tail", "samples/x.log", "--plain"]).unwrap();
    }
}
