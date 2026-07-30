use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-router",
    about = "Route a task to codex, claude, or opencode by task shape and weekly usage headroom"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Weekly and 5h headroom for both providers.
    Usage {
        #[arg(long)]
        json: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("agent-router: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> agent_router_core::Result<()> {
    match cli.command {
        Command::Usage { json } => usage(json),
    }
}

fn usage(json: bool) -> agent_router_core::Result<()> {
    let snapshot = agent_router_core::UsageSnapshot::read();
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    println!("provider  5h      weekly  weekly reset");
    for (name, headroom) in [("claude", snapshot.claude), ("codex", snapshot.codex)] {
        println!(
            "{name:<9} {:>5.1}%  {:>5.1}%  {}",
            headroom.five_hour_pct,
            headroom.weekly_pct,
            reset_label(headroom.weekly_reset_epoch)
        );
    }
    Ok(())
}

/// "in 2h13m" for a future reset, "-" when the epoch is unknown, "elapsed" once it has passed.
fn reset_label(epoch: i64) -> String {
    if epoch == 0 {
        return "-".to_string();
    }
    let remaining = epoch - agent_router_core::usage::now_epoch();
    if remaining <= 0 {
        return "elapsed".to_string();
    }
    format!("in {}h{:02}m", remaining / 3600, (remaining % 3600) / 60)
}
