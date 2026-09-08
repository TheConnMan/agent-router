use agent_router_core::adversarial_review::{
    REVIEW_CANCELLED_REASON, REVIEW_POLL, ReviewOutcome, ReviewStatus, ReviewerPin,
};
use agent_router_core::doctor::Health;
use agent_router_core::log::{
    CancelResult, DecisionLog, ReviewEntry, ReviewRow, ReviewTerminal, Row,
};
use agent_router_core::run::{Outcome, Request};
use agent_router_core::stats::{Rate, Stats, Window};
use agent_router_core::status::Report;
use clap::{Parser, Subcommand};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    name = "agent-router",
    about = "Route a task automatically to codex or claude, or dispatch explicitly to grok"
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
        /// auto, or codex, claude, or grok to pin the provider while omitted values are
        /// classified. Grok is explicit only.
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Model override, requires an explicit --provider.
        #[arg(long)]
        model: Option<String>,
        /// Reasoning effort override. Requires explicit provider and model values.
        #[arg(long)]
        effort: Option<String>,
        /// Job name. Defaults to a title the classifier model writes, and supplying one skips that
        /// call.
        #[arg(long)]
        name: Option<String>,
        /// Decide and log without dispatching.
        #[arg(long)]
        dry_run: bool,
        /// MCP config file for the claude job, repeatable. Rejected for every other provider,
        /// including grok.
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
    /// Run a synchronous read only review on an eligible alternative provider.
    AdversarialReview {
        /// The review request.
        request: String,
        /// The provider running the calling thread. This provider, including grok, is excluded.
        #[arg(long)]
        primary: String,
        /// Working directory for the review. Defaults to the current directory.
        #[arg(long)]
        dir: Option<PathBuf>,
        /// auto selects the eligible alternative with the most headroom. codex, claude, or grok
        /// pins the reviewer instead. A pin must differ from --primary and still passes every
        /// eligibility gate (authoritative fresh capacity below the ceiling, and for claude the
        /// configured reserve as a floor). An ineligible pin is reported as skipped, never
        /// rerouted.
        #[arg(long, default_value = "auto")]
        provider: String,
        /// Reviewer model, passed to the pinned provider verbatim. Requires an explicit
        /// --provider other than grok. Without it a pin runs the provider's configured review
        /// tier. A model the provider rejects fails the review rather than being substituted.
        #[arg(long)]
        model: Option<String>,
        /// Give up waiting after this many seconds and report the review as pending with exit 4.
        /// The review keeps running; `agent-router review status <ID>` returns its eventual
        /// result. 0 returns pending immediately. Without this flag the command waits for a
        /// terminal result exactly as it always has.
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long)]
        json: bool,
    },
    /// Weekly and 5h headroom for Codex and Claude, plus observed Grok capacity.
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
        /// Settled rows nobody has judged yet, newest first. A review pass's worklist.
        #[arg(long)]
        unmarked: bool,
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
    /// Report or stop an adversarial review by the id `adversarial-review` printed.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Internal re-exec target for a detached adversarial reviewer, not a supported interface.
    /// Its argv, its output, and its exit code are private to `adversarial-review` and may change
    /// without notice.
    #[command(hide = true)]
    ReviewWorker {
        #[arg(long = "review-id")]
        review_id: i64,
        #[arg(long)]
        primary: String,
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        provider: String,
        #[arg(long)]
        model: Option<String>,
        request: String,
    },
}

