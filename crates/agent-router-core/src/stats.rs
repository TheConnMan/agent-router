//! Aggregate metrics over a window of decision log rows: what routed where, which gates fired,
//! and how often a route was moved off the provider it started on.
//!
//! The fold happens in Rust over the rows one query returned, not in SQL aggregates. The `gates`
//! column is a comma joined string, so a SQL `LIKE '%tag%'` count reads a tag that is a prefix of
//! another as present on both rows, and the window this summarizes is exactly the window
//! `agent-router log` prints, so the two reconcile by construction.

use crate::error::{Error, Result};
use crate::log::{DecisionLog, Mark, StatsRow};
use crate::status::State;
use std::collections::BTreeMap;

/// The gates that move a task off the provider it started on. Any new provider moving gate belongs
/// here, or the flip rate silently under reports the moment it starts firing.
///
/// `headroom_tiebreak` is retired and no decision made since carries it, but it stays in this list
/// because 45 rows already in the log do, and a report over a window that reaches back into them
/// must count the routes that really did move.
/// `pace_flip` is retired too, and unlike `headroom_tiebreak` it is not known to have ever fired:
/// its threshold was calibrated against plan sizes that changed underneath it, and a count over
/// every row this box has logged returns zero. It stays listed anyway, because the cost of a gate
/// that matches nothing is nothing, and the cost of omitting one that some older database does
/// carry is a flip rate that under reports without saying so.
const FLIP_GATES: [&str; 5] = [
    "flipped_on_exhaustion",
    "headroom_tiebreak",
    "pace_flip",
    "projected_overdraw",
    "five_hour_pacing",
];

/// The gate a row carries when the classifier could not answer and the default provider was used.
const CLASSIFIER_FAILED: &str = "classifier_failed";

/// What a row that was never scored counts as, so the complexity distribution still sums to the
/// row count instead of quietly losing the unscored rows.
const UNSCORED: &str = "unscored";

/// What a row written before the `router_version` column counts as, so the version distribution
/// still sums to the row count instead of quietly losing the rows that predate the column. Not the
/// same claim as a row that carries a genuinely empty version string, which cannot happen since the
/// value is always `CARGO_PKG_VERSION`, but the count still has to name the bucket rather than drop
/// the row.
const UNKNOWN_ROUTER_VERSION: &str = "unknown";

/// What the caller named when it asked the router to classify rather than pick a provider.
const AUTO: &str = "auto";

/// The prefix `run` writes into `outcome` when the dispatch itself failed. Nothing reconciles such
/// a row, because it never produced a job, but the router watched that dispatch fail, so its fate
/// is settled and it is a failure.
const DISPATCH_ERROR: &str = "error: ";

const HOUR_MS: i64 = 60 * 60 * 1_000;

/// The window a report covers.
#[derive(Debug, Clone, Copy)]
pub struct Window {
    /// The most rows to read, newest first.
    pub limit: usize,
    /// The oldest `created_at_ms` a row may carry, when the window is also bounded by age.
    pub since_ms: Option<i64>,
}

/// One counted metric, carrying its own denominator so a rate can be checked by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rate {
    pub numerator: usize,
    pub denominator: usize,
}

impl Rate {
    /// PURE: the share, or None when nothing was counted. None rather than 0.0, because a zero
    /// share reads as "this never happened" when the truth is that there is no data, and rather
    /// than NaN, which is not valid JSON.
    pub fn share(&self) -> Option<f64> {
        if self.denominator == 0 {
            return None;
        }
        Some(self.numerator as f64 / self.denominator as f64)
    }
}

