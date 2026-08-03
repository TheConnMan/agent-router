//! Submitting a routed task to Codex Cloud: one `codex cloud exec`, whose stdout carries the new
//! task's URL.
//!
//! This is a separate module from `dispatch/codex.rs` rather than another arm inside it. That file
//! is a JSON-RPC client over a Unix socket with daemon lifecycle management, and none of it has
//! anything to say about a one-shot submit that returns as soon as the server has accepted the
//! task. The two share exactly one thing, the process-with-timeout helper `run_reporting_failure`,
//! which is reused rather than duplicated.
//!
//! What this deliberately does NOT do:
//!
//! - It passes no `--model` and no `--effort`, because `codex cloud exec` has neither flag: the
//!   model belongs to the cloud environment, not to the router. That is also why a cloud decision
//!   records no model. Inventing either flag would fail the submit outright, and inferring a value
//!   to record would put a router-side guess in a column the tuning data reads as a backend fact.
//! - It does not poll. Submitting returns a task URL and nothing else, so a cloud row stays
//!   `dispatched` until a later change teaches the reconciler to read a cloud task's real state.
//!   The trap waiting for whoever writes that: `codex cloud status <TASK_ID>` exits 1 while a task
//!   is PENDING with no diff yet, so a poller must parse the status TEXT and must never read the
//!   exit code as the answer. A poller keyed off the exit code would report every young task as
//!   failed.
//! - It does not retry. A failed submit is one `Err`, which `run` records as `error: ...` on the
//!   row, exactly like every other dispatch failure.
//! - It does not check that the branch exists on the remote. That costs a network round trip to
//!   learn something the submit itself is about to tell us, so the branch is sent as-is and a
//!   never-pushed local branch fails at the server and lands on the row as `error: ...`.
//! - It reads nothing from the local checkout. The environment id and the branch are the whole of
//!   what identifies the work, both already resolved by `cloud::eligibility`, so no working
//!   directory is passed here and none is needed.

use crate::cloud::CloudTarget;
use crate::error::{Error, Result};
use crate::run::Dispatch;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The submit returns as soon as the server has accepted the task, so this bounds one HTTP round
/// trip made by a child process rather than the task's own run. `run_reporting_failure` kills the
/// child on expiry, which is what keeps a wedged CLI from hanging the router forever.
const SUBMIT_TIMEOUT: Duration = Duration::from_secs(60);

/// The hosts a Codex cloud task URL may be on, as an exact set rather than a prefix.
///
/// One entry today. The set exists so that a second legitimate host is a data change rather than a
/// loosened check, which is the shape that survives someone in a hurry. Determined from two places
/// rather than guessed: `cloud.rs` resolves environments against `https://chatgpt.com/backend-api/`,
/// and two real task URLs observed in codex rollouts on this box are
/// `https://chatgpt.com/codex/tasks/task_e_<hex>`.
const TASK_URL_HOSTS: [&str; 1] = ["chatgpt.com"];

/// The path prefix a task URL carries, from the same two observed URLs. A token on the right host
/// but under some other path is a docs link, a pricing page, or a login prompt, not a task.
const TASK_PATH_PREFIX: &str = "/codex/tasks/";

/// Ceiling on the task id, which is the path segment following the task path prefix rather than the
/// URL's last segment, for the reason `parse_task_id` records. The observed ids are 42 characters
/// (`task_e_` plus 32 hex), so this is generous by design: it is a sanity bound on a value headed
/// for a database column and an argv slot, not a claim about the upstream format.
const TASK_ID_MAX_LEN: usize = 128;

/// PURE: the argv for `codex cloud exec`, task text included.
///
/// The `--` before `task` is load-bearing, not decoration: `task` is free-form user input, and a
/// task that legitimately starts with `-` (quoting a flag, a pasted diff line) is not exotic. Without
/// the delimiter, a task of `--help` is parsed by the child as its own `--help` flag rather than as
/// the query, so the CLI prints its help text and exits instead of submitting anything, and a task
/// starting with a single `-` is read as an attempt at an unrecognized short flag. Both failures were
/// reproduced live against the real `codex cloud exec` binary during review. `--` is the standard
/// clap option-terminator, confirmed against this CLI's own `--help` output (`codex-cli 0.146.0`,
/// `Usage: codex cloud exec [OPTIONS] --env <ENV_ID> [QUERY]`), and clap honors it for any positional
/// argument without the subcommand needing to opt in.
fn build_args(task: &str, target: &CloudTarget) -> Vec<String> {
    vec![
        "cloud".to_string(),
        "exec".to_string(),
        "--env".to_string(),
        target.environment_id().to_string(),
        "--branch".to_string(),
        target.branch().to_string(),
        "--".to_string(),
        task.to_string(),
    ]
}

