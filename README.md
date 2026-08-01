# agent-router

Route one task to Codex, Claude, or OpenCode by task shape and weekly usage headroom, dispatch it
as a detached background job, and record why the decision was made.

The problem it solves: two coding agents with separate weekly quotas, and a running judgement call
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

1. **Classify.** One small model call scores the task and returns strict JSON: `orchestration`,
   `missing_connector`, and a complexity tier. The call is deliberately hermetic. On the Claude engine it
   runs with `--safe-mode` and `--strict-mcp-config`; on the Codex engine with
   `--ignore-user-config`, `--ignore-rules`, and `project_doc_max_bytes=0`. Either way no project
   `CLAUDE.md`, `AGENTS.md`, skill, plugin, hook, or MCP server can shift the score. The Codex
   engine additionally runs with its shell, browser, computer use, image, app, and skill search
   tools disabled: scoring needs no tool, and a task carrying an injected instruction must have
   nothing to reach for. If the call fails or times out, the configured default provider stays in
   force and the decision is tagged `classifier_failed`.
2. **Apply the capability pin.** A missing connector, or a task needing several agents to exchange
   findings mid-run, pins to Claude regardless of usage. These are statements that the task cannot
   run on Codex at all, so every usage rule below is bypassed.
3. **Apply the hard ceiling, then the run rate override, then pace on Claude's 5 hour window.** A
   provider at or above `hard_ceiling_pct` is ineligible: exactly one ineligible sends the task to
   the other (`flipped_on_exhaustion`), both ineligible keeps the default (`over_ceiling`). With
   both eligible, the task moves only when the provider it is on is burning more than
   `pace_flip_gap` points further ahead of its own weekly window than the other is of its own
   (`pace_flip`), which is rare on purpose. Finally a task still bound for Claude moves to Codex
   when Claude's 5 hour percent is at or above `claude_five_hour_pacing_pct` and Codex is under the
   hard ceiling (`five_hour_pacing`), because an exhausted 5 hour window stalls the job rather than
   merely costing more.
