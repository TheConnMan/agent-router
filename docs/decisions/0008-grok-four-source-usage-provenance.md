# 0008. Grok four-source usage provenance

## Context

Grok has no single official usage endpoint that is always reachable. Capacity
can come from a live billing/user pair, a shared cache, the Grok log, or
nowhere. Routing fail-closes on Grok (see 0004), so an unreadable source must
not look like an idle provider.

## Measurement

A machine-wide `/tmp` cache that tests could not unset made Claude usage
assertions pass on a developer box and fail on a runner. That divergence turned
`main` red on 2026-08-06 on a merge that was green on every box it was built
on. Grok has the same cache shape. Cache starvation (writing secrets or raw
responses, or skipping the log fallback) left Grok reading as closed while a
usable log event existed.

## Decision

Resolve Grok capacity in this order:

1. Fresh cache (avoids the provider calls).
2. Live billing + user fetch; on success, write only the normalized non-secret
   payload to the cache.
3. Stale cache (still a real reading).
4. Newest official weekly paid SuperGrok billing event in the log.
5. `Headroom::closed` with source `None`.

Doctor reports the source. `GROK_USAGE_CACHE` (and `CLAUDE_USAGE_CACHE`) point
tests at a path they control; empty or unset means the shared default.

Codex is scanned first in `UsageSnapshot::read_with_grok_source` so a classifier
rollout that gains `rate_limits` while the Grok/Claude HTTP work runs does not
enter the snapshot.

## Constraint

Keep the four-source order. Never cache raw responses or the bearer token.
Unknown Grok weekly telemetry is `GrokUnavailable` plus fail-closed headroom,
not 0 percent used.
