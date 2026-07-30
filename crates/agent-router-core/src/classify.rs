//! The classifier: one Claude Haiku call that scores a task against the routing rubric and
//! returns strict JSON. Every failure (missing binary, timeout, prose instead of JSON) degrades
//! to the fallback classification, because ambiguity is itself a Claude signal.

use crate::config::Config;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Which provider the rubric points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Codex,
    Claude,
}

impl Verdict {
    pub fn backend(self) -> agent_viewer_core::BackendKind {
        match self {
            Verdict::Codex => agent_viewer_core::BackendKind::Codex,
            Verdict::Claude => agent_viewer_core::BackendKind::Claude,
        }
    }
}

/// How sure the classifier is. Confident verdicts survive headroom; borderline ones are
/// decided by it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Confidence {
    High,
    Medium,
    Low,
}

/// One scored task. The two arrays are the rubric's six criteria each, in the rubric's order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Classification {
    pub codex_ready: [bool; 6],
    pub claude_signals: [bool; 6],
    pub missing_connector: bool,
    pub verdict: Verdict,
    pub confidence: Confidence,
    pub rationale: String,
    /// True when this is the fallback rather than a real score. Not part of the model's JSON.
    #[serde(default)]
    pub classifier_failed: bool,
}

impl Classification {
    /// The classification used when the classifier could not answer: Claude, low confidence.
    /// Ambiguity is itself a Claude signal, so an unavailable classifier must not silently
    /// route work to Codex.
    pub fn fallback(why: &str) -> Classification {
        Classification {
            codex_ready: [false; 6],
            claude_signals: [false; 6],
            missing_connector: false,
            verdict: Verdict::Claude,
            confidence: Confidence::Low,
            rationale: format!("classifier failed ({why}), defaulting to claude"),
            classifier_failed: true,
        }
    }

    pub fn codex_ready_count(&self) -> usize {
        self.codex_ready.iter().filter(|held| **held).count()
    }

    pub fn claude_signal_count(&self) -> usize {
        self.claude_signals.iter().filter(|held| **held).count()
    }
}

/// IMPURE: score `task` with Claude Haiku. Never fails: an unusable answer becomes the fallback.
pub fn classify(task: &str, config: &Config) -> Classification {
    let prompt = classifier_prompt(task, &config.connectors);
    let mut cmd = Command::new("claude");
    cmd.arg("-p")
        .arg("--model")
        .arg("haiku")
        .arg("--output-format")
        .arg("json")
        // No project MCP servers: the classifier is a scoring call, and loading whatever
        // servers the task's cwd configures only adds startup latency to a timed call.
        .arg("--strict-mcp-config")
        .arg(&prompt);
    // Run from home, not the task dir, for the same reason: no project config to load.
    let home = agent_viewer_core::home_dir();
    if home.is_dir() {
        cmd.current_dir(&home);
    }
    let timeout = Duration::from_secs(config.classifier_timeout_secs);
    match capture(cmd, timeout) {
        Ok(stdout) => match parse_classifier_output(&stdout) {
            Some(classification) => classification,
            None => Classification::fallback("unparseable json"),
        },
        Err(why) => Classification::fallback(&why),
    }
}

