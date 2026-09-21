# Changelog

All notable changes to this project are documented here. Versions are the
workspace `package.version` stamped on every decision-log row.

## 0.27.1 - 2026-09-21

- Fail over a Grok adversarial review once when it times out or hits a Grok storage `openat2`
  error, and fail over a pinned reviewer once when the usage or reserve gate refuses it. The next
  eligible reviewer that is not the primary runs the same sealed request. The reviews row keeps the
  original `reason` and records `fallback_from`. A user-issued `review cancel` never fails over. If
  no other reviewer is eligible, the review still fails as before. The timeout, the gate, and the
  reserve are unchanged.

## 0.26.1 - 2026-09-18

- Keep a title whose words carry an interior dot or slash. The first real job through asynchronous
  naming kept its derived name: the task was about `v0.10.0`, the model titled it accordingly, and
  `validate_job_name` threw the whole title away over one dot. A title word is now judged by its
  edges, so `v0.10.0` and `CI/CD` survive while prose punctuation is refused exactly as before.
- Say which stage a naming call failed at. `no usable title` covered an unlaunchable CLI, a timed
  out call, a missing field, and a refused title behind one sentence; the naming log now names the
  stage and quotes a refused title verbatim. An exec that fails after the binary resolved is
  reported as a launch failure, not a bad answer.

## 0.26.0 - 2026-09-18

- Generate session titles asynchronously. No naming call sits on the dispatch path any more: a job
  launches under the name already in hand, and when that is the name derived from the task text a
  detached `setsid` worker generates a descriptive title, renames the launched session by its own
  identity, and updates the decision row. It outlives the router process.
- Rename through Agent Viewer's own mechanisms: claude's `state.json` writer, codex `thread/name/set`
  over the app-server daemon, and Grok's `x.ai/session/rename`. Claude and Codex can report the
  current name, so a rename somebody made by hand is kept rather than overwritten.
- Add `[classifier] naming_engine`, default `"claude"`. Jev scores but writes no prose, so the
  engine that names is chosen separately from the engine that scores; `"jev"` normalizes to the
  default.
- The Jev engine no longer returns a derived title of its own. It writes no prose, and returning the
  derived name read as "this job has been named", which would have suppressed the naming worker on
  every Jev-scored job. The launch name is unchanged; the title now arrives after launch.
- `run --json` gains `naming_started` and `naming_skipped`. Naming never fails, stops, or relaunches
  a job; every outcome lands in `~/.local/state/agent-router/logs/naming-*.log`.

## 0.25.0 - 2026-09-18

- Add an opt-in `[classifier] engine = "jev"` that scores the four routing fields through one
  TypeSafe System One call. Default engine stays Codex. Titles on the Jev path use `short_job_name`.
  Failures fail open. The API key is an environment variable, never config.toml.

## 0.24.4 - 2026-09-15

- An unmatched classifier `missing_connector` is ordinary auto routing, not
  `capability_blocked`. Auto was refusing research questions that named a system nobody
  inventories (Descript MCP, public web, Twitter) because recovery only matches configured
  product names. A matched name with no dispatcher still refuses, and still does not pin
  Claude. See `docs/decisions/0010-unmatched-connector-is-not-a-block.md`.

## 0.24.3 - 2026-09-15

- Pin the `/implement` skill at Grok dispatch instead of trusting Grok's own resolution order. A
  Grok launch whose task opens with `/implement` now runs `grok inspect --json` in the launch
  directory first and refuses the launch, with `outcome = skill-pin-blocked` and the reason in
  `note`, unless the `implement` skill resolves to the user-scope
  `~/.claude/skills/implement/SKILL.md` (through either the `~/.grok/skills` or `~/.agents/skills`
  symlink). A launch that passes gets two lines prepended to its task: the absolute SKILL.md path
  to read, and the absolute `python3 .../factory-telemetry.py` command, with any project-level
  `.claude/skills/implement` declared out of scope for the run. The resolved path is recorded in
  `decisions.note`. Measured 2026-09-13: 13 of 23 launched Grok `/implement` runs wrote no factory
  stage row, and the config-level fixes did not settle it because nothing in the dispatch path
  checked which copy won. Claude and Codex launches are unchanged.
- `record` now writes `decisions.note`, which until now only `mark --note` wrote. A human mark's
  note still overwrites the router's.
- Recover Auto routes against `provider_capabilities` when the task or rationale names an
  inventory connector, even if the classifier left `missing_connector` false. The 2026-09-13
  routing-quality review found the same Airtable auto-management job leaking to Grok twice
  in three runs because recovery waited on that flag.
## 0.24.2 - 2026-09-09

- Persist `reason` and `outcome_json` on the pre-ID `record_review` path, so a review that fails
  before or instead of a pending row (selection error, config load, database busy fallback) keeps
  its cause. 28 failed reviews between 2026-09-04 and 09-07 had every one of those columns NULL.
  Lifecycle `status` stays NULL on that path, as before.

## 0.24.1 - 2026-09-09

- Treat `capability-blocked` as settled for `log --unmarked`, so a review pass's worklist includes
  fail-closed connector blocks. The stats failure rate still ignores them: nothing was dispatched.

## 0.24.0 - 2026-09-08

- Route Codex complexity through Terra/high, Sol/medium, Astra/low, and Astra/high.
- Apply the same effort ladder to Claude with Sonnet, Opus, Fable, and Fable.

## 0.23.1 - 2026-09-08

- `adversarial-review` persists a review id and prints
  `agent-router: adversarial review <id> started` to stderr before any
  provider work begins, then runs the reviewer in a detached worker process
  so the review survives the caller exiting or being killed. By default the
  command still waits for a terminal result and prints the review body
  exactly as before.
- `--timeout <SECS>` bounds that wait: past the deadline the command exits
  `4` and reports `status: "pending"` with `review_id` while the review
  keeps running; `--timeout 0` returns pending immediately.
- Add `agent-router review status <ID> [--json]` and
  `agent-router review cancel <ID>` to report or stop a review by the id
  `adversarial-review` printed.
- The `reviews` table gains three nullable columns: `status`, the retained
  terminal `outcome_json`, and `reason`. A row written before this version
  has a NULL `status`; its state is derived from `exit_status` instead of
  ever reading as pending.
- `runtime::spawn_detached` now returns the spawned `Child`.

## 0.23.0 - 2026-09-08

- Treat Codex model and reasoning effort as two gears: reset effort to low when the configured
  model tier changes, then ramp it only while that model remains selected.
- Record an accepted Codex turn-level effort override as effective instead of the superseded
  thread default from user configuration.

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
