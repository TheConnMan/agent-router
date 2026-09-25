# 0012. Configurable provider priority

## Context

Ordinary routing hardcoded Codex and Grok, Codex won ties, and Claude reached ordinary work
only through the bounded shared capability comparison (0006, 0007). Adding or reordering a
provider meant a code change, and two selectors disagreed on ties and projection fallback.

## Decision

`[routing] priority` (default codex, grok) and `priority_margin_pct` (default 0). Candidates
are the list narrowed by capability. Eligibility is unchanged. One eligible wins; otherwise the
first candidate within the margin of the lowest projected draw wins, or of the lowest weekly
percent when any projection is missing (`projection_unavailable`). A move off an eligible first
candidate is `priority_overridden_by_usage` and counts as a flip; off an ineligible one is
`flipped_on_exhaustion`. The shared Claude and Codex comparison is retired and
`capability_projected_draw` is no longer emitted. Under defaults, a capability shared by Claude
and Codex now stays on Codex, and a Codex to Grok pace pick now counts toward the flip rate.
Claude model tiers all default to `claude-opus-5-5[1m]`.

## Constraint

The hard pins (orchestration, implement context window, Claude-only capability) stay outside
priority. Claude is an ordinary candidate only when listed. Eligibility runs before scoring.
Never mix projected draws and weekly percent in one comparison. Load rejects an empty or
duplicated priority, an unknown provider, and a negative or non-finite margin. Keep
`capability_projected_draw` decodable and in `FLIP_GATES`.
