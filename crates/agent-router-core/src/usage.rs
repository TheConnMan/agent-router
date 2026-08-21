//! Weekly and 5h usage readers for both providers, Rust ports of bonus-drain's `usage.sh`
//! (Claude) and `codex-usage.sh` (Codex) with the same semantics.
//!
//! Claude fails open, but Codex fails closed: an unreadable Codex capacity source must not become
//! a dispatch target.
//!
//! Every unreadable value carries `stale = true`, while a usable parsed payload carries
//! `stale = false`. `agent-router doctor` reports that provenance and the decision log records it
//! per provider on every row.
//!
//! The flag reads freshness but means provenance, and there is one path where the two diverge:
//! `claude_headroom`'s last resort reads the shared cache regardless of its age, and a cache that
//! parses carries `stale = false` however old it is. So an expired but parseable cache, read
//! because the API was unreachable, is reported `live` rather than fail open. That is deliberate:
//! the numbers came from a real reading of the provider rather than from a default, which is the
//! distinction routing acts on, and `usage.sh` reports the same cache the same way.

use crate::runtime::{default_codex_home, home_dir};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The Claude usage cache the statusline and bonus-drain already share.
pub const CLAUDE_USAGE_CACHE_DEFAULT: &str = "/tmp/claude-usage-cache.json";
/// Points the Claude reader at a different cache. Empty or unset means the shared default.
///
/// It exists so a test can decide what Claude's usage read returns. The default is a machine wide
/// path that no fixture can unset: a test with no credentials in its temp HOME still gets a live
/// read off whatever the statusline last wrote there, so a Claude usage assertion passes on a
/// developer box and fails on a runner that has neither the cache nor credentials. That divergence
/// turned `main` red on 2026-08-06, on a merge that was green on every box it was built on.
///
/// Pointed at a path that does not exist, this reproduces a runner exactly: no cache to read, and
/// `claude_oauth_token` already finds nothing under a temp HOME, so the read fails open.
pub const CLAUDE_USAGE_CACHE_ENV: &str = "CLAUDE_USAGE_CACHE";
/// How old the shared cache may be before it is refreshed from the API.
const CACHE_MAX_AGE: Duration = Duration::from_secs(300);
/// Ceiling on the usage HTTP call, matching `usage.sh`'s `curl --max-time 6`.
const USAGE_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
/// How many newest rollouts are scanned for a `rate_limits` event.
const CODEX_SCAN_N: usize = 20;
/// `window_minutes` of the 5h window.
const WINDOW_FIVE_HOUR: i64 = 300;
/// `window_minutes` of the weekly window.
const WINDOW_WEEKLY: i64 = 10080;

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
    /// IMPURE: read both providers live.
    pub fn read() -> UsageSnapshot {
        UsageSnapshot {
            claude: claude_headroom(),
            codex: codex_headroom(),
            grok: grok_headroom(),
        }
    }

    pub const fn full() -> UsageSnapshot {
        UsageSnapshot {
            claude: Headroom::full(),
            codex: Headroom::full(),
            grok: Headroom::closed(),
        }
    }
}

// ---------------------------------------------------------------- Grok

/// IMPURE: the Grok snapshot from the official CLI billing log.
pub fn grok_headroom() -> Headroom {
    grok_headroom_in(&home_dir().join(".grok/logs/unified.jsonl"), now_epoch())
}

