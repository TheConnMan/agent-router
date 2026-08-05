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

## `config_version` and in-place migrations

Because the file is generated rather than hand-authored, a value this tool once wrote can go stale
in every existing install while the default constant in the source moves on. `config_version` is
the marker that lets those be corrected exactly once.

On load, a file stamped below the current version is migrated and rewritten in place, then stamped.
A migration only ever touches a value still equal to the default this tool used to generate;
anything else is left alone.

**On a file with no stamp, that test is a guess, and it can guess wrong.** A `30` you typed
yourself is indistinguishable from the `30` the tool generated, so a v1 migration will move it. It
is a one-time, one-value cost, and it errs toward more headroom rather than less, but it is real:
if you had deliberately pinned the old default before upgrading, re-pin it after.

After that first stamp the ambiguity is gone for good. A migration runs once per file, so setting a
migrated value back is permanent, and nothing will move it again. That is the whole reason migrations
are keyed on the stamp and not on the value: a value-keyed migration would re-apply on every single
load, and restoring the old default would be impossible rather than merely inconvenient.

Two consequences worth knowing. The rewrite serializes the whole config, so hand-added comments in
the file are lost the one time a migration runs. And a file with no `config_version` key is treated
as predating versioning, so do not delete the key to "reset" anything.

| version | migration |
| --- | --- |
| 1 | `classifier_timeout_secs` of `30`, the old generated default, becomes `60`. Any other value is left alone. |
| 2 | `headroom_flip_gap` is gone and `pace_flip_gap` replaces it. Nothing is carried across: the two keys threshold different comparisons, so a number tuned for the old one means nothing under the new one. The rewrite drops the stale key from the file. |

## Defaults in full