#[derive(Subcommand)]
enum ReviewCommand {
    /// Whether the review is still running, and its retained result once it is terminal. Exits 0
    /// completed, 3 skipped, 1 failed or cancelled, 4 still pending.
    Status {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Stop an in-flight review and settle its row as cancelled. A review that already settled is
    /// reported with the state that won, not cancelled a second time.
    Cancel { id: i64 },
}

enum CliStatus {
    Success,
    Failure,
    Unrunnable,
    ReviewSkipped,
    ReviewPending,
}

fn exit_code(status: CliStatus) -> std::process::ExitCode {
    match status {
        CliStatus::Success => std::process::ExitCode::SUCCESS,
        CliStatus::Failure => std::process::ExitCode::FAILURE,
        CliStatus::Unrunnable => std::process::ExitCode::from(2),
        CliStatus::ReviewSkipped => std::process::ExitCode::from(3),
        CliStatus::ReviewPending => std::process::ExitCode::from(4),
    }
}

fn main() -> std::process::ExitCode {
    let cli = Cli::parse();
    let mut ctx = agent_router_core::Context::from_process();
    let status = match cli.command {
        Command::Doctor => doctor_status(&ctx),
        Command::Status { limit, since, json } => status_status(&ctx, limit, since, json),
        Command::AdversarialReview {
            request,
            primary,
            dir,
            provider,
            model,
            timeout,
            json,
        } => adversarial_review_status(
            &mut ctx, request, primary, dir, provider, model, timeout, json,
        ),
        Command::Review { command } => review_command_status(&ctx, command),
        Command::ReviewWorker {
            review_id,
            primary,
            dir,
            provider,
            model,
            request,
        } => review_worker_status(&mut ctx, review_id, request, primary, dir, provider, model),
        command => match run(Cli { command }, &mut ctx) {
            Ok(()) => CliStatus::Success,
            Err(e) => {
                eprintln!("agent-router: {e}");
                CliStatus::Failure
            }
        },
    };
    exit_code(status)
}

// Parameters are the adversarial-review subcommand's clap flags passed straight through, so the
// count tracks the CLI surface.
#[allow(clippy::too_many_arguments)]
fn adversarial_review_status(
    ctx: &mut agent_router_core::Context,
    body: String,
    primary: String,
    dir: Option<PathBuf>,
    provider: String,
    model: Option<String>,
    timeout: Option<u64>,
    json: bool,
) -> CliStatus {
    // Every early failure below still reports what the caller asked for, so a `--provider claude
    // --model fable` that never reached selection is distinguishable from an automatic request in
    // the JSON and, through the rationale, in the reviews log, which has no requested_* columns.
    let requested = |mut outcome: agent_router_core::adversarial_review::ReviewOutcome| {
        outcome.requested_provider = (provider != "auto").then(|| provider.clone());
        outcome.requested_model = model.clone();
        if outcome.requested_provider.is_some() || outcome.requested_model.is_some() {
            let model = model
                .as_deref()
                .map(|model| format!(" with model {model}"))
                .unwrap_or_default();
            outcome.rationale = format!(
                "{provider} requested explicitly{model} and rejected before selection; {}",
                outcome.rationale
            );
        }
        outcome
    };
    let (primary_provider, pin) = match review_selection(&primary, &provider, model.as_deref()) {
        Ok(selection) => selection,
        Err(error) => {
            return finish_adversarial_review(
                &requested(agent_router_core::adversarial_review::failed_outcome(
                    &error.primary,
                    error.reason,
                )),
                None,
                json,
                ctx,
            );
        }
    };
    let dir = match dir {
        Some(dir) => dir,
        None => match std::env::current_dir() {
            Ok(dir) => dir,
            Err(error) => {
                return finish_adversarial_review(
                    &requested(agent_router_core::adversarial_review::failed_outcome(
                        primary_provider,
                        error.to_string(),
                    )),
                    None,
                    json,
                    ctx,
                );
            }
        },
    };
    if let Err(error) = ctx.load_config() {
        return finish_adversarial_review(
            &requested(agent_router_core::adversarial_review::failed_outcome(
                primary_provider,
                error.to_string(),
            )),
            Some(&dir),
            json,
            ctx,
        );
    }
    // The id has to exist before any provider work does, or an interrupted caller destroys both
    // the record and the paid work. A router.db that cannot take the row has nothing for the
    // lifecycle to stand on, so that case runs the review in this process exactly as it always
    // has: no id, no worker, and --timeout inert.
    let started = DecisionLog::open_in(&ctx.home).ok().and_then(|log| {
        log.start_review(primary_provider, &dir)
            .ok()
            .map(|id| (log, id))
    });
    let Some((log, review_id)) = started else {
        let request = agent_router_core::adversarial_review::ReviewRequest {
            primary_provider,
            body: &body,
            dir: &dir,
        };
        let outcome = match &pin {
            None => agent_router_core::adversarial_review::review_registered(&request, ctx),
            Some(pin) => {
                agent_router_core::adversarial_review::review_registered_pinned(&request, pin, ctx)
            }
        };
        return finish_adversarial_review(&outcome, Some(&dir), json, ctx);
    };
    eprintln!("agent-router: adversarial review {review_id} started");

    let worker = std::env::current_exe()
        .map_err(agent_router_core::Error::Io)
        .and_then(|exe| {
            let mut command = std::process::Command::new(exe);
            command
                .arg("review-worker")
                .arg("--review-id")
                .arg(review_id.to_string())
                .arg("--primary")
                .arg(primary_provider)
                .arg("--dir")
                .arg(&dir)
                .arg("--provider")
                .arg(&provider);
            if let Some(model) = &model {
                command.arg("--model").arg(model);
            }
            // `--` keeps a request whose own text starts with a dash from being read as a flag on
            // the worker's fresh argv.
            command.arg("--").arg(&body);
            agent_router_core::runtime::spawn_detached(
                command,
                &agent_router_core::runtime::router_log_path(&ctx.home, "review"),
                None,
            )
        });
    let mut worker = match worker {
        Ok(worker) => worker,
        Err(error) => {
            let outcome = requested(agent_router_core::adversarial_review::failed_outcome(
                primary_provider,
                error.to_string(),
            ));
            let _ = settle_review(&log, review_id, &outcome);
            return print_review_id_or(&log, review_id, json, &outcome);
        }
    };

    let deadline = timeout.map(|seconds| Instant::now() + Duration::from_secs(seconds));
    loop {
        if let Ok(Some(row)) = log.review(review_id)
            && review_state(&row) != ReviewStatus::Pending
        {
            return print_review_row(&row, json);
        }
        // The worker writes the row and then exits, so a bare "the worker is gone" reading races
        // the write it is trying to detect. Re-read once more and only settle a row that is still
        // pending.
        if matches!(worker.try_wait(), Ok(Some(_))) {
            match log.review(review_id) {
                Ok(Some(row)) if review_state(&row) != ReviewStatus::Pending => {
                    return print_review_row(&row, json);
                }
                _ => {
                    let outcome = requested(agent_router_core::adversarial_review::failed_outcome(
                        primary_provider,
                        "review worker exited before recording a result",
                    ));
                    let _ = settle_review(&log, review_id, &outcome);
                    return print_review_id_or(&log, review_id, json, &outcome);
                }
            }
        }
        // Checked before the first sleep so `--timeout 0` returns pending immediately, with the
        // id printed and the worker already running. This iteration already read the row as
        // pending, so the report is the caller's own, carrying what it asked for.
        if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            let mut outcome = pending_review_outcome(review_id, primary_provider);
            outcome.requested_provider = (provider != "auto").then_some(provider);
            outcome.requested_model = model;
            return print_adversarial_review(&outcome, json);
        }
        std::thread::sleep(REVIEW_POLL);
    }
}