/// PURE except for reading `path`: the newest official weekly SuperGrok Plus billing event.
pub fn grok_headroom_in(path: &Path, now: i64) -> Headroom {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Headroom::closed();
    };
    let Some(value) = text.lines().rev().find_map(|line| {
        let value = serde_json::from_str::<serde_json::Value>(line).ok()?;
        (value.get("msg").and_then(serde_json::Value::as_str)
            == Some("billing: fetched credits config"))
        .then_some(value)
    }) else {
        return Headroom::closed();
    };
    if value
        .pointer("/ctx/subscriptionTier")
        .and_then(serde_json::Value::as_str)
        != Some("SuperGrok Plus")
        || value
            .pointer("/ctx/config/currentPeriod/type")
            .and_then(serde_json::Value::as_str)
            != Some("USAGE_PERIOD_TYPE_WEEKLY")
    {
        return Headroom::closed();
    }
    let Some(reset) = value
        .pointer("/ctx/config/currentPeriod/end")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_rfc3339_epoch)
        .filter(|reset| *reset > now)
    else {
        return Headroom::closed();
    };
    let usage = value
        .pointer("/ctx/config/creditUsagePercent")
        .and_then(serde_json::Value::as_f64)
        .filter(|usage| usage.is_finite() && (0.0..=100.0).contains(usage));
    Headroom {
        five_hour_pct: 0.0,
        five_hour_reset_epoch: 0,
        weekly_pct: usage.unwrap_or(0.0),
        weekly_reset_epoch: reset,
        weekly_capacity_known: usage.is_some(),
        stale: false,
    }
}

// ---------------------------------------------------------------- Claude

/// IMPURE: the cache path the Claude reader will use, from `CLAUDE_USAGE_CACHE` or the shared
/// default.
pub fn claude_usage_cache() -> PathBuf {
    claude_usage_cache_from(std::env::var_os(CLAUDE_USAGE_CACHE_ENV).as_deref())
}

/// PURE: the resolution rule behind `claude_usage_cache`, split out so it is testable without
/// touching the environment. Env vars are process global and Rust runs tests in threads, so a test
/// that set one would decide what a sibling test read.
pub fn claude_usage_cache_from(var: Option<&OsStr>) -> PathBuf {
    match var {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(CLAUDE_USAGE_CACHE_DEFAULT),
    }
}

/// IMPURE: the Claude snapshot, from the shared cache when it is under 5 minutes old and from
/// the OAuth usage endpoint otherwise. A stale cache is preferred over nothing; nothing at all
/// reads as full headroom.
pub fn claude_headroom() -> Headroom {
    let cache = claude_usage_cache();
    let cache = cache.as_path();
    if is_fresh(cache, CACHE_MAX_AGE)
        && let Some(headroom) = std::fs::read_to_string(cache)
            .ok()
            .as_deref()
            .and_then(parse_claude_usage)
    {
        return headroom;
    }
    if let Some(body) = fetch_claude_usage()
        && let Some(headroom) = parse_claude_usage(&body)
    {
        // Refresh the cache the statusline and bonus-drain share, exactly as `usage.sh` does.
        let _ = std::fs::write(cache, &body);
        return headroom;
    }
    // The API is unreachable: a stale cache still beats pretending nothing is known. A cached
    // window whose reset has since passed is NOT zeroed here, deliberately: `usage.sh` reports
    // the cached utilization as-is, and the two must not disagree about the same cache file.
    // The Codex reader zeroes expired windows because its source is a rollout event that can be
    // days old by design, which the 5-minute Claude cache is not.
    std::fs::read_to_string(cache)
        .ok()
        .as_deref()
        .and_then(parse_claude_usage)
        .unwrap_or_else(Headroom::full)
}

/// PURE: `five_hour` / `seven_day` utilization and resets out of the usage payload. None when
/// the body is not that payload at all (so the caller can fall back).
pub fn parse_claude_usage(body: &str) -> Option<Headroom> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    let seven_day = value.get("seven_day")?;
    let five_hour = value.get("five_hour");
    let weekly_pct = utilization(seven_day).unwrap_or(0.0);
    let weekly_reset_epoch = resets_at_epoch(seven_day).unwrap_or(0);
    Some(Headroom {
        five_hour_pct: five_hour.and_then(utilization).unwrap_or(0.0),
        five_hour_reset_epoch: five_hour.and_then(resets_at_epoch).unwrap_or(0),
        weekly_pct,
        weekly_reset_epoch,
        weekly_capacity_known: weekly_reset_epoch != 0,
        stale: false,
    })
}

fn utilization(window: &serde_json::Value) -> Option<f64> {
    window.get("utilization")?.as_f64()
}

fn resets_at_epoch(window: &serde_json::Value) -> Option<i64> {
    parse_rfc3339_epoch(window.get("resets_at")?.as_str()?)
}

