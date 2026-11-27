# ADR-0105: Temporary full root by a one-time code the owner gets in the owner's chat

Status: proposed
Date: 2026-11-27
Affects: crates/priv, crates/channels, contract:C9, contract:C12, dep:sha2, docs/runbooks/mvp-pilot-cutover.md

## Context

ADR-0073 gave the engine a fixed list of root commands behind the root daemon `ulei-priv serve`
and left stage 2 for its own record: full root only temporarily, by a one-time code the owner gets
through the owner's channel, so that the model never holds a standing root grant. This is that
record. It decides the mechanism and leaves the choices marked in "Open questions for the owner"
to the owner; the implementation is a later change, staged below.

Why it is needed: some of the owner's requests to the assistant need root once and cannot be
foreseen as a fixed policy entry (install a package, read a root-only log, fix a unit file, open
a port). Today the answer is "the owner does it by hand over SSH". The owner wants to be able to
say yes to one such action from the phone, without giving the assistant root for good and
without loosening the engine's sandbox (`NoNewPrivileges`, ADR-0042 section 8, ADR-0069).

### What exists

- The engine runs as the ulei account in a system unit with `NoNewPrivileges=yes`, an empty
  capability bounding set, `ProtectHome=read-only`, no user bus and the secret paths of ADR-0069
  hidden. It reaches `ulei-priv` by running the client as a terminal command.
- `ulei-priv serve` runs as root in a hardened unit (`NoNewPrivileges`, `CapabilityBoundingSet=
  CAP_NET_ADMIN CAP_NET_RAW`, `ProtectSystem=strict`, `RestrictAddressFamilies=AF_UNIX
  AF_NETLINK`). It checks the peer's uid by `SO_PEERCRED`, reads one request line, runs one argv
  of its policy as a child (fixed environment, own process group, capped output, SIGTERM then
  SIGKILL at the timeout) and writes one audit line before every reply (`crates/priv/src/serve.rs`,
  `child.rs`, `audit.rs`). Its children inherit the unit's sandbox: they are *not* full root.
- The owner's channel is `ulei-tg`, a user unit of the same ulei account. It alone polls the bot,
  admits only `telegram.owner_ids` in private chats and, before the ingress, classifies the
  owner's guest-gate traffic and hands it to the gate instead of the model (ADR-0083). Its unit
  is not reachable from the engine's tools (no user bus, `ProtectHome=read-only` keeps its binary
  and unit files unwritable from the engine).
- Everything the owner writes to the assistant goes through `ulei-inbox` and the engine into the
  dialogue archive (`ulei-memory`), the engine's trace (ADR-0092) and, by the night gardener, into
  the garden. What `ulei-tg` answers itself (ADR-0083's gate path) does not.

In this record "the gate" is the owner-command step in `ulei-tg`'s ingress, next to ADR-0083's
classifier, not the separate guest-gate product: that product is class 1 (not edited here), runs
under its own account and does not speak to the root daemon (see Alternatives).

### Threat model

Assets: root on the host; the owner's trust that root runs only what the owner approved.

| # | Attacker or fault | Can | Must not get |
|---|---|---|---|
| T1 | Prompt injection: whoever controls text the model reads (mail, guest chats via the gate, web pages, library and hive files, MCP results) | make the model call any tool with any arguments, ask the owner anything in words | a code, or a grant broader than the one the owner approved |
| T2 | A compromised engine process (code execution as the ulei account inside the engine's sandbox) | everything of T1, plus reading what the engine reads (the code the owner pastes, the archive, `ulei.toml`), raw socket calls to `ulei-priv`, running `restart-user-unit ulei-tg.service` | issuing a code, widening or reusing one, running root after the owner's window |
| T3 | A leaked or replayed code (archive, garden, trace, journal, screen, Telegram history) | present the code again, from any process of the account | a second run, or a run of anything but the approved command |
| T4 | A guest of the gate | send text that the gate previews to the owner and the model may read | issue anything: a guest is never `telegram.owner_ids` in a private chat with the bot |
| T5 | A reader of logs, audit, archive or garden | read everything written | a live code: no code in any log; a code in the archive is spent or expired by then |
| T6 | A race between issue and use, two uses, a revoke during a run | interleave calls | two runs of one code, a run of a revoked code |
| T7 | Clock: wall time stepped, NTP skew, suspend | move `CLOCK_REALTIME` | a longer window than the TTL |
| T8 | Restart of the daemon, of `ulei-tg` or of the engine | lose in-memory state | a code that outlives the process that knew what it was for |
| T9 | Brute force | guess codes over the socket | a hit before the lock-out |

Out of scope, stated as residual risk: root itself; the owner's Telegram account (whoever holds
it is the owner here, as for every other owner command); and **the ulei account outside the
engine's sandbox** (an SSH login of it, a swapped `~/.local/bin/ulei-tg`). That account holds the
bot token and the issuer key below and can forge an issue request: with this record, owning the
whole ulei account is root. The engine, the connectors, the MCP servers and the hive are sandboxed
from it; the "Open questions" list the stronger option (`ulei-tg` under its own account).

## Decision

### 1. The flow

1. The owner writes, in the private chat with the bot, `/root <command line>` (for example
   `/root apt-get install -y jq`). Only this message can start an elevation.
2. `ulei-tg`'s ingress takes it before the inbox (it never reaches the model, the archive or the
   garden) and sends an `issue` request with the command line to the daemon over contract C12.