/// How many times a terminal write retries a busy database before giving up. Bounded on purpose:
/// paid provider work must not be lost to a lock wait, and this is a retry loop, not a scheduler.
const REVIEW_SETTLE_ATTEMPTS: usize = 20;

/// The exit code map read as the process's own status. `ReviewStatus::exit_status` is the one map;
/// this only names the codes the CLI already has variants for.
fn review_cli_status(status: ReviewStatus) -> CliStatus {
    match status.exit_status() {
        0 => CliStatus::Success,
        3 => CliStatus::ReviewSkipped,
        4 => CliStatus::ReviewPending,
        _ => CliStatus::Failure,
    }
}

/// The state a row is in, over the one row-to-status map in core.
fn review_state(row: &ReviewRow) -> ReviewStatus {
    ReviewStatus::from_row(row.status.as_deref(), row.exit_status)
}

/// IMPURE: run one durable write against a database that may be busy, over a bounded number of
/// attempts on the review poll interval. None is "it never went through", which every caller
/// treats as a fact about the row rather than as an error.
fn retry_write<T>(
    attempts: usize,
    mut op: impl FnMut() -> agent_router_core::Result<T>,
) -> Option<T> {
    for attempt in 1..=attempts {
        match op() {
            Ok(value) => return Some(value),
            Err(_) if attempt == attempts => return None,
            Err(_) => std::thread::sleep(REVIEW_POLL),
        }
    }
    None
}

/// Settle a pending row with its terminal outcome, retrying a busy database a bounded number of
/// times. A refused compare-and-set is not an error: it means a cancel already settled the review.
fn settle_review(log: &DecisionLog, review_id: i64, outcome: &ReviewOutcome) -> bool {
    let usage_provenance =
        serde_json::to_string(&outcome.usage_provenance).unwrap_or_else(|_| "[]".to_string());
    let outcome_json = serde_json::to_string(outcome).ok();
    let body_bytes =
        i64::try_from(outcome.result.as_deref().map_or(0, str::len)).unwrap_or(i64::MAX);
    let terminal = ReviewTerminal {
        status: outcome.status.as_str(),
        exit_status: outcome.status.exit_status(),
        reviewer_provider: outcome.reviewer_provider.as_deref(),
        reviewer_model: outcome.reviewer_model.as_deref(),
        usage_provenance: &usage_provenance,
        rationale: &outcome.rationale,
        body_bytes,
        outcome_json: outcome_json.as_deref(),
        reason: outcome.reason.as_deref(),
    };
    retry_write(REVIEW_SETTLE_ATTEMPTS, || {
        log.finish_review(review_id, &terminal)
    })
    .unwrap_or(false)
}

