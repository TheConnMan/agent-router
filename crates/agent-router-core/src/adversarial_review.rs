use crate::classify::Complexity;
use crate::config::Config;
use crate::error::{Error, Result};
use crate::usage::{Headroom, claude_headroom, codex_headroom};
use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Output};

const WEEKLY_USAGE_CEILING: f64 = 90.0;
const CLAUDE_REVIEW_BIN_ENV: &str = "AGENT_ROUTER_CLAUDE_REVIEW_BIN";
const CODEX_REVIEW_BIN_ENV: &str = "AGENT_ROUTER_CODEX_REVIEW_BIN";

#[derive(Debug, Clone, Copy)]
pub struct ReviewRequest<'a> {
    pub primary_provider: &'a str,
    pub body: &'a str,
    pub dir: &'a Path,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ReviewStatus {
    Completed,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ReviewOutcome {
    pub status: ReviewStatus,
    pub primary_provider: String,
    pub reviewer_provider: Option<String>,
    pub reviewer_model: Option<String>,
    pub usage: Option<Headroom>,
    pub usage_provenance: Vec<CandidateUsage>,
    pub rationale: String,
    pub reason: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CandidateUsage {
    pub provider: String,
    pub weekly_pct: Option<f64>,
    pub stale: bool,
    pub eligible: bool,
    pub rejection_reason: Option<String>,
}

pub trait ReviewProvider {
    fn provider_name(&self) -> &str;
    fn reviewer_model(&self) -> &str;
    fn usage(&self) -> Option<Headroom>;
    fn review(&self, request: &ReviewRequest<'_>) -> Result<String>;
}

enum Selection<'a> {
    Selected {
        provider: &'a dyn ReviewProvider,
        usage: Headroom,
        rationale: String,
        usage_provenance: Vec<CandidateUsage>,
    },
    Skipped {
        rationale: String,
        usage_provenance: Vec<CandidateUsage>,
    },
}

pub fn review_with_providers(
    request: &ReviewRequest<'_>,
    providers: &[&dyn ReviewProvider],
) -> Result<ReviewOutcome> {
    match select_provider(request.primary_provider, providers) {
        Selection::Selected {
            provider,
            usage,
            rationale,
            usage_provenance,
        } => {
            let result = provider.review(request)?;
            Ok(completed_outcome(
                request,
                provider,
                usage,
                usage_provenance,
                rationale,
                result,
            ))
        }
        Selection::Skipped {
            rationale,
            usage_provenance,
        } => Ok(skipped_outcome(request, usage_provenance, rationale)),
    }
}

pub fn review_registered(request: &ReviewRequest<'_>, config: &Config) -> ReviewOutcome {
    if !request.dir.is_dir() {
        return failed_outcome(
            request.primary_provider,
            format!("target directory does not exist: {}", request.dir.display()),
        );
    }

    let claude = ClaudeReviewProvider {
        model: config.models.claude.pick(Complexity::High),
    };
    let codex = CodexReviewProvider {
        model: config.models.codex.pick(Complexity::High),
    };
    let providers: [&dyn ReviewProvider; 2] = [&claude, &codex];

    match select_provider(request.primary_provider, &providers) {
        Selection::Selected {
            provider,
            usage,
            rationale,
            usage_provenance,
        } => match provider.review(request) {
            Ok(result) => completed_outcome(
                request,
                provider,
                usage,
                usage_provenance,
                rationale,
                result,
            ),
            Err(error) => ReviewOutcome {
                status: ReviewStatus::Failed,
                primary_provider: request.primary_provider.to_string(),
                reviewer_provider: Some(provider.provider_name().to_string()),
                reviewer_model: Some(provider.reviewer_model().to_string()),
                usage: Some(usage),
                usage_provenance,
                rationale,
                reason: Some(error.to_string()),
                result: None,
            },
        },
        Selection::Skipped {
            rationale,
            usage_provenance,
        } => skipped_outcome(request, usage_provenance, rationale),
    }
}

pub fn failed_outcome(primary_provider: &str, reason: impl Into<String>) -> ReviewOutcome {
    ReviewOutcome {
        status: ReviewStatus::Failed,
        primary_provider: primary_provider.to_string(),
        reviewer_provider: None,
        reviewer_model: None,
        usage: None,
        usage_provenance: Vec::new(),
        rationale: "review could not evaluate registered providers".to_string(),
        reason: Some(reason.into()),
        result: None,
    }
}

fn completed_outcome(
    request: &ReviewRequest<'_>,
    provider: &dyn ReviewProvider,
    usage: Headroom,
    usage_provenance: Vec<CandidateUsage>,
    rationale: String,
    result: String,
) -> ReviewOutcome {
    ReviewOutcome {
        status: ReviewStatus::Completed,
        primary_provider: request.primary_provider.to_string(),
        reviewer_provider: Some(provider.provider_name().to_string()),
        reviewer_model: Some(provider.reviewer_model().to_string()),
        usage: Some(usage),
        usage_provenance,
        rationale,
        reason: None,
        result: Some(result),
    }
}

fn skipped_outcome(
    request: &ReviewRequest<'_>,
    usage_provenance: Vec<CandidateUsage>,
    rationale: String,
) -> ReviewOutcome {
    ReviewOutcome {
        status: ReviewStatus::Skipped,
        primary_provider: request.primary_provider.to_string(),
        reviewer_provider: None,
        reviewer_model: None,
        usage: None,
        usage_provenance,
        rationale,
        reason: Some("no eligible alternative provider".to_string()),
        result: None,
    }
}

fn select_provider<'a>(
    primary_provider: &str,
    providers: &[&'a dyn ReviewProvider],
) -> Selection<'a> {
    let mut rationale = Vec::with_capacity(providers.len() + 1);
    let mut eligible = Vec::new();
    let mut usage_provenance = Vec::with_capacity(providers.len());

    for provider in providers {
        let name = provider.provider_name();
        if name.eq_ignore_ascii_case(primary_provider) {
            rationale.push(format!("{name} excluded as primary provider"));
            usage_provenance.push(CandidateUsage {
                provider: name.to_string(),
                weekly_pct: None,
                stale: true,
                eligible: false,
                rejection_reason: Some("primary provider excluded".to_string()),
            });
            continue;
        }

        let Some(usage) = provider.usage() else {
            rationale.push(format!("{name} rejected because capacity is unavailable"));
            usage_provenance.push(CandidateUsage {
                provider: name.to_string(),
                weekly_pct: None,
                stale: true,
                eligible: false,
                rejection_reason: Some("capacity unavailable".to_string()),
            });
            continue;
        };
        if usage.stale {
            let reason = format!(
                "capacity is stale at {:.1} percent weekly usage",
                usage.weekly_pct
            );
            rationale.push(format!("{name} rejected because {reason}"));
            usage_provenance.push(CandidateUsage {
                provider: name.to_string(),
                weekly_pct: None,
                stale: true,
                eligible: false,
                rejection_reason: Some(reason),
            });
            continue;
        }
        if !usage.weekly_capacity_known || !usage.weekly_pct.is_finite() {
            let reason = format!(
                "weekly capacity is unknown at {:.1} percent usage",
                usage.weekly_pct
            );
            rationale.push(format!("{name} rejected because {reason}"));
            usage_provenance.push(CandidateUsage {
                provider: name.to_string(),
                weekly_pct: None,
                stale: false,
                eligible: false,
                rejection_reason: Some(reason),
            });
            continue;
        }
        if usage.weekly_pct >= WEEKLY_USAGE_CEILING {
            let reason = format!(
                "weekly usage {:.1} reaches the {:.1} ceiling",
                usage.weekly_pct, WEEKLY_USAGE_CEILING
            );
            rationale.push(format!("{name} rejected because {reason}"));
            usage_provenance.push(CandidateUsage {
                provider: name.to_string(),
                weekly_pct: Some(usage.weekly_pct),
                stale: false,
                eligible: false,
                rejection_reason: Some(reason),
            });
            continue;
        }

        rationale.push(format!(
            "{name} eligible at {:.1} percent weekly usage",
            usage.weekly_pct
        ));
        usage_provenance.push(CandidateUsage {
            provider: name.to_string(),
            weekly_pct: Some(usage.weekly_pct),
            stale: false,
            eligible: true,
            rejection_reason: None,
        });
        eligible.push((*provider, usage));
    }

    eligible.sort_by(
        |(left_provider, left_usage), (right_provider, right_usage)| {
            left_usage
                .weekly_pct
                .total_cmp(&right_usage.weekly_pct)
                .then_with(|| {
                    left_provider
                        .provider_name()
                        .cmp(right_provider.provider_name())
                })
        },
    );

    match eligible.first().copied() {
        Some((provider, usage)) => {
            rationale.push(format!(
                "selected {} at {:.1} percent after excluding primary {}",
                provider.provider_name(),
                usage.weekly_pct,
                primary_provider
            ));
            Selection::Selected {
                provider,
                usage,
                rationale: rationale.join("; "),
                usage_provenance,
            }
        }
        None => Selection::Skipped {
            rationale: format!(
                "{}; no eligible alternative to primary {}",
                rationale.join("; "),
                primary_provider
            ),
            usage_provenance,
        },
    }
}