3. The daemon parses the line into an argv (section 4), resolves `argv[0]`, makes a code, keeps
   only its salted hash, writes the `issue` audit line and answers `ulei-tg` with the code, a
   short public ticket id, the canonical argv and the time left.
4. `ulei-tg` sends the owner one message (not through the inbox): the ticket, the exact argv the
   daemon will run, the expiry in minutes, a warning when `argv[0]` is a shell or interpreter,
   and the code in monospace (tap to copy).
5. The owner pastes the code into the chat with the assistant ("run it: `KX4MP-R7QDA`").
6. The engine runs `ulei-priv elevate KX4MP-R7QDA`. The daemon checks the caller and the code,
   marks the code used, runs the bound argv as full root (section 5), writes the `elevate` audit
   line and returns the output and exit code to the client, as `run` does.
7. The daemon tells `ulei-tg` the outcome on the issue connection; `ulei-tg` edits the code out of
   the owner's message and reports to the owner: ticket, argv, exit code or signal, duration,
   byte counts.

The model never sees a grant before the owner hands one over, and what it is handed runs one
approved argv once.

### 2. Who may issue

Only an owner message through `ulei-tg`; never a tool call, never the engine. In `ulei-tg`, a
`message` (never `edited_message`, never a `callback_query`) whose `from.id` is in
`telegram.owner_ids`, whose chat is private and is the owner's own chat (`chat.id == from.id`),
which is not forwarded (no `forward_origin`) and whose `text` (not a caption) before the first `@`
starts with the command word `/root`. A forwarded guest text that contains `/root …` is an
ordinary message. The classifier runs before ADR-0083's: `/root` is never gate traffic.
`/root cancel` revokes (section 8); `/root` alone answers a one-line usage and the live ticket,
if any.

In the daemon an `issue` or `revoke` request is accepted only when all three hold, else
`refused:not_issuer`:

- the peer's uid (`SO_PEERCRED`) equals the policy's `issuer_uid`;
- the peer is a process of the user unit `issuer_unit` (`ulei-tg.service`): the daemon
  takes a pidfd of the peer pid (`pidfd_open`, which pins it), reads `/proc/<pid>/cgroup`, which
  must end in `user@<uid>.service/app.slice/ulei-tg.service`, and checks the pidfd still running
  afterwards, so a recycled pid cannot pass. The engine is a system unit in `system.slice` and
  `ProtectControlGroups` keeps it from moving a process anywhere;
- the request carries the issuer key whose SHA-256 the policy holds (`issuer_key_sha256`). The
  key is 32 random bytes in a file of the ulei account, mode 0600, named by the C9 key
  `telegram.priv_issuer_key_file` and hidden from the engine by its `InaccessiblePaths`
  (ADR-0069 section 1). The key is the second factor that does not rest on cgroup paths or pid
  checks.

