//! The classifier: one small-model call that scores a task against the routing rubric and
//! returns strict JSON. Which engine and model make that call is configured; every failure
//! retains the configured default and stays eligible for weekly routing.

use crate::config::{Classifier, ClassifierEngine, Config, DefaultProvider};
use crate::provider::Provider;
use crate::runtime::home_dir;
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
    pub const fn provider(self) -> Provider {
        match self {
            Verdict::Codex => Provider::Codex,
            Verdict::Claude => Provider::Claude,
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

/// How much reasoning the task needs. Orthogonal to the verdict: either provider can take a
/// simple task. This picks the model the job runs on; the model's own default effort then
/// follows from it, because the model is the better toggle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Complexity {
    Low,
    Medium,
    /// The default: what an unscored, an old, or an unparseable answer reads as. Unscored work
    /// errs toward capability, so it is high rather than the middle of the ladder.
    #[default]
    High,
    /// The rare top tier. On claude it is the only tier that reaches fable, so the rubric keeps
    /// it deliberately hard to earn.
    Ultra,
}

impl Complexity {
    pub fn tag(self) -> &'static str {
        match self {
            Complexity::Low => "low",
            Complexity::Medium => "medium",
            Complexity::High => "high",
            Complexity::Ultra => "ultra",
        }
    }
}

/// One scored task. The two arrays are the rubric's six criteria each, in the rubric's order.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Classification {
    pub codex_ready: [bool; 6],
    pub claude_signals: [bool; 6],
    pub missing_connector: bool,
    pub verdict: Verdict,
    pub confidence: Confidence,
    /// Absent from an older log row or an answer that omitted it, which both read as high.
    #[serde(default)]
    pub complexity: Complexity,
    pub rationale: String,
    /// True when this is the fallback rather than a real score. Not part of the model's JSON.
    #[serde(default)]
    pub classifier_failed: bool,
}

