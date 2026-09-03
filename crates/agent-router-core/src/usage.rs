//! Weekly and 5h usage readers for Claude, Codex, and Grok, following the corresponding
//! bonus-drain readers' semantics.
//!
//! Claude fails open, but Codex and Grok fail closed: an unreadable capacity source for either
//! must not become a dispatch target.
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
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The Claude usage cache the statusline and bonus-drain already share.
pub const CLAUDE_USAGE_CACHE_DEFAULT: &str = "/tmp/claude-usage-cache.json";
/// The Grok usage cache agent-router writes for other local consumers.
pub const GROK_USAGE_CACHE_DEFAULT: &str = "/tmp/grok-usage-cache.json";
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
/// Points the Grok reader at a different cache. Empty or unset means the shared default.
///
/// Like the Claude override, this keeps tests isolated from a machine-wide cache without mutating
/// process-global environment variables in parallel tests.
pub const GROK_USAGE_CACHE_ENV: &str = "GROK_USAGE_CACHE";
/// How old the shared cache may be before it is refreshed from the API.
const CACHE_MAX_AGE: Duration = Duration::from_secs(300);
/// Ceiling on the usage HTTP call, matching `usage.sh`'s `curl --max-time 6`.
const USAGE_HTTP_TIMEOUT: Duration = Duration::from_secs(6);
/// How many newest rollouts are scanned for a `rate_limits` event.
const CODEX_SCAN_N: usize = 20;
/// One fixed-size block used while walking a Codex rollout backwards. Memory grows only with the
/// longest individual line, not with the size of the rollout.
const CODEX_ROLLOUT_READ_BYTES: usize = 64 * 1024;
/// `window_minutes` of the 5h window.
const WINDOW_FIVE_HOUR: i64 = 300;
/// `window_minutes` of the weekly window.
const WINDOW_WEEKLY: i64 = 10080;
/// One fixed-size block used while walking the Grok log backwards. Memory grows only with the
/// longest individual line, not with the size of the log.
const GROK_LOG_READ_BYTES: usize = 64 * 1024;
/// Maximum normalized Grok cache body accepted from the shared `/tmp` path.
const GROK_USAGE_CACHE_MAX_BYTES: usize = 64 * 1024;
/// Maximum one-line Grok log event retained while scanning backwards.
const GROK_LOG_LINE_MAX_BYTES: usize = 1024 * 1024;
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const GROK_USER_URL: &str = "https://cli-chat-proxy.grok.com/v1/user?include=subscription";

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
    pub fn read() -> UsageSnapshot {
        Self::read_with_grok_source().0
    }

    /// IMPURE: read all providers once and retain the Grok snapshot's provenance for diagnostics.
    pub fn read_with_grok_source() -> (UsageSnapshot, GrokUsageSource) {
        let grok = grok_usage();
        (
            UsageSnapshot {
                claude: claude_headroom(),
                codex: codex_headroom(),
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

// ---------------------------------------------------------------- Grok

/// Where agent-router obtained a usable Grok capacity reading.
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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct GrokUsageCache {
    tier: String,
    weekly_percent: f64,
    weekly_reset: i64,
    source_ts: String,
}

/// IMPURE: the cache path the Grok reader will use, from `GROK_USAGE_CACHE` or the shared default.
pub fn grok_usage_cache() -> PathBuf {
    grok_usage_cache_from(std::env::var_os(GROK_USAGE_CACHE_ENV).as_deref())
}

/// PURE: the resolution rule behind `grok_usage_cache`, split out so it is testable without
/// touching the process-global environment.
pub fn grok_usage_cache_from(var: Option<&OsStr>) -> PathBuf {
    match var {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => PathBuf::from(GROK_USAGE_CACHE_DEFAULT),
    }
}

/// IMPURE: one Grok capacity read, retaining whether it came from live billing, cache, log, or no
/// usable source. A fresh cache avoids the provider calls; a stale cache remains the fallback when
/// the calls fail.
pub fn grok_usage() -> GrokUsage {
    let grok_home = crate::dispatch::grok::grok_home();
    let cache = grok_usage_cache();
    grok_usage_in(
        &cache,
        &grok_home.join("logs/unified.jsonl"),
        now_epoch(),
        is_fresh(&cache, CACHE_MAX_AGE),
        || fetch_grok_usage(&grok_home),
    )
}

/// IMPURE: the Grok headroom from the read-through cache and provider-owned fallbacks.
pub fn grok_headroom() -> Headroom {
    grok_usage().headroom
}

/// IMPURE only through the supplied paths and fetch function: resolve Grok capacity in the exact
/// fresh cache, live fetch, stale cache, full log, closed order used by production.
fn grok_usage_in<F>(
    cache_path: &Path,
    log_path: &Path,
    now: i64,
    cache_is_fresh: bool,
    fetch: F,
) -> GrokUsage
where
    F: FnOnce() -> Option<(String, String)>,
{
    let cached = read_grok_cache(cache_path).and_then(|body| parse_grok_cache(&body, now));
    if cache_is_fresh && let Some(headroom) = cached {
        return GrokUsage {
            headroom,
            source: GrokUsageSource::Cache,
        };
    }

    if let Some((billing, user)) = fetch()
        && let Some((headroom, normalized)) = parse_live_grok_usage(&billing, &user, now)
    {
        // Only this normalized, non-secret payload is shared. Raw responses and the bearer token
        // never enter the cache or diagnostics.
        if let Ok(body) = serde_json::to_string(&normalized) {
            let _ = write_grok_cache(cache_path, body.as_bytes());
        }
        return GrokUsage {
            headroom,
            source: GrokUsageSource::Live,
        };
    }

    if let Some(headroom) = cached {
        return GrokUsage {
            headroom,
            source: GrokUsageSource::Cache,
        };
    }
    let headroom = grok_headroom_from_log(log_path, now);
    if headroom != Headroom::closed() {
        return GrokUsage {
            headroom,
            source: GrokUsageSource::Log,
        };
    }
    GrokUsage {
        headroom: Headroom::closed(),
        source: GrokUsageSource::None,
    }
}

/// PURE except for reading `path`: the newest official weekly paid SuperGrok billing event.
pub fn grok_headroom_in(path: &Path, now: i64) -> Headroom {
    grok_headroom_from_log(path, now)
}

fn grok_paid_weekly_display_tier(tier: &str) -> bool {
    matches!(tier, "SuperGrok Plus" | "SuperGrok Heavy")
}

fn grok_display_tier_from_api(tier: &str) -> Option<&'static str> {
    match tier {
        "SuperGrokPlus" => Some("SuperGrok Plus"),
        "SuperGrokPro" => Some("SuperGrok Heavy"),
        _ => None,
    }
}

fn grok_headroom_from_log(path: &Path, now: i64) -> Headroom {
    let Some(value) = newest_grok_billing_event(path) else {
        return Headroom::closed();
    };
    if !value
        .pointer("/ctx/subscriptionTier")
        .and_then(serde_json::Value::as_str)
        .is_some_and(grok_paid_weekly_display_tier)
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

/// Scan newest-first without loading the whole log. The retained buffer is at most one JSONL line
/// plus a fixed read block, so a large log with ordinary-sized events stays bounded in memory.
fn newest_grok_billing_event(path: &Path) -> Option<serde_json::Value> {
    let mut file = File::open(path).ok()?;
    let mut remaining = file.metadata().ok()?.len();
    let mut block = vec![0; GROK_LOG_READ_BYTES];
    let mut reversed_line = Vec::new();
    let mut line_over_limit = false;

    while remaining > 0 {
        let read_len = usize::try_from(remaining.min(GROK_LOG_READ_BYTES as u64)).ok()?;
        remaining -= read_len as u64;
        file.seek(SeekFrom::Start(remaining)).ok()?;
        file.read_exact(&mut block[..read_len]).ok()?;
        for &byte in block[..read_len].iter().rev() {
            if byte == b'\n' {
                if !line_over_limit
                    && let Some(value) = grok_billing_event_from_reversed_line(&mut reversed_line)
                {
                    return Some(value);
                }
                reversed_line.clear();
                line_over_limit = false;
            } else if line_over_limit {
                continue;
            } else if reversed_line.len() < GROK_LOG_LINE_MAX_BYTES {
                reversed_line.push(byte);
            } else {
                reversed_line.clear();
                line_over_limit = true;
            }
        }
    }
    (!line_over_limit)
        .then(|| grok_billing_event_from_reversed_line(&mut reversed_line))
        .flatten()
}

fn grok_billing_event_from_reversed_line(line: &mut [u8]) -> Option<serde_json::Value> {
    if line.is_empty() {
        return None;
    }
    line.reverse();
    let value = serde_json::from_slice::<serde_json::Value>(line).ok()?;
    (value.get("msg").and_then(serde_json::Value::as_str)
        == Some("billing: fetched credits config"))
    .then_some(value)
}

fn parse_grok_cache(body: &str, now: i64) -> Option<Headroom> {
    let cache: GrokUsageCache = serde_json::from_str(body).ok()?;
    if !grok_paid_weekly_display_tier(&cache.tier)
        || !valid_grok_percentage(cache.weekly_percent)
        || cache.weekly_reset <= now
    {
        return None;
    }
    Some(known_grok_headroom(
        cache.weekly_percent,
        cache.weekly_reset,
    ))
}

/// Read the shared Grok cache without following links or trusting an unbounded `/tmp` file.
fn read_grok_cache(path: &Path) -> Option<String> {
    let mut file = open_grok_cache_for_read(path)?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file()
        || metadata.len() > GROK_USAGE_CACHE_MAX_BYTES as u64
        || !grok_cache_metadata_is_trusted(&metadata)
    {
        return None;
    }

    let mut body = String::new();
    std::io::Read::by_ref(&mut file)
        .take(GROK_USAGE_CACHE_MAX_BYTES as u64 + 1)
        .read_to_string(&mut body)
        .ok()?;
    (body.len() <= GROK_USAGE_CACHE_MAX_BYTES).then_some(body)
}

#[cfg(unix)]
fn open_grok_cache_for_read(path: &Path) -> Option<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .ok()
}

#[cfg(not(unix))]
fn open_grok_cache_for_read(path: &Path) -> Option<File> {
    OpenOptions::new().read(true).open(path).ok()
}

#[cfg(unix)]
fn grok_cache_metadata_is_trusted(metadata: &std::fs::Metadata) -> bool {
    metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o022 == 0
        && metadata.mode() & 0o400 != 0
}

#[cfg(not(unix))]
fn grok_cache_metadata_is_trusted(_metadata: &std::fs::Metadata) -> bool {
    true
}

/// Replace the normalized cache body only through a regular file owned by this user.
fn write_grok_cache(path: &Path, body: &[u8]) -> std::io::Result<()> {
    if body.len() > GROK_USAGE_CACHE_MAX_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "normalized Grok usage cache is too large",
        ));
    }

    let mut file = open_grok_cache_for_write(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || !grok_cache_write_metadata_is_trusted(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Grok usage cache must be a securely owned, single-linked regular file",
        ));
    }
    secure_grok_cache_permissions(&file)?;
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(body)
}

