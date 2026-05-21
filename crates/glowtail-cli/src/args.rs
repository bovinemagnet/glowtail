use clap::{Parser, Subcommand, ValueEnum};
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
        #[arg(long)]
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
    },
    Tail {
        #[arg(required = true)]
        paths: Vec<PathBuf>,
        #[arg(long)]
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
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum LevelArg {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
    Fatal,
}
