# ADR-0069: The engine's secret paths, its system unit where user namespaces are off, and its unit fixes

Status: accepted
Date: 2026-11-24
Affects: crates/engine

## Context

The pilot runs ulei on the host of the owner's live assistant, as the same account
(`docs/runbooks/mvp-pilot-cutover.md`). The engine's tools (`terminal`, `read_file`, MCP servers)
run as that account with a network. ADR-0042 section 8 gives the unit `ProtectHome=read-only`,
which keeps the home from being written, not from being read: the live bot's token, the live
inbox root with its `producer.key`, the reference agent's home, the qb credentials and the
account's SSH and GitHub keys are all readable by one prompt injection.

In a user unit every namespace option applies only where the user manager may create an
unprivileged user namespace (ADR-0042 section 8). On Ubuntu 24.04 with
`kernel.apparmor_restrict_unprivileged_userns=1` the manager creates the user namespace but has
no capability in it, so the unit runs in the host's mount namespace: `ProtectSystem`,
`ProtectHome`, `PrivateTmp`, `ReadWritePaths` and any `InaccessiblePaths` are silently inert
(part 6 of the cutover runbook, case (b)). The pilot host is such a host. Through the user bus
(`$XDG_RUNTIME_DIR/bus`, `systemd/private`) a command can also start a transient user unit
outside any sandbox, and the Docker socket is root for its group.

Three smaller faults of the engine unit: its working directory is the read-only home, so a
relative `write_file` fails with `EROFS`; the default `OOMPolicy=stop` stops the whole engine when
the kernel kills one heavy tool child at `MemoryMax`; and `ulei-engine serve` exits 1 on a refused
config, so `RestartPreventExitStatus=2` never matches and a typo restarts the unit once a minute
for ever.

## Decision

### 1. Paths the engine must not read

`deploy/systemd/ulei-engine.service.d/10-deny-secrets.conf` is a drop-in of the user unit with
`InaccessiblePaths=` for the account's keys and credentials (`~/.ssh`, `~/.gnupg`, `~/.netrc`,
`~/.git-credentials`, `~/.config/gh`, `~/.docker`, `~/.aws`, `~/.claude`, `~/.codex`,
`~/.config/qb`), the reference agent's home `~/.hermes` (its profiles, the live bot token and the
live inbox root), the user and session buses (`%t/bus`, `%t/systemd`, `%t/dbus-1`, `%t/gnupg`) and
the Docker socket. Each path has a leading `-`, so a host without it still starts the unit. The
host adds its own paths (an owner data directory, a token file elsewhere) in one more line. The
engine needs none of them: it reaches qb over the gateway socket, not with qb's credentials.

`deploy/install.sh` does not install it: its list is the host's decision (runbook part 6).

### 2. A system unit where the user manager has no namespaces

On a host of case (b) the drop-in is inert, and the pilot does not go on with the engine in the
user unit (runbook part 6). `deploy/systemd/system/ulei-engine.service` runs the engine as a
system unit with `User=` the ulei account: the system manager sets the mount namespace up as root
before it drops to that account, so it needs no user namespace and no change of the host's
sysctl or AppArmor, and it touches no other unit. Against the user unit it:

- keeps the restart, limits, filters and the deny list of section 1;
- shows of `/run/user/<uid>` only the ulei socket directory (written) and the qb gateway's socket
  directory (`TemporaryFileSystem=` plus `BindPaths=`), so no bus, no systemd socket and no other
  tool's socket is reachable;
- adds what only the system manager applies: `ProtectKernel*`, `ProtectControlGroups`,
  `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible`, `PrivateDevices` and an empty
  capability bounding set (offline exposure 1.6 against the user unit's 5.8);
- creates the socket and work directories in a `+` `ExecStartPre`, as root, outside the sandbox.

`deploy/systemd/system/user-engine-off.conf` is a drop-in of the user unit with
`ConditionPathExists=!/etc/systemd/system/ulei-engine.service`: the user unit of the same name is
skipped, so `ulei-tg`'s `Wants=` and the commands `deploy/install.sh` prints never start a second
engine on the socket, and an upgrade by `deploy/install.sh` keeps working. The other ulei units
stay user units. The unit name stays `ulei-engine.service`, the engine's `engine.service_unit`.

The file has placeholders (`<ULEI_USER>`, `<ULEI_UID>`, `<ULEI_HOME>`): a system unit's `%h` and
`%U` are root's, whatever `User=` says. CI does not run it; the runbook checks it on the host.

### 3. The engine unit

- `WorkingDirectory=%S/ulei/engine/work`, created by `StateDirectory=` and writable.
- `OOMPolicy=continue`: an OOM kill of a tool's child ends only that child; an OOM kill of the
  engine is still a failure that restarts the unit (ADR-0042 section 6).
- `ulei-engine serve` exits 2 when its config is refused (the file, a missing or invalid setting,
  the core settings, the prompt cache mode), so `RestartPreventExitStatus=2` leaves it stopped
  with its one stderr line; a journal it cannot open, a failed bind, ready line or `accept` stay
  exit 1 and restart. `ulei-engine run` keeps exit 1.

## Consequences

- With either unit, a tool reading a secret path gets `EACCES` (or `ENOENT` for the hidden buses).
- Part 6 of the cutover runbook: case (b) is a no-go for the engine in the user unit and a go
  with the system unit; the check reads the probe of section 1 from inside the engine's unit.
- `ulei-inbox` and `ulei-tg` keep their user units and their exit 1 on a refused config; under
  case (b) their namespace options stay inert (they run no tool on the owner's behalf).