/// IMPURE: GET the OAuth usage endpoint with the CLI's own credentials. Any failure is None.
fn fetch_claude_usage() -> Option<String> {
    let token = claude_oauth_token()?;
    ureq::get("https://api.anthropic.com/api/oauth/usage")
        .config()
        .timeout_global(Some(USAGE_HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("anthropic-beta", "oauth-2025-04-20")
        .header("Content-Type", "application/json")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()
}

fn claude_oauth_token() -> Option<String> {
    let path = home_dir().join(".claude/.credentials.json");
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value
        .pointer("/claudeAiOauth/accessToken")?
        .as_str()
        .map(str::to_string)
}

// ---------------------------------------------------------------- Codex

/// IMPURE: the Codex snapshot from the newest rollout carrying a `rate_limits` event.
pub fn codex_headroom() -> Headroom {
    codex_headroom_in(&codex_sessions_dir(), now_epoch(), CODEX_SCAN_N)
}

/// `$CODEX_SESSIONS_DIR` if set (the reference script's test hook), else `<codex home>/sessions`.
pub fn codex_sessions_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("CODEX_SESSIONS_DIR") {
        return PathBuf::from(dir);
    }
    default_codex_home().join("sessions")
}

/// The Codex snapshot from `sessions_dir`, scanning the `scan_n` newest rollouts newest-first.
/// A weekly window is authoritative when one is present. Credits are the capacity verdict only
/// when no weekly window was published at all. Capacity closes only when the payload supplies
/// neither a weekly window with a real reset nor a boolean credits verdict.
pub fn codex_headroom_in(sessions_dir: &Path, now: i64, scan_n: usize) -> Headroom {
    let Some(line) = newest_capacity_rate_limits_line(sessions_dir, scan_n) else {
        return Headroom::closed();
    };
    parse_codex_rate_limits(&line, now).unwrap_or_else(Headroom::closed)
}

/// IMPURE: the newest `rate_limits` line with a credits object or weekly window, scanning rollouts
/// newest-first and each rollout's own lines last-first.
fn newest_capacity_rate_limits_line(sessions_dir: &Path, scan_n: usize) -> Option<String> {
    for path in newest_rollouts(sessions_dir, scan_n) {
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(_) => continue,
        };
        let found = text
            .lines()
            .rev()
            .find(|line| line.contains("\"rate_limits\"") && carries_capacity_verdict(line));
        if let Some(line) = found {
            return Some(line.to_string());
        }
    }
    None
}

/// PURE: whether a rollout line's `rate_limits` payload can supply a capacity verdict. Parsing
/// the line twice (here, then in `parse_codex_rate_limits`) is deliberate: the alternative is a
/// half-parsed value threaded through the scan, and this runs at most `scan_n` times per read.
fn carries_capacity_verdict(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    let Some(limits) = value.pointer("/payload/rate_limits") else {
        return false;
    };
    limits
        .get("credits")
        .is_some_and(serde_json::Value::is_object)
        || window_with_minutes(limits, WINDOW_WEEKLY).is_some()
}

/// IMPURE: up to `scan_n` `*.jsonl` paths under `sessions_dir`, newest mtime first.
fn newest_rollouts(sessions_dir: &Path, scan_n: usize) -> Vec<PathBuf> {
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    collect_rollouts(sessions_dir, &mut found);
    found.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    found
        .into_iter()
        .take(scan_n)
        .map(|(_, path)| path)
        .collect()
}

fn collect_rollouts(dir: &Path, found: &mut Vec<(SystemTime, PathBuf)>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            collect_rollouts(&path, found);
        } else if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
            let mtime = entry
                .metadata()
                .and_then(|meta| meta.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            found.push((mtime, path));
        }
    }
}