The daemon cannot see the Telegram update; it records the `update_id` and owner id `ulei-tg`
sends, so the audit and `ulei-tg`'s journal line can be matched.

### 3. The code

- **Format**: 10 symbols of Crockford base32 (`0-9A-Z` without `I L O U`), 50 bits from
  `getrandom` (rustix `rand`), shown as `XXXXX-XXXXX`. The daemon accepts it case-insensitively,
  ignores `-` and spaces and maps `O`→`0`, `I`/`L`→`1`, so a hand-typed code works.
- **Public ticket**: a separate random 4-hex-digit id (`t3f9a`) names the grant in messages, the
  audit and the journal; it is not derived from the code.
- **Storage**: in the root daemon only, in memory, as SHA-256 of a random 16-byte salt and the
  normalised code (`sha2`, already a workspace dependency, new for this crate). The plaintext
  leaves the daemon once, in the `issue` reply, and is not kept. The comparison is in constant
  time. Nothing is written to disk: a daemon restart voids every code (T8).
- **TTL**: 300 s by default (`[elevate] ttl_s`, at most 900), counted on `CLOCK_BOOTTIME` in the
  daemon, so a wall-clock step or NTP skew changes nothing and suspend counts (T7). No other clock
  takes part: messages show "valid 5 min", computed from the daemon's `expires_in_s`, never an
  absolute time from Telegram or the phone.
- **Single use**: one state machine per code under the daemon's lock: `live` → `used` (set before
  the child starts, so two racing `elevate` calls give one run and one `refused:used`) → `done`;
  or `live` → `expired` / `revoked`. A spent, expired or revoked code is forgotten after its audit
  line; a later attempt is `refused:bad_code`, like a wrong one.
- **One live code**: a new `issue` revokes the live one (`revoked:new_issue`, the owner is told).
- **Lifetime of the issue connection**: the issue reply leaves `ulei-tg`'s connection open; the
  daemon writes the code's last event on it (`used` with the outcome, `expired`, `revoked:<why>`)
  and closes it. If that connection drops (a `ulei-tg` restart, which the engine can trigger with
  `restart-user-unit`), the code is revoked: a code must not outlive the process that reports
  on it (T8). One thread per live code waits for its event or deadline: one wake-up per issue,
  none when idle.

### 4. Binding: one exact command line (recommended), a session as the option

**Decision, pending the owner (question 1): a code is bound to one exact argv, run once.** The
daemon, not `ulei-tg`, splits the owner's line: words separated by whitespace, `'…'` literal,
`"…"` with `\"` and `\\`, `\` outside quotes escaping the next character. Unquoted shell syntax
(a pipe, `;`, `&`, a redirect, `$`, a backtick, a glob `*` or `?`, `~`, parentheses) is refused
with `refused:shell_syntax`, so the owner never thinks a pipe or a redirect runs when it would
not. An `argv[0]` without `/` is resolved against the child's `PATH`
(`child::ENV`) at issue time and must be an executable regular file; the message shows the
absolute path. At most 64 words and 4 KiB (the request cap). `ulei-priv elevate <code>` takes no
command of its own: what runs is what the daemon showed the owner, so a leaked code (T3) runs at
most that argv once.

**Option (not recommended): a session.** `/root session <minutes>` (at most 30) would issue a code
under which `ulei-priv elevate <code> -- <argv…>` runs any argv until the window closes, each run
audited and reported to the owner live on the issue connection. It saves round trips for a
repair of many steps, but within the window the model chooses every command, so one injection
inside the window is root (T1), and a leaked code is root for the rest of the window (T3).

### 5. The elevated run

The daemon's own children inherit its sandbox, so full root runs in a fresh transient system
unit, started by the daemon's child `/usr/bin/systemd-run` (the daemon already reaches PID 1 as
root over `/run/systemd/private` for the `restart-*` ids):

```text
/usr/bin/systemd-run --system --quiet --wait --pipe --collect --service-type=exec
  --unit=ulei-elevated-<ticket> -p RuntimeMaxSec=<timeout_s> -p BindsTo=ulei-priv.service
  -p After=ulei-priv.service -p WorkingDirectory=/ -E PATH=<child::ENV> -E LANG=C.UTF-8
  -- <argv…>
