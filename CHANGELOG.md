# Changelog

All notable changes to this project are documented here. Versions are the
workspace `package.version` stamped on every decision-log row.

## 0.21.9 - 2026-09-04

- Record measured routing decisions as ADRs under `docs/decisions/` and trim
  incident-narrative comments to the constraint plus a pointer.

## 0.21.8 - 2026-09-04

- Extract the MCP parity linter into `crates/agent-parity`.

## 0.21.7 - 2026-09-04

- Split the usage module by provider.

## 0.21.6 - 2026-09-04

- Bind `DecisionLog::record` by named SQLite parameters.

## 0.21.5 - 2026-09-04

- Add `log --unmarked` so routing-quality review can mark settled rows.

## 0.21.4 - 2026-09-03

- Introduce a `Context` object and delete impure seam twins.

## 0.21.3 - 2026-09-03

- Overlap the usage snapshot with classification on auto routes.

## 0.21.2 - 2026-09-03

- Cut schema v2 and drop dead config compatibility.

## 0.21.1 - 2026-09-03

- Scan Codex rollouts backwards instead of reading them whole.

## 0.21.0 - 2026-09-03

- Record each adversarial review in a `reviews` table.

## 0.20.0 - 2026-09-03

- Remove the unused OpenCode provider.

## 0.19.0 - 2026-09-02

- Accept SuperGrok Heavy as a paid weekly Grok pool.

## 0.18.0 - 2026-08-29

- Route workhorses by projected weekly pace.

## 0.17.0 - 2026-08-28

- Resolve provider binaries instead of naming them.

## 0.16.0 - 2026-08-25

- Route on capability pins (orchestration, implement context window,
  missing connector).

## 0.15.0 - 2026-08-23

- Fix Grok usage cache starvation.

## 0.14.0 - 2026-08-22

- Workhorse routing between Codex and Grok.

## 0.13.0 - 2026-08-21

- Integrate Grok lifecycle routing.

## 0.12.0 - 2026-08-21

- Route pinned providers hierarchically.

## 0.11.1 - 2026-08-12

- Patch version after formatting.

## 0.11.0 - 2026-08-12

- Codex credits routing.

## 0.10.0 - 2026-08-11

- Let a real Codex weekly window beat a no-credits verdict.

## 0.9.0 - 2026-08-11

- Treat exhausted Codex credits as unavailable.

## 0.8.0 - 2026-08-10

- Correct the merged router version after concurrent bumps.

## 0.7.0 - 2026-08-11

- Pin build-tier `/implement` runs to Claude.

## 0.6.2 - 2026-08-08

- Name a job even when its provider is named.

## 0.6.1 - 2026-08-06

- Check the CI gates locally before main reaches GitHub.

## 0.6.0 - 2026-08-06

- Route on projected weekly draw instead of a run-rate gap.

## 0.5.0 - 2026-08-06

- Refuse a provider whose weekly window nobody read.

## 0.4.0 - 2026-08-06

- Hold a five-point weekly reserve per provider.

## 0.3.0 - 2026-08-05

- Generate automatic job titles.

## 0.2.2 - 2026-08-04

- Name routed background jobs.

## 0.2.1 - 2026-08-03

- Record task context horizon.

## 0.2.0 - 2026-08-02

- Version both crates together and enforce the bump in CI.

## 0.1.0 - 2026-07-30

- Document the router and ship release binaries.
