use super::time::format_rfc3339_utc;
use super::{
    CACHE_MAX_AGE, GrokUsage, GrokUsageSource, Headroom, USAGE_HTTP_TIMEOUT, is_fresh,
    parse_rfc3339_epoch,
};
use crate::context::Context;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write as _};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::Path;

/// One fixed-size block used while walking the Grok log backwards. Memory grows only with the
/// longest individual line, not with the size of the log.
const GROK_LOG_READ_BYTES: usize = 64 * 1024;
/// Maximum normalized Grok cache body accepted from the shared `/tmp` path.
const GROK_USAGE_CACHE_MAX_BYTES: usize = 64 * 1024;
/// Maximum one-line Grok log event retained while scanning backwards.
const GROK_LOG_LINE_MAX_BYTES: usize = 1024 * 1024;
const GROK_BILLING_URL: &str = "https://cli-chat-proxy.grok.com/v1/billing?format=credits";
const GROK_USER_URL: &str = "https://cli-chat-proxy.grok.com/v1/user?include=subscription";

#[derive(Debug, serde::Serialize, serde::Deserialize)]
struct GrokUsageCache {
    tier: String,
    weekly_percent: f64,
    weekly_reset: i64,
    source_ts: String,
}

/// IMPURE: one Grok capacity read, retaining whether it came from live billing, cache, log, or no
/// usable source. A fresh cache avoids the provider calls; a stale cache remains the fallback when
/// the calls fail.
pub fn grok_usage(ctx: &Context) -> GrokUsage {
    let grok_home = ctx.grok_home().to_path_buf();
    let cache = ctx.grok_usage_cache.clone();
    grok_usage_in(
        &cache,
        &grok_home.join("logs/unified.jsonl"),
        (ctx.now_epoch)(),
        is_fresh(&cache, CACHE_MAX_AGE),
        || fetch_grok_usage(&grok_home),
    )
}

/// IMPURE: the Grok headroom from the read-through cache and provider-owned fallbacks.
pub fn grok_headroom(ctx: &Context) -> Headroom {
    grok_usage(ctx).headroom
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

#[cfg(test)]
mod tests;
