# agent-router

Route ordinary work automatically between Codex and Grok using authoritative weekly usage, pin
capability-heavy work to premium Claude, or dispatch explicitly to any provider, then record why.

The problem it solves: workhorse providers with separate weekly quotas, and a running judgement call
about which one a given task belongs to. `agent-router` makes that call explicitly, from a fixed
rubric plus live usage, and logs every decision so the routing policy can be tuned against real
data instead of memory.

```
$ agent-router run "Port usage.sh to Rust with the same fail-open semantics"
codex complexity medium model gpt-5.6-terra job 019c3f2a name "Port usage.sh to Rust with the same fail"
why: codex ready on all six criteria, no claude signals, weekly headroom codex 41% claude 12%
log: row 87 in /home/you/.local/state/agent-router/router.db
```

## How a task is routed

1. **Classify.** One small model call scores the task and returns strict JSON:

   ```json
   {"orchestration": false, "missing_connector": false, "complexity": "medium", "task_context_horizon": "ordinary", "job_name": "GH-123 Sprint 2 Bug Fixes"}
   ```

   `task_context_horizon` is `ordinary` or `extended`. It is observed and recorded for later
   analysis only: it never selects a provider or model. `unknown` is recorded only when the
   classifier falls back after a failure or timeout. The call is deliberately hermetic. On the Claude engine it
   runs with `--safe-mode` and `--strict-mcp-config`; on the Codex engine with
   `--ignore-user-config`, `--ignore-rules`, and `project_doc_max_bytes=0`. Either way no project
   `CLAUDE.md`, `AGENTS.md`, skill, plugin, hook, or MCP server can shift the score. The Codex
   engine additionally runs with its shell, browser, computer use, image, app, and skill search
   tools disabled: scoring needs no tool, and a task carrying an injected instruction must have
   nothing to reach for. If the call fails or times out, the configured default provider stays in
   force and the decision is tagged `classifier_failed`. The same classifier model also generates
   the job title. A ticket ID leads the title, followed by two to six concise Title Case words, such
   as `GH-123 Sprint 2 Bug Fixes` or `RS-123 Input Box Searching`. A run that names its provider is
   The routing classifier runs whenever provider, model, or effort is omitted for Claude or Codex.
   An explicit Claude or Codex provider therefore pins only the provider: omitted model and effort
   values still come from classification. Grok and OpenCode skip cross-provider classification;
   job naming remains a separate title call when a name was not supplied and dispatch is not a dry
   run.
2. **Apply the capability pin.** A missing connector, a task needing several agents to exchange
   findings mid-run, or a build-tier `/implement` run (`implement_context_window`), pins to Claude
   regardless of usage. These are statements that the task cannot run on Codex at all, so every
   usage rule below is bypassed. The third one is a context-window fact rather than a feature gap:
   Codex's window is 258,400 tokens, and measured 2026-08-11 across 37 Claude and 13 Codex
   `/implement` runs, the median Claude run peaks at 262,017 tokens of resident context with 51
   percent of runs peaking above the whole Codex window. It fires only when the task text actually
   dispatches `/implement` (read literally, never scored) **and** complexity is `high` or `ultra`,
   which is the build tier; `low` and `medium` implement runs are the direct and quick tiers, they
   fit comfortably, and they stay on ordinary routing. An unscored task reads as `high`, so a
   classifier failure on an implement run pins rather than gambles.
3. **Balance the workhorses by weekly usage.** Auto normal work considers Codex and Grok only. A
   provider at or above `hard_ceiling_pct`, or whose weekly usage is unknown or non-authoritative,
   is excluded. When both are available, the lower weekly percentage wins; ties go to Codex.
   If neither has usable capacity, the configured default (Codex by default) is used and the
   decision visibly records the all-unavailable fallback. Claude's 5-hour window does not pace
   automatic routing; Claude is reserved for the capability pins above. Grok remains available for
   explicit dispatch with `--provider grok`.
