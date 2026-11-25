# ADR-0073: The privileged helper `ulei-priv`: a fixed list of root commands behind a socket

Status: accepted
Date: 2026-11-25
Affects: crates/priv, dep:rustix, dep:serde_json

## Context

Blocker B3 of the cutover go/no-go. Part 5 of `docs/runbooks/mvp-pilot-cutover.md` asks that no
command of the secretary's skills needs root, and it fails: the old runtime runs, through `sudo`,
the effective sshd configuration (`sshd -T`, read by the security audit), `docker ps` (the Docker
socket is root-equivalent and hidden from the engine, ADR-0069), `ufw status`,
`fail2ban-client status` and `status sshd`, and `journalctl` and `systemctl
restart|try-restart|is-active` of the ulei units. `ulei-engine.service` is a system unit; the
inbox, the channel and the connector runner are user units of the ulei account, which root
reaches with `systemctl --user -M <user>@`.

The engine runs with `NoNewPrivileges=yes` (ADR-0042 section 8, ADR-0069), inherited by every
process of its tools. `no_new_privs` makes `execve` ignore set-user-id bits and file capabilities
(kernel `Documentation/userspace-api/no_new_privs.rst`), so `sudo` stops with
`The "no new privileges" flag is set` whatever sudoers says. The flag does not stop a process from
asking a daemon that already holds the privilege, over a Unix socket.

Giving the engine root, or dropping the flag so that a sudoers rule works, puts root one prompt
injection away from the model: any command a rule allows runs with arguments the model chooses,
and a rule's wildcard or a command with a shell escape (`journalctl` runs a pager,
`systemctl` edits units) is root for the whole host.

## Decision

1. A new class-2 crate `crates/priv`, package and binary `ulei-priv`, with `contracts = []`. It
   depends on `serde_json` (the requests, replies and audit lines) and `rustix` (`net` for
   `SO_PEERCRED`, `event` for `poll`, `process` for `pidfd_open`, `kill_process_group` and
   `geteuid`, `fs` for the owner check), both workspace declarations already in `Cargo.lock`.
   No other dependency; no `unsafe`; synchronous; no signal handler (the unit's stop kills the
   cgroup).
2. **`ulei-priv serve`** runs as root from the system unit `deploy/systemd/system/ulei-priv.service`
   and reads the policy `/etc/ulei/priv.toml` (`--policy`): `allowed_uids` and a list of commands,
   each with a fixed `id`, a fixed `argv` whose first item is absolute, an optional `arg` list of
   the only values the one argument slot `{arg}` may take, `timeout_s` and `max_output_bytes`.
   No value from the client reaches `argv` except one equal to an entry of `arg`; no shell, no
   glob, no client environment. It refuses to start when the policy file or its directory is not
   owned by the daemon's own user (root in the unit) or is writable by group or others, when an
   `argv[0]` is not absolute or a `<PLACEHOLDER>` is left. The format is a strict TOML subset
   written in the crate (top-level `allowed_uids`, `[[command]]` tables, one-line arrays, basic
   strings with `\t \n \" \\`), since the workspace has no TOML crate and C9's decoder holds only
   `ulei.toml`'s catalogue.
