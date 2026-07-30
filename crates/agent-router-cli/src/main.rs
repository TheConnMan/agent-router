use agent_router_core::log::{DecisionLog, Row};
use agent_router_core::run::{Outcome, Request};
use clap::{Parser, Subcommand};
use std::path::PathBuf;

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
    /// Route one task and dispatch it as a background job.
    Run {
        /// The task prompt.
        task: String,
        /// Working directory for the job (defaults to the current directory).
        #[arg(long)]
        dir: Option<PathBuf>,
        /// auto (classify), or codex/claude/opencode to skip classification.
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Model override, honoured only with an explicit --provider.
        #[arg(long)]
        model: Option<String>,
        /// Read-only work: skip the Codex execution-mode preamble.
        #[arg(long)]
        read_only: bool,
        /// Decide and log without dispatching.
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        json: bool,
    },
    /// Weekly and 5h headroom for both providers.
    Usage {
        #[arg(long)]
        json: bool,
    },
    /// Recent routing decisions, newest first.
    Log {
        #[arg(long, default_value_t = 10)]
        limit: usize,
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
        Command::Run {
            task,
            dir,
            provider,
            model,
            read_only,
            dry_run,
            json,
        } => route(task, dir, provider, model, read_only, dry_run, json),
        Command::Usage { json } => usage(json),
        Command::Log { limit, json } => log(limit, json),
    }
}

fn route(
    task: String,
    dir: Option<PathBuf>,
    provider: String,
    model: Option<String>,
    read_only: bool,
    dry_run: bool,
    json: bool,
) -> agent_router_core::Result<()> {
    let dir = match dir {
        Some(dir) => dir,
        None => std::env::current_dir()?,
    };
    let config = agent_router_core::Config::load()?;
    let request = Request {
        task: &task,
        dir: &dir,
        provider: agent_router_core::run::parse_provider(&provider)?,
        model,
        read_only,
        dry_run,
    };
    let outcome = agent_router_core::run::run(&request, &config)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome_json(&outcome))?);
    } else {
        print_outcome(&outcome);
    }
    Ok(())
}

fn outcome_json(outcome: &Outcome) -> serde_json::Value {
    let decision = &outcome.decision;
    serde_json::json!({
        "provider": decision.provider.name(),
        "model": decision.model,
        "effort": decision.effort,
        "gates": decision.gate_tags(),
        "classification": decision.classification,
        "usage": decision.usage,
        "rationale": decision.rationale,
        "dispatch": outcome.dispatch,
        "dry_run": outcome.dispatch.is_none(),
        "log_id": outcome.log_id,
        "log_error": outcome.log_error,
        "watch": "agent-viewer",
    })
}

fn print_outcome(outcome: &Outcome) {
    let decision = &outcome.decision;
    let mut line = decision.provider.name().to_string();
    if let Some(model) = &decision.model {
        line.push_str(&format!(" model {model}"));
    }
    if let Some(effort) = &decision.effort {
        line.push_str(&format!(" effort {effort}"));
    }
    match &outcome.dispatch {
        Some(dispatch) => {
            let id = dispatch.job_id.as_deref().unwrap_or("(id unresolved)");
            line.push_str(&format!(" job {id} name {:?}", dispatch.job_name));
        }
        None => line.push_str(" (dry run, nothing dispatched)"),
    }
    println!("{line}");
    println!("why: {}", decision.rationale);
    match (outcome.log_id, &outcome.log_error) {
        (Some(id), _) => println!("log: row {id} in {}", db_path()),
        // The job is running regardless, so this is a warning on stderr, not a failure.
        (None, error) => eprintln!(
            "log: NOT RECORDED in {}: {}",
            db_path(),
            error.as_deref().unwrap_or("unknown error")
        ),
    }
    if outcome.dispatch.is_some() {
        println!("watch: agent-viewer");
    }
}

fn db_path() -> String {
    agent_router_core::log::default_db_path()
        .display()
        .to_string()
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

fn log(limit: usize, json: bool) -> agent_router_core::Result<()> {
    let rows = DecisionLog::open()?.recent(limit)?;
    if json {
        let rows: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for row in &rows {
        println!(
            "#{id} {provider}{dry} codex_ready {ready}/6 claude_signals {signals}/6 \
             {confidence} gates[{gates}] codex {codex:.0}% claude {claude:.0}% {job}",
            id = row.id,
            provider = row.provider,
            dry = if row.dry_run { " (dry run)" } else { "" },
            ready = row
                .codex_ready_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".to_string()),
            signals = row
                .claude_signal_count
                .map(|count| count.to_string())
                .unwrap_or_else(|| "-".to_string()),
            confidence = row.confidence.as_deref().unwrap_or("-"),
            gates = row.gates,
            codex = row.codex_weekly_pct,
            claude = row.claude_weekly_pct,
            job = row
                .job_id
                .as_deref()
                .or(row.job_name.as_deref())
                .unwrap_or(&row.outcome),
        );
        println!("     {}", first_line(&row.task));
    }
    Ok(())
}

fn row_json(row: &Row) -> serde_json::Value {
    serde_json::json!({
        "id": row.id,
        "created_at_ms": row.created_at_ms,
        "task": row.task,
        "dir": row.dir,
        "requested": row.requested,
        "provider": row.provider,
        "model": row.model,
        "effort": row.effort,
        "verdict": row.verdict,
        "confidence": row.confidence,
        "codex_ready_count": row.codex_ready_count,
        "claude_signal_count": row.claude_signal_count,
        "missing_connector": row.missing_connector,
        "gates": row.gates,
        "claude_weekly_pct": row.claude_weekly_pct,
        "codex_weekly_pct": row.codex_weekly_pct,
        "dry_run": row.dry_run,
        "job_id": row.job_id,
        "job_name": row.job_name,
        "outcome": row.outcome,
        "rationale": row.rationale,
    })
}

/// The first line of a task, capped, so one log row stays one line.
fn first_line(task: &str) -> String {
    let line = task.lines().next().unwrap_or("");
    if line.chars().count() <= 100 {
        return line.to_string();
    }
    format!("{}...", line.chars().take(97).collect::<String>())
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
