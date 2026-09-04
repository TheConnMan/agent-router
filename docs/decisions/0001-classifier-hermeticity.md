# 0001. Classifier hermeticity and flag set

## Context

Every `--provider auto` call shells out to a small model that must score a task
against a fixed rubric and return one JSON object. The call shares the operator's
CLI, so project CLAUDE.md, skills, MCP servers, hooks, and extended thinking all
load unless the invocation strips them. A 30s deadline was losing about half of
the Claude calls.

## Measurement

On this box, 2026-07-30:

- Plain `claude -p --model haiku --output-format json` spent ~14s before the API
  request (hooks, plugin sync, auto-memory, CLAUDE.md discovery) and 29–38s wall.
- `CLAUDE_SUBPROCESS=1` plus `--safe-mode` took time-to-request to ~27ms.
- `--bare` does the same strip but never reads OAuth or the keychain, so it
  answers "Not logged in".
- `MAX_THINKING_TOKENS=0` is the larger win once startup is ~30ms. Re-measured
  2026-08-01 on the nine tasks that had timed out, two runs each: 12.5–48.6s
  (mean 26.7s, 6 of 18 past 30s) with thinking on, against 3.4–7.0s (mean 4.5s,
  0 of 18 past 30s) and 97–127 output tokens with it off. Scoring emits one
  ~100-token JSON object; haiku was generating 1104–4154 discarded thinking
  tokens per call.
- Disabling the tool set (`shell_tool`, `browser_use`, `computer_use`,
  `image_generation`, `apps`, `skill_search`) was 15.2k prompt tokens and 2.5s
  against 18.3k and 6.7s with the full set.
- Codex on the same nine tasks: 6.3–12.4s. `--ignore-user-config` drops
  `~/.codex/config.toml` and with it every MCP server (~3.7k prompt and most of
  the wall). `--ignore-rules` drops execpolicy, `-c project_doc_max_bytes=0`
  suppresses AGENTS.md, `--skip-git-repo-check` is required because home is not
  a repository. The sandbox is read-only.

The classifier timeout was raised from 30s to 60s so a slow tail falls back far
less often than the 6-of-18 miss rate at 30s.

## Decision

Claude classifier flags: `CLAUDE_SUBPROCESS=1`, `MAX_THINKING_TOKENS=0`, `-p`,
`--output-format json`, `--no-session-persistence`, `--safe-mode`,
`--strict-mcp-config`. Do not use `--bare`.

Codex classifier flags: `exec --json --sandbox read-only --skip-git-repo-check
--ignore-user-config --ignore-rules -c project_doc_max_bytes=0` plus the disabled
feature list. Do not add `--ephemeral` (see 0002).

## Constraint

Scoring must not load project customizations, must not think, and must not have
tools. The verdict depends on the rubric and the task, not on what this box
happens to have configured. `--bare` is forbidden because it cannot log in.
