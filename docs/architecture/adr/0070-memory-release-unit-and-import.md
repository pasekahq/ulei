# ADR-0070: `ulei-memory` in the release, its user unit, and `ulei-memory import`

Status: accepted
Date: 2026-11-25
Affects: crates/memory, deploy/systemd, deploy/install.sh, ci/release.sh, ci/install_run.py, ci/units_run.py, .github/workflows/ci.yml

## Context

The archive daemon `ulei-memory serve` (ADR-0051 section 2, C10, ADR-0062) is built and held to
its budgets, but it is not released, has no unit, and nothing fills it with history that came
before it. The move off the old runtime needs the archive installed beside the other daemons and
filled with the old runtime's history (ADR-0051, decision of 13:29 to 13:30: "the old runtime's
history and library go into the archive after the sprint"). The engine's tools `archive_search`
and `archive_read` and the connectors' runner already reach the archive on `memory.socket`.

## Decision

### 1. Release and install

`ci/release.sh` adds `ulei-memory` to its binaries, from its own release build
(`-p ulei-memory --bin ulei-memory`, the build of the budgets job), so `SHA256SUMS` lists it.
`deploy/install.sh` installs it with the others and puts `ulei-memory.service` beside the other
units; it prints `systemctl --user enable --now ulei-memory.service` as a step of its own, to run
once `ulei.toml` has `[memory]`. `ci/install_run.py` expects both. The end-to-end connector check
(ADR-0056 section 9) runs the released binary.

### 2. The unit

`deploy/systemd/ulei-memory.service`, in the shape of ADR-0042:

| Item | Value | Why |
|---|---|---|
| `ExecStart` | `%h/.local/bin/ulei-memory serve`, `Type=exec` | The ready line is on stdout; the binary speaks no readiness protocol. |
| Order | `After=ulei-engine.service`, no `Wants=` or `Requires=` | The first pull finds the engine's socket when both start; a start without the engine still succeeds, since a failed pull is no failure to start and `health` names it (ADR-0051 section 3). |
| Writable | `ReadWritePaths=%S/ulei/memory %t/ulei`, `StateDirectory=ulei/memory`, the shared `RuntimeDirectory=ulei` with `RuntimeDirectoryPreserve=yes` | `memory.archive` lies under `%S/ulei/memory`, `memory.socket` under `%t/ulei` beside `inbox.socket` and `engine.c6_socket`: the engine's system unit shows of `/run/user/<uid>` only that directory (ADR-0069 section 2), so its tools reach the archive there. |
| Network | `RestrictAddressFamilies=AF_UNIX` | Its socket and the engine's are its only peers. |
| Limits | `MemoryMax=32M`, `TasksMax=2` | Eight times its 4 MiB VmHWM budget (ADR-0051 section 7): a 1 MiB `put` body with its decoded and deflated copies, a 512 KiB C6 page and the reclaimable page cache of a growing archive; one thread, doubled. |
| Restart | as ADR-0042 section 6, `RestartPreventExitStatus=2` | `ulei-memory serve` now exits 2 on a refused config, as the engine does (ADR-0069 section 3), so a typo is not restarted for ever; a failed open, bind, ready line or `accept` stays exit 1 and restarts. |

The hardening is ADR-0042 section 8 with `MemoryDenyWriteExecute=yes`. Offline exposure 5.5 on
systemd 255, the threshold of the `systemd-units` job. `ci/units_run.py` starts the unit alone
first, with no engine running, and checks it runs and answers `health` with its failed pull; then
with the others it checks the ReadWritePaths rule of ADR-0042 section 3, the process's NoNewPrivs,
seccomp filter and cgroup limits, and a clean stop. The connectors' runner in that test puts into
this real archive instead of a fake C10.

`deploy/systemd/system/client-no-userns.conf` (ADR-0069 section 2) also applies to
`ulei-memory.service` on a host whose engine runs as the system unit: it connects to the
engine's socket like the inbox and the channel.

### 3. `ulei-memory import`

`ulei-memory import [--skip N] [--config PATH]` reads JSON Lines on stdin, each line one C10
`PutRequest`, and puts each through the daemon's socket (`POST /v1/put` on `memory.socket`, the
C10 transport the runner uses). It opens no archive file: the file stays the daemon's and
`delete`'s (ADR-0051 section 2), and dedup and conflict answers stay the daemon's.

- A line that is not a `PutRequest`, or that is longer than the 1 MiB body cap, is refused
  without a request (`invalid_request`, `too_large`). The daemon's `invalid_request`, `conflict`
  and `too_large` refuse the line too. Each refused line is one stderr line
  `import: line <n>: refused <code>`; it does not change the exit status.
- A transport failure (connect, write, read, the 30 s deadline, a missing answer) or
  `store_failed` is tried again 3 times after pauses of 1, 2 and 4 s, then stops the import:
  exit 1 and one line naming the line and the last line done, which `--skip` takes to resume.
  A repeat is safe, since a stored entry answers `duplicate`. Any other answer (`forbidden`,
  `unknown_method`, a malformed answer) stops at once.
- Every 10,000 lines one progress line; at the end one line
  `import: <lines> lines, <stored> stored, <duplicate> duplicate, <refused> refused`. Exit 0 when
  the input was read to its end, 2 on a usage error.
- It reads only `memory.socket` from `ulei.toml`, one line at a time with at most 1 MiB kept, and
  reads no compressed input: `zcat history.jsonl.gz | ulei-memory import`.

Turning the old runtime's history into `PutRequest` lines is the deployment's converter, outside
this repository; `docs/runbooks/memory-archive.md` names the format.

Rejected:
- Import into the file directly: a second writer of the archive beside the daemon, and the dedup
  and conflict rules in two places.
- A `put_batch` method in C10: a contract change for a one-off fill that one request per line,
  as the runner sends them, already does within the daemon's `put` budget.
- A converter of the old runtime's format in the crate: the archive parses no source format
  (ADR-0051 section 2).

## Consequences

- The release has seven binaries; `deploy/install.sh` installs five units.
- The pilot host runs the archive as a user unit beside the engine's system unit with the drop-in
  of section 2.
- No new dependency and no contract change.
