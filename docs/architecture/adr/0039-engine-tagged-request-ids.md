# ADR-0039: request ids of the engine tagged per run of a turn (C8 0.6.0)

Status: accepted
Date: 2026-11-20
Affects: crates/engine, contract:C8

## Context

qb refuses a turn whose `request_id` is already running (400 `bad_request`), and
`POST /v1/model/cancel` names a running turn by that id (C4G 1.0.0, ADR-0038). Ids built from
journal state alone repeat: two engines with separate journals send `s1-t1-0` for their first
step, and a turn killed with SIGKILL before its append is run again under the same numbers
while qb may still run the killed one.

Reference: there is none. The reference agent has no client of qb's gateway; the port map,
`/home/dev/ummanu-data/instance/state/knowledge/projects/ulei/hermes-port-map-u2b.md`, has no
entry for it.

## Decision

- Step `n` of turn `t` of session `s` sends `s<s>-t<t>-<n>-<tag>`. The tag is 12 lowercase hex
  digits, 6 bytes of `/dev/urandom` drawn once per run of a turn (`Turn::request_ids`), so the
  steps of one run differ by their step number and two runs differ by their tag, in one
  process or two, with one journal or two.
- A failure of the random source ends the run with an error before any request; there is no
  untagged fallback.
- Session and turn are `SQLite` integers of at most 19 digits, so the id fits the 64 bytes of
  C4G `^[A-Za-z0-9_-]{1,64}$` for any step below 10^9. The numbers stay first and readable in
  qb's logs and audit.
- C8 `Step.request_id` takes the tag, optional so the untagged steps of a 0.5.0 journal stay
  valid (C8 0.6.0).
- The side turns keep their own random ids, `compact-<16 hex>` and `ask-<16 hex>`.

## Consequences

- A turn after a restart sends the bytes an engine that never stopped would send, but for the
  tag of its request ids; tests compare request bodies with the tag cut.
- An overloaded retry of a step sends its id again; qb has ended the rejected attempt first.
