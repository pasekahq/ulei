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
- shows of `/run/user/<uid>` only the ulei socket directory (written) and, at the qb gateway's
  socket directory, the stable directory of the gateway proxy of section 4
  (`TemporaryFileSystem=` plus `BindPaths=`), so no bus, no systemd socket and no other tool's
  socket is reachable;
- adds what only the system manager applies: `ProtectKernel*`, `ProtectControlGroups`,
  `ProtectClock`, `ProtectHostname`, `ProtectProc=invisible`, `PrivateDevices` and an empty
  capability bounding set (offline exposure 1.6 against the user unit's 5.8);
- creates the socket and work directories in a `+` `ExecStartPre`, as root, outside the sandbox.

`deploy/systemd/system/user-engine-off.conf` is a drop-in of the user unit with
`ConditionPathExists=!/etc/systemd/system/ulei-engine.service`: the user unit of the same name is
skipped, so `ulei-tg`'s `Wants=` and the commands `deploy/install.sh` prints never start a second
engine on the socket, and an upgrade by `deploy/install.sh` keeps working. The other ulei units
stay user units. The unit name stays `ulei-engine.service`, the engine's `engine.service_unit`.

`deploy/systemd/system/client-no-userns.conf` is a drop-in of `ulei-inbox` and `ulei-tg` for the
same hosts. The user manager still runs a unit with namespace options in an unprivileged user
namespace, and on Ubuntu 24.04 the AppArmor profile `unprivileged_userns` of such a process
refuses `connect` to a socket bound in another mount namespace ("disconnected path"): the system
engine's socket, so neither client reached the engine (found on the pilot host). The drop-in
clears `ProtectSystem`, `ProtectHome`, `PrivateTmp` and `ReadWritePaths`, which apply on such a
host in no case, so the two units run without the user namespace and connect.

The file has placeholders (`<ULEI_USER>`, `<ULEI_UID>`, `<ULEI_HOME>`): a system unit's `%h` and
`%U` are root's, whatever `User=` says. The CI job `systemd-units` fills them for the runner's
account, installs it with the proxy of section 4 and runs its turns (`ci/units_run.py system`);
the runbook checks it on the host.

### 3. The engine unit

- `WorkingDirectory=%S/ulei/engine/work`, created by `StateDirectory=` and writable.
- `OOMPolicy=continue`: an OOM kill of a tool's child ends only that child; an OOM kill of the
  engine is still a failure that restarts the unit (ADR-0042 section 6).
- `ulei-engine serve` exits 2 when its config is refused (the file, a missing or invalid setting,
  the core settings, the prompt cache mode), so `RestartPreventExitStatus=2` leaves it stopped
  with its one stderr line; a journal it cannot open, a failed bind, ready line or `accept` stay
  exit 1 and restart. `ulei-engine run` keeps exit 1.

### 4. The qb gateway across a re-created directory

The engine opens a new connection to `qb.socket` for every request, by path. The qb gateway's
unit may delete its runtime directory with the socket and make both again while the engine runs,
and the engine's next turn must reach the new socket without an engine restart. In the user unit
`/run/user/<uid>` is the host's own directory, read-only, so the path finds the new socket. In the
system unit a bind of the gateway's directory pins the directory it found at start: once that
directory is deleted the engine sees it empty until the unit restarts. The options were:

- Bind the whole runtime directory read-only. Every socket in it becomes reachable (the user bus,
  `systemd/private`, other tools' agents) and needs an `InaccessiblePaths=` entry each, a list
  that only names what is known; and a mask over an entry the host deletes and makes again (the
  user manager's own sockets after its restart) is gone in the unit's namespace from then on.
- The engine retries a failed connect for a bounded time, with one journal line naming
  `qb.socket`. Inside a pinned directory no retry finds the new socket; it covers only the seconds
  a gateway restart takes, which neither unit needs for this case.
- A stable bind point, chosen. `deploy/systemd/system/ulei-qb-proxy.socket` listens on
  `/run/ulei-qb-proxy/gateway.sock`, a directory of root that the gateway's unit never touches,
  owned by and only open to the ulei account. `ulei-qb-proxy.service`, which it starts as that
  account, runs `systemd-socket-proxyd` with the gateway's socket path, the path `qb.socket` names,
  and connects by that path for each connection, so it reaches a re-made directory at once. The
  engine's system unit requires the socket and binds `/run/ulei-qb-proxy` over
  `/run/user/<uid>/qb-gateway`, so `qb.socket` keeps its value and the runtime directory still
  shows nothing else. The cost is one more process on the host and one relay per request; while
  the gateway is down the proxy accepts and closes the connection, so a turn then fails on a
  closed connection rather than on `ENOENT` or `ECONNREFUSED`, with the same `failed` outcome.

The user unit keeps its view and needs no proxy. CI runs both units across a deleted and re-made
gateway directory: `ci/units_run.py sandbox` the user unit, `ci/units_run.py system` the system
unit with its proxy.

## Consequences

- With either unit, a tool reading a secret path gets `EACCES` (or `ENOENT` for the hidden buses).
- Part 6 of the cutover runbook: case (b) is a no-go for the engine in the user unit and a go
  with the system unit; the check reads the probe of section 1 from inside the engine's unit.
- Case (b) installs the gateway proxy's socket and service beside the engine's system unit, each
  a copy with the placeholders filled (runbook 6.3); a change of any of the three files in the
  repository reaches the host only when the operator fills and installs them again.
- `ulei-inbox` and `ulei-tg` keep their user units and their exit 1 on a refused config; under
  case (b) their namespace options stay inert (they run no tool on the owner's behalf).
