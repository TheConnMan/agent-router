use super::*;
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

fn limits_line(primary: &str, secondary: &str) -> String {
    format!(r#"{{"payload":{{"rate_limits":{{"primary":{primary},"secondary":{secondary}}}}}}}"#)
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
    let mtime =
        std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs(200));
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