/// PURE: one rollout `rate_limits` event into a snapshot.
///
/// Windows are identified by `window_minutes` (300 = 5h, 10080 = weekly) and NEVER by
/// primary/secondary position: the current prolite plan emits a weekly-only `primary` with
/// `secondary: null`, so position-based parsing reads a weekly number as a 5h one. A window
/// whose `resets_at` has already passed reports 0 percent (its stored number belongs to a past
/// window) while keeping the reset epoch.
///
/// A weekly window WINS over the `credits` object whenever one is present. `has_credits: false`
/// means no pay-as-you-go top-up balance and states nothing about the plan's included weekly
/// quota, so credits are the capacity verdict only when no weekly window was published at all.
pub fn parse_codex_rate_limits(line: &str, now: i64) -> Option<Headroom> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    let limits = value.pointer("/payload/rate_limits")?;
    if let Some(weekly) = window_with_minutes(limits, WINDOW_WEEKLY) {
        let five_hour = window_with_minutes(limits, WINDOW_FIVE_HOUR);
        let (five_hour_pct, five_hour_reset_epoch) = expire(five_hour, now);
        let (weekly_pct, weekly_reset_epoch) = expire(Some(weekly), now);
        if weekly_reset_epoch == 0 {
            return Some(Headroom::closed());
        }
        return Some(Headroom {
            five_hour_pct,
            five_hour_reset_epoch,
            weekly_pct,
            weekly_reset_epoch,
            weekly_capacity_known: true,
            stale: false,
        });
    }
    if let Some(credits) = limits.get("credits").filter(|credits| credits.is_object()) {
        return Some(
            match credits
                .get("has_credits")
                .and_then(serde_json::Value::as_bool)
            {
                Some(false) => Headroom {
                    five_hour_pct: 0.0,
                    five_hour_reset_epoch: 0,
                    weekly_pct: 100.0,
                    weekly_reset_epoch: 0,
                    weekly_capacity_known: true,
                    stale: false,
                },
                Some(true) => Headroom {
                    five_hour_pct: 0.0,
                    five_hour_reset_epoch: 0,
                    weekly_pct: 0.0,
                    weekly_reset_epoch: 0,
                    weekly_capacity_known: true,
                    stale: false,
                },
                None => Headroom::closed(),
            },
        );
    }
    Some(Headroom::closed())
}

/// PURE: the `primary`/`secondary` window whose `window_minutes` is `minutes`, if either is.
fn window_with_minutes(limits: &serde_json::Value, minutes: i64) -> Option<&serde_json::Value> {
    ["primary", "secondary"].into_iter().find_map(|key| {
        limits
            .get(key)
            .filter(|window| window.get("window_minutes").and_then(|m| m.as_i64()) == Some(minutes))
    })
}

/// PURE: (percent, reset) for a window, zeroing the percent of a window that already reset.
fn expire(window: Option<&serde_json::Value>, now: i64) -> (f64, i64) {
    let Some(window) = window else {
        return (0.0, 0);
    };
    let reset = window
        .get("resets_at")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let used = window
        .get("used_percent")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);
    if reset <= now {
        (0.0, reset)
    } else {
        (used, reset)
    }
}

// ---------------------------------------------------------------- shared helpers