/// A review still running in its worker, as the outcome the caller prints.
fn pending_review_outcome(review_id: i64, primary_provider: &str) -> ReviewOutcome {
    ReviewOutcome {
        status: ReviewStatus::Pending,
        primary_provider: primary_provider.to_string(),
        requested_provider: None,
        requested_model: None,
        reviewer_provider: None,
        reviewer_model: None,
        reviewer_session_id: None,
        usage: None,
        usage_provenance: Vec::new(),
        rationale: "review is still running in a detached worker".to_string(),
        reason: None,
        result: None,
        review_id: Some(review_id),
    }
}

/// A row that carries no retained envelope, as the outcome the caller prints. Every legacy row and
/// every row a pre-selection failure wrote takes this path, so their columns are all there is.
fn synthesized_review_outcome(row: &ReviewRow, state: ReviewStatus) -> ReviewOutcome {
    let reason = match (row.reason.clone(), state) {
        (Some(reason), _) => Some(reason),
        (None, ReviewStatus::Cancelled) => Some(REVIEW_CANCELLED_REASON.to_string()),
        (None, _) => None,
    };
    ReviewOutcome {
        status: state,
        primary_provider: row.primary.clone(),
        requested_provider: None,
        requested_model: None,
        reviewer_provider: row.reviewer_provider.clone(),
        reviewer_model: row.reviewer_model.clone(),
        reviewer_session_id: None,
        usage: None,
        usage_provenance: Vec::new(),
        rationale: row.rationale.clone(),
        reason,
        result: None,
        review_id: Some(row.id),
    }
}

/// Print one review from its row: the retained terminal envelope where the worker left one, a
/// synthesis of the row's own columns otherwise. Shared by the waiting caller and `review status`
/// so a review reads the same either way.
fn print_review_row(row: &ReviewRow, json: bool) -> CliStatus {
    let state = review_state(row);
    let retained = match state {
        ReviewStatus::Completed | ReviewStatus::Skipped | ReviewStatus::Failed => row
            .outcome_json
            .as_deref()
            .and_then(|envelope| serde_json::from_str::<ReviewOutcome>(envelope).ok()),
        ReviewStatus::Pending | ReviewStatus::Cancelled => None,
    };
    let outcome = retained.unwrap_or_else(|| synthesized_review_outcome(row, state));
    print_adversarial_review(&outcome, json)
}

/// Print a review by id from its row, falling back to `outcome` when the row cannot be read back.
fn print_review_id_or(
    log: &DecisionLog,
    review_id: i64,
    json: bool,
    outcome: &ReviewOutcome,
) -> CliStatus {
    match log.review(review_id) {
        Ok(Some(row)) => print_review_row(&row, json),
        _ => print_adversarial_review(outcome, json),
    }
}

fn review_command_status(ctx: &agent_router_core::Context, command: ReviewCommand) -> CliStatus {
    let log = match DecisionLog::open_in(&ctx.home) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("agent-router: {error}");
            return CliStatus::Failure;
        }
    };
    match command {
        ReviewCommand::Status { id, json } => match log.review(id) {
            Ok(Some(row)) => print_review_row(&row, json),
            Ok(None) => {
                eprintln!("agent-router: unknown review {id}");
                CliStatus::Failure
            }
            Err(error) => {
                eprintln!("agent-router: {error}");
                CliStatus::Failure
            }
        },
        ReviewCommand::Cancel { id } => match log.cancel_review(id) {
            Ok(CancelResult::Cancelled) => {
                println!("review {id} cancelled");
                CliStatus::Success
            }
            Ok(CancelResult::AlreadyTerminal(state)) => {
                eprintln!("agent-router: review {id} is already {state}");
                CliStatus::Failure
            }
            Ok(CancelResult::Unknown) => {
                eprintln!("agent-router: unknown review {id}");
                CliStatus::Failure
            }
            Err(error) => {
                eprintln!("agent-router: {error}");
                CliStatus::Failure
            }
        },
    }
}

