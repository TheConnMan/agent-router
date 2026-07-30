# Deferred requirements

These requirements are intentionally deferred until the project has matured.

## 1. Routing policy configuration

Add a human editable configuration file that declares:

1. Whether weekly usage is a routing concern.
2. Which tuning changes are permitted when routing fails over because of usage.
3. The default model.
4. The declared reasons that allow a route away from the default model.

The initial policy is Codex as the default. Route to Claude only for a declared reason. The configuration must also support Claude as the default without requiring a routing logic change.

Open decisions include the configuration filename and schema, the usage signals to inspect, the permitted tuning controls, and the complete list of declared failover reasons.

## 2. Parity command

Add a `parity` command that reports whether Claude and Codex are ready to route interchangeably.

The command must inspect live state rather than trusting documentation. Its primary checks are:

1. Global MCP parity between Claude and Codex.
2. Every project folder containing `.mcp.json` has a roughly equivalent Codex `config.toml`.
3. Every relevant project has an `AGENTS.md` instruction surface.
4. Excess standalone `CLAUDE.md` files are flagged because Codex cannot consume them.

Results must classify each surface as aligned, intentional difference, or missing or drifted. Intentional differences need a recorded reason so a real gap is not hidden as expected behavior.

Open decisions include the scan roots, the exact MCP equivalence rules, how intentional differences are declared, what counts as excess `CLAUDE.md`, and the command output format and exit behavior.

## 3. Intended outcome

Routing should have one explicit default model and explainable exceptions. The parity command should make drift visible early so either model can take over without unexpected capability or instruction gaps.
