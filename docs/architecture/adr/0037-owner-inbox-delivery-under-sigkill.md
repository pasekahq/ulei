# ADR-0037: owner inbox delivery under SIGKILL: the engine's durable delivery record, orphaned `processing` rows and the kill test (C6 0.6.0, C8 0.5.0)

Status: accepted
Date: 2026-11-19
Affects: crates/core, crates/engine, contract:C6, contract:C8

## Context

The owner inbox promises that every published event ends in the owner session's history
exactly once, or its C1 row is `uncertain`, and that no event is lost, while the delivery
process (`ulei-inbox serve --deliver`, ADR-0036) and `ulei-engine serve` (ADR-0028) can each
be killed with SIGKILL at any moment and restarted.

In the reference (`THIRD_PARTY_NOTICES.md#ref-agent`, port map R12) consumer and agent are one
process, so a crash kills both. `Receipt.begin` moves the row to `processing` before the agent
runs; the store's `recover` returns an expired `leased` row to `pending` (no turn ran) and
marks an expired `processing` row `uncertain` (`expired-processing`); the 2 s pump of `start`
runs `recover` within 2 s of any expiry; `owns_recovery` keeps an inbox turn out of the
restart replay. Any crash after a turn began therefore leaves the row `uncertain`, and a row
that comes back to `pending` never had a turn.

ulei splits consumer, agent and channel into three processes (ADR-0036 section 11): the turn
starts at the `inject` accept and C1 `start` follows. This record goes through every window a
SIGKILL opens in that split, fixes the ones that break the promise, and fixes the kill test that
proves it.

## Decision

### 1. Facts the windows rest on

The current-behaviour column below describes the code this record starts from (main at
6a71c04).

- J. A turn reaches the journal in one transaction at its end: `journaled`
  (`crates/engine/src/main.rs`) calls `Journal::append` (`crates/engine/src/journal.rs`) after
  the loop, which writes the turn's input, messages and steps in one `BEGIN IMMEDIATE`
  transaction; with `journal_mode=DELETE` and `synchronous=FULL` a SIGKILL leaves the turn
  wholly in the journal or not at all. The turn's input is one user message whose text is
  `InjectRequest.prompt` unchanged (`journaled`: `text_message(Role::User, &run.prompt)`), so
  its JSON tail names the `event_id` (ADR-0036 section 4).
- R. Turn reports live only in memory (`Lanes`, `Lane::report` in `crates/engine/src/serve.rs`),
  the last `KEPT_REPORTS` (64) ended turns of a session; after an engine restart `Server::status`
  reads every earlier turn `unknown`. A `TurnId` carries the engine's start in microseconds
  (`Lanes::next_id`), so a restarted engine never gives an old id again.
- D. The engine records accepted deliveries in memory only: `Server::inject` looks up
  `Lanes.replays`, at most `KEPT_DELIVERIES` (64) entries kept by `Lanes::record`; a recorded
  `event_id` with the same or a lower `attempt` answers the recorded turn, a higher `attempt`
  starts a new turn when the session is free.
- C. The consumer keeps its unanswered `inject` (`Consumer.replay`) and its turn in flight
  (`Consumer.flight`) in memory (`crates/core/src/deliver.rs`); a restarted consumer has
  neither. `Consumer::inject` replays a failed `inject` with the same `attempt` after a
  `renew` when it may have reached the engine (`replays`: `Io` after connect, `Timeout`,
  `Malformed`, `Invalid`) and calls `retry` otherwise. `Consumer::follow` renews until it sees
  the turn ended, treats a failed `status` as "next tick", and acks `uncertain` on `dropped` or
  `unknown`.
- S. `Inbox::recover` (`crates/core/src/inbox.rs`) runs at every step and in `claim`; it
  returns an expired `leased` row to `pending` without a lease and makes an expired
  `processing` row `uncertain` (`expired-processing`). `uncertain` is final: no store call
  leaves it. `Inbox::claim` raises `attempts` by one.
- W. The idle wait (`Consumer::step`, `Consumer::run`) sleeps until a publish or
  `Inbox::next_due`, which counts the future `available` of `pending` and `deferred` rows and
  the `lease_until` of `leased` rows only.
- K. A restart needs no cleanup: `bind` in `crates/core/src/serve.rs` and in
  `crates/engine/src/serve.rs` removes a socket file that refuses a connection, the kernel
  drops the `consumer.lock` `flock` of a dead process (`deliver::open`), and SQLite rolls back
  the hot journal of a killed writer.

### 2. Windows and end states

