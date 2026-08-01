# Pre classifier gate evaluation

On `--provider auto`, `agent-router` shells out to a small model on every call and pays the full CLI
startup cost each time. This document records the evaluation of one candidate deterministic rule
intended to skip that call for tasks whose outcome is already determined.

The candidate: a task naming a system absent from the configured connector inventory pins to Claude
without calling the classifier. The reasoning was that rubric criterion 5 is scored against exactly
that inventory, and a missing connector is already an unconditional capability pin in `decide()`.

**The rule was evaluated and rejected. It is not in the routing path.** This document exists so the
idea is not re-proposed without new evidence.

## Evidence set

98 cases, each carrying the classifier's own recorded verdict from the decision log rather than a
human assigned label. 79 cases are observed real traffic. 19 are task texts authored to cover both
directions and the near miss cases, then labelled by running the live classifier over them, so each
still has a decision log row id as provenance. The decision log holds 113 rows in total. 9 rows
requested as `--provider auto` are excluded because the classifier call failed and fell back, and a
fallback carries no classifier judgment; a further 6 rows are excluded because the caller named a
provider explicitly, so no classification ran and the verdict is null. The remaining 98 rows, all
requested as `--provider auto` and all carrying a real classifier verdict, are the evidence set.

| Split | Count |
| --- | --- |
| Verdict `claude` | 57 |
| Verdict `codex` | 41 |
| `missing_connector` true | 13 |
| `missing_connector` false | 85 |

The fixture is `crates/agent-router-core/tests/fixtures/preclassifier_cases.json`, held to its
composition by `tests/preclassifier_fixture.rs`. The rule and its measurement are in
`tests/preclassifier_gate_eval.rs`. Those pinned assertions are a decision record, not a regression
guard: they hold the measured numbers in place so that a later change forces this decision to be
revisited rather than drifting silently.

## The bar

Fixed in advance, before measurement:

- Zero wrong pins. A wrong pin is a case where the rule fires but the recorded verdict is `codex`.
- The rule must fire on at least 20 percent of the cases.

## Result

The two halves of the evidence behave completely differently, so the blended numbers are never
reported on their own.

| Origin | Cases | Fired | Fire rate | Wrong pins | Oracle ceiling |
| --- | --- | --- | --- | --- | --- |
| Observed traffic | 79 | 5 | 6.3 percent | 0 | 5 (6.3 percent) |
| Authored probes | 19 | 15 | 78.9 percent | 7 | 8 (42.1 percent) |
| All cases | 98 | 20 | 20.4 percent | 7 | 13 (13.3 percent) |

Agreement rate among the 20 blended firings is 65.0 percent: 13 agreed, 7 disagreed.

The traffic grounded reason for rejection is the fire rate. On observed traffic the rule fires on
6.3 percent of cases, nowhere near the 20 percent bar, and the observed traffic oracle ceiling is
also 6.3 percent, so no tuning reaches the bar on the traffic the rule would actually run against.

The blended 20.4 percent is an artifact of the authored probe set, which was deliberately dense in
connector cases. It is not an estimate of real traffic. It also clears 20 percent by exactly one
case: 20 of 98 is 20.4 percent, and 19 of 98 would be 19.4 percent and would miss.

The 7 wrong pins prove the failure mode is real and easy to hit with ordinary tasks, but all 7 are
authored cases, so this evidence establishes that the mode exists, not how often it would occur in
production.

## Finding 1: the ceiling makes this unfixable by tuning

No rule keyed on the missing connector criterion can fire more often than the missing connector rate
itself, which is 13.3 percent across all cases and 6.3 percent on observed traffic. Both are below
the 20 percent bar, so the two bars are jointly unreachable for this family of rule, not merely
unmet by this candidate. Tuning the vocabulary to remove wrong pins necessarily drives the fire rate
down toward that ceiling, never up.

This bound is narrow on purpose. It says nothing about rules built on other grounds: 57 of the 98
cases carry verdict `claude`, so a rule predicting `claude` from some other signal could in
principle fire far more often without wrong pinning. What is ruled out is keying on the absent
connector.

## Finding 2: naming a system is not the same as needing to reach it

The 7 wrong pins are two distinct shapes, and only one of them is fixable.

Three are tokenizer artifacts, fixable by a better tokenizer. The system name sits inside a
snake_case identifier and matches only because the underscore is treated as a word boundary, so
treating the underscore as a word character removes all three. By decision log id:

- 105: `slack_webhook_url`, a dead config key being deleted.
- 106: `jira_ticket_id`, a parser getting a unit test.
- 108: `sentry_dsn`, a field getting a doc comment.

Four are not fixable by tokenizing. The system is named in ordinary prose while no connector is
needed at all:

- 95: fixing a typo in a README section that describes n8n workflows.
- 107: updating docs to state that the Notion integration was removed.
- 109: fixing the word Stripe misspelled in three comments.
- 110: grepping the repo for mentions of Snowflake and listing the files.

This second group is what kills the approach. Distinguishing a system that must be reached from a
system that is merely named is exactly the judgment the classifier makes, and no text matching rule
can recover it.

## Sensitivity to the matching rule

The conclusion does not depend on which matching rule was chosen. Every variant fails at least one
bar:

| Matching rule | Fired | Fire rate | Wrong pins | Outcome |
| --- | --- | --- | --- | --- |
| Underscore as a word character | 17 | 17.3 percent | 4 | Fails both bars |
| Underscore as a boundary (measured) | 20 | 20.4 percent | 7 | Fails the wrong pin bar |
| Loose substring matching | 21 | 21.4 percent | 8 | Fails the wrong pin bar |

## Limitations of this evidence

- The 19 authored probes were written by the same evaluator who proposed the rule, and were
  deliberately dense in connector cases. Their labels are the classifier's own recorded verdicts
  rather than the author's judgment, which is what keeps them usable, but their prevalence is
  chosen, not observed.
- Within observed traffic, only 2 distinct task texts ever cause the rule to fire, so the real
  traffic firing behavior rests on a very thin base.
- 17 of the 26 vocabulary terms never match anything in the evidence set, and the set contains no
  system outside that vocabulary, so the vocabulary's coverage of real systems is untested.

## Secondary observation: the rule cannot be built from existing configuration

The config lists only the connectors that are available. Detecting a name that is absent therefore
requires a second, separately maintained vocabulary of system names, which is a new config surface
and is inherently incomplete. That cost is real, and even in the perfect case it buys at most 13.3
percent of calls across all cases, 6.3 percent on observed traffic.

## What would justify revisiting this

Two things, together:

1. Evidence from observed traffic, not authored probes, that the determined subset is materially
   larger than the 6.3 percent measured here.
2. A rule that distinguishes a system that must be reached from a system that is merely named.
