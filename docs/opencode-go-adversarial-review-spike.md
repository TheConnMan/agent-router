# OpenCode Go adversarial review spike

**Verdict: do not register OpenCode Go as an adversarial reviewer.**

The installed client (OpenCode 1.17.20 at `~/.opencode/bin/opencode`) does not expose a first-class current Go capacity command, and `opencode run` is not a first-class sealed read-only review contract. A later adapter can be narrow, but it is new work: a remote usage reader plus an assembled permission seal. This spike does not implement it.

Review eligibility in `crates/agent-router-core/src/adversarial_review.rs` still requires a registered reviewer, an authoritative known fresh weekly reading, and weekly usage below 90 percent. Claude and Codex meet that with first-class CLI flags. Grok is the weaker registered alternative: prompt-only, YOLO lifecycle, usage from `~/.grok/logs/unified.jsonl`. OpenCode Go is not as sealed as Claude/Codex and is not as already-wired as Grok.

## Assumptions

- "Installed OpenCode client" means CLI 1.17.20, local `auth.json`, local `opencode.db`, and documented env/config. Third-party quota plugins are out of scope.
- Router provider name stays `opencode` (`Provider::Opencode`). Go is the model namespace `opencode-go/<id>`, not a fourth routing provider.
- A remote official usage API is an acceptable capacity source if the client stores the key, matching Claude's OAuth usage fetch rather than a CLI `stats` wrapper.
- Fail closed on missing key, HTTP error, or unparseable payload, matching Codex/Grok, not Claude fail-open.
- Keep OpenCode explicit-only for ordinary `run` routing. This spike is review eligibility only.
- Do not reuse `ManagedClient` `POST /session/{id}/prompt_async` or the `agent-router.background` allow rule.

## Capacity signal

### Not trustworthy: `opencode stats`

Checked:

```text
opencode --version                 # 1.17.20
opencode stats --help
opencode stats --days 7 --models
opencode stats --models
opencode db "SELECT name FROM sqlite_master WHERE type='table'"
```

`stats` is local historical session accounting from `~/.local/share/opencode/opencode.db`. It reports mixed-provider tokens and dollar cost over `--days` or all time. It has no remaining quota, no weekly percent, no reset timestamp, and no `rate-limited` status. Seven-day output on this box mixed `opencode-go/*`, `opencode/*`, and `openrouter/*` into one cost total. That cannot be a Go weekly verdict.

`opencode models opencode-go --verbose` lists Go models and a per-model `limit` object. That metadata is context/cost, not current quota.

Docs (`https://opencode.ai/docs/go/`): track usage in the **console**, not the CLI. CLI docs describe `stats` as session token usage and cost.

The 1.17.20 binary contains no `zen/go/v1/usage` string. The client does not wrap the usage API.

### Trustworthy, but not a CLI command: `GET https://opencode.ai/zen/go/v1/usage`

Checked with the stored `opencode-go` key from `~/.local/share/opencode/auth.json` (`type: api`). No key material is recorded here.

```http
GET https://opencode.ai/zen/go/v1/usage
Authorization: Bearer <opencode-go key>
Accept: application/json
```

HTTP 200, 2026-08-21:

```json
{
  "usage": {
    "rolling": { "status": "ok", "percent": 0, "resetsAt": "2026-08-22T01:02:13.950Z" },
    "weekly": { "status": "rate-limited", "percent": 100, "resetsAt": "2026-08-24T00:00:00.950Z" },
    "monthly": { "status": "ok", "percent": 50, "resetsAt": "2026-09-16T22:44:46.950Z" }
  }
}
```

This is an authoritative current capacity signal: percent 0–100, ISO reset, and `status` (`ok` vs `rate-limited`). It is the same shape community tools adopted after `anomalyco/opencode#16513`. Official Go docs still say "console" only; the endpoint is live anyway.

`opencode providers list` showed `OpenCode Go api` among three credentials. `opencode models opencode-go` listed the Go catalog (`glm-5.2`, `grok-4.5`, `kimi-k3`, …).

Today's weekly window is 100 percent / `rate-limited`. Even a correct adapter would be ineligible under the 90 percent ceiling.

## Synchronous run contract

Checked:

```text
opencode run --help
opencode agent list
opencode session list --format json --max-count 1
opencode debug paths
opencode debug config
```

Plus CLI docs, permissions docs, config merge docs, and upstream `packages/opencode/src/cli/cmd/run.ts` on `dev`.

`opencode run` is synchronous. Non-interactive mode sends `session.prompt` (not `prompt_async`), streams events, and exits when `session.status` is `idle`. `--format json` emits JSONL objects with `type` in `{text, tool_use, step_start, step_finish, reasoning, error}`. Terminal review text is the `text` event's `part.text` once `part.time.end` is set. Default format prints trimmed text to stdout after tool lines.

That is a usable terminal body **if** the process is sealed and the parser rejects an empty body.

It is **not** first-class sealed:

| Claude | Codex | OpenCode 1.17.20 `run` |
| --- | --- | --- |
| `--safe-mode --tools Read,Glob,Grep --permission-mode plan --no-session-persistence --strict-mcp-config` | `exec --sandbox read-only --ephemeral --json` | no sandbox, no ephemeral, no tool allowlist flag |
| `--auto` is absent from the review argv | sandbox is OS-enforced | default agent `build` allows `*`; `--auto` / `--yolo` / `--dangerously-skip-permissions` auto-approve asks |

Built-in `plan` denies `edit` except plan files, but still starts with `permission: * allow`, so bash remains allowed. `summary` is not a reviewer.

Session create in non-interactive `run` only denies `question`, `plan_enter`, and `plan_exit`. Bash and edit default to allow. Permission asks without `--auto` are auto-rejected, which is good for `ask` rules and useless when the rule is `allow`.