/// PURE: the classifier prompt. The rubric is verbatim from the routing plan; the connector
/// inventory is the config's, because gate 5 is scored against exactly that list.
pub fn classifier_prompt(task: &str, connectors: &[String]) -> String {
    let inventory = connectors.join(", ");
    format!(
        r#"You are a routing classifier. Score ONE task against a fixed rubric. Output ONE JSON object and NOTHING else: no prose, no reasoning, no code fence, no commentary before or after.

The anchor question: Could a capable engineer enter fresh, read the prompt and repository, run the specified checks, and finish without joining the preceding conversation? If yes, choose Codex.

Codex ready when all or nearly all hold: (1) outcome explicit, (2) source of truth in files/commands/live systems, (3) verification mechanical, (4) one coherent boundary, (5) Codex has every required connector, (6) can stop without further strategic judgment.

Claude when two or more hold: (1) requirements still being discovered, (2) dependent agents must exchange findings mid-run, (3) accumulated conversation is part of the source of truth, (4) discoveries likely reshape the plan, (5) work combines strategy + implementation + evaluation + remediation, (6) live changes need repeated risk judgment.

Hard gates, applied before any scoring: a missing connector is an automatic Claude decision regardless of shape. Durable rule as tiebreaker: Codex executes contracts, Claude manages evolving programs. Difficulty alone never routes to Claude.

Score each of the twelve criteria literally, as written. Do not invent signals: needing skill, care, or interpretation is not a Claude signal, and neither is being read-only, large, multi-repo, or long-running.

The connector inventory is authoritative: Codex on this box can reach {inventory}. Set missing_connector true ONLY when the task must reach a named system absent from that list. Never set it because you cannot see a connector yourself.

TASK
<<<
{task}
>>>

Reply with exactly this JSON object, filled in:
{{"codex_ready":[b,b,b,b,b,b],"claude_signals":[b,b,b,b,b,b],"missing_connector":false,"verdict":"codex","confidence":"high","rationale":"one sentence"}}
codex_ready and claude_signals are exactly six booleans each, in the order listed above. verdict is "codex" or "claude". confidence is "high", "medium", or "low"."#
    )
}

/// PURE: the classification out of `claude -p --output-format json` stdout. The envelope's
/// `result` field carries the model's text, which is then parsed as the classification. None
/// when either layer is unusable.
pub fn parse_classifier_output(stdout: &str) -> Option<Classification> {
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    let result = envelope.get("result")?.as_str()?;
    parse_classification(result)
}

/// PURE: the classification out of the model's own text, tolerating a code fence or a sentence
/// around the object (the object itself must still be exact).
pub fn parse_classification(text: &str) -> Option<Classification> {
    let start = text.find('{')?;
    let end = text.rfind('}')?;
    if end < start {
        return None;
    }
    let mut classification: Classification = serde_json::from_str(text.get(start..=end)?).ok()?;
    // Only `fallback` may set this; a model that echoes the field must not claim it failed.
    classification.classifier_failed = false;
    Some(classification)
}