/// The report over one window.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    pub rows_considered: usize,
    pub oldest_created_at_ms: Option<i64>,
    pub newest_created_at_ms: Option<i64>,
    /// Provider name to the number of decisions that landed there.
    pub routes: BTreeMap<String, usize>,
    /// Gate tag to the number of rows carrying it.
    pub gates: BTreeMap<String, usize>,
    /// Complexity tier to its count, with unscored rows counted as `unscored`.
    pub complexity: BTreeMap<String, usize>,
    /// Router build to the count of rows carrying it, with rows written before the column counted
    /// as `unknown`. The whole point of the column: an aggregate spanning several of these keys is
    /// visibly mixed rather than pooled as one population.
    pub router_versions: BTreeMap<String, usize>,
    pub auto_routes: usize,
    /// Rows whose route moved off the provider it started on, over auto routes.
    pub flip_rate: Rate,
    /// Rows whose classifier could not answer, over auto routes.
    pub classifier_failure_rate: Rate,
    /// Rows that dispatched nothing, over every row considered.
    pub dry_run_share: Rate,
    /// Gate tag to the rows carrying it that a human marked `bad` or `rerouted`, over the rows
    /// carrying it that a human marked at all. `good` is the only mark left out of the numerator,
    /// because the other two both say the route was wrong. A row carrying several tags counts under
    /// each of them.
    pub bad_rate_by_gate: BTreeMap<String, Rate>,
    /// Provider to the same rate, over the marked rows that landed there.
    pub bad_rate_by_provider: BTreeMap<String, Rate>,
    /// Complexity tier to the same rate, with unscored rows counted as `unscored`.
    pub bad_rate_by_complexity: BTreeMap<String, Rate>,
    /// Gate tag to the rows carrying it that settled as a failure, over the rows carrying it whose
    /// fate settled at all. A dry run and a job nobody has heard back about are in neither.
    pub failure_rate_by_gate: BTreeMap<String, Rate>,
    /// Provider to the same rate, over the settled rows that landed there.
    pub failure_rate_by_provider: BTreeMap<String, Rate>,
    /// Complexity tier to the same rate, with unscored rows counted as `unscored`.
    pub failure_rate_by_complexity: BTreeMap<String, Rate>,
}

/// PURE: fold the fetched rows into a report.
pub fn summarize(rows: &[StatsRow]) -> Stats {
    let mut routes: BTreeMap<String, usize> = BTreeMap::new();
    let mut gates: BTreeMap<String, usize> = BTreeMap::new();
    let mut complexity: BTreeMap<String, usize> = BTreeMap::new();
    let mut router_versions: BTreeMap<String, usize> = BTreeMap::new();
    let mut oldest_created_at_ms: Option<i64> = None;
    let mut newest_created_at_ms: Option<i64> = None;
    let mut auto_routes = 0;
    let mut flipped = 0;
    let mut classifier_failures = 0;
    let mut dry_runs = 0;
    let mut bad_rate_by_gate: BTreeMap<String, Rate> = BTreeMap::new();
    let mut bad_rate_by_provider: BTreeMap<String, Rate> = BTreeMap::new();
    let mut bad_rate_by_complexity: BTreeMap<String, Rate> = BTreeMap::new();
    let mut failure_rate_by_gate: BTreeMap<String, Rate> = BTreeMap::new();
    let mut failure_rate_by_provider: BTreeMap<String, Rate> = BTreeMap::new();
    let mut failure_rate_by_complexity: BTreeMap<String, Rate> = BTreeMap::new();

    for row in rows {
        *routes.entry(row.provider.clone()).or_insert(0) += 1;
        let tier = row
            .complexity
            .clone()
            .unwrap_or_else(|| UNSCORED.to_string());
        *complexity.entry(tier.clone()).or_insert(0) += 1;
        let version = row
            .router_version
            .clone()
            .unwrap_or_else(|| UNKNOWN_ROUTER_VERSION.to_string());
        *router_versions.entry(version).or_insert(0) += 1;

        let tags = gate_tags(&row.gates);
        for tag in &tags {
            *gates.entry((*tag).to_string()).or_insert(0) += 1;
        }

        // Each row contributes to one provider key, one complexity key, and every gate key it
        // carries, in both families. The entry is created whichever population the row is in, so a
        // key nobody has judged and a key nothing has settled both stay visible as zero of zero.
        let bad = marked_bad(row);
        let failure = settled_as_failure(row);
        count(&mut bad_rate_by_provider, &row.provider, bad);
        count(&mut failure_rate_by_provider, &row.provider, failure);
        count(&mut bad_rate_by_complexity, &tier, bad);
        count(&mut failure_rate_by_complexity, &tier, failure);
        for tag in &tags {
            count(&mut bad_rate_by_gate, tag, bad);
            count(&mut failure_rate_by_gate, tag, failure);
        }

        if row.dry_run {
            dry_runs += 1;
        }

        // An explicitly routed row never ran a usage rule and never ran the classifier, so it
        // belongs in neither of those denominators.
        if row.requested == AUTO {
            auto_routes += 1;
            // A route that moved counts once however many provider moving gates fired on it: the
            // task moved once.
            if tags.iter().any(|tag| FLIP_GATES.contains(tag)) {
                flipped += 1;
            }
            if tags.contains(&CLASSIFIER_FAILED) {
                classifier_failures += 1;
            }
        }

        oldest_created_at_ms = Some(match oldest_created_at_ms {
            Some(current) => current.min(row.created_at_ms),
            None => row.created_at_ms,
        });
        newest_created_at_ms = Some(match newest_created_at_ms {
            Some(current) => current.max(row.created_at_ms),
            None => row.created_at_ms,
        });
    }

    Stats {
        rows_considered: rows.len(),
        oldest_created_at_ms,
        newest_created_at_ms,
        routes,
        gates,
        complexity,
        router_versions,
        auto_routes,
        flip_rate: Rate {
            numerator: flipped,
            denominator: auto_routes,
        },
        classifier_failure_rate: Rate {
            numerator: classifier_failures,
            denominator: auto_routes,
        },
        dry_run_share: Rate {
            numerator: dry_runs,
            denominator: rows.len(),
        },
        bad_rate_by_gate,
        bad_rate_by_provider,
        bad_rate_by_complexity,
        failure_rate_by_gate,
        failure_rate_by_provider,
        failure_rate_by_complexity,
    }
}

