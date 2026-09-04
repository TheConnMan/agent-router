# 0003. Build-tier `/implement` pins to Claude

## Context

Codex's context window is 258,400 tokens. A build-tier `/implement` run that
exceeds it compacts by construction. This is a capability fact about the
destination, not a judgement about the work.

## Measurement

Measured 2026-08-11 across 37 Claude and 13 Codex `/implement` runs:

- Median Claude run peaks at 262,017 tokens of resident context.
- 51 percent of Claude runs peak above the whole Codex window.
- All four of the heaviest recorded Codex runs compacted 2 to 5 times; one
  ground for four days across 222M input tokens, and another stalled unfinished
  at 100M.
- Every one of those four heavy runs scored `high` or `ultra`; runs that
  finished cleanly on Codex scored `medium` or `low`.

Position of `/implement` matters. Scanning every line matched a kickoff task
that merely listed `/implement` commands for a ticket set (orchestration, not
an implement run). Over 283 recorded dispatches, requiring the dispatcher
position (first non-blank, non-`BACKGROUND_RUN=1` line) kept all 47 real runs
and dropped exactly that one.

## Decision

Pin to Claude when both are true:

1. The task actually dispatches `/implement`, read from the first dispatcher
   line rather than scored.
2. Complexity is `high` or `ultra` (the build-tier proxy). An unscored task
   reads as `high` by default, so a classifier failure on an implement run pins
   rather than gambles.

`low` and `medium` implement runs (`direct` / `quick`) stay on automatic
routing. Pinning them would hand Codex's share of this workload to Claude for
no measured gain.

## Constraint

`implement_exceeds_codex_window` is a capability pin: it bypasses usage rules
the same way `orchestration` does. Do not ask the classifier to re-derive the
`/implement` substring. Do not scan past the dispatcher line.
