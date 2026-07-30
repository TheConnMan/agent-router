# Configuration

`agent-router` reads `~/.config/agent-router/config.toml`. The file is written with defaults the
first time a command that needs it loads it, so the fastest way to see the current schema is to run
a throwaway decision once and open the file:

```bash
agent-router run "hello" --provider claude --dry-run
cat ~/.config/agent-router/config.toml
```

`usage` and `log` do not read the config, so neither creates it.

Two rules govern the whole file:

- **Every key is optional.** An omitted table is exactly its defaults, and a partial table
  overrides only the keys it names. Writing `[models.codex]` with just `low = "..."` leaves
  `medium`, `high`, and `ultra` at their defaults.
- **A malformed file is a hard error, not a fallback.** If the file exists but does not parse,
  commands fail rather than silently substituting defaults. Routing jobs against ceilings and a
  connector inventory the operator never wrote would be worse than refusing to run.

## Defaults in full

```toml
hard_ceiling_pct = 97.0
headroom_flip_gap = 25.0
classifier_timeout_secs = 30
connectors = [
    "local shell",
    "git",
    "gh (github)",
    "airtable",
]

[policy]
default_provider = "codex"
weekly_routing = true

[classifier]
engine = "claude"
claude_model = "haiku"
codex_model = "gpt-5.6-luna"

[models.codex]
low = "gpt-5.6-luna"
medium = "gpt-5.6-terra"
high = "gpt-5.6-sol"
ultra = "gpt-5.6-sol"

[models.claude]
low = "sonnet"
medium = "opus[1m]"
high = "opus[1m]"
ultra = "fable"

[parity]
roots = []
exceptions = []
```

## Top level keys

### `hard_ceiling_pct`

Default `97.0`. Weekly percent used at or above which a provider counts as exhausted.

This is the threshold that lets a confident verdict be overridden. A confident verdict flips only
when its own provider is at or over this ceiling and the other provider is not. When both providers
are at or over it, no flip happens at all: the verdict provider is used anyway and the decision is
tagged `over_ceiling`, because at that point there is no better destination to move to.

### `headroom_flip_gap`

Default `25.0`. How many points of weekly headroom advantage flip a borderline verdict.

Only borderline (medium or low confidence) verdicts are subject to this. If the other provider has
at least this many more points of weekly headroom remaining, the task moves there and the decision
is tagged `headroom_tiebreak`. Raising it makes routing respect the rubric more and usage less.

### `classifier_timeout_secs`

Default `30`. How long the classifier call may take before it counts as failed.

A failed classifier is not fatal. The configured `default_provider` stays in force, complexity
reads as `high`, and the decision is tagged `classifier_failed`. The default is viable only because
the classifier invocation strips CLI startup cost; see the note in `classify.rs` for the measured
numbers behind that.

### `connectors`

Default `["local shell", "git", "gh (github)", "airtable"]`. The authoritative inventory of what
Codex can reach on this machine.

