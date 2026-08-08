use agent_router_core::doctor::Health;
use agent_router_core::log::{DecisionLog, Row};
use agent_router_core::parity::{Difference, GlobalReport, ParityReport, ServerProjection, Status};
use agent_router_core::run::{Outcome, Request};
use agent_router_core::stats::{Rate, Stats, Window};
use agent_router_core::status::Report;
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
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
        /// auto (classify), or codex/claude/opencode to skip scoring. Every route still has the
        /// classifier model title the job.
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Model override, requires an explicit --provider.
        #[arg(long)]
        model: Option<String>,
        /// Job name. Defaults to a title the classifier model writes, and supplying one skips that
        /// call.
        #[arg(long)]
        name: Option<String>,
        /// Decide and log without dispatching.
        #[arg(long)]
        dry_run: bool,
        /// MCP config file for the claude job, repeatable. Rejected for other providers.
        #[arg(long = "mcp-config")]
        mcp_configs: Vec<PathBuf>,
        /// Use only the --mcp-config files, dropping every inherited MCP server. This also strips
        /// the claude.ai connectors, which no --mcp-config can restore, so a job routed to claude
        /// for a connector can lose the very connector it was routed for.
        #[arg(long)]
        strict_mcp_config: bool,
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
        /// Judge one decision: the row id and one of good, bad, or rerouted. This records whether
        /// routing the task there was the right call, which no backend can answer, and prints one
        /// confirmation line rather than the listing.
        #[arg(long, num_args = 2, value_names = ["ROW_ID", "MARK"])]
        mark: Vec<String>,
        /// What the judgement was, in free text. Requires --mark.
        #[arg(long)]
        note: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Aggregate metrics over recent routing decisions.
    Stats {
        #[arg(long, default_value_t = 200)]
        limit: usize,
        /// Also drop rows older than a lookback window, for example 24h, 7d, or 2w.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Reconcile recent dispatched decisions against the backends that ran them.
    Status {
        /// The default is smaller than the stats one on purpose: every row here costs a live
        /// backend call, where a stats window is pure SQL.
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Also drop rows older than a lookback window, for example 24h, 7d, or 2w.
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Preflight the provider binaries, credentials, usage provenance, config, and decision log.
    Doctor,
    /// Compare the global and project scoped Claude and Codex declarations.
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
        Command::Doctor => doctor_exit(),
        Command::Parity { roots, json } => parity_exit(roots, json),
        Command::Status { limit, since, json } => status_exit(limit, since, json),
        command => match run(Cli { command }) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("agent-router: {e}");
                std::process::ExitCode::FAILURE
            }
        },
    }
}