4. **Pick the model from complexity.** The complexity tier selects the model from the per provider
   tier table. Reasoning effort is deliberately not decided: the router forces none and each
   backend resolves its own, and the log records that resolution wherever the backend reports one.
   See [docs/configuration.md](docs/configuration.md#modelscodex-and-modelsclaude)
   for what that actually means per backend, because it is not the model default on Codex.
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
| `opencode` | OpenCode dispatch | Optional. Only reached via an explicit `--provider opencode`. |

The classifier engine is a budget decision rather than a quality one: scoring is a single small
strict JSON answer either model can produce, so `[classifier].engine` selects which weekly quota
the per task call is drawn from. Set it to `codex` to keep the Claude weekly budget for dispatched
work.

Usage reading fails open by design. A missing credential file, an unreachable API, or an
unparseable payload all read as full headroom, because a usage read must never be the thing that
blocks a dispatch. The consequence is worth knowing: if Claude credentials cannot be read, Claude
looks completely unused and will win every headroom tiebreak. A fail open read and a genuinely idle
provider report the same numbers, so the router records which one it got rather than leaving it to
be inferred: `agent-router doctor` reports every read as `live` or `fail-open` and exits nonzero on a
fail open one, `agent-router usage` names the same source per provider, and every decision row
records `claude_usage_stale` and `codex_usage_stale`. Routing itself is unchanged by the marker.

Usage comes from:

- **Claude**: `/tmp/claude-usage-cache.json` when it is under five minutes old, otherwise the OAuth
  usage endpoint authenticated with `~/.claude/.credentials.json`. A successful fetch refreshes
  that shared cache, which the statusline and other tooling also read.
- **Codex**: the newest rollout under `$CODEX_HOME/sessions` (default `~/.codex/sessions`) that
  carries a `rate_limits` event. Override the scan root with `$CODEX_SESSIONS_DIR`.

## Usage

### `run`

Route one task and dispatch it as a background job.

```bash
# Classify and route automatically, in the current directory.
agent-router run "Add a --json flag to the log command"

# Decide and log without dispatching. The fastest way to see what the router would do.
agent-router run "Refactor the parity scanner" --dry-run

# Skip classification entirely and name the provider.
agent-router run "Bump the lockfile" --provider codex --model gpt-5.6-luna

# Route work in another directory.
agent-router run "Fix the failing test" --dir ~/git/other-project
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | current directory | Working directory for the dispatched job. |
| `--provider <NAME>` | `auto` | `auto` classifies. `codex`, `claude`, or `opencode` skips classification. |
| `--model <NAME>` | tier table | Model override. Requires an explicit `--provider`. Pairing it with `--provider auto` is rejected: the router exits nonzero naming both flags rather than dropping the override, because the auto path chooses its own model from the tier table. |
| `--name <NAME>` | first 40 characters of the task | Name for the dispatched job. It reaches the `claude --bg --name` argv verbatim, names the Codex thread, and is recorded as `job_name` in the decision log for every provider, so callers that reconcile inflight jobs by exact name depend on it. An empty or whitespace only name is rejected. |
| `--dry-run` | off | Decide and log, dispatch nothing, and project the weekly draw the job is likely to cost on the provider it landed on. |
| `--mcp-config <PATH>` | none | MCP config file for the dispatched Claude job. Repeatable. Rejected for any other provider, and the check runs after routing, so pairing it with `--provider auto` fails whenever classification lands on a provider other than Claude. |
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

### `usage`

Weekly and 5 hour headroom for both providers.

```bash
$ agent-router usage
provider  5h      weekly  source     weekly reset
claude     12.4%   58.1%  live       in 41h07m
codex       0.0%    0.0%  fail-open  -
```

`source` is where the numbers came from: `live` for a parsed payload, `fail-open` for a read that
found nothing and defaulted to full headroom. Those two zeroes above are the fail open default, not
a measurement, and they are exactly what a genuinely idle provider also reports, so the source
column is the only thing on the line that separates them.

The weekly window is what places a single job: it decides which provider has room for the task in
front of the router. Claude's 5 hour window is what paces a stream of them, moving work away from
Claude once its 5 hour percent reaches `claude_five_hour_pacing_pct` and Codex still has weekly
room. Codex's own 5 hour number is reported here for the operator and never influences routing.

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
warn opencode_on_path    no executable opencode on PATH, so any dispatch to it will error
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
| `opencode_on_path` | An executable `opencode` on `PATH`. |
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

Both usage checks come from a single usage read, so doctor asks each provider once and its two
lines cannot disagree about the same read.

### `log`

Recent routing decisions, newest first.

```bash
$ agent-router log --limit 3
#87 codex orchestration no medium pace claude -12 codex +6 gates[] codex 23% claude 58% 019c3f2a
     Port usage.sh to Rust with the same fail-open semantics
#86 claude orchestration yes high pace claude -12 codex +6 gates[orchestration] codex 23% claude 58% Fix the ...
     Fix the flaky parity test and work out why it only fails in CI

# Judge a decision: was routing it there the right call.
agent-router log --mark 87 bad --note "routed to codex, needed connectors"
```

| Flag | Default | Meaning |
| --- | --- | --- |
| `--limit <N>` | `10` | Newest rows to print. |
| `--mark <ROW_ID> <MARK>` | none | Record the human judgement on one row: `good`, `bad`, or `rerouted`. Any other value is rejected and exits nonzero naming the accepted three. An unknown `ROW_ID` also exits nonzero, without writing anything. Short circuits: it prints one confirmation line instead of the listing. |
| `--note <TEXT>` | none | Free text alongside `--mark`. Requires `--mark`; a note with no mark is rejected. An empty or whitespace only note is rejected rather than stored. |
| `--json` | off | Emit the full decision, including gates, classification, and usage. |

`--json` emits every recorded column, including the full task text, the rationale, and the
dispatch outcome. It also still prints the scores the classifier no longer produces
(`verdict`, `confidence`, and the two rubric counts), because the rows already in the log carry
them and this is the only way to read one back through the tool; they are null on every row
written since. The log is the tuning surface: each gate tag names a specific rule that fired, so
routing behaviour can be audited against outcomes rather than recalled.

Two of those columns are about reasoning effort and they are not the same fact. `effort` is what the
router decided, which is nothing, because the model tier is the toggle. `effective_effort` is what
the backend reported the job will actually run at, and it is recorded only where a backend genuinely
says: Codex reports its resolved effort on the `thread/start` reply, so a Codex row carries it, and
it moves when your `~/.codex/config.toml` moves. Claude exposes no effective effort anywhere and
OpenCode discards effort entirely, so both stay null rather than being filled in from the model, the
decision, or a config file. Null also covers a dry run, which dispatched nothing, and a row written
before the column existed. In every case null means nobody observed an effort, which is not the same
as a job running at no effort. See
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
counted as `unscored`), the number of auto routes, and three rates. The flip rate is the auto routed
rows carrying a provider moving gate (`flipped_on_exhaustion`, `pace_flip`, `five_hour_pacing`, or
the retired `headroom_tiebreak`, which rows already in the log still carry) over all auto routes. A
row carrying more than one of them counts once, because
the route moved once. The classifier failure rate is the auto routed rows carrying `classifier_failed` over the
same denominator. Both are denominated on auto routes only, because a row that named its provider
never ran a usage rule and never ran the classifier. The dry run share is denominated on every
row instead, since any row can be a dry run. Each rate carries its numerator and denominator so it
can be checked by hand, and a rate with no denominator reads `-` rather than a percentage.

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
`env_keys_differ`, and `standalone_claude_md` (a `CLAUDE.md` with no `AGENTS.md` beside it, which
Codex cannot consume).

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
