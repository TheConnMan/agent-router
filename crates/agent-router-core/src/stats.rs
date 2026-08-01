//! Aggregate metrics over a window of decision log rows: what routed where, which gates fired,
//! and how often a route was moved off the provider it started on.
//!
//! The fold happens in Rust over the rows one query returned, not in SQL aggregates. The `gates`
//! column is a comma joined string, so a SQL `LIKE '%tag%'` count reads a tag that is a prefix of
//! another as present on both rows, and the window this summarizes is exactly the window
//! `agent-router log` prints, so the two reconcile by construction.

use crate::error::{Error, Result};
use crate::log::{DecisionLog, StatsRow};
use std::collections::BTreeMap;

/// The gates that move a task off the provider it started on. Any new provider moving gate belongs
/// here, or the flip rate silently under reports the moment it starts firing.
///
/// `headroom_tiebreak` is retired and no decision made since carries it, but it stays in this list
/// because 45 rows already in the log do, and a report over a window that reaches back into them
/// must count the routes that really did move.
const FLIP_GATES: [&str; 4] = [
    "flipped_on_exhaustion",
    "headroom_tiebreak",
    "pace_flip",
    "five_hour_pacing",
];

/// The gate a row carries when the classifier could not answer and the default provider was used.
const CLASSIFIER_FAILED: &str = "classifier_failed";

/// What a row that was never scored counts as, so the complexity distribution still sums to the
/// row count instead of quietly losing the unscored rows.
const UNSCORED: &str = "unscored";

/// What the caller named when it asked the router to classify rather than pick a provider.
const AUTO: &str = "auto";

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
    pub auto_routes: usize,
    /// Rows whose route moved off the provider it started on, over auto routes.
    pub flip_rate: Rate,
    /// Rows whose classifier could not answer, over auto routes.
    pub classifier_failure_rate: Rate,
    /// Rows that dispatched nothing, over every row considered.
    pub dry_run_share: Rate,
}

/// PURE: fold the fetched rows into a report.
pub fn summarize(rows: &[StatsRow]) -> Stats {
    let mut routes: BTreeMap<String, usize> = BTreeMap::new();
    let mut gates: BTreeMap<String, usize> = BTreeMap::new();
    let mut complexity: BTreeMap<String, usize> = BTreeMap::new();
    let mut oldest_created_at_ms: Option<i64> = None;
    let mut newest_created_at_ms: Option<i64> = None;
    let mut auto_routes = 0;
    let mut flipped = 0;
    let mut classifier_failures = 0;
    let mut dry_runs = 0;

    for row in rows {
        *routes.entry(row.provider.clone()).or_insert(0) += 1;
        let tier = row
            .complexity
            .clone()
            .unwrap_or_else(|| UNSCORED.to_string());
        *complexity.entry(tier).or_insert(0) += 1;

        let tags = gate_tags(&row.gates);
        for tag in &tags {
            *gates.entry((*tag).to_string()).or_insert(0) += 1;
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
    }
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
