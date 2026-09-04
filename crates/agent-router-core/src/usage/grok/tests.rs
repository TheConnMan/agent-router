use super::*;
use std::cell::Cell;
use std::fs::OpenOptions;
#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt as _,
    fs::{FileTypeExt as _, symlink},
};
use std::path::Path;
use std::time::Duration;

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

    let written: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&cache).expect("read normalized live cache"))
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
