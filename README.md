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

1. **Classify.** One small model call scores the task against a fixed twelve criterion rubric and
   returns strict JSON: six "Codex ready" criteria, six "Claude signal" criteria, a verdict, a
   confidence, and a complexity tier. The call is deliberately hermetic. On the Claude engine it
   runs with `--safe-mode` and `--strict-mcp-config`; on the Codex engine with
   `--ignore-user-config`, `--ignore-rules`, and `project_doc_max_bytes=0`. Either way no project
   `CLAUDE.md`, `AGENTS.md`, skill, plugin, hook, or MCP server can shift the score. The Codex
   engine additionally runs with its shell, browser, computer use, image, app, and skill search
   tools disabled: scoring needs no tool, and a task carrying an injected instruction must have
   nothing to reach for. If the call fails or times out, the configured default provider stays in
   force and the decision is tagged `classifier_failed`.
2. **Apply hard gates.** A missing connector or two or more Claude signals pins the task to Claude
   regardless of usage. These are capability decisions, so headroom never overrides them.
3. **Modulate on weekly headroom.** A confident verdict is flipped only when its provider is at or
   over the hard ceiling and the other is not. A borderline verdict is flipped when the other
   provider has a large enough weekly headroom advantage.
4. **Pick the model from complexity.** The complexity tier selects the model from the per provider
   tier table. Reasoning effort is deliberately not forced: each model runs at its own default.
5. **Dispatch and log.** The job is spawned detached, its backend job id is resolved, and the whole
   decision lands in a SQLite decision log.

Complexity is scored independently of the verdict. A low complexity task can belong to either
provider, and so can an ultra one.

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
looks completely unused and will win every headroom tiebreak.

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
| `--name <NAME>` | first 40 characters of the task | Name for the dispatched job. It reaches the `claude --bg --name` argv verbatim and is recorded as `job_name` in the decision log for every provider, so callers that reconcile inflight jobs by exact name depend on it. |
| `--dry-run` | off | Decide and log, dispatch nothing. |
| `--mcp-config <PATH>` | none | MCP config file for the dispatched Claude job. Repeatable. Rejected for any other provider. |
| `--strict-mcp-config` | off | Use only the `--mcp-config` files and drop every inherited MCP server. See the warning below before using it. |
| `--json` | off | Emit the full decision, including gates, classification, and usage. |

`--strict-mcp-config` also strips the claude.ai connectors, and no `--mcp-config` file can restore
them. That interacts badly with routing: a task sent to Claude precisely because Codex was missing
a connector can lose the very connector it was routed for. Pass it only when the job genuinely
needs nothing beyond the files given.

### `usage`

Weekly and 5 hour headroom for both providers.

```bash
$ agent-router usage
provider  5h      weekly  weekly reset
claude     12.4%   58.1%  in 41h07m
codex       3.0%   22.7%  in 96h12m
```

Routing uses the weekly window only. The 5 hour window is reported because it matters for pacing a
stream of jobs, not for placing a single one.

### `log`

Recent routing decisions, newest first.

```bash
$ agent-router log --limit 3
#87 codex codex_ready 6/6 claude_signals 0/6 high medium gates[] codex 23% claude 58% 019c3f2a
     Port usage.sh to Rust with the same fail-open semantics
#86 claude codex_ready 4/6 claude_signals 2/6 high high gates[claude_signals] codex 23% claude 58% Fix the flaky ...
     Fix the flaky parity test and work out why it only fails in CI
```

`--json` emits every recorded column, including the full task text, the rationale, and the
dispatch outcome. The log is the tuning surface: each gate tag names a specific rule that fired, so
routing behaviour can be audited against outcomes rather than recalled.

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

Each project resolves to one of three statuses. `aligned` means no differences. `intentional`
means every difference is covered by a recorded exception in the config. `drift` means at least one
difference is not.

Exit codes make this usable as a gate: `0` for aligned or intentional, `1` for drift, `2` for a
configuration or scan error.

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