4. **Complete the provider, model, and effort pins.** With no pins, classification chooses the
   provider through usage routing, then complexity chooses the Codex model from its tier table and
   maps low to low, medium to medium, and high or ultra to high effort. Grok uses its lifecycle
   default model and effort. An explicit
   Claude or Codex provider preserves that provider while classification fills omitted model and
   effort. An explicit Claude or Codex provider and model preserves both while classification fills
   effort. Three explicit values are exact and skip routing classification. Grok accepts an explicit
   model and rejects `--effort`; OpenCode has no derived
   model and receives no derived effort.
5. **Dispatch and log.** The job is spawned detached, its backend job id is resolved, and the whole
   decision lands in a SQLite decision log.

Complexity is scored independently of the capability pins and never influences which provider a
task lands on. A low complexity task can run on either provider, and so can an ultra one.

## Install

### From a release

Download a Linux x86_64 build from the [releases page](https://github.com/TheConnMan/agent-router/releases).
The `musl` build is statically linked and has no libc requirement; the `gnu` build is dynamically
linked against the system glibc.

Asset names carry the tag, so pick the version explicitly:

```bash
VERSION=v0.1.0
ASSET=agent-router-$VERSION-x86_64-unknown-linux-musl

curl -fsSLO https://github.com/TheConnMan/agent-router/releases/download/$VERSION/$ASSET.tar.gz
curl -fsSLO https://github.com/TheConnMan/agent-router/releases/download/$VERSION/$ASSET.tar.gz.sha256

sha256sum -c $ASSET.tar.gz.sha256
tar xzf $ASSET.tar.gz
install -Dm755 $ASSET/agent-router ~/.local/bin/agent-router
```

Every asset ships with a `.sha256` companion file, as used above.

### From source

Requires a stable Rust toolchain with edition 2024 support (1.85 or newer). SQLite is compiled in
via `rusqlite`'s bundled feature, so a C compiler is needed but no system SQLite is.

```bash
git clone https://github.com/TheConnMan/agent-router.git
cd agent-router
cargo install --path crates/agent-router-cli
```

## Prerequisites

| Tool | Needed for | Notes |
| --- | --- | --- |
| `claude` | Claude dispatch, Claude usage, and classification on the default engine | Must be on `PATH` and logged in. With the default `engine = "claude"` the classifier runs on every `--provider auto` call, so `claude` is exercised even when every task ends up on Codex. |
| `codex` | Codex dispatch, Codex usage, and classification when `engine = "codex"` | Dispatch goes through `codex app-server daemon`, which the router starts on demand. |
| `grok` | Grok dispatch, automatic workhorse routing, and an eligible Grok adversarial review | Optional. Router reuses Agent Viewer's public lifecycle and never configures Grok itself. |
| `opencode` | OpenCode dispatch | Optional. Only reached via an explicit `--provider opencode`. |

The classifier engine is a budget decision rather than a quality one: scoring is a single small
strict JSON answer either model can produce, so `[classifier].engine` selects which weekly quota
the per task call is drawn from. Set it to `codex` to keep the Claude weekly budget for dispatched
work.

Usage source failures never block a dispatch. An unavailable Claude source fails open as full
headroom. An unavailable or malformed Codex sessions source reports stale closed capacity, so
Codex has no known weekly capacity and is ineligible for automatic routing. If neither provider
has known capacity, the router still dispatches to the configured default. `agent-router doctor`
reports the source status and exits nonzero for a stale read, `agent-router usage` names the source
per provider, and every decision row records `claude_usage_stale` and `codex_usage_stale`.
Grok capacity participates in ordinary automatic task routing and adversarial review eligibility.
No Grok billing data at all is an unknown capacity verdict, not a reading of 100 percent usage;
it fails closed and makes Grok ineligible for both paths.

Usage comes from:

- **Claude**: `/tmp/claude-usage-cache.json` when it is under five minutes old, otherwise the OAuth
  usage endpoint authenticated with `~/.claude/.credentials.json`. A successful fetch refreshes
  that shared cache, which the statusline and other tooling also read.
- **Codex**: the newest rollout under `$CODEX_HOME/sessions` (default `~/.codex/sessions`) that
  carries a `rate_limits` event. Override the scan root with `$CODEX_SESSIONS_DIR`.
- **Grok**: a read-through cache at `/tmp/grok-usage-cache.json`, overridden with
  `$GROK_USAGE_CACHE`. A valid cache under 300 seconds old is used directly. Otherwise Router
  fetches live Grok billing and writes a normalized, non-secret cache entry on success; if that
  fails, it uses a valid stale cache, then scans the whole
  `~/.grok/logs/unified.jsonl` backwards for the newest billing event, and finally reports no
  capacity. Agent Router is the cache's sole writer; readers must not modify it.

## Usage

### `run`

Route one task and dispatch it as a background job.

```bash
# Classify and route automatically, in the current directory.
agent-router run "Add a --json flag to the log command"

# Decide and log without dispatching. The fastest way to see what the router would do.
agent-router run "Refactor the parity scanner" --dry-run

# Pin the provider while classification fills omitted values.
agent-router run "Bump the lockfile" --provider codex --model gpt-5.6-luna

# Dispatch a Grok task explicitly (auto routing may also select it for ordinary work).
agent-router run "Review this migration plan" --provider grok --model grok-4

# Route work in another directory.
agent-router run "Fix the failing test" --dir ~/git/other-project
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | current directory | Working directory for the dispatched job. |
| `--provider <NAME>` | `auto` | `auto` classifies the task, balances ordinary work between Codex and Grok, and pins Claude for capability needs. An explicit provider pins it. |
| `--model <NAME>` | tier table | Model pin. Requires an explicit `--provider`. With explicit Claude or Codex and no effort, classification fills effort. Pairing it with `--provider auto` is rejected. An explicit Grok model reaches the public lifecycle unchanged. OpenCode does not derive a model. |
| `--effort <NAME>` | complexity mapping | Effort pin. Requires an explicit provider and model. Low maps to low, medium to medium, and high or ultra maps to high for Codex and Claude. Grok rejects this flag; OpenCode does not receive derived effort. |
| `--name <NAME>` | the model's title, or three to five words derived from the task | Name for the dispatched job. Supplying it skips the naming call. It reaches the `claude --bg --name` argv verbatim, names the Codex thread, and is recorded as `job_name` in the decision log for every provider, so callers that reconcile inflight jobs by exact name depend on it. An empty or whitespace only name is rejected. |
| `--dry-run` | off | Decide and log, dispatch nothing, and project the weekly draw the job is likely to cost on the provider it landed on. |
| `--mcp-config <PATH>` | none | MCP config file for the dispatched Claude job. Repeatable. Rejected for every other provider, including Grok, and the check runs after routing, so pairing it with `--provider auto` fails whenever classification lands on a provider other than Claude. |
| `--strict-mcp-config` | off | Use only the `--mcp-config` files and drop every inherited MCP server. See the warning below before using it. |
| `--json` | off | Emit the full decision, including gates, classification, and usage. |

The projection is an upper bound, not the job's own cost. It is the median gap between this
provider's weekly percentage at consecutive decisions on the same model, so it also carries
everything else that consumed the same weekly quota over those intervals: your interactive
sessions, jobs on other models, and anything dispatched without going through the router. It is
the honest figure the decision log can support, and it is the right one for asking whether a job
fits in what is left. Fewer than three comparable jobs in the log prints an explicit insufficient
data line instead of a number.

`--strict-mcp-config` also strips the claude.ai connectors, and no `--mcp-config` file can restore
them. That interacts badly with routing: a task sent to Claude precisely because Codex was missing
a connector can lose the very connector it was routed for. Pass it only when the job genuinely
needs nothing beyond the files given.

An explicit Grok dispatch reuses `agent-viewer-core`'s public `GrokLifecycle`. Router does not
implement an ACP client or durable Grok session parser. The lifecycle's nonempty official session
identity is copied unchanged into the dispatch result and decision log as `job_id`; use that exact
identity with `agent-router status`.

The lifecycle connects only to the persistent user leader. Install and enable the reference
`grok-agent-leader.service` shipped by `agent-viewer-core` before dispatching. Router never starts
or owns the leader and never runs `grok --single`. Interactive attachment remains Viewer owned and
uses `grok --leader --resume <session-id>` so attaching cannot replace the shared leader.

### `adversarial-review`

Run a review synchronously with an eligible provider other than the provider that initiated the
request. The primary provider is always excluded, including Grok. Candidate providers must be
registered as review capable, have an authoritative known and fresh weekly capacity reading, and
be below 90 percent usage. Grok is a registered alternative when its public lifecycle reports an
authoritative leader and capacity is available.
The command does not classify the request or start a detached background job. It waits for the
review to reach a terminal result, then prints the review body.

The eligible provider with the lowest effective weekly usage is selected. By default, Claude has a
25-point reserve (`[adversarial_review] claude_usage_reserve_pct = 25.0`), protecting its premium
capacity: Claude is selected only when its raw weekly usage is at least 25 points lower than the
other eligible reviewer. Set the reserve to `0.0` for raw-usage-only selection. The reserve never
makes an ineligible provider eligible.

```bash
# Have a provider other than Codex review the request and wait for the result.
agent-router adversarial-review --primary codex "Review the proposed authentication change"

# Return the decision and review body as machine-readable JSON.
agent-router adversarial-review --primary codex --json "Review the proposed authentication change"
```

Text mode prints the completed review body. JSON reports `status`, `primary_provider`,
`reviewer_provider`, `reviewer_model`, usage provenance, the selection rationale, and `result` when
the review completes. When no eligible alternative exists, it reports the reason and exits `3`.
A completed review exits `0`; an invocation or infrastructure failure exits `1`. Review execution
uses the provider's review contract and is never routed through an ordinary task. Claude and Codex
are launched with enforced read only restrictions. Grok's persistent lifecycle currently registers
in YOLO mode: its prompt asks for read only review behavior and supplies no MCP servers, but Grok's
server side tools are not sandboxed. Selecting Grok therefore trusts it not to mutate the working
tree or execute side effects. For Grok, the result also carries the exact official lifecycle
session identity. Grok reviewer sessions are disposable: after Router reads the final review text,
it uses the same public lifecycle to remove only the session it created. A failed cleanup is
reported rather than silently leaving a reviewer session behind.

### `usage`

Weekly headroom for the Codex/Grok workhorse pair, plus 5-hour observations where providers expose them.

```bash
$ agent-router usage
provider  5h       weekly  source     weekly reset
claude     12.4%    58.1%  live       in 41h07m
codex       0.0%  unknown  fail-open  -
grok        0.0%  unknown  none       -
```

The weekly column reads `unknown` when `weekly_known` is false, meaning no usable capacity verdict
was read. An unread window reports 0 percent used, and printing that as `0.0%` states a reading
nobody took. A live Codex credits verdict can be known without a reset epoch, so it prints its
weekly percentage with a reset dash. Routing refuses a provider with an unknown weekly verdict.

`source` is where the numbers came from. Claude and Codex use `live` for a parsed provider payload
and `fail-open` when no usable capacity verdict was read. Grok preserves its own provenance:
`live` is a validated billing fetch, `cache` a validated cache entry, `log` the newest valid billing
event found by the whole-file reverse scan, and `none` no usable billing data. A `log` event without
`creditUsagePercent` retains `log` provenance but supplies no weekly capacity: it prints `unknown`
and is unhealthy in doctor. `none` also fails closed: it prints `unknown`, never claims 100 percent
usage, and excludes Grok from automatic routing and adversarial review selection. `weekly` prints
`unknown` for any unread workhorse window rather than claiming a measurement; unknown or ceiling
capacity is excluded from auto selection.

The weekly window places ordinary work: the lower known percentage wins, with Codex as the tie
break. If neither workhorse has usable capacity, the default provider is used and the fallback is
recorded. Claude's 5-hour window is informational and does not pace auto routing.

### `doctor`

Preflight the environment the router routes from, one line per check.

```bash
$ agent-router doctor
pass claude_on_path      claude at /home/you/.local/bin/claude
fail claude_credentials  /home/you/.claude/.credentials.json has no /claudeAiOauth/accessToken
fail claude_usage        fail-open, so the provider reads as completely unused whatever it has spent
pass codex_on_path       codex at /home/you/.local/bin/codex
pass codex_app_server    the app-server daemon answers
pass codex_rate_limits   live, read from the provider's own source
fail grok_usage          none, no billing data available
warn opencode_on_path    no executable opencode on PATH, so any dispatch to it will error
pass grok_binary         grok is available through /home/you/.local/bin/grok
warn grok_leader_registration  no authoritative persistent Grok leader is registered, so Grok dispatch and review are unavailable
pass config_parses       absent, defaults apply (/home/you/.config/agent-router/config.toml)
pass log_writable        /home/you/.local/state/agent-router/router.db takes a write
```

| Check | What it covers |
| --- | --- |
| `claude_on_path` | An executable `claude` on `PATH`. The classifier runs on every auto route, so this is exercised even when every task ends up on Codex. |
| `claude_credentials` | `~/.claude/.credentials.json` exists, parses, and carries `/claudeAiOauth/accessToken`. Without it the usage reader has nothing to authenticate with. |
| `claude_usage` | Whether the Claude usage read was live or fell open. |
| `codex_on_path` | An executable `codex` on `PATH`. |
| `codex_app_server` | Whether the app-server daemon answers, which is the transport every Codex dispatch goes through. Observed only: doctor does not start a daemon, so an absent one is reported rather than created. |
| `codex_rate_limits` | Whether the Codex usage read was live or fell open. |
| `grok_usage` | Grok billing provenance: `live` and `cache` are usable capacity readings. `log` is healthy only when its event supplies weekly capacity; without `creditUsagePercent`, it remains `log` but is unknown and unhealthy. `none` means no usable billing data and fails closed for routing. |
| `opencode_on_path` | An executable `opencode` on `PATH`. |
| `grok_binary` | An executable `grok` on `PATH`. Doctor only observes it and does not create Grok configuration. |
| `grok_leader_registration` | Whether the public Grok lifecycle reports an authoritative persistent leader. This is separate from the binary check and is required for explicit Grok dispatch and Grok reviewer selection. |
| `config_parses` | The config file parses, read directly so a diagnostic never creates the file it was asked to report on. An absent file is a pass: the router runs on the same defaults. |
| `log_writable` | The decision log opens and takes an actual write. Opening alone proves nothing, because the schema batch is all `IF NOT EXISTS` and can succeed on a database the next dispatch cannot write to. |

One rule decides the severity. **Fail** means the router would keep running on inputs it cannot
trust, or could not run at all: a missing classifier, unreadable credentials, a usage number that is
a default rather than a reading, a config file that does not parse, a log that cannot take a row.
**Warn** means a degraded path that fails loudly at the moment it is used, so nothing routes wrongly
because of it: a missing `codex` or `opencode` binary, or a daemon that does not answer, all error
at dispatch time rather than quietly changing where work lands. A fail open usage read is a warning
rather than a failure when that provider's binary is not on PATH, because a box that never routes
there has nothing to sign in to. A log another writer holds the lock on is a warning too: that is
contention, and the next dispatch takes the lock on its own.

Exit code is `0` when every check is pass or warn, `1` when any check fails. A missing `opencode` is
never a failure: it is a provider the router can route to on request, not one it needs, so
installing it or not never moves the exit code.

A missing Grok binary or authoritative leader is also a warning. These checks describe why Grok is
unavailable without hiding the automatic capacity decision. `grok_usage` is a failure when Grok is
installed but has no usable weekly capacity, and a warning when Grok is absent. That includes a
`log` provenance event without `creditUsagePercent`: it is known to be a log read but remains
unknown capacity. Both it and `none` fail closed instead of masquerading as an exhausted 100 percent
reading.

All three usage checks come from a single usage read, so doctor asks each provider once and its
lines cannot disagree about the same read.

### `log`

Recent routing decisions, newest first.

```bash
$ agent-router log --limit 3
#87 codex orchestration no medium proj claude 88% codex 61% gates[] codex 23% claude 58% 019c3f2a dispatched
     Port usage.sh to Rust with the same fail-open semantics
#86 claude orchestration yes high proj claude 88% codex 61% gates[orchestration] codex 23% claude 58% 019c3f19 dispatched mark bad note routed to codex, needed connectors
     Fix the flaky parity test and work out why it only fails in CI

# Judge a decision: was routing it there the right call.
agent-router log --mark 87 bad --note "routed to codex, needed connectors"
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--limit <N>` | `10` | Newest rows to print. |
| `--mark <ROW_ID> <MARK>` | none | Record the human judgement on one row: `good`, `bad`, or `rerouted`. Any other value is rejected and exits nonzero naming the accepted three. An unknown `ROW_ID` also exits nonzero, without writing anything. Short circuits: it prints one confirmation line instead of the listing. |
| `--note <TEXT>` | none | Free text alongside `--mark`. Requires `--mark`; a note with no mark is rejected. An empty or whitespace only note is rejected rather than stored. |
| `--json` | off | Emit the full decision, including gates, classification, and usage. Rejected alongside `--mark`, which prints a confirmation line rather than a listing. |

`--json` emits every recorded column, including the full task text, the rationale, and the
dispatch outcome. It also still prints the scores the classifier no longer produces
(`verdict`, `confidence`, and the two rubric counts), because the rows already in the log carry
them and this is the only way to read one back through the tool; they are null on every row
written since. It also carries `router_version`, stamped from the router's own build on every
write; null there means the row predates the column, so its provenance is genuinely unknown
rather than absent. The log is the tuning surface: each gate tag names a specific rule that fired,
so routing behaviour can be audited against outcomes rather than recalled.

Two of those columns are about reasoning effort and they are not the same fact. `effort` is what the
router requested. For classified Codex and Claude work, low maps to low, medium to medium, and high
or ultra to high. `effective_effort` is what the backend reported the job will actually run at, and
it is recorded only where a backend genuinely says: Codex reports its resolved effort on the
`thread/start` reply, so a Codex row carries it, and it moves when your `~/.codex/config.toml` moves.
Claude exposes no effective effort anywhere and OpenCode discards effort entirely, so both stay null
rather than being filled in from the model, the decision, or a config file. Null also covers a dry
run, which dispatched nothing, and a row written before the column existed. In every case null means
nobody observed an effort, which is not the same as a job running at no effort. See
[docs/configuration.md](docs/configuration.md#what-reasoning-effort-a-dispatched-job-actually-runs-at).

Marking a row is the human half of the loop: `status` can only say whether a job ran, never whether
routing it to that provider was the right call, and the mark is what makes the log tunable against
outcomes rather than intuition. Marking a row again replaces the earlier judgement outright. Because
the mark and the note are written together as one annotation, re marking without a note clears any
note stored from an earlier mark on that row.

### `stats`

Aggregate metrics over recent routing decisions, so the heuristic can be tuned against what it
actually did rather than against what it feels like it did.

```bash
# The default window: the 200 newest decisions.
agent-router stats

# Narrow the window by age as well. Accepts h, d, and w.
agent-router stats --since 7d

# Machine readable.
agent-router stats --json
```

Reported over the window: the rows considered and their oldest and newest timestamps, the count per
provider, the count per gate tag, the complexity distribution (with a row that was never scored
counted as `unscored`), the router version distribution (with a row carrying no version counted as
`unknown`), the number of auto routes, and three rates. The flip rate is the auto routed
rows carrying a provider moving gate (`flipped_on_exhaustion`, `projected_overdraw`,
`five_hour_pacing`, or the retired `pace_flip` and `headroom_tiebreak`, which rows already in the
log may still carry) over all auto routes. A
row carrying more than one of them counts once, because
the route moved once. The classifier failure rate is the auto routed rows carrying `classifier_failed` over the
same denominator. Both are denominated on auto routes only, because a row that named its provider
never ran a usage rule, and only a fully pinned row skips routing classification. The dry run share is denominated on every
row instead, since any row can be a dry run. Each rate carries its numerator and denominator so it
can be checked by hand, and a rate with no denominator reads `-` rather than a percentage.

Then the feedback breakdowns: a bad rate and a failure rate, each broken down by gate tag, by
provider, and by complexity tier. The bad rate is the rows a human marked `bad` or `rerouted` with
`agent-router log --mark`, over the rows carrying any of the three marks: `good` is the only mark
outside the numerator, because `bad` and `rerouted` both say the route was the wrong call and
`rerouted` says in addition that the job had to be moved off it. The failure rate is the rows
`agent-router status` settled to `failed`, plus the rows whose dispatch itself errored. A row
carrying several gate tags counts under each of them, since a gate breakdown is per gate by
definition, unlike the flip rate, where one route that moved counts once however many gates fired
on it.

Both denominators are deliberately narrower than the row count, and that is what makes the numbers
worth trusting. A bad rate counts only the rows a human actually judged, because an unmarked row is
absence of evidence rather than evidence of a good route, and counting it as good would drive every
bad rate toward zero as the log grows. A failure rate counts only the rows whose fate is settled
(`completed`, `failed`, or a dispatch error), because a row still reading `dispatched`, `running`,
or `unknown` has not been shown to have succeeded, and counting it as one reports a perfect record
over jobs nobody has heard back about. A dry run never enters a failure denominator at all, since it
dispatched nothing that could succeed or fail, though it does enter a bad rate, because it still has
a route a human can judge.

Every breakdown carries the full key set of the distribution it breaks down, so a key nobody has
judged reads `0 of 0` with a null share instead of vanishing from the report. That is also why a
zero and a null are different answers here: `0 of 4` is four judged routes with nothing wrong,
`0 of 0` is nothing to say yet.

`--limit` defaults to 200, which is the same window as `agent-router log --json --limit 200`, so
every number here reconciles by hand against the rows that command prints.

### `status`

Reconcile logged decisions against the backends that actually ran them, and write a terminal state
back into the decision log's `outcome` column.

```bash
$ agent-router status --limit 3
rows considered: 3
window: 1737330000000 to 1737336000000
#88 codex completed turn completed job 019c3f2c
#87 claude running working job 019c3f2a
#86 claude unknown absent job 019c2f11 no trace
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--limit <N>` | `20` | Newest matching rows to reconcile. Smaller than the `stats` default on purpose: every row here costs a live backend call, where a `stats` window is pure SQL. |
| `--since <WINDOW>` | none | Also drop rows older than a lookback window, for example `24h`, `7d`, or `2w`. Same parser as `stats --since`. |
| `--json` | off | Emit the reconciliation as structured JSON instead of one line per row. |

Only rows that actually dispatched are ever touched: the predicate is `dry_run = 0 AND job_id IS
NOT NULL`. A `--dry-run` row and a row whose dispatch itself failed (which carries no job id, and
whose `outcome` already holds the backend's own error text) are both left alone.

Claude jobs resolve through `claude agents --json --all`. That list is a bounded recent window, so
a job missing from it reports `unknown`, never `completed`: absence is equally consistent with
completed, crashed on startup, or never started, and the router refuses to guess which. A claude
job reporting `stopped` also reports `unknown` rather than `failed`, because an operator stopping a
healthy job is not a routing failure.

Codex jobs resolve through the app-server `thread/read` call with `includeTurns` set, and the state
comes from the first turn record, since the router starts exactly one turn per thread. Turn history
is read from disk, so a codex job still resolves even when the daemon has not loaded the thread.

Grok rows resolve through one public lifecycle read and an exact lookup of the stored official
session identity. Known lifecycle states map only where their meanings are proven. A missing,
ambiguous, unavailable, or unrecognized result remains `unknown` and never overwrites a proven
terminal outcome.

Every row settles into one of four states written to `outcome`: `running`, `completed`, `failed`,
or `unknown`. `unknown` never overwrites an already proven `completed` or `failed`: once a job is
proven finished, a later run that can no longer see the backend leaves that fact alone, which is
what makes rerunning `status` safe.

Claude rows also carry a `traced` flag: whether a session transcript exists on disk for that job.
It is evidence only and never changes the state, so a traced but unresolvable job still reports
`unknown`. It exists to tell "ran and we lost track of it" apart from "vanished without a trace".

`--json` emits `rows_considered`, `oldest_created_at_ms`, and `newest_created_at_ms` at the top
level, plus a `rows` array where each row carries `id`, `provider`, `job_id`, `observation`,
`state`, and `traced`.

`status` owns its own exit code the way `doctor` does: `0` when nothing in the window is known to
have failed, `1` when something is, `2` when the command could not run at all. An `unknown` never
moves it, since an absence of information is not a finding.

### `parity`

Compare project scoped Claude and Codex declarations, so either provider can take over a project
without an unexpected capability or instruction gap.

```bash
# Scan the current directory.
agent-router parity

# Scan explicit roots.
agent-router parity --root ~/git --root ~/work

# Machine readable, for a scheduled drift check.
agent-router parity --json
```

The scan walks each root for directories containing any of `.mcp.json`, `.codex/config.toml`,
`CLAUDE.md`, or `AGENTS.md`, then compares the MCP servers each side declares. Reported difference
kinds are `missing_in_codex`, `missing_in_claude`, `command_differs`, `args_differ`,
`env_keys_differ`, `transport_differs`, `endpoint_differs`, and `standalone_claude_md` (a
`CLAUDE.md` with no `AGENTS.md` beside it, which Codex cannot consume). Transport declarations are
compared semantically and private endpoints exactly, but endpoint values are never emitted in reports.

The ambient configuration every project inherits is compared too, as its own entry: `~/.claude.json`
against `~/.codex/config.toml`. It is reported first in the human output and as a top level `global`
object beside `projects` in `--json`, and it carries only the MCP server difference kinds, since
`standalone_claude_md` has no counterpart in those two files. Its differences never appear under a
project, so a global gap is never blamed on one. An absent file means no servers declared on that
side; a present file that does not parse is a scan error naming the file and its position, never its
contents. The same exception mechanism covers it: record an exception whose `path` is your home
directory, which is the root a global difference is reported under.

Each project resolves to one of three statuses. `aligned` means no differences. `intentional`
means every difference is covered by a recorded exception in the config. `drift` means at least one
difference is not. The global entry carries its own status on the same rule.

Exit codes make this usable as a gate and are unchanged by the global scope: `0` for aligned or
intentional, `1` for drift, `2` for a configuration or scan error. An uncovered global difference is
drift on its own, with no project difference anywhere in the scan.

## Configuration

`~/.config/agent-router/config.toml`, written with defaults on first run. Every key is optional and
an omitted key is exactly its default. A file that exists but does not parse is a hard error rather
than a silent fallback to defaults, because routing against ceilings and a connector list the
operator never wrote is worse than refusing to run.

The one section that genuinely needs human maintenance is `connectors`: it is the authoritative
inventory of what Codex can reach on this machine, and rubric criterion 5 is scored against exactly
that list. Anything absent from it is what forces a task to Claude.

See [docs/configuration.md](docs/configuration.md) for the full reference.

## State on disk

| Path | Contents |
| --- | --- |
| `~/.config/agent-router/config.toml` | Routing policy, ceilings, model tiers, connector inventory, parity roots and exceptions. |
| `/tmp/grok-usage-cache.json` | Normalized, non-secret Grok billing cache. Override with `$GROK_USAGE_CACHE`; Agent Router is its sole writer. |
| `~/.local/state/agent-router/router.db` | SQLite decision log. Holds full task text, so its directory is created mode `0700`. |
| `~/.local/state/agent-router/logs/` | Per dispatch stdout and stderr from detached jobs. |

## Development

To refresh the installed binary after merges and commits on `main`, enable the repository hook
path once:

```bash
git config core.hooksPath .githooks
```

The hook runs `~/.cargo/bin/cargo build --release --workspace` and copies the resulting binary to
`~/.local/bin/agent-router`. It skips commits on other branches.

```bash
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
```

The workspace is two crates. `agent-router-core` holds the routing machinery: usage readers, the
classifier, the decision engine, the decision log, the parity scanner, and provider dispatch.
`agent-router-cli` is the `agent-router` binary and holds argument parsing and output formatting
only.

Functions are marked `PURE` or `IMPURE` in their doc comments. The decision engine is pure given
its inputs, which is what makes the routing policy testable without touching a network, a provider
CLI, or the clock.

## License

MIT. See [LICENSE](LICENSE).