/// The detached reviewer. Its own exit code is read by nobody: the review's result is the row it
/// settles, and its output is the worker log.
fn review_worker_status(
    ctx: &mut agent_router_core::Context,
    review_id: i64,
    body: String,
    primary: String,
    dir: PathBuf,
    provider: String,
    model: Option<String>,
) -> CliStatus {
    let log = match DecisionLog::open_in(&ctx.home) {
        Ok(log) => log,
        Err(error) => {
            eprintln!("agent-router: {error}");
            return CliStatus::Failure;
        }
    };
    // The worker re-derives the selection from the argv the caller already validated, so a pin
    // chooses the same reviewer here as it did there rather than reselecting automatically.
    let selection = review_selection(&primary, &provider, model.as_deref())
        .map_err(|error| error.reason)
        .and_then(|selection| match ctx.load_config() {
            Ok(()) => Ok(selection),
            Err(error) => Err(error.to_string()),
        });
    let mut outcome = match selection {
        Ok((primary_provider, pin)) => {
            let request = agent_router_core::adversarial_review::ReviewRequest {
                primary_provider,
                body: &body,
                dir: &dir,
            };
            // A read error is not a cancel. A transient busy database must never stop a live paid
            // review.
            let cancelled = || {
                matches!(
                    log.review(review_id),
                    Ok(Some(row)) if review_state(&row) == ReviewStatus::Cancelled
                )
            };
            agent_router_core::adversarial_review::review_registered_with_cancel(
                &request,
                pin.as_ref(),
                ctx,
                &cancelled,
            )
        }
        Err(reason) => agent_router_core::adversarial_review::failed_outcome(&primary, reason),
    };
    outcome.review_id = Some(review_id);

    // A cancel can land at any point up to the terminal write itself, so the compare-and-set is
    // the only reading of who settled the row; a snapshot taken before the write cannot be one.
    // When the write is refused the row is re-read once: a cancelled row is owed the cleanup
    // outcome as a note, and any other terminal state belongs to whoever wrote it.
    if !settle_review(&log, review_id, &outcome) {
        let cancelled_row = matches!(
            log.review(review_id),
            Ok(Some(row)) if review_state(&row) == ReviewStatus::Cancelled
        );
        if cancelled_row {
            note_cancellation_detail(&log, review_id, cancellation_detail(&outcome));
            return review_cli_status(ReviewStatus::Cancelled);
        }
    }
    review_cli_status(outcome.status)
}

/// What the worker owes a row a cancel settled ahead of it. Every failure reason other than the
/// observed cancel is provider or session-cleanup detail that exists nowhere else, so it is kept
/// verbatim rather than recognised by shape.
fn cancellation_detail(outcome: &ReviewOutcome) -> &str {
    match (outcome.status, outcome.reason.as_deref()) {
        (ReviewStatus::Failed, Some(reason)) if reason != REVIEW_CANCELLED_REASON => reason,
        (ReviewStatus::Failed, _) => "reviewer stopped",
        // A cancelled row keeps a NULL `outcome_json` by the compare-and-set, so a body that
        // arrived after the cancel is not retained; the note records only that it did.
        _ => "reviewer finished after the cancel",
    }
}

/// IMPURE: append the cleanup detail to a cancelled row, over the terminal write's retry bound. A
/// refused append is the normal outcome of a row that is no longer cancelled, not an error.
fn note_cancellation_detail(log: &DecisionLog, review_id: i64, detail: &str) {
    let _ = retry_write(REVIEW_SETTLE_ATTEMPTS, || {
        log.note_cancellation(review_id, detail)
    });
}

/// A selection the caller's argv could not produce, carrying the primary provider string the
/// failure is reported against: the caller's raw `--primary` while that is all there is, the
/// canonical name once it has parsed. That distinction is the whole reason this is a struct.
struct SelectionError {
    primary: String,
    reason: String,
}

/// PURE: the primary provider and the reviewer pin the caller's argv asks for. The waiting caller
/// and its detached worker both derive the selection here, so a pin chooses the same reviewer in
/// the worker as it did in the caller rather than reselecting automatically.
fn review_selection(
    primary: &str,
    provider: &str,
    model: Option<&str>,
) -> std::result::Result<(&'static str, Option<ReviewerPin>), SelectionError> {
    let primary_provider = match agent_router_core::run::parse_provider(primary) {
        Ok(Some(provider)) => provider.name(),
        Ok(None) => {
            return Err(SelectionError {
                primary: primary.to_string(),
                reason: "primary provider must be codex, claude, or grok".to_string(),
            });
        }
        Err(error) => {
            return Err(SelectionError {
                primary: primary.to_string(),
                reason: error.to_string(),
            });
        }
    };
    let pin = agent_router_core::run::parse_provider(provider)
        .and_then(|provider| {
            agent_router_core::adversarial_review::reviewer_pin(primary_provider, provider, model)
        })
        .map_err(|error| SelectionError {
            primary: primary_provider.to_string(),
            reason: error.to_string(),
        })?;
    Ok((primary_provider, pin))
}

