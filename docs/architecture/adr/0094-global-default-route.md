# ADR-0094: A global `/model` stores the engine's default route in the journal

Status: accepted
Date: 2026-11-27
Affects: contract:C6, contract:C8, crates/engine, crates/channels

## Context

The reference's `/model <name> --global` (gateway `/model` handler, `resolve_persist_behavior` of
the model switch module) switches the current session and writes the model into the reference's
config file as `model.default`, so every later session, and the gateway after a restart, starts
on it. A plain `/model <name>` switches the session only.

`ulei` had no default route to change. C6 `set_route` stored the alias with the session
(C8 `Session.route`), and a session without one ran on the configured `engine.default_route`.
So after #257 `/model opus --global` switched the session until `/new` and the reply said that
`--global` was not supported; the `set_model` tool (#258) had no global mode at all. The owner
(2026-11-26): a `--global` that is not global is wrong. This is a mechanism of every owner of
the paseka, not a setting of one host.

## Decision

1. **The default route is engine state, kept in the session journal.** The journal gains a
   `settings` table (schema version 9; a version 8 file migrates with no row), whose row `route`
   is the default route alias (C8 0.9.0 `DefaultRoute`). The engine does not rewrite `ulei.toml`:
   the config is the owner's file, read at start and possibly read-only or shared, and the
   journal is already the store of routes, written under its lock and kept across restarts.
2. **Resolution.** A turn of `serve` runs on the alias its session stores, else the journal's
   default route, else `engine.default_route`. C6 `status` reports the same alias. `run --route`
   is unchanged: its alias wins.
3. **C6 0.12.0.** `SetRouteRequest` gains optional `global`. With it the engine first asks the
   model gateway for its routes (C4G `GET /v1/model/routes`) and refuses an alias it does not list
   with `bad_request`, as the reference validates a model before it persists it; a typo must not
   become the default of every later session and inbox turn. It then stores the alias as the
   default route and, as the reference does, with the current session. An alias equal to the
   configured `engine.default_route` removes the row, so a later change of the config holds again.
   `RouteSet` gains `global`, which the engine always sends.
4. **`set_model`** gains the boolean argument `global` with the same effect, written to the
   journal as the tool already writes the session's route (ADR-0016 section 15).
5. **`ulei-tg`.** `/model <alias> --global` (also with a Unicode dash, from #257) sends
   `global: true`. The reply's last line is `Сохранено как модель по умолчанию (`--global`):
   действует и после `/new``; the reference's line names its config file, which `ulei` does not
   write. `--session`, `--once` and `--refresh` stay accepted and session-only.

## Consequences

- `/model opus --global` then `/new`, or a restart of `ulei-engine`, runs on `opus`; a session
  that stored its own alias keeps it, as in the reference.
- To return to the configured default the owner sends `/model <configured alias> --global`.
- The journal of version 9 is refused by an engine built before this record
  (`journal schema version 9, this engine reads 8`): a rollback of the engine binary across it
  needs the backup of the journal or `DROP TABLE settings; PRAGMA user_version=8;`.
- `SetRouteRequest` and `RouteSet` are closed, so `ulei-tg` and `ulei-engine` deploy together.
- A global `set_route` makes one bounded call to the gateway on the serving thread (connect and
  read timeouts of 5 s); a per-session `set_route` makes none, as before.