struct ClaudeReviewProvider<'a> {
    model: &'a str,
}

impl ReviewProvider for ClaudeReviewProvider<'_> {
    fn provider_name(&self) -> &str {
        "claude"
    }

    fn reviewer_model(&self) -> &str {
        self.model
    }

    fn usage(&self) -> Option<Headroom> {
        Some(claude_headroom())
    }

    fn review(&self, request: &ReviewRequest<'_>) -> Result<String> {
        if !request.dir.is_dir() {
            return Err(Error::Command(format!(
                "target directory does not exist: {}",
                request.dir.display()
            )));
        }

        let mut command = Command::new(review_binary(CLAUDE_REVIEW_BIN_ENV, "claude"));
        command
            .current_dir(request.dir)
            .env("CLAUDE_SUBPROCESS", "1")
            .arg("-p")
            .arg("--model")
            .arg(self.model)
            .arg("--output-format")
            .arg("json")
            .arg("--no-session-persistence")
            .arg("--safe-mode")
            .arg("--tools")
            .arg("Read,Glob,Grep")
            .arg("--disable-slash-commands")
            .arg("--permission-mode")
            .arg("plan")
            .arg("--strict-mcp-config")
            .arg(request.body);
        parse_claude_output(run_review(command, "claude")?)
    }
}

