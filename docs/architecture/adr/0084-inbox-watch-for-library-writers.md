# ADR-0084: the delivery loop wakes on every writer of the root, and an unreachable engine is logged

Status: accepted
Date: 2026-11-26
Affects: crates/core, docs/runbooks/mvp-pilot-cutover.md

## Context

ADR-0036 section 3 wakes the `deliver` thread of `ulei-inbox serve` on a C1 `publish` through
its own socket or at the nearest due time, and on nothing else: with no row due it blocks on its
channel without a timeout. On the live root other processes write `events.sqlite3` directly
through the Python library of the store, without the socket: the task hive (`inbox_library`) and
the owner's scripts (pushes, watchdogs, uptime pings). Their rows stayed `pending` until some
later publish through the socket or a restart of `ulei-inbox`; on 2026-11-26 a hive card's
`cancelled` event waited for minutes while the owner session was idle. The reference consumer
does not have the gap because it polls every 2 s, which ADR-0036 replaced to keep 0 idle
wake-ups (ADR-0001).

Step 3 of ADR-0036 section 2 counts a failed C6 `is_idle` as a busy session and tries again a
tick later without a word. On a host of case (b) of the cutover runbook (part 6) a
`ReadWritePaths=` drop-in of `ulei-inbox.service` put the unit back into an unprivileged user
namespace, AppArmor's `unprivileged_userns` profile refused its `connect` to the system engine's
`c6.sock`, and nothing in the unit's journal said so.

## Decision

1. **Watch.** `serve` with `inbox.deliver` starts a third thread, `watch`, that holds an inotify
   watch (`IN_MODIFY`) of the root directory and blocks reading it. For each batch of events that
   names `events.sqlite3` it sends one `()` into the channel a publish signals (one slot, so
   writes while the loop is busy wake it once). The store's journal mode is `DELETE`, so every
   commit of every writer writes the database file, and a reader writes nothing; the loop's own
   commits wake it at most once more, and an idle root still costs 0 wake-ups. A row another
   writer commits is taken within milliseconds, as a published one. The watch uses the `fs`
   feature of `rustix`, already a dependency of the crate; no dependency is added.
2. **Fallback poll.** When the watch cannot be set up (the account's inotify limits spent),
   `serve` writes one line `ulei-inbox: owner inbox watch off (<error kind>); polling every 5 s`
   and the loop waits at most `POLL`, 5 s, where it would wait for a publish or a later due time.
   The idle wake-up budget does not hold on such a host; it is the only such case.
3. **Unreachable engine.** A failed `is_idle` still claims nothing (ADR-0036 section 2 step 3).
   When it fails, and its category differs from the one last written, the loop writes one line
   `ulei-inbox: engine unreachable: <category>`; the category is the I/O error kind with
   `connect` or `io` (for example `connect: permission denied`), `timeout`, `engine runs as
   another uid`, `request too large`, `malformed answer`, `invalid answer` or `error <C6 code>`,
   never a message. The first answer after it writes `ulei-inbox: engine reachable again`. A
   failing engine thus costs two lines, not one per tick.
4. **Runbook.** On a host of case (b), `ulei-inbox` gets no `live-root.conf`: `ProtectHome=no` of
   its `90-system-engine.conf` drop-in already lets it write the live root, and a non-empty
   `ReadWritePaths=` brings the user namespace and the AppArmor denial back. Step 2.6 checks the
   unit's user namespace against the shell's and the kernel log for a denied `connect`.

The schema the Python library shares is unchanged; `claimable` and `next_due` now also run after
each commit of another writer; they are the same queries as before, and a commit is rare next
to the reads a 2 s poll of the reference makes, so no index is added.

## Consequences

- `ci/inbox_load.py` expects three threads of `serve` with `inbox.deliver`; the idle wake-up
  budget stays 0.
- ADR-0036 section 3 holds with one more wake source: a commit of any writer of the root.
- Tests: a row written through the library without a publish is injected within a tick by the
  daemon and by an in-process watched loop; a loop without a watch takes it within its poll; an
  engine without a socket is one `engine unreachable` line over many ticks and one `reachable
  again` line once it answers.
