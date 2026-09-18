//! TypeSafe Jev scoring: one System One call, composition in code.

use super::{Classification, Complexity, TaskContextHorizon, invokes_implement};
use crate::context::Context;
use serde_json::{Value, json};
use std::time::Duration;

pub const NOUL_YES: f64 = 0.70;
#[allow(dead_code)]
pub const NOUL_NO: f64 = 0.30;
pub const MUST_REACH_YES: f64 = 0.50;
pub const NOTHING_TO_SCORE_YES: f64 = 0.50;
pub const ULTRA_MARGIN: f64 = 0.15;
#[allow(dead_code)]
pub const DEFAULT_MODEL: &str = "jev-1.13.0";
pub const ENDPOINT: &str = "https://api.typesafe.ai/v1/systemone";

const COMPLEXITY_LEVELS: [&str; 4] = ["low", "medium", "high", "ultra"];

/// PURE: the questions map for one classify call. `named_system` options follow `connectors`.
pub fn questions(connectors: &[String]) -> Value {
    let mut named = serde_json::Map::new();
    named.insert(
        "none".to_string(),
        json!("No named external system must be reached now."),
    );
    for connector in connectors {
        named.insert(
            connector.clone(),
            json!(format!(
                "The task must now reach this listed connector: {connector}"
            )),
        );
    }
    named.insert(
        "other".to_string(),
        json!({
            "what": "Any system the task must reach now is absent from connector_inventory",
            "precedence": "other wins if any required-now system is unlisted, even when another required system is listed",
            "not_for": "Systems named only for later work, docs, identifiers, or research",
        }),
    );

    json!({
        "nothing_to_score": {
            "type": "noul",
            "instructions": "Is `task` empty, a greeting, a single word, or a fragment nobody could act on?",
            "criteria": {
                "true": "Nothing to score",
                "false": "There is a real task"
            }
        },
        "several_agents": {
            "type": "noul",
            "instructions": {
                "question": "Must several agents run as part of `task`?",
                "judge": "Never infer this from difficulty, file count, duration, importance, or the words agents, subagents, or team."
            },
            "criteria": {
                "true": "More than one agent must run",
                "false": "One agent working alone, or a mere mention of agents"
            }
        },
        "mid_run_exchange": {
            "type": "noul",
            "instructions": {
                "question": "Must those agents pass findings to each other partway through `task`?",
                "judge": "Orchestration is never inferred from how difficult the task is, how large its scope is, or how many files, directories or repositories it touches."
            },
            "criteria": {
                "true": "Findings are exchanged mid-run",
                "false": "No mid-run exchange. Planning, reviewing, investigating, and debugging are not enough."
            }
        },
        "findings_change_next": {
            "type": "noul",
            "instructions": "Do those exchanged findings change what another agent does next?",
            "criteria": {
                "true": "Later work depends on earlier findings",
                "false": "Agents do not change each other's next step"
            }
        },
        "must_now_reach": {
            "type": "noul",
            "instructions": {
                "question": "Must `task` now reach a named external system?",
                "inspect": "task",
                "inventory": "connector_inventory"
            },
            "criteria": {
                "true": {
                    "what": "The task must now read or write a named external system",
                    "examples": ["Connect to Slack and retrieve standup", "Read last week's Granola transcripts"]
                },
                "false": {
                    "what": "No named system must be reached now",
                    "not_for": "Claude Code skills, local files, local SQLite, git, gh, ntfy, ZLog, public web, Twitter, usage JSONLs, researching or proposing a connector, naming a system a later job would use",
                    "examples": ["Fix a typo in a README that describes n8n", "Delete slack_webhook_url", "Grep the repo for Snowflake"]
                }
            }
        },
        "named_system": {
            "type": "choice",
            "instructions": {
                "question": "Which connector bucket does `task` require now?",
                "focus": "Pick other if any system that must be reached now is absent from connector_inventory."
            },
            "criteria": named
        },
        "complexity": {
            "type": "score",
            "instructions": {
                "question": "How much reasoning does the requested outcome of `task` need?",
                "judge": "Independent of orchestration, missing connectors, provider, duration, and importance."
            },
            "criteria": [
                "Conversational, one step, mechanical, or a single file with an obvious answer. Direct definition, location, transcription, or single fact retrieval when the answer is direct. Empty or unscoreable input.",
                "A normal well scoped implementation or investigation.",
                "Spans several files, or needs heavy reasoning or design judgment: synthesizing evidence, comparing options, tradeoffs, prioritizing, recommending, or choosing among options.",
                "The rare hardest work, where a wrong call is expensive and hard to reverse: architecture or plan review, a root cause hunt that already defeated ordinary debugging, or a design decision that sets a direction. Not large, long running, or important to the user."
            ]
        },
        "task_context_horizon": {
            "type": "choice",
            "instructions": {
                "question": "How much retained working context does `task` explicitly require?",
                "judge": "Independent from complexity, orchestration, duration, importance, and file count. Difficult or long running bounded work is ordinary."
            },
            "criteria": {
                "ordinary": "The default. Bounded work, even if difficult or long running.",
                "extended": "Explicitly requires a large corpus, resuming work whose prior history must remain available, or sustained synthesis across many artifacts or steps."
            }
        }
    })
}