/// Doctor owns its exit code the same way parity does: a failing check is reported by exiting
/// nonzero, not by an error, since the report itself is the output.
fn doctor_exit() -> std::process::ExitCode {
    let report = agent_router_core::doctor::run();
    for check in &report.checks {
        println!(
            "{:<4} {:<19} {}",
            health_label(check.health),
            check.name,
            escape_terminal_controls(&check.detail)
        );
    }
    if report.failed() {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

fn health_label(health: Health) -> &'static str {
    match health {
        Health::Pass => "pass",
        Health::Warn => "warn",
        Health::Fail => "fail",
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
    let home = agent_router_core::runtime::home_dir();
    let report = match agent_router_core::parity::check(&roots, &config, &home) {
        Ok(report) => report,
        Err(error) => {
            eprintln!(
                "agent-router: parity scan error while reading .mcp.json, .claude.json, or \
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

/// Status owns its exit code the way doctor and parity do, because the report is the output: 0 when
/// nothing in the window is known to have failed, 1 when something is, and 2 when the command could
/// not run at all. An `unknown` never moves it, since an absence of information is not a finding.
fn status_exit(limit: usize, since: Option<String>, json: bool) -> std::process::ExitCode {
    let since_ms = match since.as_deref().map(agent_router_core::stats::parse_since) {
        Some(Ok(lookback)) => Some(agent_router_core::runtime::now_ms() - lookback),
        Some(Err(error)) => return status_unrunnable(&error),
        None => None,
    };
    let log = match DecisionLog::open() {
        Ok(log) => log,
        Err(error) => return status_unrunnable(&error),
    };
    let report = match agent_router_core::status::reconcile(&log, Window { limit, since_ms }) {
        Ok(report) => report,
        Err(error) => return status_unrunnable(&error),
    };

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&status_json(&report))
                .expect("serializing a JSON value cannot fail")
        );
    } else {
        print_status(&report);
    }

    if report.failed() {
        std::process::ExitCode::FAILURE
    } else {
        std::process::ExitCode::SUCCESS
    }
}

/// A command that never ran exits 2, which is a different fact from a window carrying a failure.
fn status_unrunnable(error: &agent_router_core::Error) -> std::process::ExitCode {
    eprintln!(
        "agent-router: status could not run: {}",
        escape_terminal_controls(&error.to_string())
    );
    std::process::ExitCode::from(2)
}

fn status_json(report: &Report) -> serde_json::Value {
    let rows = report
        .rows
        .iter()
        .map(|row| {
            serde_json::json!({
                "id": row.id,
                "provider": row.provider,
                "job_id": row.job_id,
                "observation": row.observation.label(),
                "state": row.state.tag(),
                // Null on a row that was never swept, which is not the same as a sweep that found
                // nothing.
                "traced": row.traced,
            })
        })
        .collect::<Vec<_>>();
    serde_json::json!({
        "rows_considered": report.rows.len(),
        "oldest_created_at_ms": report.oldest_created_at_ms,
        "newest_created_at_ms": report.newest_created_at_ms,
        "rows": rows,
    })
}

fn print_status(report: &Report) {
    println!("rows considered: {}", report.rows.len());
    println!(
        "window: {} to {}",
        stamp(report.oldest_created_at_ms),
        stamp(report.newest_created_at_ms)
    );
    for row in &report.rows {
        println!(
            "#{id} {provider} {state} {observation} job {job}{trace}",
            id = row.id,
            provider = escape_terminal_controls(&row.provider),
            state = row.state.tag(),
            observation = escape_terminal_controls(&row.observation.label()),
            job = escape_terminal_controls(&row.job_id),
            trace = trace_label(row.traced),
        );
    }
}

/// A row nobody swept prints nothing at all, because "we did not look" is not "no trace".
fn trace_label(traced: Option<bool>) -> &'static str {
    match traced {
        Some(true) => " traced",
        Some(false) => " no trace",
        None => "",
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
            mcp_configs,
            strict_mcp_config,
            json,
        } => route(
            task,
            dir,
            provider,
            model,
            name,
            dry_run,
            &mcp_configs,
            strict_mcp_config,
            json,
        ),
        Command::Usage { json } => usage(json),
        Command::Log {
            limit,
            mark,
            note,
            json,
        } => log(limit, &mark, note.as_deref(), json),
        Command::Stats { limit, since, json } => stats(limit, since, json),
        Command::Doctor => unreachable!("doctor has a command specific exit path"),
        Command::Parity { .. } => unreachable!("parity has a command specific exit path"),
        Command::Status { .. } => unreachable!("status has a command specific exit path"),
    }
}

fn print_parity(report: &ParityReport) {
    println!("parity: {}", status_label(report.status()));
    print_global(&report.global);
    for project in &report.projects {
        let differences = project_differences(report, project);
        println!(
            "{}: {}",
            escape_terminal_controls(&project.to_string_lossy()),
            status_label(difference_status(&differences))
        );
        print_differences(&differences);
    }
}

/// The global entry prints first, because it is the ambient configuration every project inherits.
fn print_global(global: &GlobalReport) {
    let differences = global.differences.iter().collect::<Vec<_>>();
    println!("global: {}", status_label(difference_status(&differences)));
    println!(
        "  claude: {}",
        escape_terminal_controls(&global.claude_path.to_string_lossy())
    );
    println!(
        "  codex: {}",
        escape_terminal_controls(&global.codex_path.to_string_lossy())
    );
    print_differences(&differences);
}

fn print_differences(differences: &[&Difference]) {
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
            serde_json::json!({
                "root": project,
                "status": difference_status(&differences),
                "differences": differences_json(&differences),
            })
        })
        .collect::<Vec<_>>();

    let global_differences = report.global.differences.iter().collect::<Vec<_>>();
    serde_json::json!({
        "status": report.status(),
        "global": serde_json::json!({
            "claude_path": report.global.claude_path,
            "codex_path": report.global.codex_path,
            "status": difference_status(&global_differences),
            "differences": differences_json(&global_differences),
        }),
        "projects": projects,
    })
}

