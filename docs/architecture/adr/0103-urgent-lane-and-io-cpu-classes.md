# ADR-0103: The hive's urgent lane by the owner's flag, and IO and CPU classes for every ulei unit and Hive head (C2 0.7.0, C9 0.17.0)

Status: accepted
Date: 2026-11-27
Affects: hive-py, crates/engine, crates/hive-mcp, crates/core, contract:C2, contract:C9
Amends: ADR-0042 section 11 (the scheduled units' slice)

## Context

The hive's admission made an owner card urgent by its step (`urgent_max_step`, 2) and held
`urgent_reserve` slots for urgent work on every ceiling. An owner card the owner waited for at
step 3 queued behind the IO PSI gate for half an hour, while the reserve sat idle the rest of the
time. Host IO PSI cannot tell an overloaded disk from a throttled background job, so the gate
stayed closed while the owner's services saw no pressure.

The units of `deploy/systemd` declared no IO or CPU settings; the scheduled units ran in the
manager's default slice. The hive capped each head by bandwidth (`head_io_read_max`,
`head_io_write_max`), which a metadata walk of small random reads never reaches. The target disk
runs mq-deadline, which ignores `IOWeight`, and whether it honours the IO priority classes is not
known for the target host.

Neither the hive nor the slices are reference-agent behaviour: the hive is ulei's own (ADR-0068),
and the `owner-urgent` policy of section 1 is an owner-designed addition to the engine's policy
chain, not a port.

## Decision

### 1. Where `urgent` is set and enforced

- C2 `hive_create` takes the optional boolean `urgent` (0.7.0, additive). `ulei-hive-mcp` passes
  `urgent: true` as `hive.py create --urgent` and refuses it, as the hive does, with a `kind`
  other than `owner_task`, a series or a reminder.
- The one enforcement point of who may set it is the engine's policy `owner-urgent`
  (`crates/engine/src/policy/owner_urgent.rs`), always in the chain: a tool call whose arguments
  set `urgent` to `true` is refused unless the turn's origin is `Origin::TopLevelOwner`, the
  owner's input on `engine.main_channel` in a top-level session. Before the first turn the origin
  is `Other`. Sub-sessions, events, other channels and background work cannot create an urgent
  card through the engine.
- The hive stores `urgent` on the card (`cards.urgent`, additive) and accepts it only on a one-off
  `owner_task` run by a head: no epic, series, script, reminder or external executor. This is a
  rule of the card's shape, not of its origin; the CLI is the deployment's local tool, which the
  owner and the secretary already use to set `--kind owner_task`.

### 2. Admission

- The class `urgent` is an `owner_task` created urgent in `queued` or `review`, at any step, and,
  as before, a card of `[admission] urgent_sources` or its `:conditional` child.
  `urgent_max_step` is gone: no step limit applies to owner cards.
- An urgent run is extra: it counts against no ceiling (`concurrency`, `script_concurrency`,
  `max_local_runs`) and holds no slot of the other classes, so the sixth card with `concurrency`
  5 starts. At most two urgent cards (`api.URGENT_MAX`) hold active runs at once; a third waits.
- It passes the IO PSI gate and its cooldown. It keeps the hard floors: the host sample was read,
  `min_free_mb` of disk, and MemAvailable minus starting runs times `per_run_mb` at or above
  `[admission] urgent_floor_mb`, whether or not the gate is closed.
- Nothing running is preempted, paused or frozen to make room.
- `urgent_reserve` is removed. A config that still names it, `urgent_max_step`,
  `head_io_read_max` or `head_io_write_max` loads without the key and with one stderr line naming
  it, once per process and config file (`store.OBSOLETE_KEYS`).
- The `claimed` event keeps `bypass`: `pressure` when the host gate admitted no ordinary run,
  `cap` when a skipped ceiling was full. `health` and `inspect` snapshots become version 2: their
  `slots` name `urgent_max`, `urgent_used` and each run's `urgent` instead of the reserve.

### 3. The classes