#[cfg(unix)]
fn open_grok_cache_for_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}

#[cfg(not(unix))]
fn open_grok_cache_for_write(path: &Path) -> std::io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
}

#[cfg(unix)]
fn grok_cache_write_metadata_is_trusted(metadata: &std::fs::Metadata) -> bool {
    grok_cache_metadata_is_trusted(metadata) && metadata.nlink() == 1
}

#[cfg(not(unix))]
fn grok_cache_write_metadata_is_trusted(_metadata: &std::fs::Metadata) -> bool {
    true
}

#[cfg(unix)]
fn secure_grok_cache_permissions(file: &File) -> std::io::Result<()> {
    file.set_permissions(std::fs::Permissions::from_mode(0o600))
}

#[cfg(not(unix))]
fn secure_grok_cache_permissions(_file: &File) -> std::io::Result<()> {
    Ok(())
}

fn parse_live_grok_usage(
    billing: &str,
    user: &str,
    now: i64,
) -> Option<(Headroom, GrokUsageCache)> {
    let billing: serde_json::Value = serde_json::from_str(billing).ok()?;
    let user: serde_json::Value = serde_json::from_str(user).ok()?;
    let display_tier = user
        .get("subscriptionTier")
        .and_then(serde_json::Value::as_str)
        .and_then(grok_display_tier_from_api)?;
    let config = billing.get("config")?.as_object()?;
    if config
        .get("currentPeriod")?
        .get("type")
        .and_then(serde_json::Value::as_str)
        != Some("USAGE_PERIOD_TYPE_WEEKLY")
    {
        return None;
    }
    let reset = config
        .get("currentPeriod")?
        .get("end")?
        .as_str()
        .and_then(parse_rfc3339_epoch)
        .filter(|reset| *reset > now)?;
    let weekly_percent = match config.get("creditUsagePercent") {
        None => 0.0,
        Some(value) => value
            .as_f64()
            .filter(|value| valid_grok_percentage(*value))?,
    };
    let headroom = known_grok_headroom(weekly_percent, reset);
    Some((
        headroom,
        GrokUsageCache {
            tier: display_tier.to_string(),
            weekly_percent,
            weekly_reset: reset,
            source_ts: format_rfc3339_utc(now),
        },
    ))
}