```toml
config_version = 2
hard_ceiling_pct = 97.0
pace_flip_gap = 70.0
claude_five_hour_pacing_pct = 90.0
classifier_timeout_secs = 60
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

A provider at or over this ceiling is ineligible. Exactly one ineligible sends the task to the
other and tags the decision `flipped_on_exhaustion`. Both ineligible keeps the default provider and
tags it `over_ceiling`, because at that point there is no better destination to move to.

Eligibility is judged before the run rate override, not after, and that order is load bearing. A
provider with two points of weekly allowance left reads as running COLD on run rate whenever its
window is nearly elapsed (95 percent used against 99 percent elapsed is a negative pace), so an
override allowed to run first would route into a provider that is out of budget.

### `pace_flip_gap`

Default `70.0`. How many points a provider must be burning ahead of the other, each measured
against its own weekly window, before a task moves off it.

Each provider's run rate is `weekly_pct` minus its expected burn, where the expected burn is how
much of its own weekly window has elapsed. Positive is hot: 80 percent spent with half the window
gone is 30 points over pace. The two providers reset at different instants, so each is measured
against its own reset and never the other's. When either reset is unknown the override is skipped
entirely and the decision is tagged `pace_unavailable`; when it fires, the decision is tagged
`pace_flip`. The comparison is strictly greater, so a gap exactly on the threshold holds.

The default is measured rather than chosen, and it is deliberately wide enough to be rare. This box
runs two Claude 20x Max plans against one Codex 5x plan, so identical work shows as several times
the percentage on the smaller allowance, and over the recorded decisions Codex sat 43 to 58 points
over pace all week from that alone. That band is a plan size artifact, not a signal, so the dead
zone has to clear it. At a gap of 25 the override moved 27 of 39 real dispatches, 25 of them onto
the provider with LESS absolute allowance left, which is the opposite of what it is for. At 70 it
moves 2. Lower it and routing follows the percentages; raise it past about 80 and the rule never
fires at all, which makes it dead config rather than a conservative setting.

There is no `headroom_flip_gap` alias. The key it named thresholded a comparison of raw weekly
percentages, which is exactly the reading that misrouted, so a file still carrying it is ignored
rather than honoured.

### `claude_five_hour_pacing_pct`

Default `90.0`. Claude 5 hour percent used at or above which a task is paced away from Claude.

This runs after the weekly rules, on the provider they landed on. A task still bound for Claude
moves to Codex when Claude's 5 hour percent reaches this threshold, and the decision is tagged
`five_hour_pacing`. It applies however the task reached Claude, the run rate override included: a
near exhausted 5 hour window stalls a Claude dispatch rather than merely making it more expensive.

Codex having room is judged by `hard_ceiling_pct`, the same threshold the exhaustion flip uses,
rather than by a second key that could drift away from it. Codex sitting exactly on that ceiling has
no room, so no pacing happens: moving the job would relocate the stall rather than avoid it.

A capability pin overrides this entirely. A task that needs a connector Codex cannot reach, or that
needs several agents exchanging findings mid-run, stays on Claude however exhausted its 5 hour
window is, because a paced job that cannot do the work is a failed job rather than a cheaper one.

Setting `weekly_routing = false` disables pacing along with every other usage driven rule. An
operator who turned weekly routing off asked to route purely on task shape, and a usage driven flip
would contradict that, so there is no second flag to leave on by accident.

Codex's own 5 hour number is deliberately ignored, in both directions: it never paces a task away
from Codex and it never keeps one on Claude. Only Claude has a 5 hour window that constrains a
stream of jobs on this box.

### `classifier_timeout_secs`

Default `60`. How long the classifier call may take before it counts as failed.

A failed classifier is not fatal, but it is not cheap either: the configured `default_provider`
stays in force, complexity reads as `high`, and the decision is tagged `classifier_failed`, so a
timeout can pick both the wrong provider and the top model tier. The measured call is 3.4-7.0s, so
this default is headroom for a slow tail rather than a target. It is viable only because the
classifier invocation strips both CLI startup cost and the model's thinking tokens; see the note in
`classify.rs` for the measured numbers behind that.

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

Default `"codex"`. Either `"codex"` or `"claude"`. The provider every task starts on, and the one
that stands when no usage rule moves it.

The router was built so this is a one word edit. Nothing in the decision engine assumes Codex is
the default; setting `"claude"` makes Claude the default destination and Codex the exception
without any routing logic change. The run rate override is symmetric and measured from whichever
provider the task is currently on, so it works in both directions unchanged.

### `weekly_routing`

Default `true`. Whether usage is allowed to move a task off the default provider at all.

Set to `false` to route purely on task shape. Decisions are then tagged `weekly_routing_disabled`,
and neither the exhaustion flip, the run rate override, nor the 5 hour pacing rule can fire.
Capability pins still apply, because those are not usage decisions.

## `[classifier]`

Which engine scores a task, and the model each engine scores it with.

### `engine`

Default `"claude"`. Either `"claude"` or `"codex"`. Any other value is a configuration error.

Scoring and job naming are one small strict JSON answer, so either engine can do both. The choice is
about which weekly budget the per task classifier call is drawn from. If Claude weekly budget is the
scarce resource, set this to `"codex"`.

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
every automatically routed task and emits the routing scores plus a concise job title.

## `[models.codex]` and `[models.claude]`

One model per complexity tier, per provider. The classifier scores complexity independently of the
capability pins, and the resulting tier picks the model the job is spawned with.

| Tier | When the rubric assigns it |
| --- | --- |
| `low` | Conversational, one step, mechanical, or a single file with an obvious answer. |
| `medium` | A normal well scoped implementation or investigation. |
| `high` | Spans several files, or subtle enough to need heavy reasoning or design judgement. Also the fallback for an unscored or unparseable answer. |
| `ultra` | The rare hardest work, where a wrong call is expensive and hard to reverse. Architecture or plan review, a root cause hunt that has already defeated ordinary debugging, or a direction setting design decision. |

Two deliberate choices are worth knowing before tuning these.

There is no effort table, on purpose. The model is the toggle, and the reasoning effort is left to
the backend to resolve. Forcing an effort on top of a model choice was tried and removed, because
both tables read the same complexity value, so the second one carried no signal the first did not
and only multiplied the cost of a misscored task.

Complexity never changes which provider a task routes to, and the provider never changes
complexity. A low complexity task can run on either provider, and so can an ultra one.

The Codex defaults point `high` and `ultra` at the same model because `sol` is the top of the Codex
catalogue. The Claude defaults reserve `fable` for `ultra` alone, which is why the rubric is written
to keep `ultra` deliberately hard to earn.

### What reasoning effort a dispatched job actually runs at

The router decides none, so this is the backend's own resolution, and it is not the same on both.
The router does not control it on either backend. It records it on the one backend that reports it.

On Claude it is the model's own default. The router passes no `--effort` unless a decision carries
one, and nothing else sets it. Claude reports the value it settled on nowhere, so there is nothing
to record: `effective_effort` on a Claude row is null, permanently. It is null on an OpenCode row
too, because OpenCode discards effort in both directions.

On Codex it is whatever your own `~/.codex/config.toml` resolves. Dispatch goes through
`codex app-server daemon`, and the daemon loads user config, unlike the classifier, which passes
`--ignore-user-config`. So a `model_reasoning_effort` in that file applies to every routed Codex
job at every tier. The daemon reports the value it resolved on the `thread/start` reply, and the
router records that reading in the decision log's `effective_effort` column, so a Codex row says
what the job will actually run at and follows that file when you change it.

The two effort columns are different facts and the log keeps them apart on purpose. `effort` is what
the router decided, which is nothing; `effective_effort` is what the backend reported. Null in the
second one means nobody observed an effort, which is not the same as a job running at no effort: it
covers a Claude or OpenCode row, a dry run, and a row written before the column existed.

Only when that file names no `model_reasoning_effort` does a Codex job fall through to the model's
own catalogue default, and those defaults are not ordered the way the tier table is. Read them from
the running daemon rather than assuming:

```bash
{ printf '%s\n' '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"probe","version":"0"},"capabilities":{"experimentalApi":true}}}'
  sleep 2
  printf '%s\n' '{"jsonrpc":"2.0","method":"initialized"}'
  printf '%s\n' '{"jsonrpc":"2.0","id":2,"method":"model/list","params":{"includeHidden":true,"limit":50}}'
  sleep 15
} | codex app-server --listen stdio://
```

Each entry carries a `defaultReasoningEffort` and a `supportedReasoningEfforts` list. The list is
per model and not uniform, which matters if you ever do set an effort: `sol` and `terra` accept a
rung that `luna` does not, and no Codex rung above `max` is a value Claude accepts at all.

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

A difference in the global comparison of `~/.claude.json` against `~/.codex/config.toml` is excepted
the same way, by setting `path` to your home directory, because that is the root a global difference
is reported under. Such an exception also covers the home directory when it is itself scanned as a
project, which happens whenever a scan is rooted at or above it, since `.codex/config.toml` is one of
the discovery markers. The two stay separate entries in the report; one exception simply reaches
both.

Valid `kind` values are `missing_in_codex`, `missing_in_claude`, `command_differs`, `args_differ`,
`env_keys_differ`, `transport_differs`, `endpoint_differs`, and `standalone_claude_md`. Transport
declarations are compared semantically and private endpoints exactly, but endpoint values are never
emitted in reports.

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
