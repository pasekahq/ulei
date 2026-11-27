# ADR-0099: `ulei inbox reconcile`, the owner closes an `uncertain` event, with an audit line (C1 1.4.0)

Status: accepted
Date: 2026-11-27
Affects: crates/core, contract:C1
Amends: ADR-0037 section 1 (fact S: `uncertain` is final), ADR-0040 section 5 (the `queue` line)

## Context

An owner-inbox event ends `uncertain` when the consumer cannot tell whether its turn reached the
owner (ADR-0037). The uncertain alerter names it every 24 h until it leaves `uncertain`
(ADR-0097 section 5), and nothing in the store lets it leave: a later turn that did deliver the
message leaves the row `uncertain` for good. The reference agent's operator tool settles such rows
by hand outside its store, writing free text into `outcome` with no record of who or why
(`docs/runbooks/mvp-pilot-cutover.md`, 1.5).

## Decision

### 1. The path

C1 1.4.0 adds the write method `reconcile`, served by `ulei-inbox serve`, the one writer of the
store, under its peer-uid check (ADR-0007 section 6). `ulei-inbox reconcile` (and `ulei inbox
reconcile`, which execs it) is a thin C1 client of it and opens no database.

```sh
ulei inbox reconcile ID --outcome reconciled-delivered --note TEXT [--config PATH]
```

It reads `inbox.socket` from the config found as `ulei status` finds it (ADR-0040 section 2) and
makes one `POST /v1/reconcile` call with a 2 s connect bound and a 5 s bound to the last answer
byte, to a daemon that must run as the caller's uid. A call that timed out may have committed;
running it again answers `conflict` with `done (reconciled-delivered)` when it did.

### 2. Allowed outcome and the note

- `outcome` is a C1 `ReconcileOutcome`, whose one value is `reconciled-delivered`: a later turn
  delivered the event's message. Any other text is refused. `RowOutcome` gains the same value.
  A `reconciled-lost` (the message never arrived and is not to be sent) would be its symmetric
  value; it is not part of this decision.
- `note` is a C1 `ReconcileNote`: 1 to 500 characters (`inbox::MAX_NOTE_CHARS`), at least one of
  them not whitespace, none a control character, so it stays one line. The note travels in argv,
  which other local users can read in the process list: it holds the reason, never a secret.

### 3. The transaction

In one `BEGIN IMMEDIATE` transaction (`Inbox::reconcile`) the daemon reads the row of `id`, and
only when its status is `uncertain` sets status `done` and outcome `reconciled-delivered` and
inserts the audit line. Every other column of the row stays. Nothing else changes on a refusal:

| Case | C1 answer | CLI |
|---|---|---|
| unknown id | 404 `not_found`, `event not found` | exit 1 |
| status `pending`, `leased`, `processing`, `deferred` or `done`, a reconciled row included | 409 `conflict`, `event is <status> (<outcome>), not uncertain` | exit 1 |
| outcome not `reconciled-delivered`, note missing or outside section 2, id not a C1 `EventId`, another field | 400 `bad_request` | exit 2, checked before any call |

`uncertain` stays final for delivery: no `claim`, `recover` or `ack` leaves it; `reconcile` is
the owner's only way out of it. A reconciled row is `done`, so C1 `uncertain` no longer lists it
and the alerter stops naming it.

### 4. The audit line

Table `reconciled` of `events.sqlite3`, beside `events`, which the Python copies never read:

```sql
CREATE TABLE IF NOT EXISTS reconciled (
    id TEXT PRIMARY KEY, outcome TEXT NOT NULL, reason TEXT NOT NULL, note TEXT NOT NULL,
    uid INTEGER NOT NULL, pid INTEGER NOT NULL, time REAL NOT NULL)
```

`id` the event, `outcome` the given outcome, `reason` the row's `UncertainReason` before it
closed, `note` as given, `uid` and `pid` the caller as `SO_PEERCRED` of the socket names it (no
flag can claim another caller), and `time` the daemon's Unix time. The answer, C1
`ReconcileAudit`, is that line. A row closed by hand outside the store has no line.

### 5. Output

- Exit 0: one stdout line, `reconciled <id>: done, reconciled-delivered (was <reason>), by uid
  <uid> pid <pid> at <YYYY-MM-DDTHH:MM:SSZ>`; stderr empty.
- Exit 1: one stderr line, `ulei-inbox reconcile: <id>: <reason>`, the reason the status, C1 code
  and message of a refusal (`409 conflict: event is pending, not uncertain`) or the failure of
  the call as `ulei status` words it (`connect: Connection refused`).
- Exit 2: one stderr line `ulei-inbox reconcile: <reason>` naming the argument or config key, or
  the usage text for unknown or repeated flags; nothing is sent.

### 6. `ulei status`

C1 `Counts` gains the optional `reconciled`: the number of audit lines, read in the same query
as the other counts. The `queue` line ends with `, reconciled <n>` when the inbox reports it:
`queue: pending 0, leased 0, processing 0, deferred 0, uncertain 1, reconciled 2`. A reconciled
event leaves the `uncertain` count. An inbox without the field gives the line without it.

## Consequences

- The owner closes a delivered `uncertain` event with one command and the alert about it stops;
  each close keeps its reason, caller and time in the store.
- Rows with the free-text outcome `reconciled-delivered` of the reference's operator tool now
  read as a C1 `RowOutcome`; they have no audit line and are not counted as reconciled.
- The daemon and `ulei status` are one binary, `ulei-inbox`, so a release keeps them in step. A
  reader with the closed C1 1.3.0 `Counts` refuses the new field and its `queue` line reads
  `queue: unknown: malformed answer`.