fn known_grok_headroom(weekly_pct: f64, weekly_reset_epoch: i64) -> Headroom {
    Headroom {
        five_hour_pct: 0.0,
        five_hour_reset_epoch: 0,
        weekly_pct,
        weekly_reset_epoch,
        weekly_capacity_known: true,
        stale: false,
    }
}

fn valid_grok_percentage(value: f64) -> bool {
    value.is_finite() && (0.0..=100.0).contains(&value)
}

/// PURE: epoch seconds as the UTC RFC 3339 shape written by the reference shell reader.
fn format_rfc3339_utc(epoch: i64) -> String {
    let days = epoch.div_euclid(86_400);
    let seconds = epoch.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    let hour = seconds / 3_600;
    let minute = seconds % 3_600 / 60;
    let second = seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// PURE: the inverse of `days_from_civil`, returning a Gregorian UTC calendar date.
fn civil_from_days(days_since_epoch: i64) -> (i64, i64, i64) {
    let shifted = days_since_epoch + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = shifted_month + if shifted_month < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    (year, month, day)
}

/// IMPURE: read the first non-empty Grok auth key, call both provider endpoints, and reduce every
/// file, authentication, transport, status, and body error to None.
fn fetch_grok_usage(grok_home: &Path) -> Option<(String, String)> {
    let token = grok_auth_token_from(&std::fs::read_to_string(grok_home.join("auth.json")).ok()?)?;
    let billing = fetch_grok_body(GROK_BILLING_URL, &token)?;
    let user = fetch_grok_body(GROK_USER_URL, &token)?;
    Some((billing, user))
}

fn fetch_grok_body(url: &str, token: &str) -> Option<String> {
    ureq::get(url)
        .config()
        .https_only(true)
        .max_redirects(0)
        .timeout_global(Some(USAGE_HTTP_TIMEOUT))
        .build()
        .header("Authorization", &format!("Bearer {token}"))
        .header("X-XAI-Token-Auth", "xai-grok-cli")
        .call()
        .ok()?
        .body_mut()
        .read_to_string()
        .ok()
}

/// PURE: the first non-empty string key across the Grok CLI auth collection's values. The live
/// file has appeared as both an array and an object; this preserves JSON input order like jq's
/// `.[]` rather than inheriting `serde_json::Map`'s configured ordering.
fn grok_auth_token_from(body: &str) -> Option<String> {
    let mut deserializer = serde_json::Deserializer::from_str(body);
    let token = serde::Deserializer::deserialize_any(&mut deserializer, GrokAuthKeyVisitor).ok()?;
    deserializer.end().ok()?;
    token
}

struct GrokAuthKeyVisitor;

impl<'de> serde::de::Visitor<'de> for GrokAuthKeyVisitor {
    type Value = Option<String>;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a Grok auth array or object")
    }

    fn visit_seq<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::SeqAccess<'de>,
    {
        let mut first = None;
        while let Some(entry) = entries.next_element::<serde_json::Value>()? {
            first = first.or_else(|| grok_auth_key(&entry));
        }
        Ok(first)
    }

    fn visit_map<A>(self, mut entries: A) -> Result<Self::Value, A::Error>
    where
        A: serde::de::MapAccess<'de>,
    {
        let mut first = None;
        while let Some((_, entry)) =
            entries.next_entry::<serde::de::IgnoredAny, serde_json::Value>()?
        {
            first = first.or_else(|| grok_auth_key(&entry));
        }
        Ok(first)
    }
}