3. **Socket** `/run/ulei-priv/priv.sock` (`--socket`, the unit's `RuntimeDirectory`), mode 0666;
   the peer's uid from `SO_PEERCRED` must be in `allowed_uids`, checked in the accept thread
   before anything is read, so no other local user can hold a slot. One JSON request line per
   connection, at most 4 KiB, within 5 s; at most 4 connections handled at once; one command at a
   time, a second caller gets `busy`. The main thread blocks in `accept`: 0 wake-ups when idle.
4. **Child**: `execve` of the policy's argv in a process group of its own, stdin `/dev/null`,
   environment exactly `PATH=/usr/sbin:/usr/bin:/sbin:/bin` and `LANG=C.UTF-8`, cwd `/`. stdout and
   stderr are captured up to `max_output_bytes` each; past it the pipe is closed, the child gets
   `EPIPE` or `SIGPIPE` as under `| head -c`, and the reply says `truncated`. At `timeout_s` the
   group gets SIGTERM, 2 s later SIGKILL. The reply carries the exit code or signal.
5. **Audit**: every call, allowed or refused, is one JSON line appended to
   `/var/log/ulei-priv/audit.jsonl` (`--audit`; directory 0700, file 0600) and the same line on
   stderr for the journal, prefixed `ulei-priv: audit `, keys sorted: RFC 3339 UTC time, peer pid, uid, gid and systemd unit (best effort from
   `/proc/<pid>/cgroup`), operation, command id, the arguments asked for (none for a uid off the
   list and for `busy` past the connection cap, whose request is not read), the decision (`ok` or
   `refused:<reason>`: `unknown_command`, `bad_argument`, `uid_not_allowed`, `busy`, `too_large`,
   `malformed`, `exec_failed`, `audit_failed`), exit code or signal, `timed_out`, duration in ms,
   stdout and stderr byte counts and `truncated`. The audit file is opened before the command runs
   and the line is written before the reply: a file that cannot be opened refuses the call without
   running it, a line that cannot be written withholds the output and refuses with
   `audit_failed`.
6. **Client**: `ulei-priv list` prints the ids and argument choices the daemon serves to this
   caller; `ulei-priv run <id> [<arg>]` prints the child's stdout and stderr and exits with its
   exit code (128 + signal when killed); a refusal is one stderr line `ulei-priv: refused:
   <reason>` and exit 77; a daemon it cannot reach, or one that breaks the protocol, exit 69.
7. **The wire is module-internal.** Both ends are the same binary of the same crate; no other
   module speaks it, and the engine reaches the helper only by running the client as a terminal
   command, as it runs any program. So it is no contract under `contracts/`; it is documented in
   `crates/priv/AGENTS.md`. Should another module speak it, it becomes a contract with its own ADR.
8. **Install.** The binary the root daemon executes is `/usr/local/lib/ulei/ulei-priv`, root 0755,
   never a file in the ulei account's home, which the account could swap; the client is the same
   file. `ci/release.sh` releases it static like the other binaries; `deploy/install.sh` checks its
   sum but does not install it: that is the owner's root step of runbook part 5.2, with the
   example policy `deploy/priv/priv.example.toml`.
9. **Engine side.** The engine needs no drop-in: `/run` is visible read-only under its
   `ProtectSystem=strict`, its `TemporaryFileSystem` covers only `/run/user/<uid>`, and `connect()`
   to a socket on a read-only mount is allowed (write permission on a socket inode is not refused
   with `EROFS`). `RestrictAddressFamilies` keeps `AF_UNIX`.
10. **The example policy** holds `sshd-effective`, `docker-ps`, `ufw-status`, `fail2ban-status`,
    `fail2ban-sshd`, `restart-engine` (`--no-block`: it returns before systemd stops the engine
    that asked), and per kind of unit `status-engine`, `journal-engine` for the engine's system
    unit and `restart-user-unit`, `status-user-unit`, `journal-user-unit` with the three user units
    as the argument: an id has one argv, and system and user units need different ones. Root
    reaches the user units with `systemctl --user -M <user>@`; `journalctl --user-unit` as root
    matches root's own uid and finds nothing on systemd 255, so `journal-user-unit` matches
    `_UID=<uid>` and `_SYSTEMD_USER_UNIT=<unit>`.
11. **Deviation from the reference agent.** The reference agent runs these commands with `sudo`
    from its terminal tool. That cannot work under `NoNewPrivileges`; the engine's `terminal`
    tool is unchanged and the skills call `ulei-priv run <id>` instead of `sudo <command>`.

### Rejected

- **sudoers `NOPASSWD` rules**: unusable under `NoNewPrivileges`, and argument wildcards in
  sudoers are a known way to root.
- **Dropping `NoNewPrivileges`**: re-enables every set-user-id binary for the model, not just the
  few commands.
- **polkit**: needs the system bus, which the engine's sandbox hides, and its rules are
  JavaScript with free arguments.
- **A set-user-id helper**: `no_new_privs` ignores the set-user-id bit, the same as for `sudo`.
- **Root for the engine**: the whole host one prompt injection away.

### Unit hardening

`ulei-priv.service` sets `NoNewPrivileges`, `CapabilityBoundingSet=CAP_NET_ADMIN CAP_NET_RAW`,
`ProtectSystem=strict` with `ReadWritePaths=/var/log/ulei-priv`, `ProtectHome=read-only`,
`PrivateTmp`, `PrivateDevices`, `ProtectKernelTunables`, `ProtectKernelModules`,
`ProtectKernelLogs`, `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`,
`ProtectProc=invisible`, `RestrictRealtime`, `RestrictSUIDSGID`, `RestrictNamespaces`,
`LockPersonality`, `MemoryDenyWriteExecute`, `SystemCallArchitectures=native`,
`SystemCallFilter=@system-service`, `RestrictAddressFamilies=AF_UNIX AF_NETLINK`, `UMask=0077`,
`MemoryMax=256M` and `TasksMax=64`; `systemd-analyze security` rates it 2.0. Every id of the
example policy ran with exit 0 under this set in a transient root unit on Ubuntu 24.04 with
systemd 255, its client in a transient unit of the ulei account with `NoNewPrivileges` and
`ProtectSystem=strict` (the restart ids against a dummy system and user unit). The capability
bounding set also bounds a command the owner adds to the policy later. Not set:

- `ProtectHome=yes`: `docker` reads root's `~/.docker` and warns on every call without it.
- `PrivateNetwork`, `PrivateUsers`: `ufw status` must read the host's netfilter rules over
  netlink, and `systemctl` and `journalctl` must act as the host's root.
- `IPAddressDeny`: no IP address family is allowed in the first place.

## Stage 2, not in this change

Full root only temporarily, by a one-time code the owner gets through the gate (the owner's
channel), so the model never sees a standing root grant. Shape: the owner asks for an elevation
in the owner's channel; the gate issues a single-use code with a short TTL (minutes) to the owner
only; the owner gives it to the assistant; `ulei-priv elevate <code>` checks the code in the
daemon, which grants one time-boxed escalation (for example one command line, or a bounded
session) and audits it like every call, the code's issue and its use included. The model holds
nothing it can reuse. It is a separate step with its own ADR: it touches the gate, the owner's
channel and this daemon, and its threat model (who may issue, replay, the window) is its own.

## Consequences

- The ADR gate (`xtask/src/adr.rs`) checked that ADR numbers are contiguous from 0001. When this
  ADR was written, 0070 to 0072 were held by the open draft pull requests #214 to #216, so it
  takes 0073, and the gate now checks that numbers are unique and start at 0001, a gap allowed.
- `Cargo.lock` gains only the new package.
- Part 5 of the cutover runbook installs the helper and checks it from inside the engine: every
  root command of 5.1 is a helper id or dropped by the owner.
- `ci/budgets.sh` gains a section for `ulei-priv serve`, measured on the first musl build: binary
  581 KiB (limit 768 KiB), VmHWM 640 kB after 200 calls (limit 1024 kB), 0 voluntary context
  switches in 10 s idle (limit 0).