Sessions persist in `opencode.db`. There is no `--ephemeral`. Cleanup is `opencode session delete <id>` after parsing, Grok-style. `--share` and `share = "auto"` are side effects; a review must not share.

Config merge (later wins on conflicting keys): remote → global → `OPENCODE_CONFIG` → **project `opencode.json`** → `.opencode/` → `OPENCODE_CONFIG_CONTENT` → managed. A reviewed tree can therefore re-allow bash unless project config is disabled. The 1.17.20 binary honors `OPENCODE_DISABLE_PROJECT_CONFIG` (not listed in the public env table) and applies `OPENCODE_PERMISSION` after merge.

Existing router OpenCode dispatch (`crates/agent-router-core/src/dispatch/opencode.rs`) is the wrong contract: `POST /session` with `agent-router.background` allow, then `POST /session/{id}/prompt_async` returning 204. That is detached background work, not a sealed synchronous review.

## Narrow adapter (not implemented)

Do not land this until a dedicated ticket implements and tests it. Shape:

1. **Usage reader** (`opencode_headroom`, fail closed):
   - Read `~/.local/share/opencode/auth.json` → `opencode-go.key`.
   - `GET https://opencode.ai/zen/go/v1/usage` with Bearer, short timeout.
   - Map `usage.weekly.percent` → `weekly_pct`, `usage.weekly.resetsAt` → epoch, `weekly_capacity_known = true` only when percent is finite.
   - Map `usage.rolling` → 5h fields.
   - Missing key, non-200, missing `usage.weekly`, or non-finite percent → `Headroom::closed()`.
   - Never call `opencode stats`. Never treat local DB cost as percent-of-limit.

2. **Review invocation** (CLI, not ManagedClient):
   ```text
   opencode run --dir <target> --model opencode-go/<high> --format json --pure --title <review> \
     <contract + request body>
   ```
   Env:
   - `OPENCODE_DISABLE_PROJECT_CONFIG=1`
   - `OPENCODE_DISABLE_CLAUDE_CODE=1`
   - `OPENCODE_PERMISSION` deny `bash`, `edit`, `task`, `webfetch`, `websearch`, `skill`, `question`; allow `read`, `glob`, `grep`, `list`
   - `OPENCODE_CONFIG_CONTENT` defining a primary agent with the same deny/allow set, selected via `--agent`
   - no `--auto`, `--yolo`, `--dangerously-skip-permissions`, `--share`, `--continue`, `--attach`

   Parse JSONL `type == "text"` parts with nonempty `part.text`. Nonzero exit, `type == "error"`, or no text → failed review. Delete only the created session id. Pin the binary via `AGENT_ROUTER_OPENCODE_REVIEW_BIN` like the Claude/Codex review bins.

3. **Registration**: add the provider to `review_registered` only after (1) and (2) exist. Keep it out of automatic `decide()`. Treat Go like Grok: review-only. `authoritative_availability` should require the Go key and a parseable usage payload, not merely `opencode` on `PATH`.

4. **Do not** call this safe in the Claude/Codex sense. It is a config seal, not an OS sandbox. Document that the way README documents Grok YOLO.

## Required behavior tests

Usage:

- Fixture JSON with weekly 23 percent and a future `resetsAt` → known, fresh, eligible.
- Weekly `status: "rate-limited"` and `percent: 100` → ineligible at the 90 ceiling.
- Missing `auth.json` / missing `opencode-go` key / HTTP 401 / malformed body / non-finite percent → closed, ineligible, never invoked.
- Local `stats`-shaped cost totals are not a usage source (parser unit test: stats text or DB row does not produce `weekly_capacity_known`).
- Primary `opencode` is excluded even when Go weekly is 0.

Invocation (stub binary, same pattern as `crates/agent-router-cli/tests/adversarial_review.rs`):

- Argv contains `run`, `--format json`, `--pure`, `--dir`, `--model` starting `opencode-go/`.
- Argv does not contain `--auto`, `--yolo`, `--dangerously-skip-permissions`, `--share`, `--bg`, or `prompt_async`.
- Env contains `OPENCODE_DISABLE_PROJECT_CONFIG=1` and a permission JSON that denies `bash` and `edit`.
- Waits for the stub to finish (delay assertion).
- Parses a `{"type":"text","part":{"text":"...","time":{"end":1}}}` line as `result`.
- Empty JSONL, error event, or nonzero exit → `Failed`.
- Created session id is deleted; failed cleanup is reported, not swallowed.
- A fixture project `opencode.json` that allows `bash` does not appear in the review argv/env as an allow rule (project config disabled).

Selection:

- Claude-primary with known Go weekly 17 percent and Claude/Codex ineligible still does **not** select OpenCode until the provider is registered; after registration it would, provided weekly < 90.
- Current live 100 percent weekly is a skip, not a launch.

## Recommendation

Stop here. OpenCode Go has real usage and a live official weekly meter, but the installed CLI does not expose that meter, and `run` is not a sealed read-only reviewer. Registering it now would either treat `stats` as capacity (wrong) or launch an unsealed `build` agent (unsafe). Implement the two-piece adapter above only on a later ticket with the tests in this file.

Checked interfaces: `opencode --version`, `stats`/`run`/`models`/`providers`/`agent`/`session`/`db`/`debug paths`/`debug config`, `~/.local/share/opencode/auth.json` key presence, `GET /zen/go/v1/usage`, 1.17.20 binary env `OPENCODE_PERMISSION` and `OPENCODE_DISABLE_PROJECT_CONFIG`, upstream `run.ts` event loop, and router `adversarial_review.rs` plus `dispatch/opencode.rs`.