/// IMPURE: submit one task to Codex Cloud and return the identity the server gave it.
///
/// `name` is not sent anywhere: `codex cloud exec` has no naming flag. It is carried into
/// `Dispatch.job_name` so the row is still findable by name, which is the same reason the claude
/// dispatch carries it.
pub fn dispatch(task: &str, name: &str, target: &CloudTarget) -> Result<Dispatch> {
    let mut command = Command::new("codex");
    command
        .args(build_args(task, target))
        // Nothing on this path answers a prompt, so the child is given no stdin at all rather than
        // the router's own. An inherited stdin is how a CLI that decides to ask something blocks
        // forever on a terminal nobody is watching.
        .stdin(Stdio::null());
    let stdout = crate::dispatch::codex::run_reporting_failure(command, SUBMIT_TIMEOUT)
        .map_err(Error::Command)?;

    // A submitted task whose identity was lost is unrecoverable: the work is running and billing,
    // and nothing left on this machine can find it again. So an unparseable stdout is an Err naming
    // what was expected, never an Ok carrying no job id, which would record a successful dispatch
    // of a task nobody can look up.
    let Some(url) = parse_task_url(&stdout) else {
        return Err(Error::Command(format!(
            "`codex cloud exec` printed no task URL, so the submitted task cannot be identified: \
             expected an https token on {hosts} under {TASK_PATH_PREFIX}, saw: {stdout}",
            hosts = TASK_URL_HOSTS.join(" or "),
        )));
    };
    let Some(task_id) = parse_task_id(&url) else {
        return Err(Error::Command(format!(
            "`codex cloud exec` printed {url}, whose last path segment is not a usable task id: \
             expected 1 to {TASK_ID_MAX_LEN} characters of [A-Za-z0-9._-] not starting with `-`"
        )));
    };

    Ok(Dispatch {
        job_id: Some(task_id),
        job_name: name.to_string(),
        // `codex cloud exec` takes no --effort and reports none back, exactly like claude and
        // opencode. Nothing was observed, so nothing is recorded.
        effective_effort: None,
        cloud_task_url: Some(url),
    })
}

/// PURE: the task URL out of the submit's stdout, or None when there is not one.
///
/// The URL is taken from stdout rather than built from an id and a base path, because the base path
/// belongs to a third party and is not ours to assume: the URL is the honest record and the id below
/// is the value derived from it, not the other way round.
///
/// What is NOT taken from stdout is which token counts. The first https token used to win outright,
/// and that made every other URL the CLI might print (a deprecation notice, a docs link, a login
/// prompt, a status-page banner) eligible to become the recorded identity of a task that is already
/// running and already billing. Recording the WRONG identity is strictly worse than recording none:
/// the row looks complete, `agent-router log` shows a plausible URL, and nothing signals to the
/// operator that the task they are looking at is not the task the row names. So a token must be on
/// a pinned host under the task path prefix to be an answer at all, and a stdout carrying only
/// other URLs reads as no URL, which the caller turns into the `Err` it already had for that case.
///
/// **What this does NOT catch.** It constrains the SHAPE of an identity, not its existence. A
/// well-formed URL naming a task that was never created, was created and deleted, or belongs to
/// another account passes here unchanged, because nothing offline can tell those apart from a real
/// one. Confirming a task exists costs a credentialed round trip, which this module does not make;
/// the correction signal stays what it already was, a later `codex cloud status` on the recorded id.
/// `http` is no longer accepted at all: the observed surface is https, and a plaintext URL becoming
/// the recorded identity without comment is not a case worth keeping working.
fn parse_task_url(stdout: &str) -> Option<String> {
    stdout
        .split_whitespace()
        // A URL printed inside a sentence takes the sentence's punctuation with it, and that
        // punctuation would otherwise end up inside the task id.
        .map(|token| token.trim_end_matches(['.', ',', ';', ':', ')', ']', '>', '"', '\'']))
        .find(|token| is_task_url(token))
        .map(str::to_string)
}

/// PURE: whether one whitespace-delimited token is shaped like a Codex cloud task URL.
fn is_task_url(token: &str) -> bool {
    let Some(after_scheme) = token.strip_prefix("https://") else {
        return false;
    };
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let (authority, path) = after_scheme.split_at(authority_end);
    // `https://chatgpt.com@evil.example/codex/tasks/x` has a host of `evil.example`, so an authority
    // carrying userinfo is rejected outright rather than compared against its leading half. A port
    // fails the same equality check, which is intended: the pinned host is a literal, not a prefix.
    !authority.contains('@')
        && TASK_URL_HOSTS
            .iter()
            .any(|host| authority.eq_ignore_ascii_case(host))
        && path.starts_with(TASK_PATH_PREFIX)
}

