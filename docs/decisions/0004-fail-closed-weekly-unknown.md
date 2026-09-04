# 0004. Fail closed on an unread weekly window

## Context

An unread weekly window reports 0 percent used, which is the same reading as a
genuinely idle provider. Treating that as headroom routes work into a provider
that may be out of budget.

## Measurement

An exhausted Codex with no readable weekly number looked idle and kept receiving
work. The decision log recorded ordinary capacity routing rather than the missing
read, so the failure was silent until the provider refused the job.

## Decision

A provider whose weekly window nobody read is ineligible (`weekly_known() ==
false`). The `weekly_unknown` gate records that eligibility was decided against
a missing number, whether or not the destination changed.

Closing happens in `decide`, not in the reader, so Claude can still fail open
(its 5h window paces a stream; it is not an automatic destination). Both
workhorses unknown fall through to `over_ceiling` and stay on Codex; the router
does not refuse work over a ceiling.

A provider whose CLI could not launch is ineligible in the same sense. Claude
is not an automatic fallback for a launch failure (see 0007): when nothing
eligible remains, the task stays put and fails at dispatch with a named launch
error.

## Constraint

Never treat a default 0 percent as headroom for Codex or Grok. `Headroom::closed`
is 100 percent used with `weekly_capacity_known = false`. `weekly_unknown` is a
diagnostic gate, not a pin to Claude.
