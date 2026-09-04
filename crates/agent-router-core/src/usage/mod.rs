//! Weekly and 5h usage readers for Claude, Codex, and Grok, following the corresponding
//! bonus-drain readers' semantics.
//!
//! Claude fails open, but Codex and Grok fail closed: an unreadable capacity source for either
//! must not become a dispatch target. See docs/decisions/0004-fail-closed-weekly-unknown.md
//! and docs/decisions/0008-grok-four-source-usage-provenance.md.
//!
//! Every unreadable value carries `stale = true`, while a usable parsed payload carries
//! `stale = false`. The flag reads freshness but means provenance, and they diverge on
//! `claude_headroom`'s last-resort cache: an expired but parseable cache is reported live
//! because the numbers came from a real reading rather than a default.

mod claude;
mod codex;
mod grok;
mod time;

use crate::context::Context;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The Claude usage cache the statusline and bonus-drain already share.
pub const CLAUDE_USAGE_CACHE_DEFAULT: &str = "/tmp/claude-usage-cache.json";
/// The Grok usage cache agent-router writes for other local consumers.
pub const GROK_USAGE_CACHE_DEFAULT: &str = "/tmp/grok-usage-cache.json";
/// Points the Claude reader at a different cache. Empty or unset means the shared default.
/// Tests must not inherit a machine-wide cache. See
/// docs/decisions/0008-grok-four-source-usage-provenance.md.
pub const CLAUDE_USAGE_CACHE_ENV: &str = "CLAUDE_USAGE_CACHE";
/// Points the Grok reader at a different cache. Empty or unset means the shared default.
///
/// Like the Claude override, this keeps tests isolated from a machine-wide cache without mutating
/// process-global environment variables in parallel tests.
pub const GROK_USAGE_CACHE_ENV: &str = "GROK_USAGE_CACHE";
/// How old the shared cache may be before it is refreshed from the API.
const CACHE_MAX_AGE: Duration = Duration::from_secs(300);
/// Ceiling on the usage HTTP call, matching `usage.sh`'s `curl --max-time 6`.
const USAGE_HTTP_TIMEOUT: Duration = Duration::from_secs(6);

/// One provider's usage snapshot: percent of each window consumed, plus when it resets.
/// Percentages are 0-100; a reset epoch of 0 means "not known".
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Headroom {
    pub five_hour_pct: f64,
    pub five_hour_reset_epoch: i64,
    pub weekly_pct: f64,
    pub weekly_reset_epoch: i64,
    /// Whether the source supplied a verdict on weekly capacity. Credits can provide that verdict
    /// without a reset timestamp, so this is distinct from `weekly_reset_epoch != 0`.
    pub weekly_capacity_known: bool,
    /// True when this is the fail open default rather than a live read. A fail open read is
    /// indistinguishable from a genuinely idle provider by its numbers alone, and an idle looking
    /// provider wins every headroom tiebreak, so the distinction is recorded rather than inferred.
    pub stale: bool,
}

impl Headroom {
    /// The fail-open value: nothing consumed, no known resets. `stale` is what makes this
    /// distinguishable from a live read of a provider that has genuinely consumed nothing, which
    /// reports the same numbers.
    pub const fn full() -> Headroom {
        Headroom {
            five_hour_pct: 0.0,
            five_hour_reset_epoch: 0,
            weekly_pct: 0.0,
            weekly_reset_epoch: 0,
            weekly_capacity_known: false,
            stale: true,
        }
    }

    /// The fail-closed Codex value: no capacity verdict means no capacity is assumed.
    pub const fn closed() -> Headroom {
        Headroom {
            five_hour_pct: 0.0,
            five_hour_reset_epoch: 0,
            weekly_pct: 100.0,
            weekly_reset_epoch: 0,
            weekly_capacity_known: false,
            stale: true,
        }
    }

    /// Weekly percent still available. The weekly window is what places a single job; Claude's 5h
    /// window is what paces a stream of them away from Claude once it is near exhausted.
    pub fn weekly_remaining(&self) -> f64 {
        100.0 - self.weekly_pct
    }

    /// Whether the source supplied a weekly capacity verdict. A reset epoch of 0 remains "not
    /// known" as a timestamp, but Codex credits can state that capacity is available or exhausted
    /// without publishing one.
    pub fn weekly_known(&self) -> bool {
        self.weekly_capacity_known
    }
}

/// Both providers' snapshots, as one routing input.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct UsageSnapshot {
    pub claude: Headroom,
    pub codex: Headroom,
    pub grok: Headroom,
}