End states: (a) exactly one turn of the event in history and the row settled by its delivery;
(b) the row `uncertain`, whatever history shows; (c) a fix, named in sections 3 and 4, that
brings the window to (a) or (b).

| # | Window | Current behaviour | End state |
|---|---|---|---|
| 1 | Engine killed after it accepted `inject`, before the consumer read the answer | C: `Io` after connect, `renew`, replay with the same `attempt`. D: the restarted engine has no record and starts a second turn once the session is free. J: the first turn is in history when it ended before the kill, and the channel may have sent its answer. Two turns, and the row can end `done` | (c), fix A: the replay answers the recorded turn, which reads `unknown` (R), ack `uncertain`: (b) |
| 1' | As 1, but the replay's connect fails while the engine is down | C: `retry`; the next claim has a higher `attempt`, which D treats as a new delivery | (c), fix A: the later claim answers the recorded turn: (b) |
| 2 | Consumer killed after the accept, before C1 `start` | The row stays `leased`; S returns it to `pending` after `lease_until`; the next claim has a higher `attempt`; D starts a second turn while the first ran and may have been sent | (c), fix A: the later claim answers the recorded turn and the consumer follows it: (a) while the engine holds its report (R), else (b) |
| 3 | Engine killed with the event's turn running, or ended and not yet reported | J: a running turn is not in history (0 turns); an ended one is wholly there (1 turn), and the channel may have sent it. C: `status` fails while the engine is down, the lease is renewed; after the restart the turn reads `unknown` (R), ack `uncertain`, or S marks it `expired-processing` when the consumer had seen it ended without a delivery and the lease ran out first. Never partial (J), never two turns: the consumer never injects a `processing` row again and `uncertain` is final (S) | (b) |
| 4 | Consumer killed with the turn in flight (`processing`) | The turn goes on; the channel can still send its answer to the owner and report the delivery, which nobody reads, so the row does not change by it. W: the restarted consumer with nothing else queued waits with no timeout, and the row stays `processing` until the next publish wakes the loop | (c), fix B: the loop wakes at the row's `lease_until` and S marks it `expired-processing`: (b), with 1 turn in history (0 when the engine died as well) |
| 5 | `ulei-tg` reports the delivery to a restarted engine (`Holder::take` in `crates/channels/src/tg/inbox.rs`) | `Server::report_delivery` finds no such turn and answers `ok` false; a read cut by the engine's death ends without `end` and reports nothing. The consumer reads `unknown` | (b) |
| 6 | Consumer killed after `claim`, before `inject` reached the engine | S: back to `pending` after the lease; the next claim finds no record and starts the one turn | (a) |
| 7 | Consumer killed between reading the delivery and the `ack` commit | `Inbox::ack` is one `UPDATE`: committed, the row is settled; not committed, window 4 | (a), or (b) by window 4 |
| 8 | Engine killed with an `inject` in transit, before it accepted | No turn. C: `Io` after connect, replay; the restarted engine starts the one turn. A failed connect: `retry`, then the same | (a) |
| 9 | Engine killed after fix A wrote the record and before the turn started | Opened by fix A: the record names a turn that never ran; the replay or the later claim answers it, it reads `unknown`, ack `uncertain` | (b), 0 turns |
| 10 | A producer's `publish` cut by the consumer's death | `Inbox::publish` is one transaction; the same packet again succeeds without a change (`INSERT OR IGNORE`, same digest) | (a) once the producer publishes again |
| 11 | An event acked `deferred` comes back, before or after an engine restart | D: the higher `attempt` starts the turn of the re-offer, as C1 states | Each offer settles by its own turn; fix A keeps this, its record holding the `silent` delivery across a restart |
| 12 | Stale sockets, lock and SQLite journals of a killed process | K | No state effect |

Window 2 ends in (a) only while the engine holds the first turn's report; in production one
event at a time reaches the owner session and the report is one of the session's last 64. The
promise needs only (b) there.

### 3. Fix A: the engine's durable delivery record (C6 0.6.0, C8 0.5.0)

Place: `Server::inject` and `Server::report_delivery` in `crates/engine/src/serve.rs`, over a
`deliveries` table of the engine's journal. It replaces `Lanes.replays`, `Lanes::record` and
`KEPT_DELIVERIES`.

- The journal (schema version 5; version 4 files gain the table by migration) holds
  `deliveries (event_id TEXT PRIMARY KEY, attempt INTEGER NOT NULL, turn TEXT NOT NULL,
  delivery TEXT) WITHOUT ROWID`: per event the highest accepted `attempt`, the turn it started
  and that turn's `Delivery` once the channel reported it. Records are never removed: a record
  is needed as long as the event's row can come back, which a consumer's downtime does not
  bound, and a row is under 200 bytes.