/// PURE: turn a TypeSafe `answers` object into a classification, or None if unusable.
pub fn compose(answers: &Value) -> Option<Classification> {
    let nothing = noul(answers, "nothing_to_score")?;
    let several = noul(answers, "several_agents")?;
    let exchange = noul(answers, "mid_run_exchange")?;
    let changes = noul(answers, "findings_change_next")?;
    let reach = noul(answers, "must_now_reach")?;
    let named = choice(answers, "named_system")?;
    let complexity = complexity_from_score(answers.get("complexity")?)?;
    let horizon = match choice(answers, "task_context_horizon")?.as_str() {
        "ordinary" => TaskContextHorizon::Ordinary,
        "extended" => TaskContextHorizon::Extended,
        _ => return None,
    };
    if nothing >= NOTHING_TO_SCORE_YES {
        return Some(scored(
            false,
            false,
            Complexity::Low,
            TaskContextHorizon::Ordinary,
            "nothing to score",
        ));
    }

    let orch = conjunct_true(several) && conjunct_true(exchange) && conjunct_true(changes);
    let miss = reach >= MUST_REACH_YES && named == "other";
    let rationale = format!(
        "orchestration {orch} ({several:.2}/{exchange:.2}/{changes:.2}); missing_connector {miss} (reach {reach:.2}, {named}); complexity {}; horizon {}",
        complexity.tag(),
        horizon.tag()
    );
    Some(scored(orch, miss, complexity, horizon, &rationale))
}

fn conjunct_true(noul: f64) -> bool {
    noul >= NOUL_YES
}

fn noul(answers: &Value, key: &str) -> Option<f64> {
    let value = answers.get(key)?.get("noul")?.as_f64()?;
    (0.0..=1.0).contains(&value).then_some(value)
}

fn choice(answers: &Value, key: &str) -> Option<String> {
    answers.get(key)?.get("choice")?.as_str().map(str::to_owned)
}

