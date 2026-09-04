//! Reconcile logged decisions against the backends that ran them: what the router dispatched
//! against what actually happened to it.
//!
//! The IMPURE layer asks the backends and produces an `Observation`, the PURE `classify` turns
//! one observation into a `State`, and the PURE `settle` decides whether that state may be
//! written over what the row already holds. See docs/decisions/0009-reconcile-monotonicity.md.

use crate::context::Context;
use crate::error::Result;
use crate::log::{DecisionLog, StatusRow};
use crate::provider::Provider;
use crate::stats::Window;
use agent_viewer_core::{GrokLifecycle, Status as GrokStatus};
use std::collections::BTreeMap;
use std::time::Duration;

/// The claude job list is one process, and the reconciler has no reason to wait longer for it than
/// dispatch waits to resolve a short id.
const AGENTS_TIMEOUT: Duration = Duration::from_secs(10);

/// What a backend said about one job, in the backend's own vocabulary. Every string is passed
/// through untranslated, so a value a backend ships tomorrow reaches `classify` as itself rather
/// than as whichever neighbour an eager mapping picked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observation {
    /// The `state` field on a `claude agents --json --all` row.
    ClaudeState(String),
    /// The job is not in the claude list. The list is a bounded recent window, so this is not
    /// completion.
    Absent,
    /// The status of the routed turn, which is turn 0 of the codex thread. Index 0 by design
    /// and never the last turn. See docs/decisions/0009-reconcile-monotonicity.md.
    CodexTurn(String),
    /// The thread's own status, read only when the thread carries no turn record at all.
    CodexThread(String),
    /// The status returned by the public Grok lifecycle for one exact session identity.
    GrokStatus(GrokStatus),
    /// More than one Grok lifecycle row carried the requested exact session identity.
    Ambiguous,
    /// We could not ask: no daemon answered, `claude` is not on PATH, or the call errored.
    Unavailable,
    /// We have no way to ask. Historical or unknown provider names have no status API.
    Unsupported,
}

impl Observation {
    /// PURE: the observation as one field of a report. The two codex variants name which record
    /// they came from, because a turn status is proof and a thread status is a fallback.
    pub fn label(&self) -> String {
        match self {
            Observation::ClaudeState(state) => state.clone(),
            Observation::Absent => "absent".to_string(),
            Observation::CodexTurn(status) => format!("turn {status}"),
            Observation::CodexThread(status) => format!("thread {status}"),
            Observation::GrokStatus(status) => match status {
                GrokStatus::Working => "working".to_string(),
                GrokStatus::NeedsInput { .. } => "needs input".to_string(),
                GrokStatus::Idle => "idle".to_string(),
                GrokStatus::Done => "done".to_string(),
                GrokStatus::Error => "error".to_string(),
                GrokStatus::Unknown => "unknown".to_string(),
            },
            Observation::Ambiguous => "ambiguous".to_string(),
            Observation::Unavailable => "unavailable".to_string(),
            Observation::Unsupported => "unsupported".to_string(),
        }
    }
}

/// What the router knows about a job. `Unknown` is an honest "the router cannot tell", never "the
/// job is fine".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    Completed,
    Failed,
    Unknown,
}

impl State {
    pub const fn tag(self) -> &'static str {
        match self {
            State::Running => "running",
            State::Completed => "completed",
            State::Failed => "failed",
            State::Unknown => "unknown",
        }
    }
}

/// One reported row: what the router asked about, what it heard, and what it made of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Reconciled {
    pub id: i64,
    pub provider: String,
    pub job_id: String,
    pub observation: Observation,
    pub state: State,
    /// Whether a claude session transcript exists for this job. `Some(false)` is a sweep that ran
    /// and found nothing, and `None` is a row that was never swept: a codex or grok row, or a
    /// sweep that could not run. Evidence about whether there is anything to go read, never a
    /// verdict.
    pub traced: Option<bool>,
    /// What the log holds for this row once the run that produced this report is done: the state
    /// just written, or the stored value where nothing was written.
    ///
    /// A string rather than a `State` because the column is TEXT and holds values no state names,
    /// `dispatched` among them. It differs from `state` above exactly where the monotonicity rule
    /// refused a write, which is the case any verdict over the window has to read this field for.
    pub persisted: String,
}

/// One reconciliation over one window.
#[derive(Debug, Clone, PartialEq)]
pub struct Report {
    pub rows: Vec<Reconciled>,
    pub oldest_created_at_ms: Option<i64>,
    pub newest_created_at_ms: Option<i64>,
}

