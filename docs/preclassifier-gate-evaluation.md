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
still has a decision log row id as provenance. Decisions whose classifier call failed and fell back
are excluded, because a fallback carries no classifier judgment.

| Split | Count |
| --- | --- |
| Verdict `claude` | 57 |
| Verdict `codex` | 41 |
| `missing_connector` true | 13 |
| `missing_connector` false | 85 |

The fixture is `crates/agent-router-core/tests/fixtures/preclassifier_cases.json`, held to its
composition by `tests/preclassifier_fixture.rs`. The rule and its measurement are in
`tests/preclassifier_gate_eval.rs`.

## The bar

Fixed in advance, before measurement:

- Zero wrong pins. A wrong pin is a case where the rule fires but the recorded verdict is `codex`.
- The rule must fire on at least 20 percent of the cases.

## Result

| Measure | Value |
| --- | --- |
| Cases | 98 |
| Fired | 20 (20.4 percent) |
| Agreed | 13 |
| Disagreed | 7 |
| Agreement rate among fired | 65.0 percent |
| Wrong pins | 7 |
| Oracle ceiling | 13 cases (13.3 percent) |

The zero wrong pin bar is missed by 7. The 20 percent fire rate bar is nominally met at 20.4
percent, but only because the 7 wrong pins are counted among the firings. The correct firings alone
are 13, which is 13.3 percent.

## Finding 1: naming a system is not the same as needing to reach it

All 7 wrong pins have the same shape. The task names an out of inventory system as a token inside
the codebase rather than as a system to reach, so no connector is required, and the classifier sent
every one of them to Codex. Three illustrations, by decision log id:

- 95: "Fix the typo in the README section that describes our n8n workflows, then run the test suite."
- 105: "Delete the dead slack_webhook_url key from config.toml and remove the matching field from
  the config struct."
- 110: "Grep the repo for every mention of Snowflake and list the files that contain it, making no
  changes."

A text match carries no way to tell the two apart, and that distinction is exactly the judgment the
classifier is making.

## Finding 2: the ceiling makes this unfixable by tuning

Only 13 of 98 cases actually carry `missing_connector` true, so no rule that never wrong pins can
fire on more than 13.3 percent of this evidence, which is already below the 20 percent bar. The two
bars are jointly unreachable for this family of rule, not merely unmet by this candidate. Tuning the
vocabulary to remove the wrong pins necessarily drives the fire rate down toward that ceiling, never
up.

## Secondary observation: the rule cannot be built from existing configuration

The config lists only the connectors that are available. Detecting a name that is absent therefore
requires a second, separately maintained vocabulary of system names, which is a new config surface
and is inherently incomplete. That cost is real, and it buys at most 13.3 percent of calls even in
the perfect case.

## What would justify revisiting this

Two things, together:

1. Evidence that the determined subset is materially larger than 13.3 percent of real traffic.
2. A rule that distinguishes a system that must be reached from a system that is merely named.