/// Current time as epoch seconds (0 if the system clock predates the epoch).
pub fn now_epoch() -> i64 {
    crate::runtime::now_epoch()
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

/// PURE: an RFC 3339 timestamp into epoch seconds. Accepts the two shapes the usage payloads
/// carry, a `Z` suffix and a numeric `+HH:MM` offset, plus fractional seconds.
pub fn parse_rfc3339_epoch(timestamp: &str) -> Option<i64> {
    if timestamp.len() < 19 {
        return None;
    }
    let bytes = timestamp.as_bytes();
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year: i64 = timestamp.get(0..4)?.parse().ok()?;
    let month: i64 = timestamp.get(5..7)?.parse().ok()?;
    let day: i64 = timestamp.get(8..10)?.parse().ok()?;
    let hour: i64 = timestamp.get(11..13)?.parse().ok()?;
    let minute: i64 = timestamp.get(14..16)?.parse().ok()?;
    let second: i64 = timestamp.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let mut rest = timestamp.get(19..)?;
    if let Some(fraction) = rest.strip_prefix('.') {
        let digits = fraction
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(fraction.len());
        rest = fraction.get(digits..)?;
    }
    let offset_seconds = match rest {
        "" | "Z" | "z" => 0,
        offset => {
            let (sign, body) = offset.split_at(1);
            let sign = match sign {
                "+" => 1,
                "-" => -1,
                _ => return None,
            };
            let (hours, minutes) = body.split_once(':')?;
            sign * (hours.parse::<i64>().ok()? * 3600 + minutes.parse::<i64>().ok()? * 60)
        }
    };
    let days = days_from_civil(year, month, day);
    Some(days * 86_400 + hour * 3_600 + minute * 60 + second - offset_seconds)
}

/// PURE: days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm,
/// the civil date conversion used by the provider timestamp parsers).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let adjusted_year = year - i64::from(month <= 2);
    let era = if adjusted_year >= 0 {
        adjusted_year
    } else {
        adjusted_year - 399
    } / 400;
    let year_of_era = adjusted_year - era * 400;
    let shifted_month = month + if month > 2 { -3 } else { 9 };
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live shape of `/tmp/claude-usage-cache.json` on this box, trimmed to the fields the
    /// reader uses plus enough neighbours to prove the extra keys are ignored.
    const CLAUDE_LIVE: &str = r#"{
      "five_hour": {"utilization": 10.0, "resets_at": "2026-07-30T01:40:00.492061+00:00",
                    "limit_dollars": null, "used_dollars": null},
      "seven_day": {"utilization": 50.0, "resets_at": "2026-08-01T13:00:00.492085+00:00"},
      "seven_day_opus": null,
      "extra_usage": {"is_enabled": true, "monthly_limit": 20000},
      "limits": [{"kind": "weekly_all", "percent": 50, "is_active": true}]
    }"#;

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

    #[test]
    fn claude_usage_reads_both_windows_and_their_resets() {
        let got = parse_claude_usage(CLAUDE_LIVE).expect("the live shape parses");
        assert_eq!(got.five_hour_pct, 10.0);
        assert_eq!(got.weekly_pct, 50.0);
        // 2026-07-30T01:40:00+00:00 and 2026-08-01T13:00:00+00:00.
        assert_eq!(got.five_hour_reset_epoch, 1_785_375_600);
        assert_eq!(got.weekly_reset_epoch, 1_785_589_200);
        assert_eq!(got.weekly_remaining(), 50.0);
    }

    #[test]
    fn claude_usage_is_none_for_a_payload_that_is_not_the_usage_body() {
        // None, not a zeroed snapshot: the caller distinguishes "no usable body" (fall back to
        // the stale cache) from "parsed, and it says nothing is used".
        assert!(parse_claude_usage("{}").is_none());
        assert!(parse_claude_usage("not json").is_none());
        assert!(parse_claude_usage(r#"{"error": "unauthorized"}"#).is_none());
    }

    #[test]
    fn claude_usage_survives_a_body_missing_the_five_hour_window() {
        let got = parse_claude_usage(r#"{"seven_day": {"utilization": 33.0}}"#).expect("parses");
        assert_eq!(got.weekly_pct, 33.0);
        assert_eq!(got.five_hour_pct, 0.0);
        assert_eq!(got.five_hour_reset_epoch, 0);
    }

    #[test]
    fn rfc3339_accepts_z_and_numeric_offsets() {
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T01:40:00Z"),
            Some(1_785_375_600)
        );
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T01:40:00.492061+00:00"),
            Some(1_785_375_600)
        );
        // A non-UTC offset is applied, not ignored.
        assert_eq!(
            parse_rfc3339_epoch("2026-07-30T03:40:00+02:00"),
            Some(1_785_375_600)
        );
        assert_eq!(parse_rfc3339_epoch("nope"), None);
        assert_eq!(parse_rfc3339_epoch("2026-07-30 01:40:00Z"), None);
    }

    fn limits_line(primary: &str, secondary: &str) -> String {
        format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{primary},"secondary":{secondary}}}}}}}"#
        )
    }

    #[test]
    fn codex_windows_are_identified_by_duration_not_position() {
        let now = 1_000_000;
        let five = format!(
            r#"{{"window_minutes":300,"used_percent":21,"resets_at":{}}}"#,
            now + 3600
        );
        let weekly = format!(
            r#"{{"window_minutes":10080,"used_percent":67,"resets_at":{}}}"#,
            now + 7200
        );
        // Weekly in the primary slot and 5h in the secondary reads the same as the reverse.
        let reversed = parse_codex_rate_limits(&limits_line(&weekly, &five), now).expect("parses");
        let forward = parse_codex_rate_limits(&limits_line(&five, &weekly), now).expect("parses");
        assert_eq!(reversed, forward);
        assert_eq!(forward.five_hour_pct, 21.0);
        assert_eq!(forward.weekly_pct, 67.0);
    }

    #[test]
    fn codex_prolite_weekly_only_primary_is_not_read_as_a_five_hour_window() {
        // This box's plan: one weekly primary, secondary null. Position-based parsing would
        // report 56% of the 5h window and 0% weekly, which is the exact misread that matters.
        let now = 1_000_000;
        let weekly = format!(
            r#"{{"used_percent":56.0,"window_minutes":10080,"resets_at":{}}}"#,
            now + 3600
        );
        let got = parse_codex_rate_limits(&limits_line(&weekly, "null"), now).expect("parses");
        assert_eq!(got.weekly_pct, 56.0);
        assert_eq!(got.weekly_reset_epoch, now + 3600);
        assert_eq!(got.five_hour_pct, 0.0);
        assert_eq!(got.five_hour_reset_epoch, 0);
    }

    /// The payload a hard limited Codex actually writes, verbatim in shape: `limit_id` set,
    /// both window slots null, no credits left. Credits are the weekly capacity verdict when
    /// present, even though Codex publishes no reset alongside this exhausted state.
    #[test]
    fn exhausted_credits_close_codex_without_claiming_a_reset() {
        let now = 1_000_000;
        let line = r#"{"payload":{"rate_limits":{"limit_id":"premium","limit_name":null,"primary":null,"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":null,"rate_limit_reached_type":null}}}"#;

        let got = parse_codex_rate_limits(line, now).expect("parses");
        assert_eq!(got.weekly_pct, 100.0);
        assert_eq!(got.weekly_reset_epoch, 0);
        assert!(got.weekly_known());
        assert_ne!(got, Headroom::full());
    }

    #[test]
    fn available_credits_report_room_without_a_window_reset() {
        let now = 1_000_000;
        let line = r#"{"payload":{"rate_limits":{"primary":null,"secondary":null,"credits":{"has_credits":true,"unlimited":false,"balance":"12"}}}}"#;

        let got = parse_codex_rate_limits(line, now).expect("parses");
        assert_eq!(got.weekly_pct, 0.0);
        assert_eq!(got.weekly_remaining(), 100.0);
        assert_eq!(got.weekly_reset_epoch, 0);
        assert!(got.weekly_known());
    }

    #[test]
    fn a_window_payload_without_credits_keeps_its_weekly_capacity() {
        let now = 1_000_000;
        let weekly = format!(
            r#"{{"window_minutes":10080,"used_percent":37,"resets_at":{}}}"#,
            now + 7200
        );

        let got = parse_codex_rate_limits(&limits_line(&weekly, "null"), now).expect("parses");
        assert_eq!(got.weekly_pct, 37.0);
        assert_eq!(got.weekly_reset_epoch, now + 7200);
        assert!(got.weekly_known());
    }

    #[test]
    fn a_rate_limits_payload_without_windows_or_credits_closes_capacity() {
        let now = 1_000_000;
        let line = r#"{"payload":{"rate_limits":{"primary":null,"secondary":null}}}"#;

        let got = parse_codex_rate_limits(line, now).expect("parses");
        assert_eq!(got.weekly_pct, 100.0);
        assert!(!got.weekly_known());
        assert_ne!(got, Headroom::full());
    }

    /// The other side of that boundary. A window that has genuinely reset also reports 0 percent,
    /// and it IS a known number: it keeps its real past epoch, and the provider really does have a
    /// full week. Failing closed on this one would refuse a provider with everything available.
    #[test]
    fn a_reset_weekly_window_reports_no_usage_and_stays_known() {
        let now = 1_000_000;
        let weekly = format!(
            r#"{{"window_minutes":10080,"used_percent":99,"resets_at":{}}}"#,
            now - 7200
        );
        let got = parse_codex_rate_limits(&limits_line(&weekly, "null"), now).expect("parses");
        assert_eq!(got.weekly_pct, 0.0);
        assert!(
            got.weekly_known(),
            "a past reset is a capacity reading anyone read"
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
    fn codex_expired_windows_report_no_usage_but_keep_their_reset() {
        let now = 1_000_000;
        let five = format!(
            r#"{{"window_minutes":300,"used_percent":88,"resets_at":{}}}"#,
            now - 3600
        );
        let weekly = format!(
            r#"{{"window_minutes":10080,"used_percent":99,"resets_at":{}}}"#,
            now - 7200
        );
        let got = parse_codex_rate_limits(&limits_line(&five, &weekly), now).expect("parses");
        assert_eq!(got.five_hour_pct, 0.0);
        assert_eq!(got.weekly_pct, 0.0);
        assert_eq!(got.five_hour_reset_epoch, now - 3600);
        assert_eq!(got.weekly_reset_epoch, now - 7200);
    }

    #[test]
    fn codex_unknown_window_duration_is_ignored() {
        let now = 1_000_000;
        let unknown = format!(
            r#"{{"window_minutes":60,"used_percent":77,"resets_at":{}}}"#,
            now + 3600
        );
        let weekly = format!(
            r#"{{"window_minutes":10080,"used_percent":43,"resets_at":{}}}"#,
            now + 7200
        );
        let got = parse_codex_rate_limits(&limits_line(&unknown, &weekly), now).expect("parses");
        assert_eq!(got.five_hour_pct, 0.0);
        assert_eq!(got.weekly_pct, 43.0);
    }

    /// The live payload on this box: a real weekly window at 2 percent with a future reset, and a
    /// `has_credits: false` credits object beside it. On a Pro plan that flag means no
    /// pay-as-you-go top-up balance, which says nothing about the plan's included weekly quota, so
    /// the window wins and the credits object is ignored.
    #[test]
    fn a_real_weekly_window_beats_a_no_credits_verdict() {
        let line = r#"{"payload":{"rate_limits":{"limit_id":"codex","plan_type":"pro","primary":{"used_percent":2.0,"window_minutes":10080,"resets_at":1787040562},"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"}}}}"#;

        let got = parse_codex_rate_limits(line, 1_787_000_000).expect("parses");
        assert_eq!(got.weekly_pct, 2.0);
        assert_eq!(got.weekly_reset_epoch, 1_787_040_562);
        assert!(got.weekly_known());
    }

    #[test]
    fn codex_full_live_token_count_event_parses() {
        // The whole event as written by codex-cli, not just the rate_limits object. It carries a
        // real weekly window alongside `has_credits: false`, and the window wins over credits.
        let line = r#"{"timestamp":"2026-07-30T00:49:04.903Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":8346462}},"rate_limits":{"limit_id":"codex","limit_name":null,"primary":{"used_percent":69.0,"window_minutes":10080,"resets_at":1785908348},"secondary":null,"credits":{"has_credits":false},"plan_type":"prolite"}}}"#;
        let got = parse_codex_rate_limits(line, 1_785_000_000).expect("parses");
        assert_eq!(got.weekly_pct, 69.0);
        assert_eq!(got.weekly_reset_epoch, 1_785_908_348);
        assert!(got.weekly_known());
        assert_eq!(got.five_hour_pct, 0.0);
    }

    fn rollout(dir: &Path, name: &str, body: &str, mtime_secs: u64) {
        let path = dir.join(name);
        std::fs::write(&path, body).expect("write rollout");
        let mtime = std::fs::FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(mtime_secs));
        std::fs::File::options()
            .write(true)
            .open(&path)
            .expect("open rollout")
            .set_times(mtime)
            .expect("set mtime");
    }

    #[test]
    fn codex_scan_skips_a_newer_rollout_without_limits_and_takes_the_newest_that_has_them() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let nested = dir.path().join("2026/07/30");
        std::fs::create_dir_all(&nested).expect("nested dirs");
        let weekly = |pct: i64| {
            format!(
                r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":{pct},"resets_at":{}}},"secondary":null}}}}}}"#,
                now + 3600
            )
        };
        rollout(&nested, "older.jsonl", &weekly(23), 100);
        rollout(&nested, "newer.jsonl", &weekly(63), 200);
        rollout(
            &nested,
            "newest-no-limits.jsonl",
            "{\"payload\":{\"type\":\"started\"}}",
            300,
        );
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(got.weekly_pct, 63.0, "newest rollout WITH limits wins");
    }

    /// The `premium` bucket as codex-cli actually writes it, verbatim from a 2026-08-06 rollout.
    /// Both windows null, so it states nothing about the weekly plan.
    const PREMIUM_NO_WINDOWS: &str = r#"{"timestamp":"2026-08-06T09:36:39.958Z","type":"event_msg","payload":{"type":"token_count","info":null,"rate_limits":{"limit_id":"premium","limit_name":null,"primary":null,"secondary":null,"credits":{"has_credits":false,"unlimited":false,"balance":"0"},"individual_limit":null,"spend_control_reached":null,"plan_type":null,"rate_limit_reached_type":null}}}"#;

    /// Credits are newer than the old plan window and are the authoritative capacity verdict.
    #[test]
    fn a_newer_credits_verdict_overrides_an_older_window() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let exhausted = format!(
            r#"{{"payload":{{"rate_limits":{{"limit_id":"codex","plan_type":"pro","primary":{{"window_minutes":10080,"used_percent":100,"resets_at":{}}},"secondary":null}}}}}}"#,
            now + 3600
        );
        rollout(dir.path(), "plan.jsonl", &exhausted, 100);
        for index in 0..5 {
            rollout(
                dir.path(),
                &format!("premium-{index}.jsonl"),
                PREMIUM_NO_WINDOWS,
                200 + index,
            );
        }
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(
            got.weekly_pct, 100.0,
            "the credits verdict is what is reported"
        );
        assert_eq!(got.weekly_reset_epoch, 0);
        assert!(got.weekly_known());
    }

    /// The same shape one level down: the window-less event is the LAST line of the very rollout
    /// that also carries the plan reading, which is how codex-cli appends them in a live thread.
    #[test]
    fn a_credits_verdict_later_in_the_same_rollout_wins() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let body = format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":88,"resets_at":{}}},"secondary":null}}}}}}
{PREMIUM_NO_WINDOWS}"#,
            now + 3600
        );
        rollout(dir.path(), "mixed.jsonl", &body, 100);
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(got.weekly_pct, 100.0);
        assert_eq!(got.weekly_reset_epoch, 0);
    }

    /// Exhausted credits close capacity even when no weekly window is published.
    #[test]
    fn only_exhausted_credits_events_close_capacity() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        rollout(dir.path(), "premium.jsonl", PREMIUM_NO_WINDOWS, 100);
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(got.weekly_pct, 100.0);
        assert!(got.weekly_known());
        assert_ne!(got, Headroom::full());
    }

    #[test]
    fn codex_reader_closes_on_a_missing_directory_and_on_malformed_limits() {
        let now = 1_000_000;
        let missing = Path::new("/definitely/not/a/sessions/dir");
        assert_eq!(codex_headroom_in(missing, now, 20), Headroom::closed());

        let dir = tempfile::tempdir().expect("tempdir");
        rollout(
            dir.path(),
            "broken.jsonl",
            "{\"payload\":{\"rate_limits\":",
            100,
        );
        assert_eq!(codex_headroom_in(dir.path(), now, 20), Headroom::closed());
    }

    #[test]
    fn codex_scan_respects_the_scan_ceiling() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let weekly = format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":42,"resets_at":{}}},"secondary":null}}}}}}"#,
            now + 3600
        );
        rollout(dir.path(), "has-limits.jsonl", &weekly, 100);
        for index in 0..5 {
            rollout(
                dir.path(),
                &format!("newer-{index}.jsonl"),
                "{\"payload\":{\"type\":\"started\"}}",
                200 + index,
            );
        }
        assert_eq!(codex_headroom_in(dir.path(), now, 20).weekly_pct, 42.0);
        // Only the newest two are scanned, and neither carries capacity: closed.
        assert_eq!(codex_headroom_in(dir.path(), now, 2), Headroom::closed());
    }
}