- `inject`, in this order, all on the server thread, which serves one connection at a time and
  alone adds turns to a session, so the owner session stays free between steps 3 and 5:
  1. Read the record of `event_id`; a failed read answers `500 internal` (the consumer
     `retry`s).
  2. A record whose `attempt` is the request's or higher, or whose `delivery` is not `silent`:
     answer `accepted: true` with the owner session and the recorded turn; start nothing. The
     turn may be one this engine no longer holds; `status` reads it `unknown`.
  3. The owner session not free: `accepted: false`, `retry_after_s` 15.
  4. A new `TurnId`; write the record (`event_id`, `attempt`, the turn, no delivery) and commit
     it; a failed write answers `500 internal` and starts nothing.
  5. Start the turn; when its thread cannot start, delete the record and answer
     `500 internal`.
  6. Answer `accepted: true` with the new turn.
- Precedence: the record wins over the free check and over `attempt`. Only a recorded `silent`
  delivery lets a higher `attempt` start a new turn: of all ack paths of ADR-0036 section 7,
  `silent` of a kind other than `weekly` is the one that offers the event again (`deferred`),
  and a `silent` turn showed the owner nothing. Every other higher `attempt` follows a lost
  lease (windows 1', 2), and answering the recorded turn lets the consumer ack it by that
  turn's own delivery. A consumer killed after a `silent` turn and before its ack gets the
  re-offer at the next claim instead of 3,600 s later.
- `report_delivery` of a handed-over ended inbox turn with no delivery writes the `delivery`
  into the record of that turn first; a failed write answers `ok` false and stores nothing, so
  the row ends `expired-processing`. Then it stores the delivery on the report, as now.
- C8 0.5.0, additive: `$defs.InboxDelivery` `{event_id: string (C1 id), attempt: integer >= 1,
  turn: string (C6 TurnId), delivery?: "sent" | "send_failed" | "silent" | "silent_done" |
  "no_answer"}`, the description of the journal naming it, fixtures, a `CHANGELOG.md` entry.
- C6 0.6.0, descriptions only, no shape change; a minor bump because `inject` behaves
  differently: the `inject` method and `InjectRequest`: idempotent on `event_id`; the runtime
  keeps a record of every accepted event across its restarts and answers a recorded
  `event_id` with the recorded `session_id` and `turn` and starts nothing, unless the
  `attempt` is higher and the recorded turn's delivery was `silent`, which is a new delivery;
  a recorded turn the runtime no longer holds reads `unknown`. `InjectRequest.attempt`: the
  same rule from the attempt's side. `crates/runtime-adapters/PLUGIN.md` sections 5.2, 6 and 7
  follow: a runtime adapter keeps the record durable and fills its `delivery`.

### 4. Fix B: the due time of an orphaned `processing` row

Place: `Inbox::next_due` in `crates/core/src/inbox.rs`. The due time also counts the
`lease_until` of a `processing` row not expired at `now`. With a turn in flight the loop ticks
and never asks for the due time, so a `processing` row it sees while waiting is one a killed
consumer left; the loop wakes once at its expiry, `recover` marks it `expired-processing`, and
it is due no more. An empty queue still has no due time, so the idle budget stays 0 wake-ups.
This restores what the reference's 2 s pump gives (`recover` within 2 s of an expiry) under the
no-timer rule of ADR-0036 section 11.

### 5. The delivery time scale

In flight the loop polls every 2 s (ADR-0036 section 2 step 7), so 10^4 sequential events take
at least 5.5 h; a lease a killed consumer left runs 120 s, and a refused or failed `inject`
waits 15 s. `ulei-inbox serve --deliver C6_SOCKET` takes three test-only flags that set these
periods in milliseconds; each is a usage error without `--deliver` or with a value that is not
a number in its range:

| Flag | Sets | Range | Without the flag |
|---|---|---|---|
| `--deliver-tick-ms N` | the tick of ADR-0036 section 3 | 10 to 2000 | 2000 ms |
| `--lease-ttl-ms N` | the ttl the loop passes to `claim` and `renew` | 4000 to 120000 | `LEASE_TTL`, 120 s |
| `--retry-delay-ms N` | the delay the loop passes to `retry` | 100 to 15000 | `RETRY_DELAY`, 15 s |

The shortest lease is two of the longest ticks, so a lease renewed every tick does not run out
between renews. The C1 store rules and defaults do not change, only the values the loop passes.
Only the kill test passes the flags, at 50, 5000 and 1000; a deployment never does. The engine
has no timer the kill test waits on: the `retry_after_s` of a refused `inject` is advisory, and
the loop retries after its own delay. The deviation rows are in ADR-0036 section 11.

### 6. The kill test

`ci/deliver_kill.py ULEI_INBOX ULEI_ENGINE WORKDIR --events N --kills K --seed S`, stdlib
Python built on `ci/inbox_load.py` (`sign`, `call`, `spawn`, `stop`, `proc_field`, `Failure`)
and `ci/engine_idle.py` (`ready`, `SETTING_ENVS`), prints one `<name> <value>` line per
figure, exits 1 on a failed pass rule.

Processes, all under one test root in `WORKDIR`:

- `ulei-inbox serve --root ROOT --socket inbox.sock --deliver proxy.sock --deliver-tick-ms 50
  --lease-ttl-ms 5000 --retry-delay-ms 1000` on a root with `ENABLED`, the release binary. The
  harness derives its lease, retry delay and drain from the values it passes (section 5).
- `ulei-engine serve --config ulei.toml`, the release binary, with the `ulei.toml` of
  `ci/engine_idle.py` `measure` (synthetic brain files, no skills) plus `c6_socket`,
  `owner_session` `kill-owner`, `default_route`, and its journal at `state/journal.sqlite3`
  under the root; `SETTING_ENVS` removed from its environment.
- A fake C4G gateway on the `qb.socket` path: answers each `POST /v1/model/turn` with one
  assistant text as `crates/engine/tests/fake_gateway` does, after a uniform 0 to 100 ms
  delay so kills land inside running turns; usage of 1,000 prompt tokens, so no compaction
  runs. The text is a fixed sentence, or `NO_REPLY_DONE` for an event whose payload asks for
  it (one in ten).
- A fake channel: a thread that holds `inbox_turns` for `kill-owner`, reads the handed-over
  turn with `turn_events` until `end`, classifies it by the ADR-0036 section 6 table (the
  fake send always succeeds: `sent`; `NO_REPLY_DONE`: `silent_done`; no consumed `final`:
  `no_answer`), calls `report_delivery` and holds again; a failed connect is tried again after
  50 ms. It never answers `silent`: a `deferred` re-offer legitimately adds a turn and comes back
  only after 3,600 s.
- A C6 proxy on `proxy.sock`, forwarding each connection of the consumer to the engine's
  socket and seeing every method and answer. Its listener lives only while the engine runs (it
  is unlinked at the engine's kill and bound again after the ready line), so a refused connect
  stays a refused connect for the consumer.
- The harness: before the first event it calls `submit_turn` `{session: kill-owner, new: true}`
  so the owner key has a session; after every 50 settled events it does it again, so one
  session's history stays short. It publishes with C1 `publish` and repeats the same packet
  after a failed call. Ids are `kill:00001` onwards.

Kill points, each hit on purpose in the forced phase and by chance in the random phase:

| Point | Kill | On purpose | Expected (forced) |
|---|---|---|---|
| K1 | Consumer | The proxy receives an `inject` request: kill, drop it unforwarded | Window 6: (a) |
| K2 | Consumer | The proxy reads an accepted `inject` answer: kill before relaying it | Window 2: (a), after the lease |
| K3 | Engine | The gateway holds the turn; the proxy reads the accepted answer: kill the engine, close the consumer's connection | Window 1: (b), 0 turns |
| K4 | Engine | The proxy holds the accepted answer until the channel reads `end`: kill the engine, close the consumer's connection | Window 1: (b), 1 turn |
| K5 | Consumer | The proxy receives the first `status` of the turn | Window 4: (b), 1 turn, after the lease |
| K6 | Consumer | The proxy reads a `status` answer with a `delivery`: kill before relaying it | Window 7 then 4: (b), 1 turn, after the lease |
| K7 | Engine | The gateway receives the event's turn request | Window 3: (b), 0 turns |
| K8 | Engine | The channel reads `end`, before `report_delivery` | Windows 3 and 5: (b), 1 turn |
| K9 | Engine | `report_delivery` answered `ok` true, before the consumer's next `status` | Window 3: (b), 1 turn |

- Forced phase: for each point, one event is published alone, the point is armed, the process
  is killed and restarted, and the harness waits until the row is settled and checks the
  expected state exactly.
- Random phase: the other events are published at a steady 8 per second, about what the
  consumer settles at a 50 ms tick, held while 64 published events are unsettled, so a slower
  consumer sets the pace; a scheduler kills after exponential delays whose mean
  spreads `K` kills over the phase, the consumer in 45 %, the engine in 45 % and both in 10 %
  of kills. At each kill the proxy, gateway and channel name the phase they last saw
  (claimed, accepted, in flight, delivered, turn running, turn ended), and the summary counts
  kills per phase. The seed `S` makes a run repeatable.
- Restarts: a killed process is started again with the same arguments after a uniform 0 to
  500 ms pause; the consumer counts as up at its first `health` answer (`spawn`), the engine
  at its ready line (`ready`); then the proxy binds and the channel holds again.
- Drain: after the last kill no process is killed; the harness waits until every row is
  `done` or `uncertain`, at most the passed lease + retry delay + 30 s (36 s), then stops both
  processes.

Reconciliation, per published `id`:

- the C1 row by `get`: `status`, `outcome`, `attempts`;
- the count of the event's turns: with stdlib `sqlite3` read-only (`mode=ro`) on the test
  root's synthetic journal, the distinct `(session, turn)` of the sessions of key `kill-owner`
  that hold a user message whose text starts with `[INTERNAL BACKGROUND EVENT` and whose JSON
  tail has this `event_id`. A journal with a `compacted` message fails the run.

Pass rule: 0 lost, every row `done` or `uncertain` after the drain, and every `done` row with
exactly one turn; 0 duplicated, more than one turn only on an `uncertain` row; no turn for an
id never published. The summary prints events, kills per point and phase, rows by `status` and
`outcome`, turn counts of `uncertain` rows (0, 1, 2), wall time, events per second, the p50 and
p99 from publish to settle, the VmHWM of the last consumer and engine, and the journal size.

Runs:

- Reduced: CI job `kill-reduced`, on every pull request and push to `main`, parallel to
  `budgets`, builds both release binaries as `budgets` does and runs 300 events, the 9 forced
  kills and 30 random ones, measured at 1 min (wall 63 s; the job with its builds 3 min, under
  the 3.5 min of `budgets`), step timeout 5 min. Its forced phase waits out four 5 s leases
  (K1, K2, K5 and K6) and two 1 s retry delays (K3 and K4). The seed is the run id; the summary
  is the step's one `::notice` and goes to the step summary.
- Full: job `kill-full` (timeout 45 min) builds both release binaries and runs 10^4 events,
  the 9 forced kills and 300 random ones, measured at 22 min (wall 1293 s at 7.7 events per
  second; the job with its builds 24 min). It runs on `workflow_dispatch` and on a pull request
  whose diff against its base touches `ci/deliver_kill.py`, so a pull request that changes the
  harness carries one full run with its measurements in its checks and step summary.

### 7. Budget line

`idle_deliver` in `ci/inbox_load.py` also reads the VmHWM of `serve --deliver` at the end of
its 10 s idle window and prints `deliver_vmhwm_kb`; `ci/budgets.sh` prints it next to the
idle wake-ups line and fails when it exceeds `ULEI_INBOX_MAX_RSS_KB` (4096 kB). No limit is
raised.

### 8. Implementation cards, in order

Each card merges alone with the workspace green.

1. Fix A: C6 0.6.0 and C8 0.5.0 (descriptions, `InboxDelivery`, fixtures, changelog,
   generated types), journal schema version 5 with its migration, the record in `inject` and
   `report_delivery` in the order of section 3, `KEPT_DELIVERIES` and `Lanes.replays` removed,
   `crates/runtime-adapters/PLUGIN.md` sections 5.2, 6 and 7; tests of windows 1, 1', 2, 9 and
   11 against the engine's C6 server, a restart included.
2. Core: fix B in `Inbox::next_due` and `--deliver-tick-ms` (section 5); tests of the due time
   of an orphaned `processing` row and of the flag's range.
3. Budget line of section 7.
4. The kill test of section 6: `ci/deliver_kill.py`, the step in `budgets`, the `kill-full` job,
   the row of `AGENTS.md`'s layout table. Needs cards 1 and 2.

## Consequences

- Every event ends in the owner session's history once, or its row is `uncertain`, under
  SIGKILL of the consumer, the engine or both, as in the reference, where a crash after the
  turn began leaves the row `uncertain` and a row back in `pending` had no turn.
- The engine's journal grows by one small row per delivered event.
- An `uncertain` row may have 0, 1 or 2 turns in history and the owner may have seen the
  answer; reconciling it stays manual, as C1 states.