/// IMPURE: run `cmd` to completion within `timeout`, returning stdout. The failure is a
/// sentence naming what went wrong, since it lands in the fallback's rationale and in the
/// decision log.
fn capture(mut cmd: Command, timeout: Duration) -> std::result::Result<String, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run claude: {e}"))?;
    let mut stdout = child.stdout.take().ok_or("claude gave no stdout pipe")?;
    // A reader thread, not a read after wait: a classifier answer larger than the pipe buffer
    // would otherwise block the child forever and read as a timeout.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = String::new();
        let read = stdout.read_to_string(&mut buf).is_ok();
        let _ = tx.send(read.then_some(buf));
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                return rx
                    .recv_timeout(Duration::from_secs(2))
                    .ok()
                    .flatten()
                    .ok_or_else(|| "claude stdout was unreadable".to_string());
            }
            Ok(Some(status)) => return Err(format!("claude exited {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not wait for claude: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The live envelope shape of `claude -p --output-format json`, trimmed to the keys that
    /// matter plus a couple of neighbours.
    fn envelope(result: &str) -> String {
        serde_json::json!({
            "type": "result",
            "subtype": "success",
            "is_error": false,
            "duration_ms": 16423,
            "result": result,
        })
        .to_string()
    }

    const GOOD: &str = r#"{"codex_ready":[true,true,true,true,true,true],
        "claude_signals":[false,false,false,false,false,false],
        "missing_connector":false,"verdict":"codex","confidence":"high",
        "rationale":"explicit outcome, mechanical verification"}"#;

    #[test]
    fn a_well_formed_answer_parses_out_of_the_cli_envelope() {
        let got = parse_classifier_output(&envelope(GOOD)).expect("parses");
        assert_eq!(got.verdict, Verdict::Codex);
        assert_eq!(got.confidence, Confidence::High);
        assert_eq!(got.codex_ready_count(), 6);
        assert_eq!(got.claude_signal_count(), 0);
        assert!(!got.missing_connector);
        assert!(!got.classifier_failed);
    }

    #[test]
    fn a_fenced_or_prefaced_answer_still_parses() {
        let fenced = format!("Here is the scoring:\n```json\n{GOOD}\n```\n");
        let got = parse_classifier_output(&envelope(&fenced)).expect("parses");
        assert_eq!(got.verdict, Verdict::Codex);
    }

    #[test]
    fn claude_signals_and_missing_connector_come_through_in_rubric_order() {
        let text = r#"{"codex_ready":[true,false,false,true,false,false],
            "claude_signals":[true,false,true,false,true,false],
            "missing_connector":true,"verdict":"claude","confidence":"medium",
            "rationale":"needs n8n"}"#;
        let got = parse_classifier_output(&envelope(text)).expect("parses");
        assert_eq!(got.claude_signals, [true, false, true, false, true, false]);
        assert_eq!(got.codex_ready, [true, false, false, true, false, false]);
        assert_eq!(got.claude_signal_count(), 3);
        assert!(got.missing_connector);
        assert_eq!(got.verdict, Verdict::Claude);
    }

    #[test]
    fn unusable_answers_are_none_so_the_caller_falls_back() {
        // Prose with no object, an object missing required fields, a bogus enum value, and an
        // envelope with no result field at all.
        assert!(parse_classifier_output(&envelope("I cannot score this task.")).is_none());
        assert!(parse_classifier_output(&envelope(r#"{"verdict":"codex"}"#)).is_none());
        assert!(
            parse_classifier_output(&envelope(
                &GOOD.replace("\"confidence\":\"high\"", "\"confidence\":\"certain\"")
            ))
            .is_none()
        );
        assert!(parse_classifier_output(r#"{"type":"result","is_error":true}"#).is_none());
        assert!(parse_classifier_output("not json at all").is_none());
    }

    #[test]
    fn an_array_of_the_wrong_length_is_rejected_rather_than_padded() {
        let short = GOOD.replace("[true,true,true,true,true,true]", "[true,true,true]");
        assert!(parse_classifier_output(&envelope(&short)).is_none());
    }

    #[test]
    fn a_model_claiming_classifier_failed_does_not_get_to_set_it() {
        let text = GOOD.replace(
            "\"missing_connector\":false",
            "\"missing_connector\":false,\"classifier_failed\":true",
        );
        let got = parse_classifier_output(&envelope(&text)).expect("parses");
        assert!(
            !got.classifier_failed,
            "only the fallback constructor may flag a classifier failure"
        );
    }

    #[test]
    fn the_fallback_is_low_confidence_claude_and_says_why() {
        let got = Classification::fallback("timed out after 30s");
        assert_eq!(got.verdict, Verdict::Claude);
        assert_eq!(got.confidence, Confidence::Low);
        assert!(got.classifier_failed);
        assert!(got.rationale.contains("timed out after 30s"));
    }

    #[test]
    fn the_prompt_carries_the_rubric_verbatim_and_the_configured_inventory() {
        let prompt = classifier_prompt(
            "do a thing",
            &["local shell".to_string(), "airtable".to_string()],
        );
        assert!(prompt.contains(
            "Could a capable engineer enter fresh, read the prompt and repository, run the \
             specified checks, and finish without joining the preceding conversation?"
        ));
        assert!(prompt.contains("(6) can stop without further strategic judgment"));
        assert!(prompt.contains("(6) live changes need repeated risk judgment"));
        assert!(prompt.contains("a missing connector is an automatic Claude decision"));
        assert!(prompt.contains("Difficulty alone never routes to Claude"));
        assert!(prompt.contains("Codex on this box can reach local shell, airtable"));
        assert!(prompt.contains("do a thing"));
    }

    #[test]
    fn capture_reports_a_timeout_rather_than_hanging() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        let start = Instant::now();
        let err = capture(cmd, Duration::from_millis(300)).expect_err("must time out");
        assert!(err.contains("timed out"), "got {err:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn capture_returns_stdout_on_success_and_names_a_nonzero_exit() {
        let mut ok = Command::new("sh");
        ok.arg("-c").arg("printf hi");
        assert_eq!(capture(ok, Duration::from_secs(5)), Ok("hi".to_string()));

        let mut bad = Command::new("sh");
        bad.arg("-c").arg("exit 7");
        let err = capture(bad, Duration::from_secs(5)).expect_err("must fail");
        assert!(err.contains("exited"), "got {err:?}");
    }
}