/// Persist one reviews row, then print. A write failure is swallowed so it cannot change the
/// review's exit code or output.
fn finish_adversarial_review(
    outcome: &agent_router_core::adversarial_review::ReviewOutcome,
    dir: Option<&Path>,
    json: bool,
    ctx: &agent_router_core::Context,
) -> CliStatus {
    persist_adversarial_review(outcome, dir, ctx);
    print_adversarial_review(outcome, json)
}

fn persist_adversarial_review(
    outcome: &agent_router_core::adversarial_review::ReviewOutcome,
    dir: Option<&Path>,
    ctx: &agent_router_core::Context,
) {
    let exit_status = outcome.status.exit_status();
    let usage_provenance =
        serde_json::to_string(&outcome.usage_provenance).unwrap_or_else(|_| "[]".to_string());
    let body_bytes =
        i64::try_from(outcome.result.as_deref().map_or(0, str::len)).unwrap_or(i64::MAX);
    let dir = dir.unwrap_or(Path::new(""));
    let _ = DecisionLog::open_in(&ctx.home).and_then(|log| {
        log.record_review(&ReviewEntry {
            exit_status,
            primary: &outcome.primary_provider,
            reviewer_provider: outcome.reviewer_provider.as_deref(),
            reviewer_model: outcome.reviewer_model.as_deref(),
            usage_provenance: &usage_provenance,
            rationale: &outcome.rationale,
            body_bytes,
            dir,
        })
    });
}

fn print_adversarial_review(
    outcome: &agent_router_core::adversarial_review::ReviewOutcome,
    json: bool,
) -> CliStatus {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(outcome)
                .expect("serializing an adversarial review outcome cannot fail")
        );
    } else {
        match outcome.status {
            agent_router_core::adversarial_review::ReviewStatus::Completed => {
                let result = outcome.result.as_deref().unwrap_or_default();
                print!("{result}");
                if !result.ends_with('\n') {
                    println!();
                }
            }
            agent_router_core::adversarial_review::ReviewStatus::Pending => {
                let review_id = outcome.review_id.unwrap_or_default();
                println!("review {review_id} pending; poll agent-router review status {review_id}");
            }
            agent_router_core::adversarial_review::ReviewStatus::Skipped
            | agent_router_core::adversarial_review::ReviewStatus::Failed
            | agent_router_core::adversarial_review::ReviewStatus::Cancelled => {
                eprintln!(
                    "{}",
                    escape_terminal_controls(outcome.reason.as_deref().unwrap_or("review failed"))
                );
            }
        }
    }

    review_cli_status(outcome.status)
}

/// Doctor owns its exit code the same way status does: a failing check is reported by exiting
/// nonzero, not by an error, since the report itself is the output.
fn doctor_status(ctx: &agent_router_core::Context) -> CliStatus {
    let report = agent_router_core::doctor::run(ctx);
    for check in &report.checks {
        println!(
            "{:<4} {:<19} {}",
            health_label(check.health),
            check.name,
            escape_terminal_controls(&check.detail)
        );
    }
    if report.failed() {
        CliStatus::Failure
    } else {
        CliStatus::Success
    }
}

fn health_label(health: Health) -> &'static str {
    match health {
        Health::Pass => "pass",
        Health::Warn => "warn",
        Health::Fail => "fail",
    }
}

/// Status owns its exit code the way doctor does, because the report is the output: 0 when
/// nothing in the window is known to have failed, 1 when something is, and 2 when the command could
/// not run at all. An `unknown` never moves it, since an absence of information is not a finding.
fn status_status(
    ctx: &agent_router_core::Context,
    limit: usize,
    since: Option<String>,
    json: bool,
) -> CliStatus {
    let since_ms = match since.as_deref().map(agent_router_core::stats::parse_since) {
        Some(Ok(lookback)) => Some(agent_router_core::runtime::now_ms() - lookback),
        Some(Err(error)) => return status_unrunnable(&error),
        None => None,
    };
    let log = match DecisionLog::open_in(&ctx.home) {
        Ok(log) => log,
        Err(error) => return status_unrunnable(&error),
    };
    let report = match agent_router_core::status::reconcile(ctx, &log, Window { limit, since_ms }) {
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
        CliStatus::Failure
    } else {
        CliStatus::Success
    }
}

