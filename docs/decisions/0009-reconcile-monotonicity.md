# 0009. Reconcile monotonicity (`settle`)

## Context

Backends expose a bounded recent window. A job can reconcile to `completed`
today, age out of that window tomorrow, and be read as absent. Writing
`unknown` then would permanently lose a fact the router had already proven.

## Measurement

A verdict taken from the fresh reading alone reported a clean window over a
database that recorded a failure: a job proven failed aged out, classified
`Unknown`, and would have overwritten `failed` without the monotonicity rule.
`Unsupported` (a historical provider name) classifies `Unknown`; offering that
to `settle` would write `unknown` over `dispatched` on every run, for good, on
behalf of a reading that never happened.

Codex turn 0 is the routed turn, by design, and never the last turn of a later
conversation on the same thread. The hyphen in a Claude transcript filename
anchors the match to the end of a session UUID's first segment, so a short id
that is a prefix of another cannot match it.

## Decision

`settle(current, observed)`:

- A proven state (`running`, `completed`, `failed`) always writes.
- `unknown` writes only over `dispatched`, `running`, or `unknown`. Every
  other stored outcome, including one a later router invents, is kept.
- Writing `unknown` over `unknown` is intentional: it refreshes
  `reconciled_at_ms`.
- `completed` → `running` is permitted: a live backend saying the job is
  running now is fresh evidence.

`Unsupported` is never offered to `settle`. Report `failed()` from the settled
value, not the fresh reading.

## Constraint

Do not let an absence of information erase a proven outcome. Do not sweep a
Codex thread id as a Claude short id. Transcript existence is the entire
Claude file signal; do not open the file.
