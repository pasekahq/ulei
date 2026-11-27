# ADR-0095: `hive_resolve`: the author's decision on a disputed card over MCP

Status: accepted
Date: 2026-11-27
Amends: ADR-0071 (section 4, the tools; section 5, the argv).
Affects: crates/hive-mcp, contract:C2

## Context

A head that finds its card wrong ends its round with `RESULT: disputed`; the hive keeps the card
`blocked` with the wait `disputed` and tells its author (usually the secretary) that the dispute
waits for `hive.py resolve CARD accept|rework TEXT`. On ulei the secretary reaches the hive only
through `ulei-hive-mcp` (ADR-0071): its engine's sandbox has no hive CLI and often no terminal.
None of the tools of C2 0.4.0 decides a dispute, so a disputed card waited for the owner to run
the CLI by hand, or was cancelled and created again, which loses its rounds and its workspace.

## Decision

### 1. The tool (C2 0.5.0, additive)

| Tool | Input | argv after `hive_command` | Output |
|---|---|---|---|
| `hive_resolve` | `HiveResolveInput`: `card_id`, `action` (`accept` or `rework`), `text` (1 to 4000 characters) | `resolve --by=<actor> -- <id> <action> <text>` | `Ack` with the CLI's line (`<id> dispute resolved: <action>, now done\|queued`) |

- `accept`: the dispute is right; the card finishes `done` with the head's result and the event
  `hive:<id>:done` goes to the owner inbox as for any done card. `rework`: the card goes back to
  `queued` and the next round gets `text` as its instructions.
- `text` is required for both actions: the reason of an accept is kept in the card's
  `dispute_resolved` event and its result's reason, as `hive_cancel` requires a reason. The CLI
  itself allows an empty text for `accept`; the tool is stricter, not looser.
- `--by` is the server's `hive_mcp.actor`, never an input (ADR-0071 section 7). The permission
  check stays the CLI's, under its dispatch lock: only a card in `blocked` with the wait
  `disputed` is resolved, and only by the card's author, `secretary` or the config's
  `owner_actor`. Its `hive: resolve rejected: <why>` (exit 2) is the tool error `resolve
  rejected: <why>`; the server mirrors only the cheap checks (card id pattern, the enum, a
  non-blank text without NUL).
- Values go as positionals after `--`, so a text that starts with `-` stays the text.

### 2. Nothing else changes

No new flag, field or path: the tool adds one CLI command the hive already has, under the same
run rules (argv vector, cleared environment, one run at a time, timeout, output caps) and logs
only the tool, the card id, the outcome and the time. `tools/list` serves nine tools.

## Consequences

- The secretary decides its own disputes from the engine; the owner's terminal is no longer
  needed for them.
- A model mistake costs at most one wrong decision on one disputed card: `accept` closes it as
  done (the owner sees the result in the inbox), `rework` starts a round the secretary can
  `hive_cancel`. Neither touches a card in another state.