fn grok_auth_key(entry: &serde_json::Value) -> Option<String> {
    entry
        .get("key")?
        .as_str()
        .filter(|key| !key.is_empty())
        .map(str::to_string)
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
/// newest-first and each rollout's own lines last-first. Each readable file is walked backwards in
/// fixed-size blocks so a large active session does not have to be loaded whole. An unreadable
/// file is skipped, matching the previous `read_to_string` error path.
fn newest_capacity_rate_limits_line(sessions_dir: &Path, scan_n: usize) -> Option<String> {
    for path in newest_rollouts(sessions_dir, scan_n) {
        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(_) => continue,
        };
        if let Some(line) = last_capacity_rate_limits_line(&mut file) {
            return Some(line);
        }
    }
    None
}

/// Last line in `reader` that contains `"rate_limits"` and passes `carries_capacity_verdict`.
/// Generic over `Read + Seek` so a test can inject a byte-counting reader.
fn last_capacity_rate_limits_line<R: Read + Seek>(reader: &mut R) -> Option<String> {
    last_matching_line_from_end(reader, CODEX_ROLLOUT_READ_BYTES, |line| {
        line.contains("\"rate_limits\"") && carries_capacity_verdict(line)
    })
}

/// Walk `reader` backwards in `block_size` chunks, returning the last (nearest-to-EOF) line for
/// which `matches` is true. Line breaks match `str::lines()`: `\n` or `\r\n`, with no trailing
/// empty line. Memory is one reconstructed line plus one block. Invalid UTF-8 aborts the file,
/// matching `read_to_string` skipping an unreadable candidate.
fn last_matching_line_from_end<R, F>(
    reader: &mut R,
    block_size: usize,
    mut matches: F,
) -> Option<String>
where
    R: Read + Seek,
    F: FnMut(&str) -> bool,
{
    let mut remaining = reader.seek(SeekFrom::End(0)).ok()?;
    if remaining == 0 {
        return None;
    }
    let mut block = vec![0; block_size];
    let mut reversed_line = Vec::new();
    let mut skip_cr = false;

    while remaining > 0 {
        let read_len = usize::try_from(remaining.min(block_size as u64)).ok()?;
        remaining -= read_len as u64;
        reader.seek(SeekFrom::Start(remaining)).ok()?;
        reader.read_exact(&mut block[..read_len]).ok()?;
        for &byte in block[..read_len].iter().rev() {
            if skip_cr {
                skip_cr = false;
                if byte == b'\r' {
                    continue;
                }
            }
            if byte == b'\n' {
                match completed_line(&mut reversed_line, &mut matches) {
                    Ok(Some(line)) => return Some(line),
                    Ok(None) => {}
                    Err(_) => return None,
                }
                reversed_line.clear();
                skip_cr = true;
            } else {
                reversed_line.push(byte);
            }
        }
    }
    completed_line(&mut reversed_line, &mut matches)
        .ok()
        .flatten()
}