/// A command that never ran exits 2, which is a different fact from a window carrying a failure.
fn status_unrunnable(error: &agent_router_core::Error) -> CliStatus {
    eprintln!(
        "agent-router: status could not run: {}",
        escape_terminal_controls(&error.to_string())
    );
    CliStatus::Unrunnable
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

fn run(cli: Cli, ctx: &mut agent_router_core::Context) -> agent_router_core::Result<()> {
    match cli.command {
        Command::Run {
            task,
            dir,
            provider,
            model,
            effort,
            name,
            dry_run,
            mcp_configs,
            strict_mcp_config,
            json,
        } => route(
            ctx,
            task,
            dir,
            provider,
            model,
            effort,
            name,
            dry_run,
            &mcp_configs,
            strict_mcp_config,
            json,
        ),
        Command::Usage { json } => usage(ctx, json),
        Command::Log {
            limit,
            mark,
            note,
            unmarked,
            json,
        } => log(ctx, limit, &mark, note.as_deref(), unmarked, json),
        Command::Stats { limit, since, json } => stats(ctx, limit, since, json),
        Command::AdversarialReview { .. } => {
            unreachable!("adversarial review has a command specific exit path")
        }
        Command::Review { .. } => {
            unreachable!("review has a command specific exit path")
        }
        Command::ReviewWorker { .. } => {
            unreachable!("the review worker has a command specific exit path")
        }
        Command::Doctor => unreachable!("doctor has a command specific exit path"),
        Command::Status { .. } => unreachable!("status has a command specific exit path"),
    }
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

// Parameters are the run subcommand's clap flags passed straight through, so the count tracks the CLI surface.
#[allow(clippy::too_many_arguments)]
fn route(
    ctx: &mut agent_router_core::Context,
    task: String,
    dir: Option<PathBuf>,
    provider: String,
    model: Option<String>,
    effort: Option<String>,
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
    ctx.load_config()?;
    let request = Request {
        task: &task,
        dir: &dir,
        provider: agent_router_core::run::parse_provider(&provider)?,
        model,
        effort,
        name,
        dry_run,
        mcp_configs,
        strict_mcp_config,
    };
    let outcome = agent_router_core::run::run(&request, ctx)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&outcome_json(&outcome))?);
    } else {
        print_outcome(&outcome, ctx);
    }
    Ok(())
}

fn outcome_json(outcome: &Outcome) -> serde_json::Value {
    let decision = &outcome.decision;
    serde_json::json!({
        "provider": decision.provider.name(),
        "model": decision.model,
        "requested_model": decision.requested_model,
        "matched_capabilities": agent_router_core::config::MatchedCapability::encode_list(
            &decision.matched_capabilities,
        ),
        "effort": decision.effort,
        "gates": decision.gate_tags(),
        "classification": decision.classification,
        "usage": decision.usage,
        "rationale": decision.rationale,
        "dispatch": outcome.dispatch,
        "dry_run": outcome.dispatch.is_none() && outcome.capability_blocked.is_none(),
        "capability_blocked": outcome.capability_blocked,
        "log_id": outcome.log_id,
        "log_error": outcome.log_error,
        // Emitted on both paths, as null off the dry run one, so the JSON shape does not depend on
        // which path produced it.
        "estimate": outcome.estimate,
    })
}