/// PURE: the task id, which is the single path segment following the task path prefix.
///
/// It is the segment after the prefix rather than the URL's last segment, and the difference is not
/// cosmetic: `https://chatgpt.com/codex/tasks/` trimmed of its trailing slash ENDS in `tasks`, so a
/// last-segment read hands back the path itself as the identity of a submitted task. That is the
/// exact wrong-identity failure this function exists to prevent, and a test pins it.
///
/// Returns None for a URL with no path at all, so a stdout mentioning some other bare host cannot
/// have its hostname recorded as a task id, and None for a segment outside the acceptable charset.
/// That charset is the narrow one deliberately: this value is written to the decision log, printed
/// to a terminal, and handed to `codex cloud status` later, so a segment carrying `\x1b`, a quote, a
/// space, or a path separator is no answer rather than a plausible one. A leading `-` is rejected
/// because an id that reads as a flag to the next CLI that receives it is a footgun regardless of
/// how that CLI's parser is configured today. A deeper path under the prefix keeps its inner `/`
/// and so fails the charset check, which is the intended answer: nothing here knows what a nested
/// task path would mean, and guessing is how the wrong segment gets recorded.
///
/// **What this does NOT catch.** Same limit as above: a segment of the right shape is not a task
/// that exists. Widening the charset is a one-way door, so widen it once against an observed sample
/// recorded in a comment rather than removing the check.
fn parse_task_id(url: &str) -> Option<String> {
    let path_end = url.find(['?', '#']).unwrap_or(url.len());
    let (_, after_scheme) = url[..path_end].split_once("://")?;
    let path = &after_scheme[after_scheme.find('/')?..];
    // A trailing slash is trimmed rather than rejected, because a URL that ends in one is still
    // naming the task it names. An empty remainder then fails `is_task_id` on the next line.
    let segment = path.strip_prefix(TASK_PATH_PREFIX)?.trim_end_matches('/');
    is_task_id(segment).then(|| segment.to_string())
}

