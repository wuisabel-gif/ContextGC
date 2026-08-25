mod commands;
mod common;
mod protocol_server;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "contextgc",
    about = "Predictive context governor for long-running AI agents"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Ingest ContextItem JSONL from a file or stdin.
    Ingest(IngestArgs),
    /// Show current pressure, composition, and reclaim candidates.
    Status(StatusArgs),
    /// Print an inspectable compaction plan without applying it.
    Plan(PlanArgs),
    /// Apply a compaction plan and materialize a working set.
    Compact(CompactArgs),
    /// Show local persistence and compaction telemetry.
    Stats(StatsArgs),
    /// Run the newline-delimited JSON stdio protocol server.
    Protocol(ProtocolArgs),
}

#[derive(Debug, Args)]
struct SessionArgs {
    /// SQLite database path.
    #[arg(long, short = 'd')]
    db: Option<PathBuf>,
    /// Session identifier.
    #[arg(long, short = 's', default_value = "default")]
    session: String,
    /// TOML configuration path.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Model name override.
    #[arg(long)]
    model: Option<String>,
    /// Model context window override.
    #[arg(long)]
    context_window: Option<u64>,
    /// Reserved output tokens override.
    #[arg(long)]
    reserved_output: Option<u64>,
}

#[derive(Debug, Args)]
struct IngestArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// JSONL file. If omitted, read stdin.
    #[arg(long, short = 'f')]
    file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct StatusArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// Emit the status as JSON.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct PlanArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// Predicted next tool output tokens.
    #[arg(long, default_value_t = 0)]
    predicted_extra: u64,
}

#[derive(Debug, Args)]
struct CompactArgs {
    #[command(flatten)]
    session: SessionArgs,
    /// Predicted next tool output tokens.
    #[arg(long, default_value_t = 0)]
    predicted_extra: u64,
    /// Emit the working set as JSON instead of a short report.
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct StatsArgs {
    #[command(flatten)]
    session: SessionArgs,
    #[arg(long)]
    json: bool,
}

#[derive(Debug, Args)]
struct ProtocolArgs {
    /// SQLite database path.
    #[arg(long, short = 'd')]
    db: Option<PathBuf>,
    /// TOML configuration path.
    #[arg(long)]
    config: Option<PathBuf>,
}

fn main() {
    if let Err(error) = run() {
        eprintln!("contextgc: {error:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Ingest(args) => commands::ingest(
            args.session.db.as_deref(),
            &args.session.session,
            args.session.config.as_deref(),
            args.file.as_deref(),
            args.session.model,
            args.session.context_window,
            args.session.reserved_output,
        ),
        Command::Status(args) => commands::status(
            args.session.db.as_deref(),
            &args.session.session,
            args.session.config.as_deref(),
            args.json,
            args.session.model,
            args.session.context_window,
            args.session.reserved_output,
        ),
        Command::Plan(args) => commands::plan(
            args.session.db.as_deref(),
            &args.session.session,
            args.session.config.as_deref(),
            args.predicted_extra,
            args.session.model,
            args.session.context_window,
            args.session.reserved_output,
        ),
        Command::Compact(args) => commands::compact(
            args.session.db.as_deref(),
            &args.session.session,
            args.session.config.as_deref(),
            args.predicted_extra,
            args.json,
            args.session.model,
            args.session.context_window,
            args.session.reserved_output,
        ),
        Command::Stats(args) => commands::stats(
            args.session.db.as_deref(),
            &args.session.session,
            args.session.config.as_deref(),
            args.json,
            args.session.model,
            args.session.context_window,
            args.session.reserved_output,
        ),
        Command::Protocol(args) => protocol_server::run(args.db.as_deref(), args.config.as_deref()),
    }
}
