use super::Headroom;
use crate::context::Context;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// How many newest rollouts are scanned for a `rate_limits` event.
const CODEX_SCAN_N: usize = 20;
/// One fixed-size block used while walking a Codex rollout backwards. Memory grows only with the
/// longest individual line, not with the size of the rollout.
const CODEX_ROLLOUT_READ_BYTES: usize = 64 * 1024;
/// `window_minutes` of the 5h window.
const WINDOW_FIVE_HOUR: i64 = 300;
/// `window_minutes` of the weekly window.
const WINDOW_WEEKLY: i64 = 10080;

/// IMPURE: the Codex snapshot, read live from the running app-server daemon when one answers and
/// otherwise from the newest rollout carrying a `rate_limits` verdict.
///
/// The daemon comes first because codex-cli 0.158 writes only null windows into rollouts, so a
/// rollout verdict is at best days old. The rollout scan stays as the fallback for a box with no
/// daemon up, and for older CLIs that still fill the event.
pub fn codex_headroom(ctx: &Context) -> Headroom {
    let now = (ctx.now_epoch)();
    crate::dispatch::codex::account_rate_limits(ctx)
        .and_then(|line| live_reading(&line, now))
        .unwrap_or_else(|| codex_headroom_in(&ctx.codex_sessions_dir, now, CODEX_SCAN_N))
}

/// IMPURE only in the RPC it is handed: the daemon's account reading, when it supplies a verdict.
pub fn codex_account_headroom_on_rpc(
    rpc: &mut impl crate::dispatch::codex::CodexRpc,
    now: i64,
) -> Option<Headroom> {
    crate::dispatch::codex::account_rate_limits_on_rpc(rpc)
        .and_then(|line| live_reading(&line, now))
}

/// PURE: a daemon reply as a reading only when it carried a capacity verdict. An all-null reply is
/// no better than no reply, so it defers to the rollouts rather than closing capacity outright.
fn live_reading(line: &str, now: i64) -> Option<Headroom> {
    parse_codex_account_rate_limits(line, now).filter(|headroom| !headroom.stale)
}

/// PURE: an `account/rateLimits/read` reply into a snapshot, under the same rules as a rollout
/// event. The reply is the rollout payload in camelCase, so it is renamed field for field and
/// handed to the shared parser. None for an error reply or one without `rateLimits`.
pub fn parse_codex_account_rate_limits(line: &str, now: i64) -> Option<Headroom> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }
    let limits = value
        .pointer("/result/rateLimits")
        .filter(|limits| limits.is_object())?;
    let window = |key: &str| {
        limits.get(key).filter(|w| w.is_object()).map(|w| {
            serde_json::json!({
                "used_percent": w.get("usedPercent"),
                "window_minutes": w.get("windowDurationMins"),
                "resets_at": w.get("resetsAt"),
            })
        })
    };
    let credits = limits
        .get("credits")
        .filter(|c| c.is_object())
        .map(|c| serde_json::json!({"has_credits": c.get("hasCredits")}));
    Some(headroom_from_limits(
        &serde_json::json!({
            "primary": window("primary"),
            "secondary": window("secondary"),
            "credits": credits,
        }),
        now,
    ))
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
    Some(headroom_from_limits(limits, now))
}

/// PURE: a rollout-shaped `rate_limits` object into a snapshot. See `parse_codex_rate_limits`.
fn headroom_from_limits(limits: &serde_json::Value, now: i64) -> Headroom {
    if let Some(weekly) = window_with_minutes(limits, WINDOW_WEEKLY) {
        let five_hour = window_with_minutes(limits, WINDOW_FIVE_HOUR);
        let (five_hour_pct, five_hour_reset_epoch) = expire(five_hour, now);
        let (weekly_pct, weekly_reset_epoch) = expire(Some(weekly), now);
        if weekly_reset_epoch == 0 {
            return Headroom::closed();
        }
        return Headroom {
            five_hour_pct,
            five_hour_reset_epoch,
            weekly_pct,
            weekly_reset_epoch,
            weekly_capacity_known: true,
            stale: false,
        };
    }
    if let Some(credits) = limits.get("credits").filter(|credits| credits.is_object()) {
        return match credits
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
        };
    }
    Headroom::closed()
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

#[cfg(test)]
mod tests;
