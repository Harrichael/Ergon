use clap::Parser;

/// Minimal task tracker for dispatching work to Claude Code sessions.
#[derive(Parser)]
#[command(version)]
struct Cli {}

fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();
    Ok(())
}