/// `Ok(Some)` is a match, `Ok(None)` is an empty or non-matching line, `Err` is invalid UTF-8.
fn completed_line<F>(
    reversed: &mut [u8],
    matches: &mut F,
) -> Result<Option<String>, std::str::Utf8Error>
where
    F: FnMut(&str) -> bool,
{
    if reversed.is_empty() {
        return Ok(None);
    }
    reversed.reverse();
    let line = std::str::from_utf8(reversed)?;
    Ok(matches(line).then(|| line.to_string()))
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
    use std::cell::Cell;
    use std::io::{Cursor, Read, Seek, SeekFrom};
    #[cfg(unix)]
    use std::os::unix::{
        ffi::OsStrExt as _,
        fs::{FileTypeExt as _, symlink},
    };

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

    struct CountingReader<R> {
        inner: R,
        bytes_read: usize,
    }

    impl<R: Read> Read for CountingReader<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.bytes_read += n;
            Ok(n)
        }
    }

    impl<R: Seek> Seek for CountingReader<R> {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn capacity_line(pct: i64, reset: i64) -> String {
        format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":{pct},"resets_at":{reset}}},"secondary":null}}}}}}"#
        )
    }

    fn filler_line() -> &'static str {
        r#"{"payload":{"type":"item"}}"#
    }

    fn scan_capacity_line(bytes: &[u8]) -> (Option<String>, usize) {
        let mut reader = CountingReader {
            inner: Cursor::new(bytes),
            bytes_read: 0,
        };
        let found = last_capacity_rate_limits_line(&mut reader);
        (found, reader.bytes_read)
    }

    #[test]
    fn codex_tail_scan_reads_only_the_final_blocks_of_a_large_rollout() {
        let reset = 1_000_000 + 3600;
        let verdict = capacity_line(41, reset);
        let filler = format!("{}\n", filler_line());
        let mut body = String::new();
        while body.len() < CODEX_ROLLOUT_READ_BYTES * 3 {
            body.push_str(&filler);
        }
        body.push_str(&verdict);
        body.push('\n');

        let (found, bytes_read) = scan_capacity_line(body.as_bytes());
        assert_eq!(found.as_deref(), Some(verdict.as_str()));
        assert!(
            bytes_read <= CODEX_ROLLOUT_READ_BYTES * 2,
            "tail scan must stop after a small constant number of blocks; read {bytes_read} of {}",
            body.len()
        );
        assert!(
            bytes_read < body.len() / 2,
            "must not read the whole rollout: read {bytes_read} of {}",
            body.len()
        );
    }

    #[test]
    fn codex_tail_scan_matches_a_verdict_line_that_spans_a_block_boundary() {
        let reset = 1_000_000 + 3600;
        let verdict = capacity_line(41, reset);
        let mut verdict_nl = verdict.clone();
        verdict_nl.push('\n');
        assert!(
            verdict_nl.len() > 20,
            "the split keeps 20 trailing bytes of the verdict in the last block"
        );

        let mut trailing = Vec::new();
        let filler = format!("{}\n", filler_line());
        while trailing.len() < CODEX_ROLLOUT_READ_BYTES - 20 {
            trailing.extend_from_slice(filler.as_bytes());
        }
        trailing.truncate(CODEX_ROLLOUT_READ_BYTES - 20);

        let mut body = format!("{}\n", filler_line()).into_bytes();
        body.extend_from_slice(verdict_nl.as_bytes());
        body.extend_from_slice(&trailing);

        let (found, bytes_read) = scan_capacity_line(&body);
        assert_eq!(found.as_deref(), Some(verdict.as_str()));
        assert!(
            bytes_read > CODEX_ROLLOUT_READ_BYTES,
            "a split line must pull the previous block; read {bytes_read}"
        );
        assert!(
            bytes_read <= CODEX_ROLLOUT_READ_BYTES * 2,
            "a split line still stays within two blocks; read {bytes_read}"
        );
    }

    #[test]
    fn codex_tail_scan_uses_the_last_verdict_even_when_later_lines_follow() {
        let reset = 1_000_000 + 3600;
        let older = capacity_line(11, reset);
        let newer = capacity_line(41, reset);
        let body = format!("{older}\n{newer}\n{}\n{}\n", filler_line(), filler_line());
        let (found, _) = scan_capacity_line(body.as_bytes());
        assert_eq!(found.as_deref(), Some(newer.as_str()));
    }

    #[test]
    fn codex_tail_scan_returns_none_for_an_empty_rollout() {
        let (found, bytes_read) = scan_capacity_line(b"");
        assert_eq!(found, None);
        assert_eq!(bytes_read, 0);
    }

    #[test]
    fn codex_tail_scan_aborts_a_file_when_a_later_line_is_invalid_utf8() {
        let reset = 1_000_000 + 3600;
        let mut body = capacity_line(11, reset).into_bytes();
        body.push(b'\n');
        body.extend_from_slice(&[0xff, 0xfe, 0xfd, b'\n']);
        let (found, _) = scan_capacity_line(&body);
        assert_eq!(
            found, None,
            "invalid UTF-8 must skip the candidate, not an older verdict in the same file"
        );
    }

    #[test]
    fn codex_headroom_falls_through_a_newer_rollout_with_no_verdict() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let weekly = format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":42,"resets_at":{}}},"secondary":null}}}}}}"#,
            now + 3600
        );
        rollout(dir.path(), "older-with-verdict.jsonl", &weekly, 100);
        rollout(
            dir.path(),
            "newer-without-verdict.jsonl",
            &format!("{}\n{}\n", filler_line(), filler_line()),
            200,
        );
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(
            got.weekly_pct, 42.0,
            "a newer rollout with no capacity verdict must not hide the next-newest"
        );
    }

    #[test]
    fn codex_headroom_skips_an_empty_newer_rollout_and_reads_the_next() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let weekly = format!(
            r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":42,"resets_at":{}}},"secondary":null}}}}}}"#,
            now + 3600
        );
        rollout(dir.path(), "older-with-verdict.jsonl", &weekly, 100);
        rollout(dir.path(), "empty.jsonl", "", 200);
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(got.weekly_pct, 42.0);

        let only_empty = tempfile::tempdir().expect("tempdir");
        rollout(only_empty.path(), "empty.jsonl", "", 100);
        assert_eq!(
            codex_headroom_in(only_empty.path(), now, 20),
            Headroom::closed()
        );
    }

    #[test]
    fn codex_headroom_skips_a_newer_rollout_with_invalid_utf8() {
        let now = 1_000_000;
        let dir = tempfile::tempdir().expect("tempdir");
        let weekly = |pct: i64| {
            format!(
                r#"{{"payload":{{"rate_limits":{{"primary":{{"window_minutes":10080,"used_percent":{pct},"resets_at":{}}},"secondary":null}}}}}}"#,
                now + 3600
            )
        };
        rollout(dir.path(), "older-with-verdict.jsonl", &weekly(42), 100);
        let mut newer = weekly(11).into_bytes();
        newer.push(b'\n');
        newer.extend_from_slice(&[0xff, 0xfe, 0xfd, b'\n']);
        let newer_path = dir.path().join("newer-invalid.jsonl");
        std::fs::write(&newer_path, newer).expect("write invalid utf8 rollout");
        let mtime = std::fs::FileTimes::new()
            .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(200));
        std::fs::File::options()
            .write(true)
            .open(&newer_path)
            .expect("open invalid utf8 rollout")
            .set_times(mtime)
            .expect("set mtime");
        let got = codex_headroom_in(dir.path(), now, 20);
        assert_eq!(
            got.weekly_pct, 42.0,
            "a newer unreadable rollout must fall through like read_to_string failure"
        );
    }

    const GROK_BILLING: &str = r#"{
      "config": {
        "currentPeriod": {
          "type": "USAGE_PERIOD_TYPE_WEEKLY",
          "end": "2026-08-22T00:00:00+00:00"
        },
        "creditUsagePercent": 37.5
      }
    }"#;
    const GROK_USER: &str = r#"{"subscriptionTier":"SuperGrokPlus"}"#;

    fn grok_cache(percent: f64) -> String {
        format!(
            r#"{{"tier":"SuperGrok Plus","weekly_percent":{percent},"weekly_reset":1787356800,"source_ts":"2026-08-21T16:35:37Z"}}"#
        )
    }

    fn seed_grok_cache(path: &Path, percent: f64) {
        write_grok_cache(path, grok_cache(percent).as_bytes()).expect("seed secure Grok cache");
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

    #[test]
    fn grok_usage_fresh_cache_short_circuits_both_live_fetch_outcomes() {
        let directory = tempfile::tempdir().expect("temporary Grok cache");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        seed_grok_cache(&cache, 61.0);

        for live_result in [
            Some((GROK_BILLING.to_string(), GROK_USER.to_string())),
            None,
        ] {
            let calls = Cell::new(0);
            let usage = grok_usage_in(&cache, &log, 1_787_313_600, true, || {
                calls.set(calls.get() + 1);
                live_result
            });

            assert_eq!(usage.source, GrokUsageSource::Cache);
            assert_eq!(usage.headroom.weekly_pct, 61.0);
            assert_eq!(
                calls.get(),
                0,
                "a fresh cache must avoid every network outcome"
            );
        }
    }

    #[test]
    fn grok_usage_stale_cache_uses_live_when_available_and_writes_normalized_cache() {
        let directory = tempfile::tempdir().expect("temporary Grok cache");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        seed_grok_cache(&cache, 61.0);

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(usage.source, GrokUsageSource::Live);
        assert_eq!(usage.headroom.weekly_pct, 37.5);
        assert!(usage.headroom.weekly_capacity_known);
        assert!(!usage.headroom.stale);

        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cache).expect("read normalized live cache"),
        )
        .expect("cache is normalized JSON rather than a raw provider response");
        assert_eq!(written["tier"], "SuperGrok Plus");
        assert_eq!(written["weekly_percent"], 37.5);
        assert_eq!(written["weekly_reset"], 1_787_356_800);
        assert!(written["source_ts"].is_string());
        assert_eq!(written.as_object().map(|fields| fields.len()), Some(4));
    }

    #[test]
    fn grok_usage_stale_cache_survives_a_failed_live_fetch() {
        let directory = tempfile::tempdir().expect("temporary Grok cache");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        seed_grok_cache(&cache, 61.0);

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || None);

        assert_eq!(usage.source, GrokUsageSource::Cache);
        assert_eq!(usage.headroom.weekly_pct, 61.0);
        assert!(usage.headroom.weekly_capacity_known);
    }

    #[test]
    fn grok_usage_absent_cache_uses_live_and_writes_it() {
        let directory = tempfile::tempdir().expect("temporary Grok cache");
        let cache = directory.path().join("new-grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(usage.source, GrokUsageSource::Live);
        assert_eq!(usage.headroom.weekly_pct, 37.5);
        assert!(
            cache.is_file(),
            "a successful live read must become the shared cache"
        );
    }

    #[test]
    fn grok_usage_absent_cache_falls_back_to_the_log_then_closed_none() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("absent-cache.json");
        let log = directory.path().join("unified.jsonl");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok-billing-known.jsonl"),
            &log,
        )
        .expect("seed fallback billing log");

        let from_log = grok_usage_in(&cache, &log, 1_787_313_600, false, || None);
        assert_eq!(from_log.source, GrokUsageSource::Log);
        assert_eq!(from_log.headroom.weekly_pct, 37.5);

        let none = grok_usage_in(
            &cache,
            &directory.path().join("missing-log.jsonl"),
            1_787_313_600,
            false,
            || None,
        );
        assert_eq!(none.source, GrokUsageSource::None);
        assert_eq!(none.headroom, Headroom::closed());
    }

    #[test]
    fn grok_usage_rejects_invalid_cache_and_live_capacity_contracts() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        let invalid_caches = [
            r#"{"tier":"Free","weekly_percent":37.5,"weekly_reset":1787356800,"source_ts":"x"}"#,
            r#"{"tier":"SuperGrok Plus","weekly_percent":101,"weekly_reset":1787356800,"source_ts":"x"}"#,
            r#"{"tier":"SuperGrok Plus","weekly_percent":37.5,"weekly_reset":1,"source_ts":"x"}"#,
        ];
        for body in invalid_caches {
            std::fs::write(&cache, body).expect("write invalid cache");
            let usage = grok_usage_in(&cache, &log, 1_787_313_600, true, || None);
            assert_eq!(usage.source, GrokUsageSource::None, "invalid cache: {body}");
        }

        let invalid_live = [
            (GROK_BILLING, r#"{"subscriptionTier":"Free"}"#),
            (
                r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_DAILY","end":"2026-08-22T00:00:00+00:00"},"creditUsagePercent":37.5}}"#,
                GROK_USER,
            ),
            (
                r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2020-08-22T00:00:00+00:00"},"creditUsagePercent":37.5}}"#,
                GROK_USER,
            ),
            (
                r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-08-22T00:00:00+00:00"},"creditUsagePercent":-1}}"#,
                GROK_USER,
            ),
            (
                r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-08-22T00:00:00+00:00"},"creditUsagePercent":101}}"#,
                GROK_USER,
            ),
        ];
        let absent_cache = directory.path().join("absent-cache.json");
        for (billing, user) in invalid_live {
            let usage = grok_usage_in(&absent_cache, &log, 1_787_313_600, false, || {
                Some((billing.to_string(), user.to_string()))
            });
            assert_eq!(
                usage.source,
                GrokUsageSource::None,
                "invalid live billing: {billing}"
            );
        }
    }

    #[test]
    fn grok_live_capacity_without_a_percent_is_known_zero() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("absent-cache.json");
        let log = directory.path().join("no-log.jsonl");
        let billing = r#"{"config":{"currentPeriod":{"type":"USAGE_PERIOD_TYPE_WEEKLY","end":"2026-08-22T00:00:00+00:00"}}}"#;

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((billing.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(usage.source, GrokUsageSource::Live);
        assert_eq!(usage.headroom.weekly_pct, 0.0);
        assert!(
            usage.headroom.weekly_capacity_known,
            "the live billing endpoint's absent percent is an authoritative zero"
        );
    }

    const GROK_USER_HEAVY: &str = r#"{"subscriptionTier":"SuperGrokPro"}"#;

    #[test]
    fn grok_live_supergrok_pro_normalizes_to_heavy_display_tier() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("new-grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((GROK_BILLING.to_string(), GROK_USER_HEAVY.to_string()))
        });

        assert_eq!(usage.source, GrokUsageSource::Live);
        assert_eq!(usage.headroom.weekly_pct, 37.5);
        assert!(usage.headroom.weekly_capacity_known);
        let written: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&cache).expect("read normalized Heavy cache"),
        )
        .expect("cache is normalized JSON");
        assert_eq!(written["tier"], "SuperGrok Heavy");
        assert_eq!(written["weekly_percent"], 37.5);
    }

    #[test]
    fn grok_cache_accepts_heavy_display_tier() {
        let directory = tempfile::tempdir().expect("temporary Grok cache");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        write_grok_cache(
            &cache,
            br#"{"tier":"SuperGrok Heavy","weekly_percent":1.0,"weekly_reset":1787356800,"source_ts":"2026-08-21T16:35:37Z"}"#,
        )
        .expect("seed Heavy cache");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, true, || None);

        assert_eq!(usage.source, GrokUsageSource::Cache);
        assert_eq!(usage.headroom.weekly_pct, 1.0);
        assert!(usage.headroom.weekly_capacity_known);
    }

    #[test]
    fn grok_log_accepts_heavy_display_tier() {
        let directory = tempfile::tempdir().expect("temporary Grok log");
        let log_path = directory.path().join("unified.jsonl");
        let event = serde_json::json!({
            "msg": "billing: fetched credits config",
            "ctx": {
                "subscriptionTier": "SuperGrok Heavy",
                "config": {
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "end": "2026-08-22T00:00:00+00:00",
                    },
                    "creditUsagePercent": 1.0,
                },
            },
        });
        std::fs::write(&log_path, format!("{event}\n")).expect("write Heavy billing event");

        let usage = grok_headroom_in(&log_path, 1_787_313_600);

        assert_eq!(usage.weekly_pct, 1.0);
        assert_eq!(usage.weekly_reset_epoch, 1_787_356_800);
        assert!(usage.weekly_capacity_known);
        assert!(!usage.stale);
    }

    #[cfg(unix)]
    #[test]
    fn grok_usage_rejects_a_symlink_cache_without_reading_or_overwriting_its_target() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let target = directory.path().join("unrelated-target.json");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        let original = grok_cache(61.0);
        std::fs::write(&target, &original).expect("seed symlink target");
        symlink(&target, &cache).expect("create cache symlink");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, true, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(
            usage.source,
            GrokUsageSource::Live,
            "a symlink must not be trusted as a fresh cache read"
        );
        assert_eq!(
            std::fs::read_to_string(&target).expect("read protected symlink target"),
            original,
            "a successful live refresh must not write through a cache symlink"
        );
        assert!(
            std::fs::symlink_metadata(&cache)
                .expect("cache symlink remains present")
                .file_type()
                .is_symlink()
        );
    }

    #[cfg(unix)]
    #[test]
    fn grok_usage_does_not_block_or_write_through_a_fifo_cache() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("grok-usage-cache.fifo");
        let log = directory.path().join("no-log.jsonl");
        let cache_c_string = std::ffi::CString::new(cache.as_os_str().as_bytes())
            .expect("temporary cache path contains no NUL");
        assert_eq!(
            unsafe { libc::mkfifo(cache_c_string.as_ptr(), 0o600) },
            0,
            "create attacker-controlled FIFO cache: {}",
            std::io::Error::last_os_error()
        );

        let (result_tx, result_rx) = std::sync::mpsc::sync_channel(1);
        let worker_cache = cache.clone();
        let worker = std::thread::spawn(move || {
            let usage = grok_usage_in(&worker_cache, &log, 1_787_313_600, false, || {
                Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
            });
            result_tx.send(usage).expect("return FIFO cache result");
        });

        let (blocked, usage) = match result_rx.recv_timeout(Duration::from_millis(200)) {
            Ok(usage) => (false, usage),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                // The pre-fix read opens a FIFO as a normal cache file and blocks here. Pair a
                // writer with it before asserting, so this red test never leaks a worker thread.
                drop(
                    OpenOptions::new()
                        .write(true)
                        .open(&cache)
                        .expect("release a blocked FIFO cache reader"),
                );
                let usage = result_rx
                    .recv_timeout(Duration::from_secs(1))
                    .expect("released FIFO cache worker returns promptly");
                (true, usage)
            }
            Err(error) => panic!("FIFO cache worker disconnected: {error}"),
        };
        worker.join().expect("FIFO cache worker does not panic");

        assert!(
            !blocked,
            "an attacker-created FIFO must be rejected before either cache read or live cache write can block"
        );
        assert_eq!(
            usage.source,
            GrokUsageSource::Live,
            "valid live billing remains useful when the cache path is unsafe"
        );
        assert!(
            std::fs::symlink_metadata(&cache)
                .expect("FIFO cache remains present")
                .file_type()
                .is_fifo(),
            "the live refresh must not replace or write through the FIFO"
        );
    }

    #[test]
    fn oversized_grok_cache_falls_through_to_live_log_and_none() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("oversized-cache.json");
        let log = directory.path().join("unified.jsonl");
        let oversized = format!("{}{}", " ".repeat(1_048_577), grok_cache(61.0));
        std::fs::write(&cache, oversized).expect("write oversized but otherwise valid cache");
        std::fs::copy(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/grok-billing-known.jsonl"),
            &log,
        )
        .expect("seed fallback billing log");

        let live = grok_usage_in(&cache, &log, 1_787_313_600, true, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });
        assert_eq!(live.source, GrokUsageSource::Live);

        std::fs::write(
            &cache,
            format!("{}{}", " ".repeat(1_048_577), grok_cache(61.0)),
        )
        .expect("restore oversized cache after live refresh");
        let logged = grok_usage_in(&cache, &log, 1_787_313_600, true, || None);
        assert_eq!(logged.source, GrokUsageSource::Log);
        assert_eq!(logged.headroom.weekly_pct, 37.5);

        let none = grok_usage_in(
            &cache,
            &directory.path().join("missing-log.jsonl"),
            1_787_313_600,
            true,
            || None,
        );
        assert_eq!(none.source, GrokUsageSource::None);
        assert_eq!(none.headroom, Headroom::closed());
    }

    #[cfg(unix)]
    #[test]
    fn successful_grok_cache_refresh_sets_owner_only_permissions() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(usage.source, GrokUsageSource::Live);
        assert_eq!(
            std::fs::metadata(&cache)
                .expect("refreshed cache metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "normalized billing data must not remain group/world readable"
        );
    }

    #[cfg(unix)]
    #[test]
    fn grok_usage_does_not_mutate_an_existing_writable_cache_inode() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        let original = grok_cache(61.0);
        std::fs::write(&cache, &original).expect("seed owner-matched writable cache");
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o666))
            .expect("make the unsafe mode deterministic");

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, true, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(
            usage.source,
            GrokUsageSource::Live,
            "a rejected cache must not prevent a valid live billing reading"
        );
        assert_eq!(
            std::fs::read_to_string(&cache).expect("read protected cache inode"),
            original,
            "an unsafe existing inode must not be truncated or overwritten"
        );
        assert_eq!(
            std::fs::metadata(&cache)
                .expect("protected cache metadata")
                .mode()
                & 0o777,
            0o666,
            "an unsafe existing inode must not be chmodded during live refresh"
        );
    }

    #[cfg(unix)]
    #[test]
    fn grok_usage_does_not_mutate_a_hard_linked_cache_inode() {
        let directory = tempfile::tempdir().expect("temporary Grok usage paths");
        let protected = directory.path().join("unrelated-hard-link.json");
        let cache = directory.path().join("grok-usage-cache.json");
        let log = directory.path().join("no-log.jsonl");
        let original = grok_cache(61.0);
        std::fs::write(&protected, &original).expect("seed protected cache inode");
        std::fs::hard_link(&protected, &cache).expect("create cache hard link");
        let protected_metadata = std::fs::metadata(&protected).expect("protected link metadata");
        let cache_metadata = std::fs::metadata(&cache).expect("cache link metadata");
        assert_eq!(protected_metadata.nlink(), 2, "fixture has two hard links");
        assert_eq!(protected_metadata.ino(), cache_metadata.ino());

        let usage = grok_usage_in(&cache, &log, 1_787_313_600, false, || {
            Some((GROK_BILLING.to_string(), GROK_USER.to_string()))
        });

        assert_eq!(
            usage.source,
            GrokUsageSource::Live,
            "a rejected hard link must not prevent a valid live billing reading"
        );
        for path in [&protected, &cache] {
            assert_eq!(
                std::fs::read_to_string(path).expect("read protected hard link"),
                original,
                "live refresh must not overwrite either name for a shared inode"
            );
            let metadata = std::fs::metadata(path).expect("protected hard link metadata");
            assert_eq!(metadata.ino(), protected_metadata.ino());
            assert_eq!(metadata.nlink(), protected_metadata.nlink());
            assert_eq!(metadata.mode() & 0o777, protected_metadata.mode() & 0o777);
        }
    }

    #[test]
    fn grok_log_skips_one_oversized_billing_line_and_finds_the_earlier_event() {
        let directory = tempfile::tempdir().expect("temporary Grok log");
        let log_path = directory.path().join("unified.jsonl");
        let mut log = std::fs::File::create(&log_path).expect("create Grok log");
        let earlier = serde_json::json!({
            "msg": "billing: fetched credits config",
            "ctx": {
                "subscriptionTier": "SuperGrok Plus",
                "config": {
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "end": "2026-08-22T00:00:00+00:00",
                    },
                    "creditUsagePercent": 37.5,
                },
            },
        });
        let oversized_newer = serde_json::json!({
            "msg": "billing: fetched credits config",
            "padding": "x".repeat(1_048_577),
            "ctx": {
                "subscriptionTier": "SuperGrok Plus",
                "config": {
                    "currentPeriod": {
                        "type": "USAGE_PERIOD_TYPE_WEEKLY",
                        "end": "2026-08-22T00:00:00+00:00",
                    },
                    "creditUsagePercent": 88.0,
                },
            },
        });
        writeln!(log, "{earlier}").expect("write earlier billing event");
        writeln!(log, "{oversized_newer}").expect("write oversized newer billing event");
        drop(log);

        let usage = grok_headroom_in(&log_path, 1_787_313_600);

        assert_eq!(
            usage.weekly_pct, 37.5,
            "an over-limit line must be discarded without hiding bounded earlier events"
        );
        assert!(usage.weekly_capacity_known);
        assert!(!usage.stale);
    }

    #[test]
    fn grok_auth_token_uses_the_first_nonempty_key_without_reporting_it() {
        let auth = r#"[
          {"key": ""},
          {"key": 7},
          {"key": "first usable test key"},
          {"key": "later test key"}
        ]"#;

        assert_eq!(
            grok_auth_token_from(auth),
            Some("first usable test key".to_string())
        );
        assert_eq!(
            grok_auth_token_from(r#"{"first":{"key":""},"later":{"key":"object test key"}}"#),
            Some("object test key".to_string()),
            "top-level auth objects must be scanned across their child values like jq `.[]`"
        );
        assert_eq!(grok_auth_token_from(r#"[{"key":""}]"#), None);
        assert_eq!(grok_auth_token_from("not json"), None);
    }
}