impl Classification {
    /// The classification used when the classifier could not answer. The configured default
    /// remains eligible for weekly routing, so failure does not invent a capability signal.
    pub fn fallback(why: &str, default_provider: DefaultProvider) -> Classification {
        let (verdict, provider_name) = match default_provider {
            DefaultProvider::Codex => (Verdict::Codex, "codex"),
            DefaultProvider::Claude => (Verdict::Claude, "claude"),
        };
        Classification {
            codex_ready: [false; 6],
            claude_signals: [false; 6],
            missing_connector: false,
            verdict,
            confidence: Confidence::Low,
            complexity: Complexity::High,
            rationale: format!("classifier failed ({why}), defaulting to {provider_name}"),
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

/// IMPURE: score `task` with the configured classifier engine. Never fails: an unusable answer
/// becomes the fallback.
pub fn classify(task: &str, config: &Config) -> Classification {
    let prompt = classifier_prompt(task, &config.connectors);
    let timeout = Duration::from_secs(config.classifier_timeout_secs);
    let engine = config.classifier.engine;
    let cmd = classifier_command(&prompt, &config.classifier);
    match capture(cmd, engine.name(), timeout) {
        Ok(stdout) => match parse_classifier_output(&stdout, engine) {
            Some(classification) => classification,
            None => Classification::fallback("unparseable json", config.policy.default_provider),
        },
        Err(why) => Classification::fallback(&why, config.policy.default_provider),
    }
}

/// PURE builder: the classifier invocation for the configured engine.
pub fn classifier_command(prompt: &str, classifier: &Classifier) -> Command {
    match classifier.engine {
        ClassifierEngine::Claude => claude_classifier_command(prompt, &classifier.claude_model),
        ClassifierEngine::Codex => codex_classifier_command(prompt, &classifier.codex_model),
    }
}

/// PURE builder: the claude classifier invocation. Every flag here is about the CLI's own startup
/// cost, which dominated this call and is the whole reason the 30s timeout is viable.
///
/// Measured on this box 2026-07-30: plain `claude -p --model haiku --output-format json` spent
/// ~14s before it even issued the API request (hooks, plugin sync, auto-memory, CLAUDE.md
/// discovery) and took 29-38s wall, so it lost a 30s deadline about half the time.
/// `CLAUDE_SUBPROCESS=1` plus `--safe-mode` (all customizations off: CLAUDE.md, skills, plugins,
/// hooks, MCP servers, commands, agents) took time-to-request to ~27ms and the whole call to
/// 12-16s. `--bare` would do the same but is unusable here: it never reads OAuth or the keychain,
/// so it answers "Not logged in". This is the same posture the compound-learning hooks use for
/// their own haiku calls.
///
/// `--safe-mode` also makes the scoring hermetic, which matters independently of speed: the
/// verdict must not shift because a project's CLAUDE.md or a skill happened to load.
pub fn claude_classifier_command(prompt: &str, model: &str) -> Command {
    let mut cmd = Command::new("claude");
    cmd.env("CLAUDE_SUBPROCESS", "1")
        .arg("-p")
        .arg("--model")
        .arg(model)
        .arg("--output-format")
        .arg("json")
        .arg("--no-session-persistence")
        .arg("--safe-mode")
        .arg("--strict-mcp-config")
        .arg(prompt);
    run_from_home(&mut cmd);
    cmd
}

/// The capabilities the classifier is stripped of. Scoring reads a prompt and answers with one
/// JSON object, so it needs no tool at all, and a tool left in the set is both prompt tokens and
/// something an injected task could reach for: the read-only sandbox stops writes but not reads,
/// and the call runs from home. Dropping the shell is what makes "the classifier cannot read your
/// files" true by construction rather than by the model's good behaviour.
///
/// Measured on this box 2026-07-30: 15.2k prompt tokens and 2.5s against 18.3k and 6.7s with the
/// full tool set.
const DISABLED_FEATURES: [&str; 6] = [
    "shell_tool",
    "browser_use",
    "computer_use",
    "image_generation",
    "apps",
    "skill_search",
];

/// PURE builder: the codex classifier invocation. Same posture as the claude one, expressed in
/// codex's own flags: every customization off, so the score depends on the rubric and the task
/// and on nothing else this box happens to have configured.
///
/// Measured on this box 2026-07-30, scoring the fixed prompt from home: 4-8s wall against
/// claude haiku's 12-16s, so it clears the same 30s deadline with room to spare.
/// `--ignore-user-config` is the load-bearing one (it drops `~/.codex/config.toml` and with it
/// every MCP server, which was ~3.7k of prompt and most of the wall time); `--ignore-rules` drops
/// execpolicy, `-c project_doc_max_bytes=0` suppresses AGENTS.md discovery, and
/// `--skip-git-repo-check` is required because home is not a repository. The sandbox is read-only:
/// scoring reads a prompt and answers, and must never be able to touch the box.
///
/// Deliberately NOT `--ephemeral`, though it would suit a throwaway call: `codex_headroom` reads
/// the newest rollout carrying a `rate_limits` event, and an ephemeral run writes no rollout. On
/// this engine the classifier fires on every auto-routed task, so suppressing those rollouts would
/// let scoring burn codex quota while the router kept deciding against the last dispatched job's
/// percentage, and a codex at its ceiling would keep reading as having headroom. Persisting the
/// rollout costs a session file per task and keeps the routing input honest.
pub fn codex_classifier_command(prompt: &str, model: &str) -> Command {
    let mut cmd = Command::new("codex");
    cmd.arg("exec")
        .arg("--model")
        .arg(model)
        .arg("--json")
        .arg("--sandbox")
        .arg("read-only")
        .arg("--skip-git-repo-check")
        .arg("--ignore-user-config")
        .arg("--ignore-rules")
        .arg("-c")
        .arg("project_doc_max_bytes=0");
    for feature in DISABLED_FEATURES {
        cmd.arg("--disable").arg(feature);
    }
    cmd.arg(prompt);
    run_from_home(&mut cmd);
    cmd
}

/// Run from home, never the task dir: nothing about the task's own project should be loaded.
fn run_from_home(cmd: &mut Command) {
    let home = home_dir();
    if home.is_dir() {
        cmd.current_dir(&home);
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

Judge the task as stated, at the level of detail it is stated. When the task says the commands, metrics, baselines, or acceptance criteria are specified, take that as given and score criteria 1 to 3 as held; a summary that does not inline them is NOT requirements still being discovered. Signal 5 needs all four of strategy, implementation, evaluation, and remediation, not evaluation alone. Signal 2 needs the task to actually call for several agents exchanging findings.

Separately, and independently of the verdict, judge how much reasoning the task needs. complexity is "low" when it is conversational, one step, mechanical, or a single file with an obvious answer; "medium" for a normal well scoped implementation or investigation; "high" when it spans several files or is subtle enough to need heavy reasoning or design judgment; "ultra" only for the rare hardest work, where a wrong call is expensive and hard to reverse: architecture or plan review, a root cause hunt that has already defeated ordinary debugging, or a design decision that sets a direction. Ultra is not "large" or "long running", and it is not "important to the user": when torn between high and ultra, answer high. Complexity is orthogonal to the provider: a low task can belong to either provider, and so can an ultra one. Never let complexity change the verdict, and never let the verdict change complexity.

The connector inventory is authoritative: Codex on this box can reach {inventory}. Set missing_connector true ONLY when the task must reach a named system absent from that list. Never set it because you cannot see a connector yourself.

TASK
<<<
{task}
>>>

Reply with exactly this JSON object, filled in:
{{"codex_ready":[b,b,b,b,b,b],"claude_signals":[b,b,b,b,b,b],"missing_connector":false,"verdict":"codex","confidence":"high","complexity":"medium","rationale":"one sentence"}}
codex_ready and claude_signals are exactly six booleans each, in the order listed above. verdict is "codex" or "claude". confidence is "high", "medium", or "low". complexity is "low", "medium", "high", or "ultra"."#
    )
}

/// PURE: the classification out of `engine`'s stdout. Each engine wraps the model's text in its
/// own envelope, so unwrapping is per engine and the classification parse below is shared.
pub fn parse_classifier_output(stdout: &str, engine: ClassifierEngine) -> Option<Classification> {
    let text = match engine {
        ClassifierEngine::Claude => claude_answer(stdout)?,
        ClassifierEngine::Codex => codex_answer(stdout)?,
    };
    parse_classification(&text)
}

/// PURE: the model's text out of `claude -p --output-format json` stdout, which is one JSON
/// envelope whose `result` field carries the answer.
fn claude_answer(stdout: &str) -> Option<String> {
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim()).ok()?;
    Some(envelope.get("result")?.as_str()?.to_string())
}

/// PURE: the model's text out of `codex exec --json` stdout, which is JSONL: thread and turn
/// events interleaved with items. The answer is the last completed `agent_message`, taken from
/// the end so a preamble message cannot be mistaken for the verdict. Anything unparseable or of
/// another shape is skipped rather than failing, because the stream carries non-item lines by
/// design and a failed turn simply never emits an agent message.
fn codex_answer(stdout: &str) -> Option<String> {
    stdout.lines().rev().find_map(|line| {
        let event: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
        if event.get("type")?.as_str()? != "item.completed" {
            return None;
        }
        let item = event.get("item")?;
        if item.get("type")?.as_str()? != "agent_message" {
            return None;
        }
        Some(item.get("text")?.as_str()?.to_string())
    })
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
/// decision log. `engine` names the CLI in those messages, so a fallback says which classifier
/// failed rather than always blaming claude.
fn capture(
    mut cmd: Command,
    engine: &str,
    timeout: Duration,
) -> std::result::Result<String, String> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("could not run {engine}: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{engine} gave no stdout pipe"))?;
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
                    .ok_or_else(|| format!("{engine} stdout was unreadable"));
            }
            Ok(Some(status)) => return Err(format!("{engine} exited {status}")),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!("timed out after {}s", timeout.as_secs()));
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => return Err(format!("could not wait for {engine}: {e}")),
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

    fn parse_claude(stdout: &str) -> Option<Classification> {
        parse_classifier_output(stdout, ClassifierEngine::Claude)
    }

    /// The live JSONL shape of `codex exec --json`: thread and turn events around the items,
    /// with the answer carried by a completed `agent_message`.
    fn stream(answer: &str) -> String {
        [
            serde_json::json!({"type": "thread.started", "thread_id": "019fb3ab-8d2a-7d21"}),
            serde_json::json!({"type": "turn.started"}),
            serde_json::json!({
                "type": "item.completed",
                "item": {"id": "item_0", "type": "agent_message", "text": answer},
            }),
            serde_json::json!({
                "type": "turn.completed",
                "usage": {"input_tokens": 17512, "output_tokens": 132},
            }),
        ]
        .map(|event| event.to_string())
        .join("\n")
    }

    fn parse_codex(stdout: &str) -> Option<Classification> {
        parse_classifier_output(stdout, ClassifierEngine::Codex)
    }

    const GOOD: &str = r#"{"codex_ready":[true,true,true,true,true,true],
        "claude_signals":[false,false,false,false,false,false],
        "missing_connector":false,"verdict":"codex","confidence":"high",
        "rationale":"explicit outcome, mechanical verification"}"#;

    #[test]
    fn a_well_formed_answer_parses_out_of_the_cli_envelope() {
        let got = parse_claude(&envelope(GOOD)).expect("parses");
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
        let got = parse_claude(&envelope(&fenced)).expect("parses");
        assert_eq!(got.verdict, Verdict::Codex);
    }

    #[test]
    fn claude_signals_and_missing_connector_come_through_in_rubric_order() {
        let text = r#"{"codex_ready":[true,false,false,true,false,false],
            "claude_signals":[true,false,true,false,true,false],
            "missing_connector":true,"verdict":"claude","confidence":"medium",
            "rationale":"needs n8n"}"#;
        let got = parse_claude(&envelope(text)).expect("parses");
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
        assert!(parse_claude(&envelope("I cannot score this task.")).is_none());
        assert!(parse_claude(&envelope(r#"{"verdict":"codex"}"#)).is_none());
        assert!(
            parse_claude(&envelope(
                &GOOD.replace("\"confidence\":\"high\"", "\"confidence\":\"certain\"")
            ))
            .is_none()
        );
        assert!(parse_claude(r#"{"type":"result","is_error":true}"#).is_none());
        assert!(parse_claude("not json at all").is_none());
    }

    /// Complexity is what picks the model, so each value must survive the parse, and an answer
    /// that omits the field must read as high rather than failing the whole classification.
    #[test]
    fn complexity_parses_and_an_omitted_one_reads_as_high() {
        for (answer, want) in [
            ("low", Complexity::Low),
            ("medium", Complexity::Medium),
            ("high", Complexity::High),
            ("ultra", Complexity::Ultra),
        ] {
            let text = GOOD.replace(
                "\"missing_connector\":false",
                &format!("\"missing_connector\":false,\"complexity\":\"{answer}\""),
            );
            let got = parse_claude(&envelope(&text)).expect("parses");
            assert_eq!(got.complexity, want);
            assert_eq!(got.verdict, Verdict::Codex, "complexity is not the verdict");
        }

        assert!(!GOOD.contains("complexity"), "the fixture omits the field");
        let omitted = parse_claude(&envelope(GOOD)).expect("parses");
        assert_eq!(omitted.complexity, Complexity::High);

        let bogus = GOOD.replace(
            "\"missing_connector\":false",
            "\"missing_connector\":false,\"complexity\":\"epic\"",
        );
        assert!(parse_claude(&envelope(&bogus)).is_none());
    }

    #[test]
    fn an_array_of_the_wrong_length_is_rejected_rather_than_padded() {
        let short = GOOD.replace("[true,true,true,true,true,true]", "[true,true,true]");
        assert!(parse_claude(&envelope(&short)).is_none());
    }

    #[test]
    fn a_model_claiming_classifier_failed_does_not_get_to_set_it() {
        let text = GOOD.replace(
            "\"missing_connector\":false",
            "\"missing_connector\":false,\"classifier_failed\":true",
        );
        let got = parse_claude(&envelope(&text)).expect("parses");
        assert!(
            !got.classifier_failed,
            "only the fallback constructor may flag a classifier failure"
        );
    }

    #[test]
    fn the_fallback_is_low_confidence_claude_and_says_why() {
        let got = Classification::fallback("timed out after 30s", DefaultProvider::Claude);
        assert_eq!(got.verdict, Verdict::Claude);
        assert_eq!(got.confidence, Confidence::Low);
        assert_eq!(got.complexity, Complexity::High);
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
        // The anti-drift instructions are load-bearing: without them haiku reads a task summary
        // that does not inline its metrics as "requirements still being discovered" and routes a
        // bounded evaluation to claude.
        assert!(prompt.contains("Judge the task as stated, at the level of detail it is stated."));
        assert!(prompt.contains(
            "Signal 5 needs all four of strategy, implementation, evaluation, and remediation"
        ));
        assert!(prompt.contains("Codex on this box can reach local shell, airtable"));
        assert!(prompt.contains("do a thing"));
        // The complexity rubric and its independence from the verdict, which is what stops a
        // trivial conversational task from being scored hard just because it routed to codex.
        assert!(prompt.contains("conversational, one step, mechanical, or a single file"));
        assert!(prompt.contains("a normal well scoped implementation or investigation"));
        assert!(prompt.contains("Complexity is orthogonal to the provider"));
        // Ultra is the only tier that reaches fable, so the brake on over-assigning it is
        // load-bearing rather than decorative.
        assert!(prompt.contains("when torn between high and ultra, answer high"));
        assert!(prompt.contains("\"complexity\":\"medium\""));
        assert!(prompt.contains("complexity is \"low\", \"medium\", \"high\", or \"ultra\""));
    }

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The startup-stripping flags are the reason the classifier fits its timeout at all, so they
    /// are pinned: losing one silently doubles the call and every task starts falling back to
    /// claude with `classifier_failed`.
    #[test]
    fn the_claude_invocation_pins_the_startup_stripping_flags() {
        let cmd = claude_classifier_command("score this", "haiku");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("claude"));
        let args = args_of(&cmd);
        for flag in [
            "-p",
            "--output-format",
            "json",
            "--no-session-persistence",
            "--safe-mode",
            "--strict-mcp-config",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag} must be passed");
        }
        let model = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[model + 1], "haiku");
        assert_eq!(args.last().map(String::as_str), Some("score this"));

        // `--safe-mode` disables hooks, but the env var is what keeps a hook-driven harness from
        // treating this as a fresh interactive session.
        let subprocess = cmd
            .get_envs()
            .find(|(key, _)| *key == std::ffi::OsStr::new("CLAUDE_SUBPROCESS"))
            .and_then(|(_, value)| value)
            .map(|value| value.to_string_lossy().into_owned());
        assert_eq!(subprocess.as_deref(), Some("1"));
    }

    /// Same contract on the codex side: these flags are what make the call fast and hermetic, and
    /// the read-only sandbox is what keeps a scoring call from touching the box.
    #[test]
    fn the_codex_invocation_pins_its_hermetic_flags_and_a_read_only_sandbox() {
        let cmd = codex_classifier_command("score this", "gpt-5.6-luna");
        assert_eq!(cmd.get_program(), std::ffi::OsStr::new("codex"));
        let args = args_of(&cmd);
        assert_eq!(args.first().map(String::as_str), Some("exec"));
        for flag in [
            "--json",
            "--skip-git-repo-check",
            "--ignore-user-config",
            "--ignore-rules",
        ] {
            assert!(args.contains(&flag.to_string()), "{flag} must be passed");
        }
        let sandbox = args.iter().position(|a| a == "--sandbox").expect("sandbox");
        assert_eq!(args[sandbox + 1], "read-only");
        let config = args.iter().position(|a| a == "-c").expect("-c");
        assert_eq!(args[config + 1], "project_doc_max_bytes=0");
        let model = args.iter().position(|a| a == "--model").expect("--model");
        assert_eq!(args[model + 1], "gpt-5.6-luna");
        assert_eq!(args.last().map(String::as_str), Some("score this"));

        // Scoring must reach the model with no tool it could be talked into using.
        for feature in DISABLED_FEATURES {
            let at = args
                .iter()
                .position(|a| a == feature)
                .unwrap_or_else(|| panic!("{feature} must be passed"));
            assert_eq!(args[at - 1], "--disable");
        }

        // `--ephemeral` would suppress the rollout that `codex_headroom` reads, so scoring would
        // spend codex quota invisibly and the router would keep deciding on a stale percentage.
        assert!(
            !args.contains(&"--ephemeral".to_string()),
            "the classifier rollout is what keeps the codex usage reading fresh"
        );
    }

    /// codex rejects an unknown feature name outright, and its own list shows names do get
    /// retired. A retirement would therefore make every scoring call exit nonzero, which the
    /// router absorbs as `classifier_failed` and a silent fall back to the default provider on
    /// every task. This is the loud version of that failure, run against the installed codex.
    #[test]
    fn every_disabled_feature_is_a_name_the_installed_codex_still_knows() {
        let Ok(listed) = Command::new("codex").arg("features").arg("list").output() else {
            // No codex on this box: it cannot be the classifier engine here either.
            return;
        };
        if !listed.status.success() {
            return;
        }
        let stdout = String::from_utf8_lossy(&listed.stdout);
        let known: Vec<&str> = stdout
            .lines()
            .filter_map(|line| line.split_whitespace().next())
            .collect();
        assert!(!known.is_empty(), "`codex features list` printed nothing");
        for feature in DISABLED_FEATURES {
            assert!(
                known.contains(&feature),
                "codex no longer knows the feature `{feature}`, so every classifier call on the \
                 codex engine would exit nonzero and fall back"
            );
        }
    }

    /// The engine setting is what picks the CLI, and each engine takes its own configured model
    /// rather than the other's.
    #[test]
    fn the_engine_setting_picks_the_cli_and_that_engines_model() {
        let mut classifier = Classifier {
            engine: ClassifierEngine::Claude,
            claude_model: "haiku".to_string(),
            codex_model: "gpt-5.6-luna".to_string(),
        };

        let on_claude = classifier_command("score this", &classifier);
        assert_eq!(on_claude.get_program(), std::ffi::OsStr::new("claude"));
        assert!(args_of(&on_claude).contains(&"haiku".to_string()));
        assert!(!args_of(&on_claude).contains(&"gpt-5.6-luna".to_string()));

        classifier.engine = ClassifierEngine::Codex;
        let on_codex = classifier_command("score this", &classifier);
        assert_eq!(on_codex.get_program(), std::ffi::OsStr::new("codex"));
        assert!(args_of(&on_codex).contains(&"gpt-5.6-luna".to_string()));
        assert!(!args_of(&on_codex).contains(&"haiku".to_string()));

        // A retuned model reaches the invocation; nothing pins the catalogue names in code.
        classifier.codex_model = "gpt-5.6-terra".to_string();
        let retuned = classifier_command("score this", &classifier);
        assert!(args_of(&retuned).contains(&"gpt-5.6-terra".to_string()));
    }

    /// The codex envelope is JSONL rather than one object, so the answer is dug out of the event
    /// stream. The same rubric answer must classify identically on either engine: the engine is
    /// who scores, never what the score means.
    #[test]
    fn a_codex_answer_parses_out_of_the_jsonl_event_stream() {
        let got = parse_codex(&stream(GOOD)).expect("parses");
        assert_eq!(got, parse_claude(&envelope(GOOD)).expect("parses"));
        assert_eq!(got.verdict, Verdict::Codex);
        assert_eq!(got.confidence, Confidence::High);
        assert!(!got.classifier_failed);

        let fenced = format!("Here is the scoring:\n```json\n{GOOD}\n```\n");
        assert_eq!(
            parse_codex(&stream(&fenced)).expect("parses").verdict,
            Verdict::Codex
        );
    }

    /// A turn can emit several messages; the verdict is the last one, not a preamble.
    #[test]
    fn the_last_codex_agent_message_is_the_answer() {
        let preamble = serde_json::json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "agent_message", "text": "Scoring the task now."},
        })
        .to_string();
        let stdout = format!("{preamble}\n{}", stream(GOOD));
        assert_eq!(
            parse_codex(&stdout).expect("parses").verdict,
            Verdict::Codex
        );
    }

    /// Reasoning and command items ride the same stream, and a failed turn emits no agent message
    /// at all. None of those may be read as a verdict.
    #[test]
    fn codex_streams_without_a_usable_agent_message_are_none_so_the_caller_falls_back() {
        // A turn that failed outright.
        let failed = [
            serde_json::json!({"type": "thread.started", "thread_id": "t"}),
            serde_json::json!({"type": "turn.failed", "error": {"message": "model overloaded"}}),
        ]
        .map(|event| event.to_string())
        .join("\n");
        assert!(parse_codex(&failed).is_none());

        // Non-message items must not be mined for a verdict, even when one contains the JSON.
        let reasoning = serde_json::json!({
            "type": "item.completed",
            "item": {"id": "item_0", "type": "reasoning", "text": GOOD},
        })
        .to_string();
        assert!(parse_codex(&reasoning).is_none());

        // An agent message that is not a classification, and an empty stream.
        assert!(parse_codex(&stream("I cannot score this task.")).is_none());
        assert!(parse_codex("").is_none());
        assert!(parse_codex("not json at all").is_none());
    }

    /// Each engine's envelope is read only by its own parser: reading a codex stream as a claude
    /// envelope (or the reverse) must fail rather than half-parse.
    #[test]
    fn an_engines_output_does_not_parse_as_the_other_engines() {
        assert!(parse_claude(&stream(GOOD)).is_none());
        assert!(parse_codex(&envelope(GOOD)).is_none());
    }

    #[test]
    fn capture_reports_a_timeout_rather_than_hanging() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("sleep 5");
        let start = Instant::now();
        let err = capture(cmd, "codex", Duration::from_millis(300)).expect_err("must time out");
        assert!(err.contains("timed out"), "got {err:?}");
        assert!(start.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn capture_returns_stdout_on_success_and_names_a_nonzero_exit() {
        let mut ok = Command::new("sh");
        ok.arg("-c").arg("printf hi");
        assert_eq!(
            capture(ok, "claude", Duration::from_secs(5)),
            Ok("hi".to_string())
        );

        let mut bad = Command::new("sh");
        bad.arg("-c").arg("exit 7");
        let err = capture(bad, "codex", Duration::from_secs(5)).expect_err("must fail");
        assert!(err.contains("exited"), "got {err:?}");
        // The failure sentence lands in the decision log, so it must name the engine that
        // actually ran rather than always blaming claude.
        assert!(err.contains("codex"), "got {err:?}");
    }
}
