# 0002. Codex classifier stays non-ephemeral

## Context

`--ephemeral` would suit a throwaway scoring call: no session file, no leftover
state. `codex_headroom` reads the newest rollout that carries a `rate_limits`
event. An ephemeral run writes no rollout.

## Measurement

The classifier fires on every auto-routed task. Suppressing those rollouts would
let scoring burn Codex quota while the router kept deciding against the last
dispatched job's percentage. A Codex at its ceiling would keep reading as having
headroom.

## Decision

The Codex classifier invocation does not pass `--ephemeral`. Persisting the
rollout costs a session file per task and keeps the routing input honest.

## Constraint

Do not add `--ephemeral` to the Codex classifier command. Headroom must observe
classifier spend.
