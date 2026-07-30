use agent_router_core::log::{DecisionLog, Row};
use agent_router_core::parity::{Difference, ParityReport, ServerProjection, Status};
use agent_router_core::run::{Outcome, Request};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};

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
        /// Model override, requires an explicit --provider.
        #[arg(long)]
        model: Option<String>,
        /// Job name (defaults to the first 40 characters of the task).
        #[arg(long)]
        name: Option<String>,
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
    /// Compare project scoped Claude and Codex declarations.
    Parity {
        #[arg(long = "root")]
        roots: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    match cli.command {
        Command::Parity { roots, json } => parity_exit(roots, json),
        command => match run(Cli { command }) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agent-router: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

fn parity_exit(roots: Vec<PathBuf>, json: bool) -> std::process::ExitCode {
    let config = match agent_router_core::Config::load() {
        Ok(config) => config,
        Err(error) => {
            eprintln!(
                "agent-router: parity configuration error: {}",
                escape_terminal_controls(&error.to_string())
            );
            return std::process::ExitCode::from(2);
        }
    };
    let report = match agent_router_core::parity::check(&roots, &config) {
        Ok(report) => report,
        Err(error) => {
            eprintln!(
                "agent-router: parity scan error while reading .mcp.json or \
                 .codex/config.toml: {}",
                escape_terminal_controls(&error.to_string())
            );
            return std::process::ExitCode::from(2);
        }
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&parity_json(&report))
                .expect("serializing a JSON value cannot fail")
        );
    } else {
        print_parity(&report);
    }

    match report.status() {
        Status::Aligned | Status::Intentional => std::process::ExitCode::SUCCESS,
        Status::Drift => std::process::ExitCode::FAILURE,
    }
}

fn run(cli: Cli) -> agent_router_core::Result<()> {
    match cli.command {
        Command::Run {
            task,
            dir,
            provider,
            model,
            name,
            dry_run,
            json,
        } => route(task, dir, provider, model, name, dry_run, json),
        Command::Usage { json } => usage(json),
        Command::Log { limit, json } => log(limit, json),
        Command::Parity { .. } => unreachable!("parity has a command specific exit path"),
    }
}

fn print_parity(report: &ParityReport) {
    println!("parity: {}", status_label(report.status()));
    for project in &report.projects {
        let differences = project_differences(report, project);
        println!(
            "{}: {}",
            escape_terminal_controls(&project.to_string_lossy()),
            status_label(difference_status(&differences))
        );
        for difference in differences {
            match &difference.server {
                Some(server) => {
                    println!(
                        "  {} server {}",
                        kind_label(difference),
                        escape_terminal_controls(server)
                    );
                }
                None => println!("  {}", kind_label(difference)),
            }
            print_projection("claude", difference.claude.as_ref());
            print_projection("codex", difference.codex.as_ref());
            if let Some(reason) = &difference.intentional_reason {
                println!("    reason: {}", escape_terminal_controls(reason));
            }
        }
    }
}

fn print_projection(label: &str, projection: Option<&ServerProjection>) {
    let Some(projection) = projection else {
        return;
    };
    println!(
        "    {label}: command {} args {} env keys {}",
        escape_terminal_controls(projection.command.as_deref().unwrap_or("(unset)")),
        escaped_string_list(&projection.args),
        escaped_string_list(&projection.env_keys)
    );
}

fn parity_json(report: &ParityReport) -> serde_json::Value {
    let projects = report
        .projects
        .iter()
        .map(|project| {
            let differences = project_differences(report, project);
            let status = difference_status(&differences);
            let differences = differences
                .into_iter()
                .map(|difference| {
                    serde_json::json!({
                        "server": difference.server.as_deref(),
                        "kind": difference.kind,
                        "claude": &difference.claude,
                        "codex": &difference.codex,
                        "intentional_reason": difference.intentional_reason.as_deref(),
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "root": project,
                "status": status,
                "differences": differences,
            })
        })
        .collect::<Vec<_>>();

    serde_json::json!({
        "status": report.status(),
        "projects": projects,
    })
}

fn escaped_string_list(values: &[String]) -> String {
    let json = serde_json::to_string(values).expect("serializing string lists cannot fail");
    escape_terminal_controls(&json)
}

fn escape_terminal_controls(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_control() {
            escaped.extend(character.escape_default());
        } else {
            escaped.push(character);
        }
    }
    escaped
}

fn project_differences<'a>(report: &'a ParityReport, project: &Path) -> Vec<&'a Difference> {
    report
        .differences
        .iter()
        .filter(|difference| difference.root == project)
        .collect()
}

fn difference_status(differences: &[&Difference]) -> Status {
    if differences.is_empty() {
        Status::Aligned
    } else if differences
        .iter()
        .all(|difference| difference.intentional_reason.is_some())
    {
        Status::Intentional
    } else {
        Status::Drift
    }
}

fn status_label(status: Status) -> &'static str {
    match status {
        Status::Aligned => "aligned",
        Status::Intentional => "intentional",
        Status::Drift => "drift",
    }
}

fn kind_label(difference: &Difference) -> &'static str {
    match difference.kind {
        agent_router_core::ParityKind::MissingInCodex => "missing_in_codex",
        agent_router_core::ParityKind::MissingInClaude => "missing_in_claude",
        agent_router_core::ParityKind::CommandDiffers => "command_differs",
        agent_router_core::ParityKind::ArgsDiffer => "args_differ",
        agent_router_core::ParityKind::EnvKeysDiffer => "env_keys_differ",
        agent_router_core::ParityKind::StandaloneClaudeMd => "standalone_claude_md",
    }
}

// Parameters are the run subcommand's clap flags passed straight through, so the count tracks the CLI surface.
#[allow(clippy::too_many_arguments)]
fn route(
    task: String,
    dir: Option<PathBuf>,
    provider: String,
    model: Option<String>,
    name: Option<String>,
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
        name,
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
    })
}

fn print_outcome(outcome: &Outcome) {
    let decision = &outcome.decision;
    let mut line = decision.provider.name().to_string();
    if let Some(classification) = &decision.classification {
        line.push_str(&format!(" complexity {}", classification.complexity.tag()));
    }
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
             {confidence} {complexity} gates[{gates}] codex {codex:.0}% claude {claude:.0}% {job}",
            id = row.id,
            provider = row.provider,
            dry = if row.dry_run { " (dry run)" } else { "" },
            complexity = row.complexity.as_deref().unwrap_or("-"),
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
        "complexity": row.complexity,
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