```

- The argv is the bound one, passed as separate arguments: no shell unless the owner's argv is a
  shell (question 3). stdin is `/dev/null`, no terminal, no environment from any client.
- The transient unit has no sandbox options: root with the full capability set and
  `no_new_privs` off, the "full root" the owner approves.
- `child::run` is reused as is: the output caps (`[elevate] max_output_bytes`, default 256 KiB per
  stream), the process group and the timeout apply to `systemd-run`, which with `--wait --pipe`
  relays the unit's streams and exits with its status. At `[elevate] timeout_s` (default 600, at
  most 3600) `RuntimeMaxSec` stops the unit; the daemon's own SIGTERM/SIGKILL of `systemd-run`
  follows. `BindsTo=` stops the unit when the daemon stops, and `serve` at start stops any
  `ulei-elevated-*` unit left (an audit line `op: orphan_stopped`), so no root command outlives
  the daemon that audits it.
- One elevated run at a time, on a lock of its own: a long elevated command does not make the
  ADR-0073 ids `busy`. The issue connection is counted apart from `MAX_CONNECTIONS` (at most one).
- Only the engine may elevate: `[elevate] user_unit` (`ulei-engine.service`) must be the peer's
  unit by the same pidfd check as section 2, and its uid in `allowed_uids`. A connector or MCP
  server of the account holding a leaked code gets `refused:not_user`.
- The client prints the command's stdout and stderr and exits with its code, exactly as `run`;
  refusals are `ulei-priv: refused: <reason>`, exit 77.

### 6. Audit

Every operation is one line in `/var/log/ulei-priv/audit.jsonl` and the journal, the format of
ADR-0073 section 5, with new `op` values and fields:

- `issue`: `ticket`, `argv`, `ttl_s`, `owner_id`, `update_id`, the peer's unit, `decision`.
- `elevate`: `ticket`, `argv`, `unit` (`ulei-elevated-<ticket>.service`), then as `run`: exit,
  signal, `timed_out`, duration, byte counts, `truncated`.
- `revoke` (with `by`: `owner`, `new_issue`, `failures`, `issuer_gone`), `expire`,
  `orphan_stopped`.
- Refusals: `not_issuer`, `not_user`, `elevate_off`, `shell_syntax`, `not_found` (argv[0]),
  `bad_code`, `used`, `locked`, `rate`, plus ADR-0073's.

**Never logged anywhere: the code, its hash or salt, the issuer key.** `ulei-tg`'s journal line
is the update id, `root`, the ticket and the outcome, as ADR-0083's lines. The output of the
elevated command is not in the audit (byte counts only), as for `run`. Audit before action holds:
an audit file that cannot be opened refuses `issue` and `elevate` (`audit_failed`); for `elevate`
the code is then not consumed.

### 7. What the owner sees

- The issue message: `Root, ticket t3f9a, valid 5 min, one run of:` the argv as a code block,
  `⚠ shell` or `⚠ interpreter` when `argv[0]`'s file name is a shell, `env`, `sudo`, `su` or an
  interpreter (`python*`, `perl`, `ruby`, `node`), then the code.
- On the event, `ulei-tg` edits that message, replacing the code with `used`, `expired` or
  `revoked`, so it does not stay in the owner's Telegram history, and sends one report: ticket,
  argv, `exit N` or the signal, `timed out` if so, duration, stdout and stderr bytes. The output
  itself reaches the owner through the assistant's answer, as for any tool (question 6).
- The audit stays the record: `journalctl -u ulei-priv` or the audit file, as root.

### 8. Revocation and rate limits

- `/root cancel`: revokes the live code; if its command runs, the daemon stops
  `ulei-elevated-<ticket>.service` (the run's audit line says `revoked` and the signal).
- A wrong code (`bad_code`) while a code is live counts; the third revokes it
  (`revoked:failures`) and the owner is told: an attacker has 3 guesses in 2^50 (T9). Ten wrong
  codes in a rolling hour, live code or not, lock `elevate` for an hour (`refused:locked`), and
  the owner is told once.
- At most 10 `issue` per rolling hour (`refused:rate`), so a confused or hijacked `ulei-tg` cannot
  flood the owner.
- No `[elevate]` table in the policy: every new op is `refused:elevate_off`; stage 1 is unchanged.

### 9. Configuration

The root side lives in the policy `/etc/ulei/priv.toml` (root-owned, ADR-0073 section 2), a new
optional table in the same strict TOML subset:

```toml
[elevate]
issuer_uid = <ULEI_UID>
issuer_unit = "ulei-tg.service"
issuer_key_sha256 = "<64 hex digits>"
user_unit = "ulei-engine.service"
ttl_s = 300
timeout_s = 600
max_output_bytes = 262144
```

The ulei side is C9, additive: `[telegram] priv_socket` (default `/run/ulei-priv/priv.sock`) and
`priv_issuer_key_file` (mode 0600, one line of 64 hex digits, read once at start, hidden from
`Debug`). Without `priv_issuer_key_file` `ulei-tg` does not intercept `/root`; the message then
goes to the model as today.

### 10. The wire

ADR-0073 section 7 keeps the `ulei-priv` wire inside the crate until another module speaks it.
`ulei-tg` (crates/channels) now speaks `issue` and `revoke`, so those lines become contract
**C12, "ulei-priv issuer wire"** (JSON Schema, `contracts/c12-priv-issuer.schema.json`, version
0.1.0, draft): the request line `{"op":"issue","key":…,"line":…,"owner_id":…,"update_id":…}` or
`{"op":"revoke","key":…,"ticket":…}`, the reply line (`refused` with a reason, or `ok` with
`ticket`, `code`, `argv`, `expires_in_s`, `warn`) and the one event line. `elevate`, `list` and
`run` stay module-internal: only the `ulei-priv` client speaks them.

### 11. Failure modes

| Fault | Result |
|---|---|
| Daemon down or unreachable | `ulei-tg` answers the owner "root helper not reachable"; nothing issued. `elevate`: exit 69 |
| Audit file not writable | no issue, no run (`audit_failed`); a code is not consumed by a failed audit |
| `sendMessage` of the code fails | `ulei-tg` revokes the ticket at once: a code the owner never saw is not left live |
| `ulei-tg` restarts or stops | issue connection EOF: the code is revoked (`issuer_gone`) |
| Daemon restarts | all codes void (memory only); elevated units stopped by `BindsTo=` and at start |
| `systemd-run` fails to start the unit | `exec_failed`; the code is spent; the owner issues again |
| Command exceeds `timeout_s` | stopped by `RuntimeMaxSec` / SIGTERM, `timed_out: true` in reply, audit and report |
| Engine restarts during a run | the run finishes under the daemon; its output is lost to the model, not to the audit or the owner's report |
| Clock stepped | no effect on the TTL (`CLOCK_BOOTTIME`); only the audit's wall-clock stamps move |

## Alternatives rejected

- **Standing sudo (sudoers `NOPASSWD`)**: impossible under `NoNewPrivileges`, and a standing
  grant is exactly what the owner refuses: one injection away from root at any time.
- **Dropping `NoNewPrivileges` on the engine**: re-enables every set-user-id binary for the model
  and gives no place for the owner's per-action consent.
- **A root shell over the socket** (an interactive or session channel the client keeps open):
  a standing grant for the session, keystrokes the owner never sees, no binding to an approved
  command; the audit could record only bytes.
- **TOTP on the owner's phone**: it has one real merit, it does not trust the ulei account. But a
  TOTP code authorises nothing in particular: the owner cannot see what it will be used for
  except through the bot, which is the ulei account again, so a model holding a valid TOTP runs
  what it chooses. It also depends on two clocks agreeing, is reusable within its step unless
  tracked, and needs a second enrolment and a seed on the host.
- **polkit / `pkexec`**: the engine's sandbox hides the system bus; `pkexec` is set-user-id and
  dies under `no_new_privs`; its rules are JavaScript with free arguments; its agents ask for a
  local password, not the owner's chat.
- **The engine requests, the owner presses "Run" on a button** (no code): considered and kept as
  question 4, not the decision. It puts the model in the initiating role, so the owner meets
  requests the model chose, and a press is given by habit far more easily than a code is copied.
- **The guest-gate product issues the code**: it is a class-1 product outside this repository,
  under its own account, without a path to the root daemon; `ulei-tg` already sees the owner's
  updates first. If that product, or `ulei-tg`, moves to an account of its own, the issuer check
  of section 2 only changes its uid.
- **The code in the dialogue archive redacted by `ulei-memory`**: not needed for safety (a code in
  the archive is spent or dead within minutes and unrelated to the next one) and it would couple
  the archive to this daemon's format.

The reference agent has no counterpart: it ran root through `sudo` (ADR-0073 section 11). This is
an owner directive on top of the port, not a port of reference behaviour.

## Open questions for the owner

1. **Scope: one command or a session?** Recommendation: one exact command line per code (section
   4). A session is root at the model's choice for its window. If repairs of many steps turn out
   common, add sessions later with a short cap (15 min) and a live report per command.
2. **TTL of a code.** Recommendation: 5 min (configurable up to 15). Enough to read the argv,
   copy the code and let the assistant act; short enough that a leaked code is dead before
   anyone reads an archive or a trace. Elevated command timeout: 10 min, up to 60.
3. **Is a shell ever allowed?** Recommendation: never an interactive shell or a terminal; `sh -c
   '<script>'` only as an explicit argv the owner writes, shown verbatim with `⚠ shell`. Refusing
   interpreters outright only pushes the same script into a file the model wrote, which the owner
   then cannot read in the message.
4. **Typed code or a button press?** Recommendation: the typed (copied) code for stage 2, as
   ADR-0073 sketched: the owner starts every elevation, and copying a code is a deliberate act. A
   "Run" button on the issue message (ulei-tg arms the ticket for the engine, no code in the
   dialogue at all) is a small later step if copying proves a burden; an engine-initiated
   request with a button is not recommended (approval by habit).
5. **Issuer trust: the ulei account or an account of its own?** Recommendation: accept for
   stage 2 that the ulei account outside the engine's sandbox can issue (key file hidden from the
   engine plus the unit check), and record it in the runbook; move `ulei-tg` (or only the issuing
   step) to its own system account when the channel is next reworked.
6. **What the owner's report shows.** Recommendation: the summary only (argv, exit, duration,
   bytes). Output goes to the assistant, which answers the owner; root output in Telegram history
   (a `shadow` file, keys) is a leak of its own.
7. **The command word.** Recommendation: `/root`, with `/root cancel`; Telegram shows it in the
   command menu only if registered, and it is not registered, so a guest never sees it.

## Implementation plan (not in this change)

Each stage is one pull request with green CI; none weakens stage 1, and with no `[elevate]` table
the daemon behaves exactly as today.

- **E1, the daemon** (`crates/priv`): `[elevate]` in `policy.rs` with its checks (absolute
  `issuer_unit`/`user_unit` names, hex key hash, caps); `code.rs` (generation, normalisation,
  salted hash, constant-time compare); `elevate.rs` (the state machine, the per-code thread, rate
  limits and lock-out on an injected monotonic clock); the line splitter and `argv[0]` resolution;
  the peer unit check by pidfd; the `systemd-run` path with the runner's path from a hidden
  `serve --elevate-runner` flag for tests. Unit tests: every state transition and refusal, two
  racing `elevate` calls give one run, TTL with a fake clock, the third wrong code revokes, the
  hourly lock, issuer EOF revokes, new issue revokes the old, the splitter's quoting and refusals,
  and **the code and key never appear in the audit file or stderr** (grep in the test).
  Integration test in `crates/priv/tests/` with a fake runner script and fake cgroup readers.
  Dependencies: `sha2` (workspace) and rustix's `rand` feature.
- **E2, the contracts**: C12 schema, fixtures and `index.json` (0.1.0); C9 0.13.0 with the two
  `[telegram]` keys; `cargo xtask gen-contracts`; `docs/ulei.example.toml`.
- **E3, the channel** (`crates/channels`): the `/root` classifier in the ingress before
  ADR-0083's (owner, private, own chat, not forwarded, not edited), the issuer client on its own
  thread (one blocking read per live code, no polling), the owner's messages and edits, `/root
  cancel`, revoke on a failed send. Tests in `tests/tg.rs` with the fake Bot API and a fake C12
  socket: the update never reaches the inbox, a forwarded, edited, group or guest `/root` is
  ordinary, the code is edited out after each event, the journal line holds no code.
- **E4, CI**: `ci/priv_run.py` gains an elevation pass under the runner's sudo: the real daemon as
  root with `[elevate]`, a fake issuer in a transient user unit named `ulei-tg.service`, an
  `elevate` from a transient unit named `ulei-engine.service` running `/usr/bin/id -u` (`0`) and
  `grep CapEff /proc/self/status` (full set), replay `used`, a caller from another unit
  `not_user`, the issuer from another unit `not_issuer`, the daemon stopped mid-run leaving no
  `ulei-elevated-*` unit, and the audit lines. `ci/budgets.sh`: binary still under 768 KiB, VmHWM
  under 1024 kB, 0 wake-ups idle with no live code.
- **E5, deploy and runbook**: the example policy's commented `[elevate]` table; the engine's deny
  list (`10-deny-secrets.conf`, the system unit) gains the issuer key path; runbook part 5.4.

### Runbook part 5.4 (to be written in E5)

"Temporary root by a one-time code", as root and then as the owner in Telegram:

1. Make the key as root: 32 random bytes as 64 hex digits into the file `telegram.
   priv_issuer_key_file` names, owned by `<ULEI_USER>`, 0600; its `sha256sum` into the policy's
   `[elevate]`; add the file to the engine's `InaccessiblePaths`; restart `ulei-priv`, the engine
   and `ulei-tg`.
2. From inside the engine (5.3's way): `cat <key file>` is refused with `Permission denied`; a raw
   `issue` line to the socket (`python3 -c` of a C12 request) gets `refused:not_issuer`.
3. Owner: `/root id -u`. Expected: one message with a ticket, `/usr/bin/id -u`, `valid 5 min`
   and a code; nothing of it in the assistant's dialogue.
4. Owner to the assistant: "run `ulei-priv elevate <code>` and reply with its output only".
   Expected: `0`; the code message edited to `used`; the report `exit 0`.
5. The same code again: `ulei-priv: refused: bad_code`, exit 77.
6. `/root id -u`, then wait 5 min: the message edited to `expired`; the code refused.
7. `/root sleep 600`, use the code, then `/root cancel`: the run is stopped, the report says
   `revoked` with the signal, `systemctl list-units 'ulei-elevated-*'` is empty.
8. `/root ls | head`: `refused: shell_syntax`, no code issued.
9. As root: `tail /var/log/ulei-priv/audit.jsonl`: an `issue`, `elevate`, `expire`, `revoke` line
   for each step above, each with its ticket; `grep -c '<the codes>'` over the audit, the journal
   of `ulei-priv` and `ulei-tg` prints `0`.

Roll back: remove the `[elevate]` table and restart `ulei-priv`; every new op is then
`refused:elevate_off` and stage 1 runs as before; remove the two C9 keys to stop `ulei-tg`
intercepting `/root`.

## Consequences

- Root becomes reachable for the model, one approved argv at a time, only after the owner writes
  `/root …` and hands the code over; nothing the model holds is reusable after one run or 5 min.
- Owning the whole ulei account outside the engine's sandbox becomes root (open question 5).
- `crates/priv` grows a state machine, a splitter and a second execution path; its dependency on
  `sha2` and rustix's `rand` feature are recorded here. `ulei-tg` gains one thread per live code
  and a C12 client; C9 gains two keys; C12 is new.
- The `ulei-priv` unit keeps its hardening: full root runs in transient units the daemon starts
  and stops, never in its own sandbox.