impl Report {
    /// PURE: whether any job in the window is known to have failed, read from what the log holds
    /// rather than from the fresh reading alone. An unknown never counts. See
    /// docs/decisions/0009-reconcile-monotonicity.md.
    pub fn failed(&self) -> bool {
        self.rows
            .iter()
            .any(|row| row.persisted == State::Failed.tag())
    }
}

/// PURE: the state one observation proves. This is the whole mapping table and it invents nothing.
///
/// It deliberately takes the observation and nothing else. The transcript sweep is not a parameter
/// here, so no expression exists in which a file on disk could move a verdict, and wiring one in
/// would take a visible signature change rather than a quiet edit.
pub fn classify(observation: Observation) -> State {
    match observation {
        Observation::ClaudeState(state) => match state.as_str() {
            "working" => State::Running,
            "done" => State::Completed,
            // An operator killing a healthy job, or stopping one that had already produced what was
            // needed, is not a routing failure, and `stopped` says nothing about whether the work
            // succeeded.
            _ => State::Unknown,
        },
        Observation::CodexTurn(status) => match status.as_str() {
            "completed" => State::Completed,
            "failed" => State::Failed,
            "inProgress" => State::Running,
            // `interrupted` ended early, which is unknowable rather than failed, and an unrecognized
            // status must not be bucketed into a neighbour.
            _ => State::Unknown,
        },
        // The fallback, reached only with no turn record. The daemon reporting that the thread
        // itself faulted is a fault of the dispatch; every other thread status, `notLoaded`, `idle`,
        // and `active` alike, proves nothing without a turn.
        Observation::CodexThread(status) => match status.as_str() {
            "systemError" => State::Failed,
            _ => State::Unknown,
        },
        Observation::GrokStatus(status) => match status {
            GrokStatus::Working => State::Running,
            GrokStatus::Done => State::Completed,
            GrokStatus::Error => State::Failed,
            GrokStatus::NeedsInput { .. } | GrokStatus::Idle | GrokStatus::Unknown => {
                State::Unknown
            }
        },
        Observation::Absent
        | Observation::Ambiguous
        | Observation::Unavailable
        | Observation::Unsupported => State::Unknown,
    }
}

/// PURE: the state to write over `current`, or None to leave the row alone.
///
/// `unknown` writes over a whitelist of exactly three values (`dispatched`, `running`,
/// `unknown`); every other stored outcome is kept. Writing `unknown` over `unknown` refreshes
/// `reconciled_at_ms`. `completed` → `running` is permitted: a live backend is fresh evidence.
/// See docs/decisions/0009-reconcile-monotonicity.md.
pub fn settle(current: &str, observed: State) -> Option<State> {
    match observed {
        State::Unknown => match current {
            "dispatched" | "running" | "unknown" => Some(State::Unknown),
            _ => None,
        },
        proven => Some(proven),
    }
}

/// PURE: assemble one reported row. `state` comes from `classify(observation)` alone, and `traced`
/// rides beside it rather than into it. `written` is the state this reconciliation stores, or None
/// where it stores nothing, so `persisted` names what the log holds either way.
fn report_row(
    row: &StatusRow,
    observation: Observation,
    traced: Option<bool>,
    written: Option<State>,
) -> Reconciled {
    Reconciled {
        id: row.id,
        provider: row.provider.clone(),
        job_id: row.job_id.clone(),
        state: classify(observation.clone()),
        observation,
        traced,
        persisted: match written {
            Some(state) => state.tag().to_string(),
            None => row.outcome.clone(),
        },
    }
}

