# 0010. An unmatched connector miss is not a dispatch block

## Context

0007 made Claude a capability-only destination. A classifier
`missing_connector` with no inventory-backed provider became
`CapabilityBlocked` so the miss would not halo onto Claude. Recovery later
searched the task text as well as the rationale, so Slack/Airtable/Granola
jobs whose one-sentence rationale omitted the product name could still find
a provider.

What remained unmatched was treated as a refuse.

## Measurement

On 2026-09-15 two automatic routes in a local markdown wiki refused a
research question about whether any provider could connect to Descript
through MCP, and whether a YouTube video of editing practice was visible.
The classifier set `missing_connector`. Descript is not in the configured
inventories (Granola, Notion, Slack, Airtable on Claude and Codex; Grok has
none), so `matched_capabilities` was empty and auto routing fail-closed.
An explicit `--provider codex` dispatch of the same text ran, because the
explicit path does not apply this filter.

The same refuse class had already shown up as classifier false positives on
public web, Twitter, ntfy, ZLog, and git/gh work (0007). Those misses do not
name an inventoried product, so task-text recovery cannot save them.

A matched Slack/Granola/Airtable name still needs the filter: send the job
only to providers that advertise it.

## Decision

A classifier miss constrains routing only after an inventory name matches in
the task or the rationale.

- Matched, at least one provider advertises it: exclude the others, then
  apply ordinary capacity policy inside that pool. Claude-only remains a
  capability pin.
- Matched, no dispatcher: `CapabilityBlocked`. Still not a Claude pin.
- Unmatched: record `missing_connector` for the log, then ordinary Codex or
  Grok routing. Do not refuse. Do not pin Claude.

Explicit `--provider` is unchanged: it never uses this filter.

## Constraint

Do not restore unmatched-miss as `CapabilityBlocked`. Do not treat an
unmatched miss as evidence that Claude can reach the unnamed system. Keep
the matched-name filter; a Slack job must not land on a provider that does
not advertise Slack.