impl UsageSnapshot {
    /// IMPURE: read all providers live.
    pub fn read(ctx: &Context) -> UsageSnapshot {
        Self::read_with_grok_source(ctx).0
    }

    /// IMPURE: read all providers once and retain the Grok snapshot's provenance for diagnostics.
    ///
    /// Codex is scanned first: `run` overlaps this read with the classifier, and the classifier's
    /// Codex rollout only gains `rate_limits` when that call completes. See
    /// docs/decisions/0008-grok-four-source-usage-provenance.md.
    pub fn read_with_grok_source(ctx: &Context) -> (UsageSnapshot, GrokUsageSource) {
        let codex = codex_headroom(ctx);
        let grok = grok_usage(ctx);
        (
            UsageSnapshot {
                claude: claude_headroom(ctx),
                codex,
                grok: grok.headroom,
            },
            grok.source,
        )
    }

    pub const fn full() -> UsageSnapshot {
        UsageSnapshot {
            claude: Headroom::full(),
            codex: Headroom::full(),
            grok: Headroom::closed(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrokUsageSource {
    Live,
    Cache,
    Log,
    None,
}

/// One Grok capacity reading together with the provenance doctor reports.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GrokUsage {
    pub headroom: Headroom,
    pub source: GrokUsageSource,
}

/// PURE: the resolution rule behind the Grok usage cache path, split out so it is testable
/// without touching the process-global environment.
pub fn grok_usage_cache_from(var: Option<&OsStr>) -> PathBuf {
    match var {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(GROK_USAGE_CACHE_DEFAULT),
    }
}

/// PURE: the resolution rule behind the Claude usage cache path, split out so it is testable
/// without touching the environment. Env vars are process global and Rust runs tests in threads,
/// so a test that set one would decide what a sibling test read.
pub fn claude_usage_cache_from(var: Option<&OsStr>) -> PathBuf {
    match var {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(CLAUDE_USAGE_CACHE_DEFAULT),
    }
}

fn is_fresh(path: &Path, max_age: Duration) -> bool {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .and_then(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .map_err(|_| std::io::Error::other("mtime is in the future"))
        })
        .map(|age| age <= max_age)
        .unwrap_or(false)
}

pub use claude::{claude_headroom, parse_claude_usage};
pub use codex::{codex_headroom, codex_headroom_in, parse_codex_rate_limits};
pub use grok::{grok_headroom, grok_headroom_in, grok_usage};
pub use time::{now_epoch, parse_rfc3339_epoch};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_usage_cache_path_prefers_the_environment_over_the_shared_default() {
        assert_eq!(
            claude_usage_cache_from(None),
            PathBuf::from(CLAUDE_USAGE_CACHE_DEFAULT),
            "with nothing set, the reader must still share bonus-drain's cache"
        );
        assert_eq!(
            claude_usage_cache_from(Some(OsStr::new("/nonexistent/usage.json"))),
            PathBuf::from("/nonexistent/usage.json"),
            "an override must win outright, including when it names a path that does not exist"
        );
        // An empty value is how a shell writes "unset" by accident (`CLAUDE_USAGE_CACHE= cmd`).
        // Taken literally it is the current working directory, which reads as a cache that never
        // parses, so the reader would fail open forever and blame the API.
        assert_eq!(
            claude_usage_cache_from(Some(OsStr::new(""))),
            PathBuf::from(CLAUDE_USAGE_CACHE_DEFAULT),
            "an empty override is unset, not a path"
        );
    }

    /// The fail open default is unknown by construction, so a provider nobody could read at all
    /// lands in the same bucket as one that reported no window.
    #[test]
    fn the_fail_open_default_is_not_a_known_weekly_capacity() {
        assert!(Headroom::full().stale);
        assert!(!Headroom::full().weekly_known());
    }

    #[test]
    fn grok_usage_cache_path_prefers_an_override_and_treats_empty_as_unset() {
        assert_eq!(
            grok_usage_cache_from(None),
            PathBuf::from(GROK_USAGE_CACHE_DEFAULT),
            "the default stays a shared /tmp path when no override is set"
        );
        assert_eq!(
            grok_usage_cache_from(Some(OsStr::new("/tmp/isolated-grok-cache.json"))),
            PathBuf::from("/tmp/isolated-grok-cache.json"),
            "an explicit test or process cache path wins"
        );
        assert_eq!(
            grok_usage_cache_from(Some(OsStr::new(""))),
            PathBuf::from(GROK_USAGE_CACHE_DEFAULT),
            "an empty environment variable must not become the working directory"
        );
    }
}