fn complexity_from_score(answer: &Value) -> Option<Complexity> {
    let probs = answer.get("probabilities")?.as_object()?;
    let mut best_idx = None;
    let mut best_p = -1.0;
    for (idx, name) in COMPLEXITY_LEVELS.iter().enumerate() {
        let p = probs
            .get(&idx.to_string())
            .or_else(|| probs.get(*name))?
            .as_f64()?;
        if p > best_p {
            best_p = p;
            best_idx = Some(idx);
        }
    }
    let idx = best_idx?;
    let ultra = probs
        .get("3")
        .or_else(|| probs.get("ultra"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let high = probs
        .get("2")
        .or_else(|| probs.get("high"))
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    let picked = match idx {
        0 => Complexity::Low,
        1 => Complexity::Medium,
        2 => Complexity::High,
        3 => Complexity::Ultra,
        _ => return None,
    };
    if picked == Complexity::Ultra && ultra - high < ULTRA_MARGIN {
        return Some(Complexity::High);
    }
    Some(picked)
}

fn scored(
    orchestration: bool,
    missing_connector: bool,
    complexity: Complexity,
    task_context_horizon: TaskContextHorizon,
    rationale: &str,
) -> Classification {
    Classification {
        orchestration,
        missing_connector,
        complexity,
        task_context_horizon,
        rationale: rationale.to_string(),
        classifier_failed: false,
        invokes_implement: false,
        unlaunchable: None,
    }
}

pub fn api_key_from_env() -> Option<String> {
    for name in ["TYPESAFE_API_KEY", "TYPESAFE_AI_KEY"] {
        if let Ok(value) = std::env::var(name)
            && !value.is_empty()
        {
            return Some(value);
        }
    }
    None
}

pub trait SystemOneTransport {
    fn evaluate(&self, body: &Value, timeout: Duration, key: &str) -> Result<Value, String>;
}

pub struct UreqTransport;

impl SystemOneTransport for UreqTransport {
    fn evaluate(&self, body: &Value, timeout: Duration, key: &str) -> Result<Value, String> {
        post_system_one(body, timeout, key)
    }
}

/// IMPURE: score via TypeSafe. Never panics; unusable answers are the fallback.
pub fn classify_with_name(ctx: &Context, task: &str) -> super::ClassifiedTask {
    classify_with_transport(ctx, task, api_key_from_env().as_deref(), &UreqTransport)
}

pub fn classify_with_transport(
    ctx: &Context,
    task: &str,
    key: Option<&str>,
    transport: &dyn SystemOneTransport,
) -> super::ClassifiedTask {
    // No title, ever. Jev answers a fixed rubric and writes no prose, and the derived name it used
    // to return here is exactly the name the asynchronous namer exists to replace: returning it
    // would read as "this job has been named" and suppress the naming worker on every Jev-scored
    // job. The launch name is unchanged either way — `dispatch` derives the same name from the
    // task when none is supplied.
    let Some(key) = key else {
        return failed("missing typesafe api key", task);
    };
    let body = json!({
        "state": {
            "task": task,
            "connector_inventory": ctx.config.connectors,
        },
        "model": ctx.config.classifier.model(),
        "questions": questions(&ctx.config.connectors),
    });
    let timeout = Duration::from_secs(ctx.config.classifier_timeout_secs.max(1));
    match transport.evaluate(&body, timeout, key) {
        Ok(response) => match response.get("answers").and_then(compose) {
            Some(mut classification) => {
                classification = super::reconcile_configured_local_capabilities(
                    task,
                    classification,
                    &ctx.config.connectors,
                );
                classification.invokes_implement = invokes_implement(task);
                super::ClassifiedTask {
                    classification,
                    job_name: None,
                }
            }
            None => failed("unparseable json", task),
        },
        Err(why) => failed(&why, task),
    }
}

fn failed(why: &str, task: &str) -> super::ClassifiedTask {
    let mut classification = Classification::fallback(why);
    classification.invokes_implement = invokes_implement(task);
    super::ClassifiedTask {
        classification,
        job_name: None,
    }
}

fn post_system_one(body: &Value, timeout: Duration, key: &str) -> Result<Value, String> {
    let encoded = serde_json::to_vec(body).map_err(|_| "unparseable json".to_string())?;
    let mut response = ureq::post(ENDPOINT)
        .config()
        .timeout_global(Some(timeout))
        .build()
        .header("Authorization", &format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .send(&encoded)
        .map_err(|error| match error {
            ureq::Error::StatusCode(code) => format!("typesafe http {code}"),
            _ => "typesafe unreachable".to_string(),
        })?;
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|_| "unparseable json".to_string())?;
    serde_json::from_str(&text).map_err(|_| "unparseable json".to_string())
}

/// PURE: whether the anti-halo wording is in the questions (tests pin this).
pub fn questions_carry_anti_halo(connectors: &[String]) -> bool {
    let text = questions(connectors).to_string();
    text.contains("Never infer this from difficulty, file count")
        && text.contains("how many files, directories or repositories")
        && text.contains("Claude Code skills, local files")
        && text.contains("other wins if any required-now system is unlisted")
}