| Class | Slice | Members | CPUWeight | CPUQuota | Read IOPS | Write IOPS | Per unit |
|---|---|---|---|---|---|---|---|
| interactive | `ulei.slice` | engine, inbox, tg, memory, hive-mcp, garden, uncertain, hive-watchdog | 100 (default) | none | uncapped | uncapped | as shipped |
| Hive heads | `hive-heads.slice` | every head and script of the hive | 50 | 50 % of the CPUs | 33 % | 20 % | idle IO class, nice 10 |
| urgent owner heads | `hive-urgent.slice` | the urgent runs | 1000 | none | 15 % | 10 % | `CPUWeight=1000`, best-effort IO class at level 0 |
| background | `background.slice` | night, connector runner and its runs, the host's backups and imports | 1 | 25 % of the CPUs | 25 % | 20 % | idle IO class, nice 10 |

- The IOPS shares are the issue's. The build class is not shipped: nothing in ulei starts build
  work in a slice of its own (`ulei run --class` is not built), and host-side build work belongs
  to background. The capped classes take at most 73 % of the reads and 50 % of the writes; the
  rest stays with the interactive class and the host.
- The urgent class's high IO class is best-effort level 0: the realtime class needs a privilege
  the user manager lacks. The head classes' IO classes and CPU weight are set per unit by the
  dispatcher (`systemd-run --slice=`); the background units set theirs in their unit files, and
  the connector runner gives each run's transient unit `--slice=background.slice` with the same.
- No unit, slice or launcher sets `IOWeight`.
- Every `deploy/systemd/*.service` sets one `Slice=` of these four in `[Service]`; `cargo xtask
  units`, part of `cargo xtask check`, refuses a unit without one or with another. Timers run no
  process; the system units of `deploy/systemd/system/` are outside the user classes.

### 4. Keys and how `install.sh` derives the caps

- `ulei.toml` gains `[host.io]` (C9 0.17.0): `device`, the block device under `/dev/`, and
  `read_iops` and `write_iops`, what it sustains in random 4 KiB IO. No daemon reads it.
  `install.sh` reads only that table form, and refuses another form of a `host` key, a missing key
  or a malformed value before it copies anything.
- `install.sh` writes each capped class slice as its shipped file followed by its caps:
  `CPUQuota` as the class's percent times `nproc`, and with `[host.io]`
  `IOReadIOPSMax=<device> <share>` and `IOWriteIOPSMax=<device> <share>`, the share the class's
  percent of the capacity, rounded down, at least 1. `CPUWeight` is the class's own, in the
  shipped file. It writes no drop-in, so a `systemctl --user show -p DropInPaths` of a class slice
  lists only the owner's.

### 5. Without the values or without io delegation

- Without `ulei.toml` or its `[host.io]`, `install.sh` installs the slices with their CPU
  settings and no IOPS caps and prints a line saying so.
- It reads `cgroup.controllers` of the user manager and prints a line when `io` is not delegated
  or it cannot tell; the IOPS caps of a user slice act only with io delegated
  (`Delegate=... io ...` on `user@.service`). It still installs. The runbook
  `docs/runbooks/io-classes.md` is the host step.

### 6. Per-head caps

`[resources] head_io_read_iops` and `head_io_write_iops`, positive integers with
`head_io_device`, become `IOReadIOPSMax` and `IOWriteIOPSMax` of every head's and script's
transient unit, within the caps of its slice. They replace the bandwidth keys of section 2.

### 7. Measurement

The CI job `systemd-units` delegates io to the runner's user manager, installs the slices with a
synthetic `[host.io]` naming the runner's disk and runs `ci/io_classes_run.py`: probe reads of
the urgent and background classes under a random 4 KiB O_DIRECT read load, the disk's scheduler,
and the `io.stat` read rate of `hive-heads.slice` against its `IOReadIOPSMax`. It reports them in
notices and says when the IO priority shows no effect; it fails only when the capped slice reads
more than 15 % above its cap. When the priority shows no effect, the IOPS caps are the mechanism.

## Consequences

- An owner card the owner waits for, created urgent from the main chat, starts at the next
  dispatcher tick whatever the queue, the gate's PSI or its step, unless two urgent cards run,
  memory or disk is short, or no vendor can take it.
- The secretary's external-executor ceiling that subtracted the urgent reserve has no reserve to
  subtract.
- A host whose user manager lacks the io controller gets the CPU settings but not the IOPS caps
  until the owner delegates it; hand-made drop-ins of the same slices override the installed
  values until removed (the runbook lists them).
- The scheduled units' IO no longer competes uncapped with the owner's turn.
