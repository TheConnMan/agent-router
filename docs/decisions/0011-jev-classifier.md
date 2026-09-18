# 0011. TypeSafe Jev as an opt-in classifier engine

## Context

Every `--provider auto` call shells out to a hermetic Claude or Codex CLI to score four
routing fields and a job title. Measured wall time on this box is 7 to 10 seconds. The
pain is CLI startup plus a generative JSON answer, not token cost.

TypeSafe Jev (`jev-1.13.0`) returns typed Noul, Choice, and Score answers over HTTP in
hundreds of milliseconds. It cannot generate `job_name` or a free-text rationale.

## Decision

`[classifier] engine = "jev"` is a third engine. The crate default remains Codex.

Jev scores via one `POST https://api.typesafe.ai/v1/systemone` call. Code composes:

- orchestration as the AND of three Nouls, with noul below 0.70 treated as false
- `missing_connector` as must-now-reach AND `named_system == other`, where `other`
  wins if any system required now is absent from `connectors`
- complexity as the argmax of a four-level Score, with torn ultra reading as high
- horizon as a Choice

This path returns no title: Jev answers a fixed rubric and writes no prose, so a Jev-scored job
launches under the name derived from its task and is retitled afterwards by the asynchronous namer
(`[classifier] naming_engine`). Failures fail open (`classifier_failed`,
`unlaunchable` unset). The API key is `TYPESAFE_API_KEY` or `TYPESAFE_AI_KEY` in the
environment, never config.toml.

Claude and Codex engines, `decide.rs` capability rules, and the rejected missing-connector
pre-gate are unchanged.

## Constraint

Do not make Jev the default until it has been run as an explicit opt-in. Do not spawn a
classifier CLI on the Jev path. Do not store the TypeSafe key in router config.
