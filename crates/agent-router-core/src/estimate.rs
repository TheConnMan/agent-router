//! The dry run projection: what one job is likely to draw from its provider's weekly window,
//! derived from the decision log's own history and reported as an upper bound.
//!
//! The number is a bound rather than a measurement, and the wording everywhere it is rendered says
//! so. The step between two consecutive decisions is the provider's total weekly movement over that
//! whole interval, so it also carries that provider's jobs on other models, every interactive
//! session on this box, and anything dispatched without going through the router. That is the only
//! figure the log can support: it records percentages at decision time, not per job consumption.

use crate::decide::Decision;
use crate::error::Result;
use crate::log::DecisionLog;

/// How many observed draws the projection needs before it will answer with a number. Below this it
/// reports the shortfall instead, because a median over one or two gaps is noise wearing a number.
const MIN_SAMPLES: usize = 3;

/// How many recent comparable rows the series is read over. The median wants recent draws, not the
/// whole history: an older working week describes a different pattern of jobs.
const SERIES_LIMIT: usize = 50;

/// An upper bound on the weekly draw of one job on the decided provider and model. Derived from the
/// provider's total weekly movement between comparable decisions, so it also carries whatever else
/// consumed that provider's quota over the same interval.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Estimate {
    pub provider: &'static str,
    /// The tier the sample key is narrowed to. The model is what the row records and what a pinned
    /// invocation names, so it is the comparable, not the complexity that chose it.
    pub model: Option<String>,
    /// The number of positive steps the median was taken over, which is what `draws` returned. The
    /// count the median came from is the only count worth printing beside it.
    pub samples: usize,
    /// The `samples` the projection needs before it answers, reported so a short sample explains
    /// itself rather than looking like a failure.
    pub needed: usize,
    /// The upper bound, in weekly percent. None when `samples < needed`. Never a fabricated number.
    pub weekly_pct: Option<f64>,
}

/// PURE: the positive step between consecutive observations. A non positive step is dropped: it is
/// a weekly window that reset between the two rows, not a job that drew nothing. Folding it in as
/// its magnitude would invent a draw out of a reset.
pub fn draws(series: &[f64]) -> Vec<f64> {
    series
        .windows(2)
        .map(|pair| pair[1] - pair[0])
        .filter(|step| *step > 0.0)
        .collect()
}

/// PURE: the middle value of a sorted copy, averaging the two middles for an even count. A median
/// rather than a mean, because with a handful of samples one long job would carry a mean away.
pub fn median(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|left, right| left.total_cmp(right));
    let middle = sorted.len() / 2;
    if sorted.len().is_multiple_of(2) {
        Some((sorted[middle - 1] + sorted[middle]) / 2.0)
    } else {
        Some(sorted[middle])
    }
}

/// IMPURE: the estimate for a decision, from the log's own history.
///
/// The sample key is (provider, model) and there is no fallback tier: widening the key on a miss
/// would produce a number that quietly answers a different question.
pub fn project(log: &DecisionLog, decision: &Decision) -> Result<Estimate> {
    let provider = decision.provider.name();
    let model = decision.model.clone();
    // A provider the router names no model for keeps no weekly series of its own to sample, so the
    // estimate is short of data rather than taken over another key's rows.
    let series = match model.as_deref() {
        Some(model) => log.weekly_series(provider, model, SERIES_LIMIT)?,
        None => Vec::new(),
    };
    let observed = draws(&series);
    let samples = observed.len();
    let weekly_pct = if samples >= MIN_SAMPLES {
        median(&observed)
    } else {
        None
    };
    Ok(Estimate {
        provider,
        model,
        samples,
        needed: MIN_SAMPLES,
        weekly_pct,
    })
}
