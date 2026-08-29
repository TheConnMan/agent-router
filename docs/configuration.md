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
| 3 | A file stamped below version 3 with `hard_ceiling_pct = 97.0`, the old generated default, is corrected to `98.0`. Any other value is left alone. |
| 4 | `pace_flip_gap` is gone and `projection_overdraw_pct` replaces it. Nothing is carried across, and here that is not a convenience: the old key's value existed to clear a chronic band produced by one pair of plan sizes, so reading a `70` as a projection threshold would let a provider run to 70 percent OVER its allowance before anything moved. The rewrite drops the stale key from the file. |

A file already stamped `config_version = 4` is not migrated, so its chosen ceiling remains in force.

## Defaults in full

```toml
config_version = 4
hard_ceiling_pct = 98.0
classifier_timeout_secs = 60
connectors = ["local shell"]

[policy]
default_provider = "codex"
weekly_routing = true

[classifier]
engine = "codex"
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

## Runtime usage cache

Usage-cache paths are environment variables rather than TOML keys. They are useful for an isolated
runner or test; an empty value has the same meaning as an unset value.

### `GROK_USAGE_CACHE`

Default `/tmp/grok-usage-cache.json`. This is the Grok billing cache path; set
`GROK_USAGE_CACHE=/path/to/cache.json` to override it.

The cache is read through in this exact order: a parseable cache younger than 300 seconds; a live
Grok billing fetch, which writes the cache on success; a parseable stale cache; the newest valid
billing event found by scanning the entire `~/.grok/logs/unified.jsonl` backwards; then no capacity.
The reverse log scan is bounded in memory, but not by a byte tail, so an older billing event is not
lost merely because newer non-billing log output is large.

Agent Router is the cache's sole writer. Successful live reads store only normalized billing fields
(tier, weekly percent, weekly reset, and source timestamp); raw API responses and bearer credentials
never enter the cache or diagnostics. Other local consumers may read this file but must not write it.

`agent-router usage` reports Grok provenance as `live`, `cache`, `log`, or `none`, and
`agent-router doctor` reports the same value through its `grok_usage` check. `live` and `cache`
carry usable weekly capacity. A `log` event without `creditUsagePercent` retains `log` provenance,
but has unknown weekly capacity and is reported unhealthy by doctor. `none` means no usable billing
data at all, not that billing reported 100 percent usage. Unknown `log` capacity and `none`
deliberately fail closed, so Grok is excluded from automatic routing and adversarial-review
selection until a usable capacity source is available.

## Top level keys

### `hard_ceiling_pct`

Default `98.0`. Weekly percent used at or above which a provider counts as exhausted.

A provider at or over this ceiling is ineligible, and so is a provider whose weekly window nobody
read. The lower weekly percentage wins when both workhorse providers are eligible; ties go to Codex.
Exactly one ineligible sends the task to the other. Both ineligible keeps the default provider and
records the all-unavailable fallback visibly.

An unread weekly window is ineligible because it reports no capacity verdict, so trusting it would
hand every job to whichever provider failed to report. A Codex Premium credits event is different:
its `rate_limits` payload has `limit_id = "premium"`, both window slots null, and
`credits.has_credits = false`. It parses as a live, known 100 percent weekly reading, so Codex is
ineligible and a decision flips to Claude when Claude has confirmed room.

A window that has genuinely reset is a different input and stays eligible: it reports 0 percent
against a real past reset epoch, which is a provider that does have a full week.

Missing, unreadable, or malformed weekly sources are unknown and therefore ineligible for automatic
routing. This applies to both Codex and Grok. If neither has authoritative capacity, the default
provider still dispatches and the decision records that fallback rather than silently pretending
one provider had headroom.

The default keeps a 2 point reserve, so a provider within 2 points of its weekly limit takes no
more routed work. The reserve is what the last points are for, because the router is not the only
thing spending them: an interactive session, the classifier's own per task call, and an explicit
`--provider` dispatch all draw on the same weekly window without consulting this ceiling. A router
that spends down to the limit leaves nothing for the work a person is doing by hand.

The reserve is not a refusal. `over_ceiling` still dispatches, because the router's job is to pick
the better of two providers and there is no third one; declining work outright belongs to whatever
queued it. So the reserve buys headroom while both providers are not yet in it, and the last thing
it protects against is routing a job into an exhausted provider while the other one still has room.

Eligibility is judged before the projection override, not after, and that order is load bearing. A
provider down to its reserve projects to finish INSIDE its allowance whenever its window is nearly
elapsed (98 percent used against 99 percent elapsed projects to 99), so an override allowed to run
first would see nothing wrong and route into a provider that is out of budget.

### `projection_overdraw_pct`

Default `100.0`. The projected weekly draw, as a percent of a provider's own allowance, above which
a task moves off that provider.

A provider's projected draw is `weekly_pct` divided by the fraction of its own weekly window that
has elapsed: what its spending so far extrapolates to by the time its window resets. 80 percent
spent with half the window gone projects to 160, meaning it runs out with days to spare. The two
providers reset at different instants, so each is measured against its own reset and its own
allowance, which is what makes two providers on different sized plans comparable at all.

The override moves a task when the provider it is on projects above this threshold AND the other
provider projects lower. Both halves matter. The first keeps it quiet: a provider finishing its week
inside its allowance is not a problem to route around, and moving work off it would strand budget
that was going to be spent. The second makes the destination an improvement rather than merely
different, and it deliberately compares the two projections rather than testing the other against
the threshold, so that when BOTH providers are overdrawing the task still goes to whichever runs out
later and the two drain together.

When either projection cannot be computed the override is skipped entirely and the decision is
tagged `projection_unavailable`; when it fires, the decision is tagged `projected_overdraw`. The
comparison is strictly greater, so a projection exactly on the threshold holds.

In practice `projection_unavailable` means one thing only: less than a twentieth of a provider's
window has elapsed, so dividing by that elapsed fraction would turn a couple of jobs into a four
figure projection. A projection is also uncomputable when a reset was never read, but the override
runs only with both providers eligible and eligibility already requires a known weekly window, so
that decision carries `weekly_unknown` instead, which names the reason rather than the consequence.

The retired `pace_unavailable` recorded the same idea under the run rate rule. Rows already in the
log carry it, which is why it stays documented.

The default is not a tuned number. A projection of 100 is precisely "finishes the week having spent
exactly its plan", so above it is the definition of running out early, which is the whole question
the rule asks. Raise it to let a provider run further past its pace before work moves; there is no
reason to lower it.

Neither `pace_flip_gap` nor `headroom_flip_gap` is read as an alias. Both named rules that
thresholded a difference between two providers' numbers, and `pace_flip_gap` in particular had to be
tuned above whatever chronic band the two plan sizes happened to produce. That made it a plan sized
constant wearing the clothes of a policy: it was set to 70 points to clear the band a 5x Codex plan
against 20x Claude plans produced, the Codex plan grew on 2026-08-01, the band collapsed to under 38
points, and the configured value then sat above every reading the box could produce. The override
stopped firing entirely and nothing reported that it had. A ratio against each provider's own
allowance has no such dependency, which is why this key has no measured default to go stale.

### `claude_five_hour_pacing_pct` (observational)

Default `90.0`. Retained for compatibility and reporting; Claude's 5-hour usage does not pace
automatic routing.

No automatic decision uses this threshold. Normal work balances Codex and Grok by authoritative
weekly usage; Claude is selected only by capability/context pins.

Codex having room is judged by `hard_ceiling_pct`, the same threshold the exhaustion flip uses,
rather than by a second key that could drift away from it. Codex sitting exactly on that ceiling has
no room, so no pacing happens: moving the job would relocate the stall rather than avoid it.

A capability pin overrides this entirely. A task requiring several agents exchanging findings
mid-run stays on Claude however exhausted its 5 hour window is. A named connector instead first
filters providers to inventories that establish it, then applies the ordinary policy within that
eligible set; it is blocked only if no provider qualifies.

Setting `weekly_routing = false` disables weekly balancing along with every other usage-driven rule.

Codex's own 5 hour number is deliberately ignored, in both directions: it never paces a task away
from Codex and it never keeps one on Claude. Only Claude has a 5 hour window that constrains a
stream of jobs on this box.

### `classifier_timeout_secs`

Default `60`. How long the classifier call may take before it counts as failed.

A failed classifier is not fatal, but it is not cheap either: the fallback pins nothing, so
ordinary capacity routing picks the provider, complexity reads as `high`, and the decision is
tagged `classifier_failed`, so a timeout can pick both the wrong provider and the top model tier.
A classifier the router could not *launch* is tagged `classifier_unlaunchable` as well, and that
provider is excluded as one conjunct of the automatic capacity eligibility test — the same test
that already excludes a provider over the hard ceiling or carrying an unread weekly number. That
test only runs for ordinary auto-routed work; a capability pin or an explicit `--provider` skips it
entirely, so unlaunchability never touches either of those paths. Within the test, an unlaunchable
Codex commonly leaves nothing eligible, because Grok is ineligible whenever its own weekly reading
is unavailable, which is Grok's normal state. `decide` does not invent a fallback for that case: it
deliberately keeps the work on Codex, adds the `over_ceiling` gate, and lets the dispatch fail loudly
with a named `launch failed:` error — the router routes, and rerouting an unlaunchable classifier's
task to Claude would make Claude an automatic destination, contradicting the rule that Claude is a
capability destination only. The measured call is 3.4-7.0s, so this default is headroom for a slow
tail rather than a target. It is viable only because the classifier invocation strips both CLI
startup cost and the model's thinking tokens; see the note in `classify.rs` for the measured numbers
behind that.

### `connectors`

Default `["local shell"]`. The authoritative local-shell capability shown to the classifier.

This is the one section that genuinely needs human maintenance. Rubric criterion 5 is scored
against exactly this local-shell inventory. The shell covers its local executables, files, session
JSONLs, and authenticated endpoints without advertising each one as a connector. A genuinely absent
capability returns `capability_blocked` only after provider
inventories have also been checked; absence is not evidence that Claude can reach it. The classifier is explicitly told
never to set `missing_connector` because it cannot see a connector itself, only because a named
system is absent from this list.

Keep it accurate in both directions. Listing a connector Codex cannot actually reach sends work to
a provider that will fail; omitting one it can reach sends work to Claude that did not need to go
there.

### `provider_capabilities`

Optional provider-scoped capability registration. Codex MCP server names and enabled plugins are
discovered from `~/.codex/config.toml`; Claude.ai connector registrations are read from
`~/.claude.json`. A missing source is unknown rather than evidence a provider is unavailable.
Register a provider manually only when its local inventory cannot be inspected, for example:

```toml
[provider_capabilities]
claude = ["Granola"]
codex = ["Granola"]
```

For an Auto route whose classifier observes a missing connector, providers without a matching
inventory entry are excluded before the existing capacity policy runs. Explicit `--provider`
requests remain exact and do not use this automatic eligibility filter.

## `[policy]`

### `default_provider`

Default `"codex"`. Either `"codex"` or `"grok"` for the workhorse fallback. The provider used when
both workhorses are unavailable (Codex by default).

Claude is selected by capability/context pins and is not a workhorse usage-routing destination.

Still parsed, defaulted, and validated on load, but no longer consulted by routing, which selects
between Codex and Grok by capacity instead.

### `weekly_routing`

Default `true`. Whether usage is allowed to move a task off the default provider at all.

Set to `false` to route purely on task shape. Decisions are then tagged `weekly_routing_disabled`,
and neither the exhaustion flip, the projection override, nor the 5 hour pacing rule can fire.
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

Defaults `"haiku"` and `"gpt-5.6-luna"`; the default engine is Codex. The model each engine scores with. Both are kept
regardless of which engine is in force, so flipping `engine` is a one word edit rather than a
re-pick of the model.

Both want the cheapest model that reliably holds the output contract, since the classifier runs on
every automatically routed task and emits the routing scores plus a concise job title. The same
model is called on a run that names its provider, for the title alone, so the engine choice sets
which weekly budget every dispatch draws one small call from, not only the automatic ones.

## `[models.codex]` and `[models.claude]`

One model per complexity tier, per provider. The classifier scores complexity independently of the
capability pins, and the resulting tier picks the model the job is spawned with.

| Tier | When the rubric assigns it |
| --- | --- |
| `low` | Conversational, one step, mechanical, or a single file with an obvious answer. Direct definition, location, transcription, or single fact retrieval also stays low, even when labeled research or analysis or requiring preliminary search, if no synthesis or evaluation is requested. |
| `medium` | A normal well scoped implementation or investigation. |
| `high` | Spans several files, or subtle enough to need heavy reasoning or design judgement. Substantive synthesis, comparison against criteria, tradeoff evaluation, prioritization, recommendations, strategic interpretation, or choosing among options is also high. Also the fallback for an unscored or unparseable answer. |
| `ultra` | The rare hardest work, where a wrong call is expensive and hard to reverse. Architecture or plan review, a root cause hunt that has already defeated ordinary debugging, or a direction setting design decision. |

Two deliberate choices are worth knowing before tuning these.

The routing inputs form a contiguous hierarchy. These four forms are valid:

1. No provider, model, or effort: classify the task, route the provider using task shape and usage,
   then derive model and effort.
2. Provider only: preserve the provider and classify omitted model and effort.
3. Provider plus model: preserve both pins and classify only the omitted effort.
4. Provider plus model plus effort: preserve all three pins and skip routing classification.

Any noncontiguous combination is rejected. A model requires a provider, and an effort requires both
provider and model. This keeps every omitted value downstream of the values before it.

For Codex and Claude, classified complexity maps to fixed effort: `low` to `low`, `medium` to
`medium`, and both `high` and `ultra` to `high`. The model tier table remains separate because
complexity chooses the model while the fixed mapping chooses effort. OpenCode is explicit only and
preserves its existing dispatch contract: it has no derived model and receives no derived effort.

Complexity never changes which provider a task routes to, and the provider never changes
complexity. A low complexity task can run on either provider, and so can an ultra one.

The Codex defaults point `high` and `ultra` at the same model because `sol` is the top of the Codex
catalogue. The Claude defaults reserve `fable` for `ultra` alone, which is why the rubric is written
to keep `ultra` deliberately hard to earn.

### What reasoning effort a dispatched job actually runs at

The router's `effort` value is the requested effort, not necessarily the effort a job reports after
dispatch. For classified Codex and Claude work it is the fixed complexity mapping above. A fully
pinned request keeps the supplied value. OpenCode receives no effort and keeps its existing
dispatch contract.

On Claude the router passes the requested value as `--effort`. Claude reports the value it settled on
nowhere, so there is nothing to record: `effective_effort` on a Claude row is null, permanently. It
is null on an OpenCode row too, because OpenCode discards effort in both directions.

On Codex it is whatever your own `~/.codex/config.toml` resolves. Dispatch goes through
`codex app-server daemon`, and the daemon loads user config, unlike the classifier, which passes
`--ignore-user-config`. So a `model_reasoning_effort` in that file applies to every routed Codex
job at every tier. The daemon reports the value it resolved on the `thread/start` reply, and the
router records that reading in the decision log's `effective_effort` column, so a Codex row says
what the job will actually run at and follows that file when you change it. A pinned effort is sent
to the turn, while the daemon still reports the effective value it resolved.

The two effort columns are different facts and the log keeps them apart on purpose. `effort` is what
the router requested, either from the fixed complexity mapping or an explicit pin; `effective_effort`
is what the backend reported. Null in the
second one means nobody observed an effort, which is not the same as a job running at no effort: it
covers a Claude or OpenCode row, a dry run, and a row written before the column existed.

When no pinned effort or `model_reasoning_effort` is present, a Codex job falls through to the
model's catalogue default, and those defaults are not ordered the way the tier table is. Read them from
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