struct CodexReviewProvider<'a> {
    model: &'a str,
}

impl ReviewProvider for CodexReviewProvider<'_> {
    fn provider_name(&self) -> &str {
        "codex"
    }

    fn reviewer_model(&self) -> &str {
        self.model
    }

    fn usage(&self) -> Option<Headroom> {
        Some(codex_headroom())
    }

    fn review(&self, request: &ReviewRequest<'_>) -> Result<String> {
        if !request.dir.is_dir() {
            return Err(Error::Command(format!(
                "target directory does not exist: {}",
                request.dir.display()
            )));
        }

        let mut command = Command::new(review_binary(CODEX_REVIEW_BIN_ENV, "codex"));
        command
            .current_dir(request.dir)
            .arg("exec")
            .arg("--sandbox")
            .arg("read-only")
            .arg("review")
            .arg("--model")
            .arg(self.model)
            .arg("--json")
            .arg("--ephemeral")
            .arg(request.body);
        parse_codex_output(run_review(command, "codex")?)
    }
}

fn review_binary(environment: &str, fallback: &str) -> OsString {
    std::env::var_os(environment).unwrap_or_else(|| OsString::from(fallback))
}

fn run_review(mut command: Command, provider: &str) -> Result<String> {
    let Output {
        status,
        stdout,
        stderr,
    } = command.output()?;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr).trim().to_string();
        let suffix = if detail.is_empty() {
            String::new()
        } else {
            format!(": {detail}")
        };
        return Err(Error::Command(format!(
            "{provider} review exited {status}{suffix}"
        )));
    }
    String::from_utf8(stdout)
        .map_err(|_| Error::Command(format!("{provider} review printed non UTF-8 output")))
}

fn parse_claude_output(stdout: String) -> Result<String> {
    let envelope: serde_json::Value = serde_json::from_str(stdout.trim())
        .map_err(|error| Error::Command(format!("claude review returned invalid JSON: {error}")))?;
    if envelope
        .get("is_error")
        .and_then(serde_json::Value::as_bool)
        == Some(true)
    {
        return Err(Error::Command(
            envelope
                .get("result")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("claude review reported an error")
                .to_string(),
        ));
    }
    envelope
        .get("result")
        .and_then(serde_json::Value::as_str)
        .filter(|result| !result.is_empty())
        .map(str::to_string)
        .ok_or_else(|| Error::Command("claude review returned no review body".to_string()))
}

fn parse_codex_output(stdout: String) -> Result<String> {
    stdout
        .lines()
        .rev()
        .find_map(|line| {
            let event: serde_json::Value = serde_json::from_str(line.trim()).ok()?;
            if event.get("type")?.as_str()? != "item.completed" {
                return None;
            }
            let item = event.get("item")?;
            if item.get("type")?.as_str()? != "agent_message" {
                return None;
            }
            item.get("text")?
                .as_str()
                .filter(|result| !result.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| Error::Command("codex review returned no review body".to_string()))
}
