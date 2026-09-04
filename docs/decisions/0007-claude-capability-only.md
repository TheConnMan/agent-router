# 0007. Claude is a capability-only destination

## Context

Automatic capacity routing has two workhorses: Codex and Grok. Claude is the
only engine that can run multi-agent orchestration and the only one whose
context window fits a build-tier `/implement` run (see 0003). Treating Claude
as a third workhorse would spend the scarce window on ordinary tasks.

## Measurement

Under the retired twelve-criterion rubric, the orchestration signal never fired
below four total signals lit, across all 108 recorded decisions: the model was
halo-scoring the whole array off an impression of difficulty. That was harmless
while orchestration was one of twelve signals feeding a verdict. It is not
harmless once orchestration is the only route to Claude.

The retired `claude_signals >= 2` pin fired 45 times in those 108 decisions,
and every one of those rows already carried a Claude verdict, so it decided
nothing.

Degenerate input (empty prompt, greeting, fragment) answered Claude under the
old rubric because "I cannot tell" read as "requirements still being
discovered".

Measured 2026-08-21 over 282 automatic routes, every likely-incorrect
destination was `missing_connector`: false positives treated Claude skills,
local SQLite, local files, `gh`, and later-named systems as missing connectors;
false negatives left Granola transcripts and a Slack URL on Codex.

## Decision

Claude is selected only by capability pins: `orchestration`,
`implement_exceeds_codex_window`, or an explicit `--provider claude`. A
classifier failure is not a pin. A launch failure is not a pin. Exhaustion of
both workhorses falls through to Codex (`over_ceiling`), never to Claude.

The classifier prompt attacks halo scoring directly: orchestration is never
inferred from difficulty, scope, file count, or duration; unscoreable input
scores false on both booleans; `missing_connector` is judged only against the
configured inventory.

A missing connector with no inventory-backed provider is `CapabilityBlocked`,
never assumed to be a Claude capability.

## Constraint

Do not add Claude to the workhorse eligibility set. Do not restore a
`claude_signals` pin. A fallback classification names no destination.
