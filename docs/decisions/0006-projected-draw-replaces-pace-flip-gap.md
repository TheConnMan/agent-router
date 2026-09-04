# 0006. Projected draw replaces a run-rate gap

## Context

Workhorse selection used to compare current weekly percent, then a run-rate gap
(`pace_flip_gap`). Percent used is not comparable when two providers' weekly
windows started at different times, and a gap in jobs-per-hour depends on plan
size.

## Measurement

Spending 20 percent in the first tenth of the week projects to 200: at that rate
the allowance runs out with most of the week still to go. A provider further
into its week can sit at a higher used percentage and still be the one
under-burning. Dividing by a tiny elapsed fraction (a couple of jobs in the
first hours) produces a four-figure projection that is not a diagnostic.

Schema v2 rewrote legacy `pace_flip` / `projected_overdraw` / `five_hour_pacing`
tags on open.

## Decision

When both Codex and Grok are eligible and both projected draws exist, the lower
projected draw wins (the provider further below its own week's pace). An exact
tie stays on Codex. When either projection is missing, fall back to lower
current weekly percent and record `projection_unavailable`.

A projection is computed only when the reset epoch is known and at least a
twentieth of the week (~8.4 hours) has elapsed. Each provider is measured
against its own reset and its own allowance; the percent is already normalized,
so no plan sizes appear in the comparison.

## Constraint

Do not bring back `pace_flip_gap` or any jobs-per-hour comparison. Do not
project from a zero reset epoch or from a window with less than
`MIN_PROJECTION_ELAPSED`. Claude is not in this comparison (see 0007).