fn print_outcome(outcome: &Outcome, ctx: &agent_router_core::Context) {
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
    if let Some(reason) = &outcome.capability_blocked {
        line.push_str(" (capability blocked, nothing dispatched)");
        println!("{line}");
        println!("why: {reason}");
        return;
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
        (Some(id), _) => println!("log: row {id} in {}", db_path(ctx)),
        // The job is running regardless, so this is a warning on stderr, not a failure.
        (None, error) => eprintln!(
            "log: NOT RECORDED in {}: {}",
            db_path(ctx),
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

fn db_path(ctx: &agent_router_core::Context) -> String {
    ctx.db_path().display().to_string()
}

fn usage(ctx: &agent_router_core::Context, json: bool) -> agent_router_core::Result<()> {
    let (snapshot, grok_source) = agent_router_core::UsageSnapshot::read_with_grok_source(ctx);
    if json {
        println!("{}", serde_json::to_string_pretty(&snapshot)?);
        return Ok(());
    }
    println!("provider  5h       weekly  source     weekly reset");
    for (name, headroom, source) in [
        (
            "claude",
            snapshot.claude,
            usage_source_label(snapshot.claude.stale),
        ),
        (
            "codex",
            snapshot.codex,
            usage_source_label(snapshot.codex.stale),
        ),
        ("grok", snapshot.grok, grok_usage_source_label(grok_source)),
    ] {
        println!(
            "{name:<9} {:>5.1}%  {:>7}  {:<9}  {}",
            headroom.five_hour_pct,
            weekly_label(&headroom),
            source,
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
    ctx: &agent_router_core::Context,
    limit: usize,
    mark: &[String],
    note: Option<&str>,
    unmarked: bool,
    json: bool,
) -> agent_router_core::Result<()> {
    if !mark.is_empty() {
        // --mark short circuits the listing, so there is no listing for --json or --unmarked to
        // shape. The combination is refused rather than accepted and ignored, on the same rule as
        // --note below: a caller passing a flag believes it did something.
        if json {
            return Err(agent_router_core::Error::Command(
                "--json cannot be combined with --mark: a mark prints one confirmation line, not \
                 a listing"
                    .to_string(),
            ));
        }
        if unmarked {
            return Err(agent_router_core::Error::Command(
                "--unmarked cannot be combined with --mark: a mark prints one confirmation line, \
                 not a listing"
                    .to_string(),
            ));
        }
        return mark_row(ctx, mark, note);
    }
    // A note with nothing to attach it to is refused rather than dropped, mirroring the rule that
    // --model requires an explicit --provider: a caller passing a note believes it recorded one.
    if note.is_some() {
        return Err(agent_router_core::Error::Command(
            "--note requires --mark: a note is the reason for one row's judgement".to_string(),
        ));
    }
    let log = DecisionLog::open_in(&ctx.home)?;
    let rows = if unmarked {
        log.recent_unmarked(limit)?
    } else {
        log.recent(limit)?
    };
    if json {
        let rows: Vec<serde_json::Value> = rows.iter().map(row_json).collect();
        println!("{}", serde_json::to_string_pretty(&rows)?);
        return Ok(());
    }
    for row in &rows {
        println!(
            "#{id} {provider}{dry} orchestration {orchestration} {complexity} \
             proj claude {claude_pace} codex {codex_pace} grok {grok_pace} \
             gates[{gates}] claude {claude:.0}% codex {codex:.0}% grok {grok} {job} \
             {outcome}{judgement}",
            id = row.id,
            provider = row.provider,
            dry = if row.dry_run { " (dry run)" } else { "" },
            complexity = row.complexity.as_deref().unwrap_or("-"),
            orchestration = flag(row.orchestration),
            claude_pace = pace(row.claude_projected_draw),
            codex_pace = pace(row.codex_projected_draw),
            grok_pace = pace(row.grok_projected_draw),
            gates = row.gates,
            claude = row.claude_weekly_pct,
            codex = row.codex_weekly_pct,
            grok = weekly(row.grok_weekly_pct),
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
fn mark_row(
    ctx: &agent_router_core::Context,
    mark: &[String],
    note: Option<&str>,
) -> agent_router_core::Result<()> {
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
    DecisionLog::open_in(&ctx.home)?.mark(id, mark, note)?;
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

fn stats(
    ctx: &agent_router_core::Context,
    limit: usize,
    since: Option<String>,
    json: bool,
) -> agent_router_core::Result<()> {
    let since_ms = match &since {
        Some(window) => {
            let lookback = agent_router_core::stats::parse_since(window)?;
            Some(agent_router_core::runtime::now_ms() - lookback)
        }
        None => None,
    };
    let log = DecisionLog::open_in(&ctx.home)?;
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
        "requested_model": row.requested_model,
        "matched_capabilities": row.matched_capabilities,
        // What the router decided, which is not what the job ran at: that is `effective_effort`,
        // null wherever no backend reported one.
        "effort": row.effort,
        "effective_effort": row.effective_effort,
        "complexity": row.complexity,
        "task_context_horizon": row.task_context_horizon,
        "orchestration": row.orchestration,
        "missing_connector": row.missing_connector,
        "claude_projected_draw": row.claude_projected_draw,
        "codex_projected_draw": row.codex_projected_draw,
        "grok_projected_draw": row.grok_projected_draw,
        "gates": row.gates,
        "claude_weekly_pct": row.claude_weekly_pct,
        "codex_weekly_pct": row.codex_weekly_pct,
        "grok_weekly_pct": row.grok_weekly_pct,
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

/// A recorded weekly percent, or "-" when the row does not know. Older rows predate the Grok
/// columns, and a row whose Grok weekly window was unread stores null rather than a sentinel.
fn weekly(value: Option<f64>) -> String {
    match value {
        Some(value) => format!("{value:.0}%"),
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

/// Grok keeps its actual capacity provenance: the router either fetched billing, read its cache,
/// recovered from the CLI log, or had no usable billing record at all.
fn grok_usage_source_label(source: agent_router_core::usage::GrokUsageSource) -> &'static str {
    match source {
        agent_router_core::usage::GrokUsageSource::Live => "live",
        agent_router_core::usage::GrokUsageSource::Cache => "cache",
        agent_router_core::usage::GrokUsageSource::Log => "log",
        agent_router_core::usage::GrokUsageSource::None => "none",
    }
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