fn differences_json(differences: &[&Difference]) -> Vec<serde_json::Value> {
    differences
        .iter()
        .map(|difference| {
            serde_json::json!({
                "server": difference.server.as_deref(),
                "kind": difference.kind,
                "claude": &difference.claude,
                "codex": &difference.codex,
                "intentional_reason": difference.intentional_reason.as_deref(),
            })
        })
        .collect()
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
        agent_router_core::ParityKind::TransportDiffers => "transport_differs",
        agent_router_core::ParityKind::EndpointDiffers => "endpoint_differs",
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
    mcp_configs: &[PathBuf],
    strict_mcp_config: bool,
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
        mcp_configs,
        strict_mcp_config,
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
        // Emitted on both paths, as null off the dry run one, so the JSON shape does not depend on
        // which path produced it.
        "estimate": outcome.estimate,
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
    if let Some(estimate) = &outcome.estimate {
        print_estimate(estimate);
    }
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

/// The projection is an upper bound, and the wording is what keeps it from being read as the job's
/// own cost, so "up to" and the clause naming what else is inside the number are not trimmed. A
/// short sample prints its shortfall rather than a number it cannot support.
fn print_estimate(estimate: &agent_router_core::estimate::Estimate) {
    match estimate.weekly_pct {
        Some(weekly_pct) => println!(
            "estimate: up to {weekly_pct:.1}% of the {} weekly window (median gap over {} \
             comparable jobs, includes other usage in the same period)",
            estimate.provider, estimate.samples
        ),
        None => println!(
            "estimate: insufficient data ({} comparable jobs, {} needed)",
            estimate.samples, estimate.needed
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
    println!("provider  5h       weekly  source     weekly reset");
    for (name, headroom) in [("claude", snapshot.claude), ("codex", snapshot.codex)] {
        println!(
            "{name:<9} {:>5.1}%  {:>7}  {:<9}  {}",
            headroom.five_hour_pct,
            weekly_label(&headroom),
            usage_source_label(headroom.stale),
            reset_label(headroom.weekly_reset_epoch)
        );
    }
    Ok(())
}

/// The weekly column. An unread window reports 0 percent used, and printing that as `0.0%` states
/// a reading nobody took: this is the table that was showing a hard limited Codex as completely
/// idle. Routing refuses such a provider, so the table has to say so too.
fn weekly_label(headroom: &agent_router_core::Headroom) -> String {
    if headroom.weekly_known() {
        format!("{:.1}%", headroom.weekly_pct)
    } else {
        "unknown".to_string()
    }
}

fn log(
    limit: usize,
    mark: &[String],
    note: Option<&str>,
    json: bool,
) -> agent_router_core::Result<()> {
    if !mark.is_empty() {
        // --mark short circuits the listing, so there is no listing for --json to shape. The
        // combination is refused rather than accepted and ignored, on the same rule as --note
        // below: a caller passing a flag believes it did something.
        if json {
            return Err(agent_router_core::Error::Command(
                "--json cannot be combined with --mark: a mark prints one confirmation line, not \
                 a listing"
                    .to_string(),
            ));
        }
        return mark_row(mark, note);
    }
    // A note with nothing to attach it to is refused rather than dropped, mirroring the rule that
    // --model requires an explicit --provider: a caller passing a note believes it recorded one.
    if note.is_some() {
        return Err(agent_router_core::Error::Command(
            "--note requires --mark: a note is the reason for one row's judgement".to_string(),
        ));
    }
    let rows = DecisionLog::open()?.recent(limit)?;
    if json {
        let rows: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for row in &rows {
        println!(
            "#{id} {provider}{dry} orchestration {orchestration} {complexity} \
             proj claude {claude_pace} codex {codex_pace} \
             gates[{gates}] codex {codex:.0}% claude {claude:.0}% {job} \
             {outcome}{judgement}",
            id = row.id,
            provider = row.provider,
            dry = if row.dry_run { " (dry run)" } else { "" },
            complexity = row.complexity.as_deref().unwrap_or("-"),
            orchestration = flag(row.orchestration),
            claude_pace = pace(row.claude_projected_draw),
            codex_pace = pace(row.codex_projected_draw),
            gates = row.gates,
            codex = row.codex_weekly_pct,
            claude = row.claude_weekly_pct,
            // The job handle and what became of it are both printed, because a reconciled row
            // carries a job id, so a state only reachable through --json would be a column written
            // and never read back.
            job = row
                .job_id
                .as_deref()
                .or(row.job_name.as_deref())
                .unwrap_or("-"),
            outcome = escape_terminal_controls(&row.outcome),
            judgement = judgement_label(row.mark.as_deref(), row.note.as_deref()),
        );
        println!("     {}", first_line(&row.task));
    }
    Ok(())
}

/// `--mark` short circuits: it records one judgement and prints one confirmation line, rather than
/// following it with a listing the caller did not ask for. Every value is validated in core, which
/// owns the accepted vocabulary and is the only thing that writes the columns.
fn mark_row(mark: &[String], note: Option<&str>) -> agent_router_core::Result<()> {
    // clap appends on a repeated option, so a second --mark arrives here as four values rather
    // than as a parse failure. A repeat is a command error like any other, never a panic.
    let [row_id, value] = mark else {
        return Err(agent_router_core::Error::Command(
            "--mark takes one ROW_ID and one MARK and cannot be repeated: judge one row per \
             invocation"
                .to_string(),
        ));
    };
    let id = agent_router_core::log::parse_row_id(row_id)?;
    let mark = agent_router_core::log::parse_mark(value)?;
    DecisionLog::open()?.mark(id, mark, note)?;
    println!(
        "#{id} marked {}{}",
        mark.tag(),
        judgement_note(note.map(first_line).as_deref())
    );
    Ok(())
}

/// An unjudged row prints nothing at all, because nobody having judged it is not a judgement. The
/// note is operator supplied free text on its way back to a terminal, so it is escaped here and
/// capped to one line by the same rule the task text already follows.
fn judgement_label(mark: Option<&str>, note: Option<&str>) -> String {
    match mark {
        Some(mark) => format!(
            " mark {}{}",
            escape_terminal_controls(mark),
            judgement_note(note.map(first_line).as_deref())
        ),
        None => String::new(),
    }
}

fn judgement_note(note: Option<&str>) -> String {
    match note {
        Some(note) => format!(" note {}", escape_terminal_controls(note)),
        None => String::new(),
    }
}

fn stats(limit: usize, since: Option<String>, json: bool) -> agent_router_core::Result<()> {
    let since_ms = match &since {
        Some(window) => {
            let lookback = agent_router_core::stats::parse_since(window)?;
            Some(agent_router_core::runtime::now_ms() - lookback)
        }
        None => None,
    };
    let log = DecisionLog::open()?;
    let stats = agent_router_core::stats::collect(&log, Window { limit, since_ms })?;
    if json {
        println!("{}", serde_json::to_string_pretty(&stats_json(&stats))?);
        return Ok(());
    }
    print_stats(&stats);
    Ok(())
}

fn stats_json(stats: &Stats) -> serde_json::Value {
    serde_json::json!({
        "rows_considered": stats.rows_considered,
        "oldest_created_at_ms": stats.oldest_created_at_ms,
        "newest_created_at_ms": stats.newest_created_at_ms,
        "routes": stats.routes,
        "gates": stats.gates,
        "complexity": stats.complexity,
        "router_versions": stats.router_versions,
        "auto_routes": stats.auto_routes,
        "flip_rate": rate_json(&stats.flip_rate),
        "classifier_failure_rate": rate_json(&stats.classifier_failure_rate),
        "dry_run_share": rate_json(&stats.dry_run_share),
        "bad_rate_by_gate": rate_map_json(&stats.bad_rate_by_gate),
        "bad_rate_by_provider": rate_map_json(&stats.bad_rate_by_provider),
        "bad_rate_by_complexity": rate_map_json(&stats.bad_rate_by_complexity),
        "failure_rate_by_gate": rate_map_json(&stats.failure_rate_by_gate),
        "failure_rate_by_provider": rate_map_json(&stats.failure_rate_by_provider),
        "failure_rate_by_complexity": rate_map_json(&stats.failure_rate_by_complexity),
    })
}

/// One breakdown as an object keyed the same way the distribution it breaks down is keyed, so the
/// two reconcile key by key.
fn rate_map_json(rates: &BTreeMap<String, Rate>) -> serde_json::Value {
    rates
        .iter()
        .map(|(key, rate)| (key.clone(), rate_json(rate)))
        .collect::<serde_json::Map<String, serde_json::Value>>()
        .into()
}

/// Both counts travel with the share, so a reader can check the rate rather than trust it. The
/// share is null when there was nothing to divide by.
fn rate_json(rate: &Rate) -> serde_json::Value {
    serde_json::json!({
        "numerator": rate.numerator,
        "denominator": rate.denominator,
        "share": rate.share(),
    })
}

fn print_stats(stats: &Stats) {
    println!("rows considered: {}", stats.rows_considered);
    println!(
        "window: {} to {}",
        stamp(stats.oldest_created_at_ms),
        stamp(stats.newest_created_at_ms)
    );
    println!("auto routes: {}", stats.auto_routes);
    print_counts("routes", &stats.routes);
    print_counts("gates", &stats.gates);
    print_counts("complexity", &stats.complexity);
    print_counts("router versions", &stats.router_versions);
    print_rate("flip rate", &stats.flip_rate);
    print_rate("classifier failure rate", &stats.classifier_failure_rate);
    print_rate("dry run share", &stats.dry_run_share);
    print_rate_map("bad rate by gate", &stats.bad_rate_by_gate);
    print_rate_map("bad rate by provider", &stats.bad_rate_by_provider);
    print_rate_map("bad rate by complexity", &stats.bad_rate_by_complexity);
    print_rate_map("failure rate by gate", &stats.failure_rate_by_gate);
    print_rate_map("failure rate by provider", &stats.failure_rate_by_provider);
    print_rate_map(
        "failure rate by complexity",
        &stats.failure_rate_by_complexity,
    );
}

fn print_counts(label: &str, counts: &BTreeMap<String, usize>) {
    if counts.is_empty() {
        println!("{label}: -");
        return;
    }
    let rendered = counts
        .iter()
        .map(|(name, count)| format!("{} {count}", escape_terminal_controls(name)))
        .collect::<Vec<_>>()
        .join(", ");
    println!("{label}: {rendered}");
}

/// A rate with no denominator prints as "-": any percentage on the screen would be invented, and a
/// zero over zero share renders as NaN.
fn print_rate(label: &str, rate: &Rate) {
    match rate.share() {
        Some(share) => println!(
            "{label}: {:.1}% ({} of {})",
            share * 100.0,
            rate.numerator,
            rate.denominator
        ),
        None => println!("{label}: - (0 of 0)"),
    }
}

/// One breakdown, key by key, with the same dash a rate with no denominator gets on its own line: a
/// key nobody has judged yet is present and unanswered rather than absent or invented.
fn print_rate_map(label: &str, rates: &BTreeMap<String, Rate>) {
    if rates.is_empty() {
        println!("{label}: -");
        return;
    }
    let rendered = rates
        .iter()
        .map(|(name, rate)| {
            let name = escape_terminal_controls(name);
            match rate.share() {
                Some(share) => format!(
                    "{name} {:.1}% ({} of {})",
                    share * 100.0,
                    rate.numerator,
                    rate.denominator
                ),
                None => format!("{name} - (0 of 0)"),
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    println!("{label}: {rendered}");
}

fn stamp(created_at_ms: Option<i64>) -> String {
    match created_at_ms {
        Some(ms) => ms.to_string(),
        None => "-".to_string(),
    }
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
        // What the router decided, which is not what the job ran at: that is `effective_effort`,
        // null wherever no backend reported one.
        "effort": row.effort,
        "effective_effort": row.effective_effort,
        "verdict": row.verdict,
        "confidence": row.confidence,
        "complexity": row.complexity,
        "task_context_horizon": row.task_context_horizon,
        // Null on every row written since the classifier stopped scoring them. Still printed
        // because the rows recorded under them are the corpus every backtest replays, and this is
        // the only way to read one back through the tool.
        "codex_ready_count": row.codex_ready_count,
        "claude_signal_count": row.claude_signal_count,
        "orchestration": row.orchestration,
        "missing_connector": row.missing_connector,
        "claude_projected_draw": row.claude_projected_draw,
        "codex_projected_draw": row.codex_projected_draw,
        "gates": row.gates,
        "claude_weekly_pct": row.claude_weekly_pct,
        "codex_weekly_pct": row.codex_weekly_pct,
        "dry_run": row.dry_run,
        "job_id": row.job_id,
        "job_name": row.job_name,
        "outcome": row.outcome,
        "rationale": row.rationale,
        // Null on a row written before the marker, which is not the same as a live read.
        "claude_usage_stale": row.claude_usage_stale,
        "codex_usage_stale": row.codex_usage_stale,
        // Null on a row no reconciliation has ever read a backend for.
        "reconciled_at_ms": row.reconciled_at_ms,
        // Null on a row nobody has judged, which is not the same as judging it good.
        "mark": row.mark,
        "note": row.note,
        // Null on a row written before this column, which is not the same as a genuinely empty
        // version: the point of the column is that an aggregate spanning several of these is
        // visibly mixed rather than pooled as one population.
        "router_version": row.router_version,
    })
}

/// A recorded boolean, or "-" when the row does not know. An older row scored no orchestration and
/// an explicit route was never scored at all, and neither is the same as a scored false.
fn flag(value: Option<bool>) -> &'static str {
    match value {
        Some(true) => "yes",
        Some(false) => "no",
        None => "-",
    }
}

/// A recorded projected weekly draw as a percent of that provider's own allowance, or "-" when no
/// projection could be computed and the override did not run. Over 100 is the provider running out
/// before its window resets, which is the whole reading, so it is printed as the percentage it is
/// rather than signed against some baseline.
fn pace(projected_draw: Option<f64>) -> String {
    match projected_draw {
        Some(projected_draw) => format!("{projected_draw:.0}%"),
        None => "-".to_string(),
    }
}

/// The first line of a task, capped, so one log row stays one line.
fn first_line(task: &str) -> String {
    let line = task.lines().next().unwrap_or("");
    if line.chars().count() <= 100 {
        return line.to_string();
    }
    format!("{}...", line.chars().take(97).collect::<String>())
}

/// Where a provider's numbers came from. Two zeroes from a fail open read and two zeroes from a
/// provider that has consumed nothing are the same line without this, and only one of them means
/// the router knows anything.
fn usage_source_label(stale: bool) -> &'static str {
    if stale { "fail-open" } else { "live" }
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