This is the one section that genuinely needs human maintenance. Rubric criterion 5 ("Codex has
every required connector") is scored against exactly this list, and a task needing a system absent
from it trips the `missing_connector` hard gate, which pins the task to Claude regardless of shape
or usage. The classifier is explicitly told never to set `missing_connector` because it cannot see
a connector itself, only because a named system is absent from this list.

Keep it accurate in both directions. Listing a connector Codex cannot actually reach sends work to
a provider that will fail; omitting one it can reach sends work to Claude that did not need to go
there.

## `[policy]`

### `default_provider`

Default `"codex"`. Either `"codex"` or `"claude"`. The provider in force when the classifier cannot
answer, and the starting point before the verdict is applied.

The router was built so this is a one word edit. Nothing in the decision engine assumes Codex is
the default; setting `"claude"` makes Claude the default destination and Codex the exception
without any routing logic change.

### `weekly_routing`

Default `true`. Whether weekly usage is allowed to modulate the verdict at all.

Set to `false` to route purely on task shape. Decisions are then tagged `weekly_routing_disabled`,
and neither the exhaustion flip nor the headroom tiebreak can fire. Capability hard gates still
apply, because those are not usage decisions.

## `[classifier]`

Which engine scores a task, and the model each engine scores it with.

### `engine`

Default `"claude"`. Either `"claude"` or `"codex"`. Any other value is a configuration error.

Scoring is one small strict JSON answer, so either engine can do it. The choice is about which
weekly budget the per task classifier call is drawn from. If Claude weekly budget is the scarce
resource, set this to `"codex"`.

Two consequences of `"codex"` worth knowing. Scoring runs with every tool disabled, so it cannot
read a file even though the sandbox is read only. And it writes a session rollout per scored task
rather than running ephemeral, deliberately: `codex_headroom` reads the newest rollout carrying a
`rate_limits` event, so an ephemeral classifier would spend Codex quota invisibly and leave the
router deciding against a frozen percentage. The cost is one session file per automatically routed
task.

### `claude_model` and `codex_model`

Defaults `"haiku"` and `"gpt-5.6-luna"`. The model each engine scores with. Both are kept
regardless of which engine is in force, so flipping `engine` is a one word edit rather than a
re-pick of the model.

Both want the cheapest model that reliably holds the output contract, since the classifier runs on
every automatically routed task and its only job is to emit one JSON object.

## `[models.codex]` and `[models.claude]`

One model per complexity tier, per provider. The classifier scores complexity independently of the
verdict, and the resulting tier picks the model the job is spawned with.

| Tier | When the rubric assigns it |
| --- | --- |
| `low` | Conversational, one step, mechanical, or a single file with an obvious answer. |
| `medium` | A normal well scoped implementation or investigation. |
| `high` | Spans several files, or subtle enough to need heavy reasoning or design judgement. Also the fallback for an unscored or unparseable answer. |
| `ultra` | The rare hardest work, where a wrong call is expensive and hard to reverse. Architecture or plan review, a root cause hunt that has already defeated ordinary debugging, or a direction setting design decision. |

Two deliberate choices are worth knowing before tuning these.

There is no effort table, on purpose. The model is the toggle, and each model then runs at its own
default reasoning effort. Forcing an effort on top of a model choice was tried and removed.

Complexity never changes the verdict and the verdict never changes complexity. A low complexity
task can belong to either provider, and so can an ultra one.

The Codex defaults point `high` and `ultra` at the same model because `sol` is the top of the Codex
catalogue. The Claude defaults reserve `fable` for `ultra` alone, which is why the rubric is written
to keep `ultra` deliberately hard to earn.

## `[parity]`

Inputs for the `agent-router parity` command.

### `roots`

Default `[]`. Directories to scan for projects. Relative paths resolve against the current working
directory. There is no tilde expansion: `~` is only expanded by your shell, so a path written in
this file must be absolute or relative, never `~/git`.

Precedence is explicit flags first, then this list, then the current directory. So
`agent-router parity --root ~/git` ignores this setting, and `agent-router parity` with an empty
list scans only where it was run from. Nested roots are collapsed: if both `~/git` and
`~/git/project` are listed, only `~/git` is scanned.

A root that does not exist or is not a directory is a configuration error, exit code 2.

### `exceptions`

Default `[]`. Recorded intentional differences, so a deliberate divergence does not read as drift.

Each exception is a table array entry:

```toml
[[parity.exceptions]]
path = "/home/you/git/legacy-project"
reason = "Codex has no equivalent for the Figma MCP server and does not need one"

[[parity.exceptions]]
path = "/home/you/git/other-project"
server = "airtable"
kind = "args_differ"
reason = "Codex intentionally runs the read-only Airtable profile"
```

| Field | Required | Meaning |
| --- | --- | --- |
| `path` | yes | The project directory the exception covers. Must not be empty. |
| `reason` | yes | Why the difference is intentional. Must not be blank. |
| `server` | no | Narrow the exception to one MCP server. Omit to cover every server at that path. |
| `kind` | no | Narrow to one difference kind. Omit to cover every kind. |

`path` matches one project directory exactly, not a subtree, so it names the directory holding the
`.mcp.json` or `.codex/config.toml` rather than a parent of it. As with `roots`, `~` is not
expanded here.

Valid `kind` values are `missing_in_codex`, `missing_in_claude`, `command_differs`, `args_differ`,
`env_keys_differ`, and `standalone_claude_md`.

A `reason` is mandatory by design. An exception without one would let a real gap hide as expected
behaviour, which is the exact failure the parity command exists to prevent. A project whose
differences are all covered by exceptions reports as `intentional` rather than `aligned`, so the
divergence stays visible even though it does not fail the command.

## Adding a section

One constraint applies when extending `Config` in Rust rather than editing the TOML. The TOML data
model cannot express a scalar key after a table has been opened, so the serializer emits fields in
declaration order and fails at write time if a plain value follows a field that serializes as a
table. Any new table typed field must be declared last in its struct, with all scalar fields above
it. Parsing is order insensitive, so a test that only parses a fixture will not catch this; a
config change needs at least one test that serializes the struct back out.