/// PURE: add one row's contribution to the rate under `key`.
///
/// The entry is created even when the row is in no population, because a key that vanishes from a
/// breakdown reads as a key nobody routed to, where the truth is a key nobody has judged. `hit` is
/// None for a row outside this rate's denominator entirely.
fn count(rates: &mut BTreeMap<String, Rate>, key: &str, hit: Option<bool>) {
    let rate = rates.entry(key.to_string()).or_default();
    let Some(hit) = hit else {
        return;
    };
    rate.denominator += 1;
    if hit {
        rate.numerator += 1;
    }
}

/// PURE: whether a human judged this row's route wrong, or None when nobody judged it at all.
///
/// `bad` and `rerouted` both count, and `good` is the only mark that does not. Both of the first
/// two record that routing the task to that provider was the wrong call; `rerouted` additionally
/// records that the human had to move the job off it. Counting `rerouted` as not wrong put it in
/// the denominator and never in the numerator, so every rerouted decision made routing look better
/// and the bad rate understated routing errors the more rerouting was recorded.
///
/// An unmarked row is absence of evidence, not evidence of a good route, so it stays out of the
/// denominator. Counting it as good would drive every bad rate toward zero as the log grows, which
/// is exactly as the log becomes worth reading.
fn marked_bad(row: &StatsRow) -> Option<bool> {
    row.mark
        .as_deref()
        .map(|mark| mark == Mark::Bad.tag() || mark == Mark::Rerouted.tag())
}

/// PURE: whether this row settled as a failure, or None when its fate is not settled.
///
/// `dispatched` is the value written at dispatch time, `running` is a live job, and `unknown` is
/// reconciliation reporting that it could not tell, so none of the three has been shown to have
/// succeeded and counting any of them as one reports a perfect record over jobs nobody has heard
/// back about. An outcome a later router invents lands here too, in neither count rather than
/// guessed into a neighbour. A dry run is excluded on its flag rather than on its outcome, because
/// it dispatched nothing that could succeed or fail.
fn settled_as_failure(row: &StatsRow) -> Option<bool> {
    if row.dry_run {
        return None;
    }
    let outcome = row.outcome.as_str();
    if outcome == State::Failed.tag() || outcome.starts_with(DISPATCH_ERROR) {
        return Some(true);
    }
    if outcome == State::Completed.tag() {
        return Some(false);
    }
    None
}

/// IMPURE: fetch the window and summarize it.
pub fn collect(log: &DecisionLog, window: Window) -> Result<Stats> {
    let rows = log.stats_rows(window.limit, window.since_ms)?;
    Ok(summarize(&rows))
}

/// PURE: "7d", "24h", or "2w" as a millisecond lookback. A bare number names no unit and a
/// negative one would put the floor in the future, so both are errors rather than a silently
/// different window.
pub fn parse_since(value: &str) -> Result<i64> {
    let mut characters = value.chars();
    let unit = characters.next_back().ok_or_else(|| rejected(value))?;
    let multiplier = match unit {
        'h' => HOUR_MS,
        'd' => 24 * HOUR_MS,
        'w' => 7 * 24 * HOUR_MS,
        _ => return Err(rejected(value)),
    };
    let count: u32 = characters.as_str().parse().map_err(|_| rejected(value))?;
    Ok(i64::from(count) * multiplier)
}

/// PURE: the reason a window string was not accepted.
fn rejected(value: &str) -> Error {
    Error::Command(format!(
        "invalid --since window {value:?}: expected a count and a unit, for example 24h, 7d, or 2w"
    ))
}

/// PURE: the tags on one row. The column is a comma joined string, and an empty one is no gate at
/// all rather than a tag whose name is empty.
fn gate_tags(gates: &str) -> Vec<&str> {
    gates.split(',').filter(|tag| !tag.is_empty()).collect()
}
