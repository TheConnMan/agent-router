use super::{CACHE_MAX_AGE, Headroom, USAGE_HTTP_TIMEOUT, is_fresh, parse_rfc3339_epoch};
use crate::context::Context;

/// IMPURE: the Claude snapshot, from the shared cache when it is under 5 minutes old and from
/// the OAuth usage endpoint otherwise. A stale cache is preferred over nothing; nothing at all
/// reads as full headroom.
pub fn claude_headroom(ctx: &Context) -> Headroom {
    let cache = ctx.claude_usage_cache.as_path();
    if is_fresh(cache, CACHE_MAX_AGE)
        && let Some(headroom) = std::fs::read_to_string(cache)
            .ok()
            .as_deref()
            .and_then(parse_claude_usage)
    {
        return headroom;
    }
    if let Some(body) = fetch_claude_usage(ctx)
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
fn fetch_claude_usage(ctx: &Context) -> Option<String> {
    let token = claude_oauth_token(ctx)?;
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

fn claude_oauth_token(ctx: &Context) -> Option<String> {
    let path = ctx.claude_credentials();
    let value: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    value
        .pointer("/claudeAiOauth/accessToken")?
        .as_str()
        .map(str::to_string)
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
}