/// PURE: whether the segment following the task path prefix is acceptable as a task id. The shared
/// rule at `cloud::is_safe_identifier`, bounded by this module's own ceiling.
fn is_task_id(segment: &str) -> bool {
    crate::cloud::is_safe_identifier(segment, TASK_ID_MAX_LEN)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live shape of a Codex cloud task URL, from two real URLs observed in codex rollouts on
    /// this box: the host, the `/codex/tasks/` path, and a `task_e_` plus 32 hex id.
    const REAL_TASK_URL: &str =
        "https://chatgpt.com/codex/tasks/task_e_69b7f22993d08330827ec176e15cece9";
    const REAL_TASK_ID: &str = "task_e_69b7f22993d08330827ec176e15cece9";

    #[test]
    fn a_task_url_in_stdout_is_recorded_and_its_id_derived_from_it() {
        let stdout = format!("Submitted task.\nView it at {REAL_TASK_URL}\n");
        let url = parse_task_url(&stdout).expect("the task URL is the answer");
        assert_eq!(url, REAL_TASK_URL);
        assert_eq!(parse_task_id(&url).as_deref(), Some(REAL_TASK_ID));
    }

    #[test]
    fn a_banner_url_printed_first_does_not_beat_the_task_url() {
        let stdout = format!(
            "See https://developers.openai.com/codex/cloud for details.\nTask: {REAL_TASK_URL}\n"
        );
        assert_eq!(parse_task_url(&stdout).as_deref(), Some(REAL_TASK_URL));
    }

    #[test]
    fn a_url_on_an_unexpected_host_is_not_a_task_url() {
        for stdout in [
            // A lookalike host, the case the old prefix-free parse recorded silently.
            "https://chatgpt.com.evil.example/codex/tasks/task_e_1",
            // Userinfo: the real host is after the `@`, not before it.
            "https://chatgpt.com@evil.example/codex/tasks/task_e_1",
            // A port is not the pinned host either.
            "https://chatgpt.com:8443/codex/tasks/task_e_1",
            // The right host, but not a task.
            "https://chatgpt.com/pricing",
            // Plaintext is not accepted even on the pinned host.
            "http://chatgpt.com/codex/tasks/task_e_1",
        ] {
            assert_eq!(parse_task_url(stdout), None, "should be rejected: {stdout}");
        }
    }

    #[test]
    fn a_task_url_with_no_id_path_segment_yields_no_id() {
        // Passes the URL check (host and prefix are right) and fails at the id, which is what keeps
        // an identity-less submit an `Err` rather than a row carrying the host as its job id.
        let url = "https://chatgpt.com/codex/tasks/";
        assert_eq!(parse_task_url(url).as_deref(), Some(url));
        assert_eq!(parse_task_id(url), None);
        // A bare host has no path at all and is not a task URL in the first place.
        assert_eq!(parse_task_url("https://chatgpt.com"), None);
        assert_eq!(parse_task_id("https://chatgpt.com"), None);
    }

    #[test]
    fn the_id_is_the_segment_after_the_task_path_not_the_last_segment() {
        // A trailing slash still names its task.
        assert_eq!(
            parse_task_id(&format!("{REAL_TASK_URL}/")).as_deref(),
            Some(REAL_TASK_ID)
        );
        // A query or fragment is not part of the id.
        assert_eq!(
            parse_task_id(&format!("{REAL_TASK_URL}?attempt=2")).as_deref(),
            Some(REAL_TASK_ID)
        );
        // A deeper path keeps its inner `/` and fails the charset check rather than silently
        // handing back `diff`, which is a page about the task and not the task's identity.
        assert_eq!(
            parse_task_id("https://chatgpt.com/codex/tasks/task_e_1/diff"),
            None
        );
    }

    #[test]
    fn an_id_segment_carrying_an_ansi_escape_is_rejected() {
        let url = "https://chatgpt.com/codex/tasks/task_e_1\u{1b}[2Jspoofed";
        // The escape is not whitespace, so it survives tokenisation and the URL check.
        assert_eq!(parse_task_url(url).as_deref(), Some(url));
        // The charset is where it dies, before it can reach a log row or a terminal.
        assert_eq!(parse_task_id(url), None);
    }

    #[test]
    fn an_id_segment_outside_the_acceptable_shape_is_rejected() {
        for segment in [
            "-leading-dash",
            "has space",
            "has\"quote",
            "has'quote",
            "back\\slash",
            "tab\there",
            &"a".repeat(TASK_ID_MAX_LEN + 1),
        ] {
            assert!(!is_task_id(segment), "should be rejected: {segment:?}");
        }
        assert!(is_task_id(REAL_TASK_ID));
        assert!(is_task_id(&"a".repeat(TASK_ID_MAX_LEN)));
    }

    /// A `CloudTarget` for argv tests. Only `environment_id` and `branch` are read by `build_args`,
    /// so their values here are arbitrary placeholders rather than anything meaningful.
    fn test_target() -> CloudTarget {
        CloudTarget::resolved("env-123".to_string(), "task/some-branch".to_string())
    }

    #[test]
    fn a_task_beginning_with_double_dash_is_placed_after_the_delimiter_verbatim() {
        let args = build_args("--help", &test_target());
        assert_eq!(args.last().map(String::as_str), Some("--help"));
        assert!(
            args.iter().any(|a| a == "--"),
            "argv must carry a `--` delimiter: {args:?}"
        );
        // The delimiter must come before the task text, not after it.
        let dash_dash = args.iter().position(|a| a == "--").unwrap();
        let task_pos = args.len() - 1;
        assert!(dash_dash < task_pos, "`--` must precede the task: {args:?}");
    }

    #[test]
    fn a_task_beginning_with_a_single_dash_is_placed_after_the_delimiter_verbatim() {
        let args = build_args("-x", &test_target());
        assert_eq!(args.last().map(String::as_str), Some("-x"));
        let dash_dash = args
            .iter()
            .position(|a| a == "--")
            .expect("argv must carry a `--` delimiter");
        assert!(dash_dash < args.len() - 1, "`--` must precede the task");
    }

    #[test]
    fn an_ordinary_task_still_gets_the_delimiter_and_survives_verbatim() {
        let args = build_args("fix the login bug", &test_target());
        assert_eq!(args.last().map(String::as_str), Some("fix the login bug"));
        let dash_dash = args
            .iter()
            .position(|a| a == "--")
            .expect("argv must carry a `--` delimiter even for an ordinary task");
        assert!(dash_dash < args.len() - 1, "`--` must precede the task");
    }

    #[test]
    fn stdout_with_no_url_at_all_is_no_answer() {
        for stdout in ["", "   \n\t ", "error: environment not found\n"] {
            assert_eq!(
                parse_task_url(stdout),
                None,
                "should be rejected: {stdout:?}"
            );
        }
    }
}