/// IMPURE: read the window, ask each backend about the jobs in it, and record what came back.
///
/// A backend that does not answer costs its own rows their reconciliation and nothing else: the
/// report is partial rather than absent, which is the useful output when one provider is down.
pub fn reconcile(ctx: &Context, log: &DecisionLog, window: Window) -> Result<Report> {
    let rows = log.status_rows(window.limit, window.since_ms)?;
    let claude = claude_states(ctx, &rows);
    let codex = codex_states(ctx, &rows);
    let grok = grok_states(ctx, &rows);
    let transcripts = rows
        .iter()
        .any(|row| row.provider == "claude")
        .then(|| list_claude_transcripts(&ctx.claude_projects()));

    let mut reported = Vec::with_capacity(rows.len());
    for row in &rows {
        let observation = match row.provider.as_str() {
            "claude" => match &claude {
                Some(states) => match states.get(&row.job_id) {
                    Some(state) => Observation::ClaudeState(state.clone()),
                    None => Observation::Absent,
                },
                None => Observation::Unavailable,
            },
            "codex" => codex
                .get(&row.job_id)
                .cloned()
                .unwrap_or(Observation::Unavailable),
            "grok" => match &grok {
                Some(states) => states
                    .get(&row.job_id)
                    .cloned()
                    .unwrap_or(Observation::Absent),
                None => Observation::Unavailable,
            },
            _ => Observation::Unsupported,
        };
        // Selected by the row's own provider column, never by the shape of the id string, so a
        // codex thread id that happens to look like a short id is never swept for.
        let traced = match row.provider.as_str() {
            "claude" => transcripts.as_ref().and_then(|names| {
                names
                    .as_ref()
                    .map(|names| transcript_exists(names, &row.job_id))
            }),
            _ => None,
        };
        // A provider the reconciler cannot ask about is never offered to `settle`.
        // `Unsupported` classifies `Unknown`, which would overwrite `dispatched`. See
        // docs/decisions/0009-reconcile-monotonicity.md.
        let written = match observation {
            Observation::Unsupported => None,
            _ => settle(&row.outcome, classify(observation.clone())),
        };
        if let Some(state) = written {
            log.reconcile(row.id, state.tag())?;
        }
        reported.push(report_row(row, observation, traced, written));
    }

    Ok(Report {
        oldest_created_at_ms: rows.iter().map(|row| row.created_at_ms).min(),
        newest_created_at_ms: rows.iter().map(|row| row.created_at_ms).max(),
        rows: reported,
    })
}

/// IMPURE: the claude job list, or None when the router could not read it. One call serves the
/// whole window, and a window holding no claude row never runs `claude` at all.
fn claude_states(ctx: &Context, rows: &[StatusRow]) -> Option<BTreeMap<String, String>> {
    if !rows.iter().any(|row| row.provider == "claude") {
        return None;
    }
    crate::dispatch::claude::agent_states(ctx, AGENTS_TIMEOUT).ok()
}

/// IMPURE: what the app-server knows about the codex threads in the window. Empty when there are
/// none, so a window with no codex row never even probes for a daemon.
fn codex_states(ctx: &Context, rows: &[StatusRow]) -> BTreeMap<String, Observation> {
    let thread_ids: Vec<String> = rows
        .iter()
        .filter(|row| row.provider == "codex")
        .map(|row| row.job_id.clone())
        .collect();
    if thread_ids.is_empty() {
        return BTreeMap::new();
    }
    crate::dispatch::codex::thread_states(ctx, &thread_ids)
}

/// IMPURE: one public lifecycle listing serves every Grok row in the window. Exact duplicate
/// identities are ambiguous rather than whichever row happened to be listed last.
fn grok_states(ctx: &Context, rows: &[StatusRow]) -> Option<BTreeMap<String, Observation>> {
    if !rows.iter().any(|row| row.provider == "grok") {
        return None;
    }
    // Resolved rather than named: a bare "grok" here would keep reaching `execvp` with whatever
    // `PATH` the caller inherited, so every Grok row in the window would read as unresolvable on
    // exactly the boxes dispatch was just taught to handle. Reconciliation only observes, so an
    // unresolvable grok is `None` — the honest partial report — not a hard failure.
    let binary = crate::binary::resolve(Provider::Grok, &ctx.environment).ok()?;
    let sessions = GrokLifecycle::new(binary, ctx.grok_home()).list().ok()?;
    let mut states = BTreeMap::new();
    for session in sessions {
        use std::collections::btree_map::Entry;
        match states.entry(session.id) {
            Entry::Vacant(entry) => {
                entry.insert(Observation::GrokStatus(session.status));
            }
            Entry::Occupied(mut entry) => {
                entry.insert(Observation::Ambiguous);
            }
        }
    }
    Some(states)
}

/// IMPURE: list session transcript file names under `~/.claude/projects` once per report.
///
/// None when the sweep could not run at all, which is a different fact from finding nothing.
fn list_claude_transcripts(projects_dir: &std::path::Path) -> Option<Vec<String>> {
    let projects = std::fs::read_dir(projects_dir).ok()?;
    let mut names = Vec::new();
    for project in projects.flatten() {
        let Ok(transcripts) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for transcript in transcripts.flatten() {
            names.push(transcript.file_name().to_string_lossy().into_owned());
        }
    }
    Some(names)
}

/// PURE: whether a claude session left a transcript on disk.
///
/// The hyphen anchors the match to the end of a session UUID's first segment, so a short id that
/// is a prefix of another cannot match it. The transcript is never opened: existence is the
/// entire signal. See docs/decisions/0009-reconcile-monotonicity.md.
fn transcript_exists(names: &[String], short_id: &str) -> bool {
    let prefix = format!("{short_id}-");
    names
        .iter()
        .any(|name| name.starts_with(&prefix) && name.ends_with(".jsonl"))
}
