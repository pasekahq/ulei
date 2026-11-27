# Runbook: MVP pilot, cutover and rollback

A manual procedure, never run in CI and never run by an agent: the owner runs it on the deployment
host. It has seven parts, in this order:

1. Pilot: ulei beside the old runtime, on a test bot and an inbox root of its own, for 7 days.
2. Cutover: the old consumer of the live owner inbox stops, ulei delivers from the live file.
3. Rollback: one command returns the inbox and the owner bot to the old runtime.
4. The scenario checklist, run in the pilot on the test bot and again after the cutover.
5. The root commands of the pilot host through the privileged helper `ulei-priv`.
6. The user namespace check of the pilot host.
7. The hive's reminder tools and its stall watchdog, after the cutover.

Parts 5 and 6 run during the pilot, before the cutover. The archive daemon and the import of the
old runtime's history into it are `docs/runbooks/memory-archive.md`, and the garden's unit,
its embedder and the move from a hand-made garden unit are `docs/runbooks/garden-service.md`,
both run after 1.6 (and after 6.3 on a host of case (b)). Every step names its command, what it
must show, how to tell pass from fail, and how to roll it back. A step that fails stops the run:
record the command, its output and exit code, `ulei status` and `journalctl --user -u <unit> -n 50`
of each ulei unit, then roll back from that step upwards.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_USER>`, `<ULEI_UID>` | The account that owns `<LIVE_ROOT>` and runs `<OLD_UNIT>` as a user unit, and its uid. The ulei units run as this account too: the inbox store and the shadow open an inbox root only as its owner (`docs/runbooks/inbox-shadow.md`). |
| `<HERMES_HOME>/profiles/<PROFILE>` | The reference agent's profile of the owner's assistant. Read only, but for the `ENABLED` file of its inbox root in part 2. |
| `<LIVE_ROOT>` | `<HERMES_HOME>/profiles/<PROFILE>/owner-inbox`, the live inbox root: `events.sqlite3`, `producer.key`, `ENABLED` and `consumer.lock`. |
| `<OLD_UNIT>` | The user unit of the reference agent's gateway for `<PROFILE>`: the old consumer of `<LIVE_ROOT>` and the poller of the live owner bot. |
| `<OWNER_CHAT_ID>` | The owner's Telegram user id, also the id of the owner's direct chat with either bot. |
| `<PILOT_BOT_TOKEN_FILE>` | Token of a test bot made for the pilot, which no other process polls; mode 0600, owned by `<ULEI_USER>`. |
| `<LIVE_BOT_TOKEN_FILE>` | Token of the live owner bot that `<OLD_UNIT>` polls; mode 0600, owned by `<ULEI_USER>`. Read by ulei in case (direct) only. |
| `<LIVE_BOT_API_BASE>` | Case (gate): the gate's loopback Bot API base, the part of a method URL before `<credential>/<method>`; it ends with `/bot`. |
| `<LIVE_BOT_API_FILE_BASE>` | Case (gate): the gate's file base, the part of a file URL before `<credential>/<file path>`; it ends with `/file/bot`. |
| `<LIVE_BOT_CREDENTIAL_FILE>` | Case (gate): the credential the gate gives the old runtime, in a file of mode 0600 owned by `<ULEI_USER>`. It has the shape of a bot token (`digits:chars`) but is not the token. |
| `<GUEST_GATE_TOKEN_FILE>` | A copy of the bearer token the guest gate's `/internal/telegram` checks (the token the old runtime's `guest-gate-ingress` plugin reads), mode 0600, owned by `<ULEI_USER>`, outside the brain directories and outside `<HERMES_HOME>`. Only when a guest gate runs on the host. |
| `<STT_ENV_FILE>` | Env-file with the speech to text key of voice notes (ADR-0035 section 3); mode 0600. |
| `<PILOT_ROOT>` | `<STATE_HOME>/ulei/inbox`, the pilot's inbox root. |
| `<COPY_DIR>` | A fresh directory of `<ULEI_USER>`, outside `<LIVE_ROOT>`, for the shadow's copy. |
| `<HIVE_TASK>` | A small task the owner hands to the hive, in the words used with the old runtime. |
| `<HIVE_CLI>` | The hive's CLI as the deployment's hive runs it: the absolute path of the interpreter that has the hive's packages, a space, the absolute path of its `scripts/hive.py`. |
| `<HIVE_CONFIG>` | The absolute path of the hive's TOML config (its `HIVE_CONFIG`). |
| `<HIVE_ROOT_DIRS>` | The directories the hive CLI writes, space-separated: the directory of the config's `db` (the store and `dispatcher.lock`), `workspaces` and `results_dir`. |
| `<HIVE_DB_DIR>` | The directory of the hive config's `db` alone: the store and `dispatcher.lock`. |
| `<ESCALATIONS_COMMAND>` | The deployment's read-only command that prints its escalations as a JSON list of `{"id": string, "created_at": unix seconds, "read": boolean}`, as a TOML list of strings. |
| `<BRIDGE_UNIT>` | The user unit of the MCP bridge whose journal says `token refresh refused`. |
| `<HIVE_MCP_PORT>` | A free loopback TCP port for `ulei-hive-mcp`. |
| `<EVENT_ID_IDLE>`, `<EVENT_ID_LOAD>`, `<EVENT_ID_HIVE>` | Event ids not used before on the root they are published to, matching `^[a-zA-Z0-9:_-]{1,96}$`; `<EVENT_ID_LOAD>` also with the suffixes `-1` to `-5` of 4.7. |
| `<COMMIT>`, `<ULEI_CHECKOUT>`, `<RELEASE_DIR>`, `<CONFIG_HOME>`, `<STATE_HOME>`, `<RUNTIME_DIR>`, `<QB_USER>`, `<QBD_SOCKET>`, `<QB_GATEWAY_SOCKET>`, `<QB_LOGIN_SUB>`, `<QB_LOGIN_KEY>`, `<ROUTE_MAIN>`, `<ROUTE_KEY>`, `<VENDOR_KEY>`, `<MODEL_KEY>` | As in `docs/runbooks/engine-live-smoke.md`, with `<ULEI_USER>` of this table. |

The live bot is reached in one of two ways; find out which before part 2:

- (direct) ulei polls the live bot with its token, `<LIVE_BOT_TOKEN_FILE>`, at the public Bot API,
  as `<OLD_UNIT>` does.
- (gate) a gate process is the only holder of the live bot's token and the only `getUpdates`
  consumer. `<OLD_UNIT>` reaches the bot through the gate's loopback facade: a method URL is
  `<LIVE_BOT_API_BASE><credential>/<method>`, a file URL
  `<LIVE_BOT_API_FILE_BASE><credential>/<file path>`, with the credential of
  `<LIVE_BOT_CREDENTIAL_FILE>`. ulei goes through the same facade with the same credential. The
  facade refuses a `getUpdates` `timeout` above its own cap with a 409 and caps a download at
  20 MiB. Two clients of its `getUpdates` get no 409 from Telegram: they silently split the
  updates between them, so `ulei-tg` does not exit with a conflict when another process polls.
  The token stays with the gate; ulei never needs it.

Where the engine runs is the other case of the host, (a) or (b) of 6.1: in case (b) the engine is
the system unit `/etc/systemd/system/ulei-engine.service` of 6.3, and the user unit of the same
name is skipped by its condition, so `systemctl --user` neither starts nor stops it. Parts 2 and
3 give the case (b) commands beside the user ones.

Shell helpers, as `<ULEI_USER>`: `J`, `S`, `c1` and `steps` of `engine-live-smoke.md`, and
`publish` of its step 12, defined as written there. ulei has no publisher command; `publish`
signs a C1 packet with the root's `producer.key` and posts it to `inbox.socket`. It signs with
`<PILOT_ROOT>/producer.key` as written; after the cutover, write `<LIVE_ROOT>/producer.key` in its
place. A last-user-message helper for part 4:

```sh
prompt() {
  sqlite3 -readonly "$J" "SELECT substr(message, 1, 600) FROM messages
    WHERE session = (SELECT max(id) FROM sessions WHERE key = '$S')
      AND json_extract(message, '$.role') = 'user' ORDER BY seq DESC LIMIT 1"
}
```

Read the journal only while `ulei status` shows the session `idle`.

## Part 1. Pilot

### 1.1 The host and the old runtime

As `<ULEI_USER>`; nothing here writes:

```sh
id; loginctl show-user <ULEI_USER> -p Linger
systemctl --user show -p LoadState -p ActiveState -p UnitFileState <OLD_UNIT>
stat -c '%a %U %F %n' <LIVE_ROOT> <LIVE_ROOT>/events.sqlite3 <LIVE_ROOT>/producer.key \
  <LIVE_ROOT>/ENABLED
lslocks -o COMMAND,PID,TYPE,PATH | grep -F '<LIVE_ROOT>/consumer.lock'
systemctl --user status <OLD_UNIT> --no-pager | sed -n '/CGroup/,$p'
```

Expected: `Linger=yes`; `LoadState=loaded`, `ActiveState=active`, `UnitFileState=enabled`; the root
`700 <ULEI_USER> directory`, the database and the key `600 <ULEI_USER> regular file`, `ENABLED` a
regular file; one lock line of type `WRITE` whose PID is in the control group of `<OLD_UNIT>`: the
old consumer took `consumer.lock` at its start because `ENABLED` was there (reference R12,
`start`).

Pass: all hold. Fail: another owner or a wider mode (the ulei store refuses a root, key or
database that is not the uid's alone, so the cutover would fail), or no lock line (the old
consumer does not run). Record it and decide before part 2; a mode is the owner's change.

Roll back: none.

### 1.2 qb gateway

`engine-live-smoke.md` steps 1 to 4, as written there, with `<ULEI_USER>` of this runbook. Their
expected results, pass, fail and roll back are the ones given there. Keep the gateway running for
the pilot and the cutover; roll those steps back only when the pilot is abandoned.

Pass: the `routes` call of step 4 answers as `<ULEI_USER>`.

### 1.3 The release of `<COMMIT>`

As `<ULEI_USER>`, from the artifact `ulei-x86_64-unknown-linux-musl` of the CI run of `<COMMIT>`
(README, "Release and install"; ADR-0042 section 10):

```sh
unzip ulei-x86_64-unknown-linux-musl.zip -d <RELEASE_DIR>
(cd <RELEASE_DIR> && sha256sum -c SHA256SUMS)
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
~/.local/bin/ulei --version
```

Expected: `sha256sum -c` prints `OK` for the six binaries and the two files of
`connectors/test/`, and `SHA256SUMS` equals the `SHA256SUMS` notice of the run's `release` job.
The script prints `installed` or `unchanged` for each binary in `$HOME/.local/bin` and each unit in
`<CONFIG_HOME>/systemd/user`, the line `write <CONFIG_HOME>/ulei/ulei.toml before the first start`
and the `systemctl --user` commands; it starts nothing and installs no connector.

Pass: exit 0 and the six binaries in place. Fail: `install.sh: verify: ...` (nothing was copied)
or another `install.sh: <step>: <reason>` line.

Roll back: `rm ~/.local/bin/ulei ~/.local/bin/ulei-inbox ~/.local/bin/ulei-engine
~/.local/bin/ulei-web ~/.local/bin/ulei-tg ~/.local/bin/ulei-connectors
<CONFIG_HOME>/systemd/user/ulei-engine.service <CONFIG_HOME>/systemd/user/ulei-inbox.service
<CONFIG_HOME>/systemd/user/ulei-tg.service <CONFIG_HOME>/systemd/user/ulei-connectors.service`,
`systemctl --user daemon-reload`.

### 1.4 `ulei.toml`, the brain files and the pilot inbox root

As `<ULEI_USER>`. The account must have no ulei deployment yet, since the roll back removes the
two directories whole:

```sh
ls -d <CONFIG_HOME>/ulei <STATE_HOME>/ulei
```

Expected: `No such file or directory` for both. When either exists, stop: it is another
deployment.

```sh
umask 077
mkdir -p <CONFIG_HOME>/ulei <STATE_HOME>/ulei/engine/brain/memories <PILOT_ROOT>
cp <HERMES_HOME>/profiles/<PROFILE>/SOUL.md <STATE_HOME>/ulei/engine/brain/SOUL.md
cp <HERMES_HOME>/profiles/<PROFILE>/memories/MEMORY.md \
  <HERMES_HOME>/profiles/<PROFILE>/memories/USER.md <STATE_HOME>/ulei/engine/brain/memories/
cp -R <HERMES_HOME>/profiles/<PROFILE>/skills <STATE_HOME>/ulei/engine/brain/skills
touch <PILOT_ROOT>/ENABLED
cp <ULEI_CHECKOUT>/docs/ulei.example.toml <CONFIG_HOME>/ulei/ulei.toml
"${EDITOR:-vi}" <CONFIG_HOME>/ulei/ulei.toml
```

The brain files are copies: the pilot's agent edits only them, and nothing is written under
`<HERMES_HOME>`. Leave out a memory file the profile does not have. In `ulei.toml` keep exactly
these keys, every other key of the example removed (ADR-0040 sections 2 and 3; paths in the layout
of ADR-0042 section 3):

```toml
[inbox]
root = "<PILOT_ROOT>"
socket = "<RUNTIME_DIR>/ulei/inbox.sock"
deliver = true

[engine]
journal = "<STATE_HOME>/ulei/engine/journal.sqlite3"
c6_socket = "<RUNTIME_DIR>/ulei/c6.sock"
owner_session = "agent:main:telegram:dm:<OWNER_CHAT_ID>"
default_route = "<ROUTE_MAIN>"
service_unit = "ulei-engine.service"
main_channel = "telegram"
soul_file = "<STATE_HOME>/ulei/engine/brain/SOUL.md"
index_file = "<STATE_HOME>/ulei/engine/brain/INDEX.md"
memory_file = "<STATE_HOME>/ulei/engine/brain/memories/MEMORY.md"
user_file = "<STATE_HOME>/ulei/engine/brain/memories/USER.md"
skills_dir = "<STATE_HOME>/ulei/engine/brain/skills"
terminal_output = "<STATE_HOME>/ulei/engine/terminal"

[engine.compression]
route = "<ROUTE_MAIN>"

[qb]
socket = "<QB_GATEWAY_SOCKET>"

[telegram]
token_file = "<PILOT_BOT_TOKEN_FILE>"
owner_ids = "<OWNER_CHAT_ID>"
attachments_dir = "<STATE_HOME>/ulei/telegram/attachments"
stt_env_file = "<STT_ENV_FILE>"
```

```sh
realpath <PILOT_ROOT> <LIVE_ROOT>
systemctl --user show-environment | grep '^ULEI_'
stat -c '%a %U %n' <CONFIG_HOME>/ulei/ulei.toml <PILOT_BOT_TOKEN_FILE> <STT_ENV_FILE>
```

Expected: two different paths, neither inside the other; `grep` prints nothing; each file `600
<ULEI_USER>`.

Pass: all three hold. Fail: the same root (the pilot would consume the live inbox), a `ULEI_` line
(remove it with `systemctl --user unset-environment` or from `environment.d`), a wider mode.

Roll back: `rm -r <CONFIG_HOME>/ulei <STATE_HOME>/ulei`.

### 1.5 One `ulei-inbox shadow` run on a copy of the live file

As `<ULEI_USER>`. The copy is taken by SQLite's online backup through a read-only connection, so
the old consumer and the producers keep running; nothing under `<LIVE_ROOT>` is written:

```sh
umask 077
install -d -m 0700 <COPY_DIR>/live <COPY_DIR>/state <COPY_DIR>/out
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 ".backup <COPY_DIR>/live/events.sqlite3"
install -m 0600 <LIVE_ROOT>/producer.key <COPY_DIR>/live/producer.key
sqlite3 -readonly <COPY_DIR>/live/events.sqlite3 'SELECT count(*) FROM events'
~/.local/bin/ulei-inbox shadow --live-root <COPY_DIR>/live --state <COPY_DIR>/state \
  --out <COPY_DIR>/out/divergences.jsonl; echo "exit $?"
```

Expected: `ulei-inbox shadow: N rows, 0 divergences` with `N` the count above, and `exit 0`. On an
empty state the pass replays every row through the packet, digest, MAC and store checks; it has
no snapshot to compare transitions with (`inbox-shadow.md`, "Scheduling").

Pass: exit 0 with 0 divergences. Fail: `M divergences` above 0 (read the lines as `inbox-shadow.md`,
"Reading the JSONL", says), exit 1 with `ulei-inbox: shadow: ...`, or exit 75 (the backup caught a
hot journal: take the copy again).

One kind of divergence has a known cause. An operator tool of the reference agent settles rows by
hand, outside the store, and writes the outcome as free text (`reconciled-dismissed`,
`delivered`, `reconciled manually <time>: <text>`, a receipt id). C1 `RowOutcome` does not list
such texts, and they cannot be listed, so the shadow reports each such row as a `row` line
`column outcome is not valid for C1 Row`. Its `reconciled-delivered` is the C1 outcome of
`ulei inbox reconcile` (ADR-0099), which the shadow accepts. When the pass shows `M divergences`, check whether
every one is of that kind on a `done` row:

```sh
jq -c 'select(.check != "row" or .detail != "column outcome is not valid for C1 Row")' \
  <COPY_DIR>/out/divergences.jsonl
wc -l < <COPY_DIR>/out/divergences.jsonl
sqlite3 -readonly <COPY_DIR>/live/events.sqlite3 "SELECT status, count(*) FROM events
  WHERE outcome NOT IN ('success', 'silent', 'uncertain', 'deferred', 'expired-processing',
    'deferred-exhausted', 'reconciled-delivered') GROUP BY status"
```

Expected: `jq` prints nothing; the query prints one line, `done|M`, with `M` the line count. Then
normalize those outcomes on a second, fresh copy and run the pass on it again. Only the copy is
written, never `<LIVE_ROOT>`:

```sh
umask 077
install -d -m 0700 <COPY_DIR>/norm/live <COPY_DIR>/norm/state <COPY_DIR>/norm/out
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 ".backup <COPY_DIR>/norm/live/events.sqlite3"
install -m 0600 <LIVE_ROOT>/producer.key <COPY_DIR>/norm/live/producer.key
sqlite3 <COPY_DIR>/norm/live/events.sqlite3 "UPDATE events SET outcome = 'success'
  WHERE status = 'done' AND outcome NOT IN ('success', 'silent', 'uncertain', 'deferred',
    'expired-processing', 'deferred-exhausted', 'reconciled-delivered'); SELECT changes()"
sqlite3 -readonly <COPY_DIR>/norm/live/events.sqlite3 'SELECT count(*) FROM events'
~/.local/bin/ulei-inbox shadow --live-root <COPY_DIR>/norm/live --state <COPY_DIR>/norm/state \
  --out <COPY_DIR>/norm/out/divergences.jsonl; echo "exit $?"
```

The `UPDATE` leaves every listed C1 outcome as it is, `deferred` of a `done` row included, and
touches only the free texts. `SELECT changes()` prints the number of normalized rows; record it.

Pass: `ulei-inbox shadow: N rows, 0 divergences` with `N` the count above and `exit 0`; the
normalized count is `M`, or more when a row was settled by hand after the first copy. Fail: any
other divergence, in the first pass or in this one, or any such row whose status is not `done`.

The live file needs no such change at the cutover. The ulei store never reads an outcome back:
it opens such a file, `counts` leaves `done` rows out, `recover` and `claim` never select them,
and new events publish, claim and ack as on a clean file (the test
`done_rows_with_free_text_outcomes_are_left_alone` in `crates/core/tests/inbox.rs`).

Roll back: `rm -r <COPY_DIR>`. Remove it after the pass in any case: it holds every payload and a
copy of the producer key.

### 1.6 Start the units

As `<ULEI_USER>`:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service \
  ulei-uncertain.timer
~/.local/bin/ulei status; echo "exit $?"
systemctl --user is-active <OLD_UNIT>
lslocks -o COMMAND,PID,TYPE,PATH | grep -F consumer.lock
```

Then, in Telegram, to the test bot: `Reply with the word: pilot`.

Expected: `ulei status` prints the lines of `engine-live-smoke.md` step 7 and `exit 0`;
`<OLD_UNIT>` is `active`; two lock lines, the old consumer's on `<LIVE_ROOT>/consumer.lock` and
`ulei-inbox`'s on `<PILOT_ROOT>/consumer.lock`; the test bot answers `pilot`, and the live bot
still answers from the old runtime.

Pass: all hold. Fail: as `engine-live-smoke.md` step 7 reads the `down` and `unknown` lines; a
lock line of `ulei-inbox` on `<LIVE_ROOT>` (1.4 is wrong: roll back at once).

Roll back: `systemctl --user disable --now ulei-uncertain.timer ulei-tg.service ulei-inbox.service
ulei-engine.service`.

The garden runs after this step as `ulei-garden.service`, by `docs/runbooks/garden-service.md`.

In case (b) of 6.1 the engine in its user unit is a no-go: after 6.3 the engine runs as the system
unit, the `systemctl --user` commands above skip `ulei-engine.service`, and its start, stop and
log are the root commands of 6.3.

### 1.7 Seven days

The owner uses the test bot beside the live bot for 7 days and runs part 4 on the test bot, and
parts 5 and 6 on this host. Every day, as `<ULEI_USER>`:

```sh
~/.local/bin/ulei status; echo "exit $?"
systemctl --user show -p Id -p NRestarts ulei-engine.service ulei-inbox.service ulei-tg.service
```

Expected: exit 0 and `NRestarts=0` for each unit; a restart is a fatal exit whose one stderr line
is in the unit's journal (ADR-0042 section 7).

Pass: every item of part 4 passes, or each failure is filed into U5 (`docs/roadmap.md`, "Pilot").
Fail: a restart or an item that is neither green nor filed.

Roll back of the whole pilot, in order: 1.6, 1.4, 1.3, then 1.2 when qb is not kept; revoke the
test bot's token with BotFather and remove `<PILOT_BOT_TOKEN_FILE>`. Nothing under `<LIVE_ROOT>`
or `<HERMES_HOME>` was written.

## Part 2. Cutover

At most one consumer runs on `<LIVE_ROOT>` at any moment. The gate and the lock keep it so: the
old consumer and `ulei-inbox serve` each claim only while `<LIVE_ROOT>/ENABLED` is a file, and
each takes the exclusive `consumer.lock` at its start and runs no loop without it (reference R12
`start`; ADR-0036 section 1). The cutover
stops the old consumer before ulei delivers from the file; part 3 reverses it.

Preconditions: 1.7 passed; parts 5 and 6 recorded; the 14-day window of
`docs/runbooks/inbox-shadow.md` passed (ADR-0006); the pilot units run with `ulei status` exit 0;
a quiet hour of the owner. From 2.5 on the way back is part 3.

### 2.1 Save and check

As `<ULEI_USER>`:

```sh
cp -p <CONFIG_HOME>/ulei/ulei.toml <CONFIG_HOME>/ulei/ulei.toml.pilot
stat -c '%a %U %F %n' <LIVE_ROOT> <LIVE_ROOT>/events.sqlite3 <LIVE_ROOT>/producer.key \
  <LIVE_ROOT>/ENABLED <LIVE_BOT_TOKEN_FILE>
systemctl --user show -p LoadState -p ActiveState -p UnitFileState <OLD_UNIT>
```

In case (gate), stat `<LIVE_BOT_CREDENTIAL_FILE>` in place of `<LIVE_BOT_TOKEN_FILE>`, and check
that the facade answers to the credential without printing it:

```sh
curl -sS -o /dev/null -w '%{http_code}\n' \
  "<LIVE_BOT_API_BASE>$(cat <LIVE_BOT_CREDENTIAL_FILE>)/getMe"
```

Expected: as 1.1; the token file, or in case (gate) the credential file, `600 <ULEI_USER> regular
file`; in case (gate) `curl` prints `200`.

Pass: all hold. Fail: any other mode, owner or state, or another HTTP code (the facade is down or
refuses the credential); stop the cutover.

Roll back: `rm <CONFIG_HOME>/ulei/ulei.toml.pilot`.

### 2.2 Stop the pilot

```sh
systemctl --user stop ulei-tg.service ulei-inbox.service ulei-engine.service
systemctl --user show -p Id -p ActiveState -p Result \
  ulei-engine.service ulei-inbox.service ulei-tg.service
```

In case (b) the first line skips the engine, which keeps running; stop it as root:

```sh
systemctl stop ulei-engine.service
systemctl show -p ActiveState ulei-engine.service
```

Expected: each unit `ActiveState=inactive`, `Result=success`; in case (b) the system unit too.
The test bot no longer answers.

Pass: all three inactive. Fail: a unit `failed` or active (its journal holds the cause; the
system engine's is `journalctl -u ulei-engine`, as root or a member of `adm`).

Roll back: `systemctl --user start ulei-engine.service ulei-inbox.service ulei-tg.service`; in
case (b) first, as root, `systemctl start ulei-engine.service`.

### 2.3 Drain the old consumer

```sh
rm <LIVE_ROOT>/ENABLED
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 \
  "SELECT status, count(*) FROM events WHERE status IN ('leased', 'processing') GROUP BY status"
```

Repeat the query every 10 s until it prints nothing. Without `ENABLED` the old consumer makes no
new claim, and a turn in flight runs to its ack (reference R12 `pump_once`); new events stay
`pending`, and the live bot keeps answering the owner.

Expected: nothing printed within the length of the turn in flight plus its 120 s lease.

Pass: no `leased` or `processing` row. Fail: a row that stays `processing` for minutes: a long
turn (wait for it, or stop it with `/stop` in the live bot) or a consumer that is not the old one.

Roll back: `touch <LIVE_ROOT>/ENABLED`: the old consumer reads the file at every pump and claims
again without a restart.

### 2.4 Stop the old runtime

```sh
systemctl --user disable --now <OLD_UNIT>
systemctl --user show -p ActiveState -p UnitFileState <OLD_UNIT>
lslocks -o COMMAND,PID,TYPE,PATH | grep -F '<LIVE_ROOT>/consumer.lock'; echo "exit $?"
```

Expected: `ActiveState=inactive`, `UnitFileState=disabled`; `grep` prints nothing and `exit 1`:
no process holds the live root's lock. The old consumer and the live bot's poller are gone; until
2.6 nothing consumes, events stay `pending`, and the live bot does not answer. In case (gate) the
gate keeps polling Telegram while nothing polls the facade: the owner's messages wait in the gate
and reach ulei after 2.6. They are not lost.

Pass: all hold. Fail: the unit still active, or a lock line (find its process with the PID before
going on).

Roll back: `touch <LIVE_ROOT>/ENABLED`, then `systemctl --user enable --now <OLD_UNIT>`.
`deploy/rollback.sh` does not apply yet: `ulei.toml` still names `<PILOT_ROOT>`.

### 2.5 Point ulei at the live root and the live bot

In case (a) of 6.1 `<LIVE_ROOT>` lies outside the units' writable directories, so `ulei-inbox`
gets it in a drop-in (ADR-0042 section 3):

```sh
mkdir -p <CONFIG_HOME>/systemd/user/ulei-inbox.service.d
printf '[Service]\nReadWritePaths=%s\n' '<LIVE_ROOT>' \
  > <CONFIG_HOME>/systemd/user/ulei-inbox.service.d/live-root.conf
systemctl --user daemon-reload
systemctl --user show -p ReadWritePaths ulei-inbox.service
```

In case (b) **do not install `live-root.conf`**: `ProtectHome=no` of the `90-system-engine.conf`
drop-in of 6.3 already lets `ulei-inbox` write `<LIVE_ROOT>`. A non-empty `ReadWritePaths=` makes
the user manager start the unit in an unprivileged user namespace again, and the AppArmor profile
`unprivileged_userns` then refuses its `connect` to the system engine's `c6.sock` ("Failed name
lookup - disconnected path"): every event stays `pending` while `ulei status` may still read
`inbox: up`. That drop-in empties the list on purpose. If a `live-root.conf` is left from an
earlier run, remove it:

```sh
rm -f <CONFIG_HOME>/systemd/user/ulei-inbox.service.d/live-root.conf
systemctl --user daemon-reload
systemctl --user show -p ReadWritePaths -p ProtectHome ulei-inbox.service
```

Then, in either case:

```sh
"${EDITOR:-vi}" <CONFIG_HOME>/ulei/ulei.toml
diff <CONFIG_HOME>/ulei/ulei.toml.pilot <CONFIG_HOME>/ulei/ulei.toml
```

In the editor set `root = "<LIVE_ROOT>"` under `[inbox]` (`deliver = true` stays), and under
`[telegram]`:

- case (direct): `token_file = "<LIVE_BOT_TOKEN_FILE>"`;
- case (gate): `token_file = "<LIVE_BOT_CREDENTIAL_FILE>"`, `base_url = "<LIVE_BOT_API_BASE>"`
  and `file_base_url = "<LIVE_BOT_API_FILE_BASE>"`. Never the token of the live bot: `ulei-tg`
  would become a second `getUpdates` consumer beside the gate.

If a guest gate runs on the host (the old runtime's `guest-gate-ingress` plugin posts to it), add
under `[telegram]` in both cases (ADR-0083):

```toml
guest_gate_url = "http://127.0.0.1:8792/internal/telegram"
guest_gate_token_file = "<GUEST_GATE_TOKEN_FILE>"
```

Make the token file first, as `<ULEI_USER>`: `install -m 600 /dev/null <GUEST_GATE_TOKEN_FILE>`,
then put the gate's token into it with an editor (never on a command line, so it stays out of the
shell history), and check `stat -c '%a %U %F' <GUEST_GATE_TOKEN_FILE>` prints
`600 <ULEI_USER> regular file`. With these keys `ulei-tg` posts the owner's gate traffic to the
gate before the engine sees it: the gate's buttons (`ap:`, `bi:`, `po:`, `rq:`), `/requests`,
`отменить <id>`, `ответить <id>`, `шли`, `отправь`, `отправить`, `не надо`, and every reply. What
the gate answers `handled` stops there; a reply it does not take goes on to the engine; when the
gate fails, a button or command is dropped and a reply goes on. Without the keys no gate is
called and those messages reach the engine as ordinary text. `journalctl --user -u
ulei-tg.service` shows one `guest gate: update <id> <category>: <outcome>` line per call, never
the text or the token.

`file_base_url` is needed: when `base_url` is set and is not the public one, `ulei-tg` takes
`file_base_url` equal to `base_url`, as a self-hosted Bot API server serves files there, and
raises its download limit from 20 MiB to 2 GiB (`Config::new` in
`crates/channels/src/bot_api/config.rs`; ADR-0017, ADR-0032). The facade serves files under
`<LIVE_BOT_API_FILE_BASE>` only, so without the key every photo, voice note and document fails to
download. A file over 20 MiB is then refused by the facade's cap at the download, not by
`ulei-tg`'s own limit. `telegram.poll_timeout` must stay at or below the facade's cap of the
`getUpdates` `timeout`, or every poll is refused with a 409; leave it unset (10 s).

Expected: in case (a) `ReadWritePaths=` lists `<STATE_HOME>/ulei/inbox`, `<RUNTIME_DIR>/ulei` and
`<LIVE_ROOT>`; in case (b) `ReadWritePaths=` is empty and `ProtectHome=no`, and no
`ulei-inbox.service.d/live-root.conf` exists.
`diff` shows, in case (direct), exactly these two lines changed: `root` and `token_file`; in case
(gate), `root` and `token_file` changed and the two lines `base_url` and `file_base_url` added;
with a guest gate, in either case, also the two lines `guest_gate_url` and
`guest_gate_token_file` added. `ulei-tg` checks them at its start in 2.6: a refused key is one
journal line `ulei-tg: telegram.guest_gate_...` naming the key and the rule, and the unit does not
stay up.

Pass: both hold. Fail: any other change; edit again; in case (b) a `<LIVE_ROOT>` in
`ReadWritePaths=` (remove `live-root.conf` as above).

Roll back: part 3.

### 2.6 Open the gate and start ulei

In case (gate) first make sure ulei will be the only poller of the facade: two pollers are not
reported (see the placeholders). As `<ULEI_USER>`:

```sh
systemctl --user show -p ActiveState <OLD_UNIT>
```

Expected: `ActiveState=inactive`. Then make sure that no other process calls the facade's
`getUpdates` with the credential of `<LIVE_BOT_CREDENTIAL_FILE>`: the gate's own log of its
clients shows no such call for a minute, or the gate's owner confirms it. Fail: the unit active
or another poller; find it and stop it before going on, since ulei would get only part of the
owner's messages.

In case (b), as root, start the system engine first:

```sh
systemctl start ulei-engine.service
systemctl show -p ActiveState ulei-engine.service
```

Expected: `ActiveState=active`. Then, as `<ULEI_USER>`:

```sh
touch <LIVE_ROOT>/ENABLED
systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service \
  ulei-uncertain.timer
~/.local/bin/ulei status; echo "exit $?"
systemctl --user show -p MainPID --value ulei-inbox.service
lslocks -o COMMAND,PID,TYPE,PATH | grep -F '<LIVE_ROOT>/consumer.lock'
journalctl --user -u ulei-inbox -n 20 -o cat
```

`ENABLED` comes first: `ulei-inbox` checks it at its start. `<OLD_UNIT>` is stopped and disabled,
so `ulei-inbox` is the only process that can take the lock. In case (b) the `--user` start skips
`ulei-engine.service` by its condition and exits 0; the engine is the system unit started above,
and its log is `journalctl -u ulei-engine` (as root or a member of `adm`).

Then check that `ulei-inbox` can reach the engine, as `<ULEI_USER>`:

```sh
pid=$(systemctl --user show -p MainPID --value ulei-inbox.service)
readlink /proc/$pid/ns/user /proc/self/ns/user
journalctl -k --since -10min | grep 'apparmor="DENIED"' | grep 'operation="connect"' \
  | grep -F "pid=$pid "
journalctl --user -u ulei-inbox --since -10min -o cat | grep -F 'engine unreachable'
```

In case (b) the two `user:[...]` lines are the same: the unit runs in the shell's user namespace,
not in an unprivileged one (in case (a) they differ, which is right there). Both `grep`s print
nothing: no AppArmor denial of a `connect` by `ulei-inbox` (reading the kernel log may need root
or a member of `adm`), and no `ulei-inbox: engine unreachable: <category>` line, which the loop
writes once when the engine's `is_idle` starts failing (and `engine reachable again` once it
answers). Fail in case (b): another `user:[...]` or a denial; a `ReadWritePaths=` or other
namespace option reached the unit (the `live-root.conf` of 2.5, a missing `90-system-engine.conf`
of 6.3); fix it, `systemctl --user daemon-reload && systemctl --user restart ulei-inbox.service`
and check again.

Expected: `ulei status` prints the lines of `engine-live-smoke.md` step 7 and `exit 0`; one lock
line, `ulei-inbox` with the MainPID above; the journal has no `owner inbox delivery off` line and
no `engine unreachable` line; in case (b) the same `user:[...]` twice and no denial. The
events that waited since 2.3 reach the live chat one by one as quiet messages, and the queue line
reads `pending 0` once they are delivered. In case (gate) the owner's messages that waited in the
gate since 2.4 arrive now. In Telegram, to the live bot, `Reply with the word: cutover` is answered
`cutover`.

Pass: all hold. Fail: `owner inbox delivery off; ENABLED is missing` (the order above),
`owner inbox delivery off; consumer.lock is held by another consumer` (2.4), a `ulei-inbox`
restart naming `Root`, `Key` or `Database` (the modes of 2.1, or in case (a) the drop-in of 2.5),
an `engine unreachable` line or, in case (b), the user namespace check above, a `ulei-tg` exit
naming a conflict (another process polls the live bot), in case (gate) owner messages that
do not arrive or arrive only in part (a second poller of the facade, which nothing reports; the
check above). Any of them: part 3 at once.

The owner session has the key of the pilot's session, so the pilot conversation continues; `/new`
starts a fresh one.

Roll back: part 3.

### 2.7 The first 7 days

Within the first day run part 4 on the live bot, item 4.6 with the hive's real return. The old
runtime stays installed and its files stay in place for 30 days (`docs/roadmap.md`, "Cutover");
never start `<OLD_UNIT>` by hand while the ulei units run: part 3 is the way back. Every day, as
`<ULEI_USER>`:

```sh
~/.local/bin/ulei status; echo "exit $?"
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 "SELECT status, outcome, count(*) FROM events
  WHERE created > strftime('%s', 'now', '-1 day') GROUP BY status, outcome"
```

Expected: exit 0; every event of the day `done` with `success`, `silent` or `deferred`, none
`uncertain`. Older `done` rows that an operator tool settled by hand keep their free-text
outcomes (1.5); the store leaves them alone, so they need no change.

An `uncertain` row whose message a later turn did deliver (the owner saw it in the chat) is
closed by the owner, while `ulei-inbox.service` runs, with the reason on one line:

```sh
~/.local/bin/ulei inbox reconcile <EVENT_ID> --outcome reconciled-delivered \
  --note '<why, e.g. answered in the turn of <time>>'; echo "exit $?"
```

Expected: `reconciled <EVENT_ID>: done, reconciled-delivered (was <reason>), by uid <uid> pid
<pid> at <time>` and `exit 0`; `ulei status` then reads one `uncertain` fewer and one
`reconciled` more, and the uncertain alerter no longer names the event. Exit 1 with `409
conflict: event is <status>, not uncertain` or `404 not_found` changed nothing: check the id.
Exit 2 names the wrong argument. An event that did not reach the chat stays `uncertain`.

Pass: 7 days so. Fail: an `uncertain` row not delivered later, an event that did not reach the
chat, a restart of a unit; record it and decide on part 3.

## Part 3. Rollback

One command, as `<ULEI_USER>`, at any time from step 2.5 on:

```sh
<ULEI_CHECKOUT>/deploy/rollback.sh <OLD_UNIT>; echo "exit $?"
```

It needs no root. Before it changes anything it checks `<CONFIG_HOME>/ulei/ulei.toml`, the
`[inbox] root` it names with its `events.sqlite3`, that it can write both, that `<OLD_UNIT>`
loads, and whether `ulei-hive-mcp.service` is installed. Then, in order:

1. `systemctl --user disable --now ulei-tg.service ulei-inbox.service ulei-engine.service
   ulei-hive-mcp.service`: the owner bot's poller and `ulei-inbox` stop, which releases
   `consumer.lock`, and nothing of ulei serves the hive tools any more. On a host without
   `ulei-hive-mcp.service` (its `LoadState` `not-found`, item 4.6 never set up) the script prints
   `skipped ulei-hive-mcp.service: not installed` and stops and disables the other three.
2. `inbox.deliver = false` in `ulei.toml`, so a ulei unit started by hand serves C1 only.
3. `<LIVE_ROOT>/ENABLED` created when it is missing, since the old consumer checks it at its start.
4. `systemctl --user enable --now <OLD_UNIT>`: its consumer takes the lock at its start, and its
   gateway polls the live bot again.

It deletes nothing: the inbox root, the engine journal, the secrets, the binaries, the units and
the drop-ins stay (in case (a) `live-root.conf` of 2.5; in case (b) there is none). A second run
changes nothing more.

It leaves `ulei-uncertain.timer` enabled, whose alerter would tell the owner every 24 h that the
inbox cannot be read. Right after the script, as `<ULEI_USER>`:
`systemctl --user disable --now ulei-uncertain.timer`.

In case (gate) nothing in the gate changes: the old runtime keeps its own credential and reaches
the facade again once `ulei-tg` has stopped. The script acts on user units only; in case (b) the
system engine keeps running after it, and `ulei status` still reads `engine: up`. Stop it as root
right after the script:

```sh
systemctl disable --now ulei-engine.service
systemctl show -p ActiveState -p UnitFileState ulei-engine.service
```

Expected: `ActiveState=inactive`, `UnitFileState=disabled`. The unit file and its drop-in stay.

Expected output:

```text
stopped and disabled ulei-tg.service ulei-inbox.service ulei-engine.service ulei-hive-mcp.service
set inbox.deliver = false in <CONFIG_HOME>/ulei/ulei.toml
unchanged <LIVE_ROOT>/ENABLED
enabled and started <OLD_UNIT>
exit 0
```

Then:

```sh
systemctl --user show -p Id -p ActiveState -p UnitFileState <OLD_UNIT> ulei-inbox.service
lslocks -o COMMAND,PID,TYPE,PATH | grep -F '<LIVE_ROOT>/consumer.lock'
~/.local/bin/ulei status; echo "exit $?"
```

Expected: `<OLD_UNIT>` `active` and `enabled`, `ulei-inbox.service` `inactive` and `disabled`;
`ulei status` reads `hive-mcp: down ...: connect: Connection refused` when item 4.6 was set up
(`hive-mcp: not configured` when it was not), which never changes its exit status;
within a minute one lock line whose PID is in `<OLD_UNIT>`'s control group; `ulei status` reads
`inbox: down` and `engine: down` and exits 1, in case (b) once the system engine is stopped
(before that, `inbox: down`, `engine: up` and `channel: down: no channel holds inbox_turns`,
exit 1); the live bot answers from the old runtime.

Pass: exit 0 and the old consumer holds the lock. Fail: one stderr line `rollback.sh: <step>:
<reason>`. A `check` failure changed nothing: fix the named cause and run the command again. A
`stop`, `config`, `gate` or `start` failure left the steps before it done: fix the cause and run
the command again. Exit 2 is a usage error.

A turn in flight at the stop leaves its row `processing` until its lease ends, at most 120 s
later; the store's `recover` then marks it `uncertain` and it is not delivered again. Look at such
rows with `sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 "SELECT id, outcome FROM events WHERE
status = 'uncertain'"`.

Roll back of the rollback: the cutover again from 2.3, with `deliver = true` set back in 2.5; in
case (b) without `live-root.conf` (2.5), with the start of the system engine as root at the head
of 2.6 and its user namespace check.

## Part 4. Scenario checklist

Run every item on the test bot in the pilot and on the live bot after the cutover, waiting for
each answer before the next message. Record per item: date, bot, pass or fail, and for a fail the
`ulei status` lines and the journal rows of the turn. The texts the owner sees are the ones of
`ulei-tg` (ADR-0032 to ADR-0035) and of the engine (ADR-0034).

### 4.1 Voice

Send: a voice note of a few seconds saying "Reply with the word: voice".

Must come back: first `🎙️ "<transcript>"` with the words spoken, then the reply `voice` (ADR-0035
section 7).

Fail: no echo and a reply about an audio file: the transcription failed, and the `ulei-tg` journal
holds `ulei-tg: voice transcription failed for <path>: <error>`.

### 4.2 Photo

Send: one photo with the caption `Reply with the file name of the image I sent, nothing else.`

Must come back: one reply with a file name that `ls <STATE_HOME>/ulei/telegram/attachments` lists.
The agent does not describe the picture: the turn carries the note that it cannot view images
(ADR-0033 section 8).

Fail: no reply, a name not in the directory, or a description of the picture.

### 4.3 Forward

Send: forward one text message from another chat, then at once `Who wrote the forwarded message?
One line.`

Must come back: one reply naming the author as Telegram shows them. `prompt` shows the label
`[Переслано от <author>, <DD.MM.YYYY HH:MM>]` above the forwarded text and the question in the
same turn.

Fail: two replies (the two messages were not joined) or another author.

### 4.4 Album

Send: three photos as one album, the caption `How many images did I send? Reply with the number
only.` on the first.

Must come back: one reply, `3`, after one typing period. `prompt` shows three `[The user sent an
image.` notes, then the caption.

Fail: more than one reply or another number.

### 4.5 Long reply

Send: `Write the numbers from 1 to 1500, one per line, and nothing else.`

Must come back: the answer in two or more messages, each ending in ` (k/n)`, together all numbers
from 1 to 1500 in order (Telegram's limit is 4096 characters per message).

Fail: a missing or repeated part, or a send error in the `ulei-tg` journal.

### 4.6 Delegation to the hive and its return

The engine reaches the hive only through `ulei-hive-mcp` and its typed tools: the five of ADR-0071
(`hive_create`, `hive_list`, `hive_show`, `hive_steer`, `hive_cancel`), `hive_resolve` (ADR-0095),
`hive_reminder` (ADR-0101) and the three read-only snapshots of ADR-0086 (`hive_inspect`,
`hive_health`, `hive_file`); it gets no path of the hive.
Set it up once, as `<ULEI_USER>`, before the first run of this item. The token goes into the
engine's state directory, which the engine (and its `terminal`) can read: it opens only the same
typed calls.

The release ships the server (`ulei-hive-mcp` in `ci/release.sh`, its unit in `deploy/install.sh`)
and every tool is on by default: the server has no per-tool switch, and `[engine.mcp.hive]` with
no `tools.include` registers every tool the server lists. An upgrade therefore needs no config change:
the release's `ulei-hive-mcp` and the hive's CLI at the same commit, then `systemctl --user
try-restart ulei-hive-mcp.service`; the engine lists the tools again on its next run. A
`tools.include` that names tools one by one must add the three snapshot tools itself.

```sh
umask 077
head -c 32 /dev/urandom | od -An -tx1 | tr -d ' \n' > <CONFIG_HOME>/ulei/hive-mcp.token
printf 'Authorization=Bearer %s\n' "$(cat <CONFIG_HOME>/ulei/hive-mcp.token)" \
  > <STATE_HOME>/ulei/engine/hive-mcp.env
mkdir -p <CONFIG_HOME>/systemd/user/ulei-hive-mcp.service.d
printf '[Service]\nReadWritePaths=%s\n' '<HIVE_ROOT_DIRS>' \
  > <CONFIG_HOME>/systemd/user/ulei-hive-mcp.service.d/10-hive-root.conf
"${EDITOR:-vi}" <CONFIG_HOME>/ulei/ulei.toml
```

Add to `ulei.toml`, `mcp_servers` under the existing `[engine]`. `hold = true` makes every card the
engine creates wait for the owner's `hive.py release`; the owner decides which mode the pilot and
the cutover run (ADR-0071 section 7):

```toml
[engine]
mcp_servers = "hive"

[engine.mcp.hive]
url = "http://127.0.0.1:<HIVE_MCP_PORT>/mcp"
env_file = "<STATE_HOME>/ulei/engine/hive-mcp.env"
timeout = 150
connect_timeout = 10

[hive_mcp]
listen = "127.0.0.1:<HIVE_MCP_PORT>"
token_file = "<CONFIG_HOME>/ulei/hive-mcp.token"
hive_command = "<HIVE_CLI>"
hive_config = "<HIVE_CONFIG>"
actor = "secretary"
timeout = 120
hold = false
```

`timeout` of the engine's server is above the server's own, so a slow cancel ends with the
server's error, not the engine's. Then:

```sh
stat -c '%a %U %n' <CONFIG_HOME>/ulei/hive-mcp.token <STATE_HOME>/ulei/engine/hive-mcp.env
systemctl --user daemon-reload
systemctl --user enable --now ulei-hive-mcp.service
systemctl --user restart ulei-engine.service   # sudo systemctl restart for the system unit of 6.3
journalctl --user -u ulei-hive-mcp.service -n 5 --no-pager
~/.local/bin/ulei status | grep '^hive-mcp:'
```

Expected: both files `600 <ULEI_USER>`; the journal shows `ulei-hive-mcp: ready:
127.0.0.1:<HIVE_MCP_PORT>`; `ulei status` reads `hive-mcp: up 127.0.0.1:<HIVE_MCP_PORT>` (with
` (engine.mcp.hive.url)` after it when `[hive_mcp]` lives in a config file of its own that the
unit's drop-in names in `ULEI_CONFIG`, and `ulei.toml` has only `[engine.mcp.hive]`); the engine's journal has no `Failed to connect to MCP server 'hive'`
after its next turn.

Roll back: `systemctl --user disable --now ulei-hive-mcp.service`, remove `mcp_servers`, the two
`hive` sections and the three files written above, restart the engine.

Send: `<HIVE_TASK>`.

Must come back: a reply that names the handed-off task as the delegation skill copied in 1.4
reports it. The turn's tool calls are `mcp__hive__hive_create` (and, as the skill does,
`mcp__hive__hive_list`, `mcp__hive__hive_show`, `mcp__hive__hive_steer` or
`mcp__hive__hive_cancel`), and their results, none of them an error:

```sh
sqlite3 -readonly "$J" "SELECT seq, substr(message, 1, 200) FROM messages
  WHERE session = (SELECT max(id) FROM sessions WHERE key = '$S') ORDER BY seq DESC LIMIT 12"
```

Then the return: the hive publishes its result as a `task_result` event to the inbox root its
producer writes, which comes back as in 4.7.

In the pilot the producer still writes to `<LIVE_ROOT>`, and the old runtime delivers the return
in the live chat. Check the pilot's side of the return with a stand-in on the pilot root:

```sh
publish <EVENT_ID_HIVE> 'Hive result for the task handed off: done. Tell the owner in one line.'
```

After the cutover the real return reaches ulei (2.7).

The card is on the hive's board with `created_by` `secretary` (with `hold = true`, it waits in
`created` until `hive.py release`):

```sh
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> list | head -3
journalctl --user -u ulei-hive-mcp.service -n 10 --no-pager
```

Fail: no hand-off; the engine using `terminal` for the hive instead of the `mcp__hive__` tools (the
delegation skill still names the CLI: adapt the copy of 1.4 to the tools); a tool result with
`isError` (the journal of `ulei-hive-mcp` names the tool and the card, the result the hive's
reason); a `Read-only file system` from the hive CLI (`<HIVE_ROOT_DIRS>` misses a directory the
hive writes).

### 4.7 An inbox event, idle and under load

Idle: with `ulei status` showing the session `idle`:

```sh
publish <EVENT_ID_IDLE> 'Check: the nightly build finished green. Tell the owner in one line.'
c1 get '{"id": "<EVENT_ID_IDLE>"}'
```

Must come back: within seconds one message about the build, with sound, replying to nothing, with
no typing before it; the row `done`, `success`, `attempts` 1 (`engine-live-smoke.md` step 12).

Under load: send

```text
Use the terminal tool to run `sleep 60; echo slept`, then reply with its output only.
```

and, while typing shows, publish five events:

```sh
for n in 1 2 3 4 5; do
  publish <EVENT_ID_LOAD>-$n "Check $n of 5: build $n finished green. Tell the owner in one line."
done
~/.local/bin/ulei status
```

Must come back: `queue: pending 5, ...` while the turn runs; `slept` as the reply to the owner's
message, then five event messages, one per event, none before `slept`; each row `done`,
`success`, `attempts` 1, and the queue line all 0 afterwards (`c1 get` per id).

Fail: an event message during the turn, an event twice, `attempts` above 1, or a row not `done` two
minutes after `slept` (`engine-live-smoke.md` step 12 reads the row states).

### 4.8 `/stop`

Send

```text
Use the terminal tool to run `sleep 120; echo late`, then reply with its output only.
```

and, while typing shows, `/stop`; then `/stop` again.

Must come back: `⚡ Остановлено. Вы можете продолжить этот сеанс.` as the reply to the first
`/stop`, `Нет активной задачи для остановки.` to the second, and never `late`. `ulei status` reads
`last turn: <turn> interrupted at <time>`, and `pgrep -f 'sleep 120'` prints nothing: the tool's
process group ended with the turn (ADR-0029).

Fail: `late` arrives, the first answer is the nothing-to-stop text, or the `sleep` outlives the
turn.

### 4.9 Quota exhaustion

As `engine-live-smoke.md` step 10, without the `ulei-engine run` line: the two `http_429` reports
as `<QB_USER>`, then `Reply with the word: pong`. The marks are qbd's, so every client of these two
logins, the old runtime included when it uses them, waits until they end; run it at a quiet time.

Must come back: at once and exactly one message, `quota exhausted on route <ROUTE_MAIN>; nearest
reset YYYY-MM-DD HH:MM:SS UTC`; `ulei status` reads `route <ROUTE_MAIN>: exhausted, resets <time>`.

Fail: a delay of minutes, more than one message, `The request failed: ...`, or `pong`.

### 4.10 `/model`

Send, one at a time: `/model`, `/model <ROUTE_KEY>`, `Reply with the word: switched`, `/model
<ROUTE_MAIN>`.

Must come back: `Текущая: ` and `<ROUTE_MAIN>`; `Модель изменена на ` and `<ROUTE_KEY>` with the
line about the session scope; `switched`, whose step in `steps` is on `<VENDOR_KEY>` and
`<MODEL_KEY>`; the switched text for `<ROUTE_MAIN>`, and `ulei status` reads `route <ROUTE_MAIN>`
in its `session:` line (`engine-live-smoke.md` step 11).

Fail: `Ошибка: ` and an engine error on a valid alias, or `switched` on another vendor.

## Part 5. Root commands on the pilot host

The engine has no sudo: `ulei-engine.service` sets `NoNewPrivileges=yes` (ADR-0042 section 8,
ADR-0069), which every process of its tools inherits, so `sudo` stops with `The "no new
privileges" flag is set` whatever sudoers says, and no sudoers rule helps (ADR-0016,
issue:6c58239734285dfd6357). The few root commands the skills need run through the privileged
helper `ulei-priv` (ADR-0073): a root daemon that runs only the commands of its policy
`/etc/ulei/priv.toml` for `<ULEI_UID>`, over `/run/ulei-priv/priv.sock`, and audits every call.
The engine runs `/usr/local/lib/ulei/ulei-priv run <id> [<arg>]` in place of `sudo <command>`.
The owner makes these steps as root; an agent never installs the helper or edits its policy on a
host, and nobody edits sudoers for ulei.

### 5.1 The commands that need root

As `<ULEI_USER>`:

```sh
grep -rn 'sudo ' <STATE_HOME>/ulei/engine/brain/skills <STATE_HOME>/ulei/engine/brain/memories
```

Add the root commands the owner knows the old runtime runs. Write the list down, one exact
command line each, and map each to an id of `deploy/priv/priv.example.toml` or mark it
"dropped, owner's decision". The known ones:

| Command of the old runtime | Helper id |
|---|---|
| `sudo -n sshd -T` (the audit greps passwordauthentication, permitrootlogin, kbdinteractive, allowusers) | `sshd-effective` (greps stay in the skill: `ulei-priv run sshd-effective \| grep -i ...`) |
| `sudo docker ps --format '{{.Names}} {{.Ports}}'` or `--format '{{.Names}}'` | `docker-ps` (names, status and ports, tab-separated) |
| `sudo -n ufw status` | `ufw-status` (`ufw status verbose`) |
| `fail2ban-client status`, `fail2ban-client status sshd` | `fail2ban-status`, `fail2ban-sshd` |
| `systemctl restart` or `try-restart ulei-engine.service` | `restart-engine` |
| `systemctl is-active ulei-engine.service` | `status-engine` |
| `journalctl -u ulei-engine.service` | `journal-engine` (last 200 lines) |
| `systemctl --user restart` or `try-restart` of `ulei-inbox`, `ulei-tg`, `ulei-connectors` | `restart-user-unit <unit>.service` |
| `systemctl --user is-active <unit>` of those three | `status-user-unit <unit>.service` |
| `journalctl --user -u <unit>` of those three | `journal-user-unit <unit>.service` (last 200 lines) |

A command of the list without an id: the owner either adds a `[[command]]` for it to the policy
(a fixed absolute argv, no shell, nothing that writes files, installs packages or edits sudoers;
ADR-0073 section 2) or drops it. The user units need `<ULEI_USER>`'s user manager running
(lingering on), as for the pilot already.

### 5.2 Install the helper

As root, from `<ULEI_CHECKOUT>` at `<COMMIT>`, with `<RELEASE_DIR>` checked by `sha256sum -c
SHA256SUMS` (1.3):

```sh
install -d -o root -g root -m 0755 /usr/local/lib/ulei /etc/ulei
install -o root -g root -m 0755 <RELEASE_DIR>/ulei-priv /usr/local/lib/ulei/ulei-priv
sed -e 's/<ULEI_USER>/<the account>/g; s/<ULEI_UID>/<its uid>/g' deploy/priv/priv.example.toml \
  > /etc/ulei/priv.toml
chown root:root /etc/ulei/priv.toml && chmod 0644 /etc/ulei/priv.toml
grep -n '<[A-Z_]*>' /etc/ulei/priv.toml   # prints nothing: no placeholder left
install -o root -g root -m 0644 deploy/systemd/system/ulei-priv.service /etc/systemd/system/
systemctl daemon-reload && systemctl enable --now ulei-priv.service
systemctl is-active ulei-priv.service && journalctl -u ulei-priv.service -n 3 --no-pager
```

Expected: `active`, and the journal line `ulei-priv: ready: /run/ulei-priv/priv.sock with 11
commands`. A refused policy exits 2 with one line naming the file and the
line (not root's, writable by group or others, a relative `argv[0]`, a placeholder left) and the
unit does not restart. The binary stays root's in `/usr/local/lib/ulei`: never point the unit or
the engine at a copy in `<ULEI_USER>`'s home, which that account could swap. `deploy/install.sh`
does not install or upgrade it; after a release with a new `ulei-priv`, repeat the second line and
`systemctl restart ulei-priv.service`.

Then as `<ULEI_USER>`:

```sh
/usr/local/lib/ulei/ulei-priv list
```

Expected: one line `ulei-priv run <id> [<arg>]` per call the policy allows, 17 with the example.
As another account it prints `ulei-priv: refused: uid_not_allowed` and exits 77.

### 5.3 Root commands inside the engine

In Telegram, to the test bot (or a C6 turn of a test session, `engine-live-smoke.md` step 14),
one message each:

```text
Use the terminal tool to run `sudo -n true; echo "exit $?"`, then reply with its output only.
Use the terminal tool to run `/usr/local/lib/ulei/ulei-priv run sshd-effective | grep -iE '^(passwordauthentication|permitrootlogin|kbdinteractiveauthentication|allowusers) '; echo "exit ${PIPESTATUS[0]}"`, then reply with its output only.
Use the terminal tool to run `/usr/local/lib/ulei/ulei-priv run docker-ps; echo "exit $?"; docker ps; echo "exit $?"`, then reply with its output only.
Use the terminal tool to run `/usr/local/lib/ulei/ulei-priv run status-user-unit ulei-tg.service; echo "exit $?"`, then reply with its output only.
Use the terminal tool to run `/usr/local/lib/ulei/ulei-priv run reboot; echo "exit $?"; /usr/local/lib/ulei/ulei-priv run restart-user-unit ssh.service; echo "exit $?"`, then reply with its output only.
```

Expected, in order:

1. A `sudo` line naming the "no new privileges" flag and `exit 1`: unchanged, the engine itself
   has no root.
2. The four sshd settings and `exit 0`.
3. The containers' names, status and ports and `exit 0`; then `docker ps` refused with
   `permission denied` on `/var/run/docker.sock` (the engine's sandbox hides it, ADR-0069) and a
   non-zero exit.
4. `active` and `exit 0`.
5. `ulei-priv: refused: unknown_command`, `exit 77`, `ulei-priv: refused: bad_argument`,
   `exit 77`.

Then as root:

```sh
tail -n 5 /var/log/ulei-priv/audit.jsonl
```

Expected: five JSON lines, one per `ulei-priv run` above, each with `uid` `<ULEI_UID>`, `unit`
`ulei-engine.service`, its `id` and `args`, and `decision` `ok` (with `exit`, `duration_ms` and
the byte counts) or `refused:unknown_command` and `refused:bad_argument`. Run the other ids of
5.1 the same way, but not the `restart-*` ones while the owner bot is live: `restart-engine`
restarts the engine that runs the turn.

Pass: every command of 5.1 is either a helper id that passes 5.3 or is dropped by the owner, and
every call of 5.3 has its audit line. Fail: a helper id that does not return its command's output
and exit code, a call without an audit line, or any root command that runs without the helper;
record it and file it into U5 before the cutover (the units' hardening changes only with an
ADR).

Roll back: as root `systemctl disable --now ulei-priv.service`, then `rm
/etc/systemd/system/ulei-priv.service /etc/ulei/priv.toml /usr/local/lib/ulei/ulei-priv` and
`systemctl daemon-reload`; keep `/var/log/ulei-priv/audit.jsonl` as the record. The skills then
have no root command again, as before 5.2.

## Part 6. User namespaces and the engine's secret paths

The engine runs tools for the model as `<ULEI_USER>`, the account that also owns `<LIVE_ROOT>`,
`<LIVE_BOT_TOKEN_FILE>` and `<HERMES_HOME>`. It must not read them (ADR-0069). The units'
`ProtectSystem`, `ProtectHome`, `ReadWritePaths`, `PrivateTmp` and `InaccessiblePaths` apply only
where the user manager may create an unprivileged user namespace; the other hardening always
applies (ADR-0042 section 8). Do this part before 1.6 starts the units for the pilot. In case
(gate) the same holds for `<LIVE_BOT_CREDENTIAL_FILE>`: add it beside `<LIVE_BOT_TOKEN_FILE>` in
6.2, 6.3 and the probe of 6.4.

### 6.1 Which case the host is

As `<ULEI_USER>`, with the units running:

```sh
readlink /proc/self/ns/mnt
for unit in ulei-engine ulei-inbox ulei-tg; do
  pid=$(systemctl --user show -p MainPID --value $unit.service)
  echo "$unit $(readlink /proc/$pid/ns/mnt)"
done
cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns 2>/dev/null
```

- (a) each unit's `mnt:[...]` differs from the shell's: the options apply. Go to 6.2.
- (b) each unit's `mnt:[...]` is the shell's (for example Ubuntu 24.04, where the last line prints
  `1`): the options are configured, as `systemctl --user show` prints, but do not apply, and the
  engine's tools read the whole home. The engine in its user unit is a **no-go** for the pilot; go
  to 6.3.

Agents never change the host's sysctl or AppArmor, and this runbook gives no command for it: that
would change every user unit of every account on the host. The owner alone decides; after such a
change, restart the user manager as root (`systemctl restart user@<ULEI_UID>.service`, which
restarts every user unit of the account, `<OLD_UNIT>` included) and repeat 6.1.

### 6.2 Case (a): the drop-in of the user unit

As `<ULEI_USER>`, from `<ULEI_CHECKOUT>`:

```sh
d=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-engine.service.d
install -d -m 0755 "$d"
install -m 0644 deploy/systemd/ulei-engine.service.d/10-deny-secrets.conf "$d/"
printf 'InaccessiblePaths=-%s\n' '<LIVE_BOT_TOKEN_FILE>' >> "$d/10-deny-secrets.conf"
systemctl --user daemon-reload && systemctl --user restart ulei-engine.service
```

Add every other secret path of the host to that last line the same way (an owner data directory,
other tokens). Then check as in 6.4.

### 6.3 Case (b): the engine as a system unit

As root, from `<ULEI_CHECKOUT>`, with `<ULEI_HOME>` the home of `<ULEI_USER>`:

```sh
sed -e 's/<ULEI_USER>/<the account>/g; s/<ULEI_UID>/<its uid>/g; s#<ULEI_HOME>#<its home>#g' \
  deploy/systemd/system/ulei-engine.service > /etc/systemd/system/ulei-engine.service
grep -n '<' /etc/systemd/system/ulei-engine.service   # prints nothing: no placeholder left
mkdir -p /etc/systemd/system/ulei-engine.service.d
printf '[Service]\nInaccessiblePaths=-%s\n' '<LIVE_BOT_TOKEN_FILE>' \
  > /etc/systemd/system/ulei-engine.service.d/10-host.conf
systemd-analyze verify /etc/systemd/system/ulei-engine.service
```

Add every other secret path of the host to `10-host.conf`. Then, as `<ULEI_USER>`, skip the user
unit of the same name and hand the socket over:

```sh
d=${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/ulei-engine.service.d
install -d -m 0755 "$d"
install -m 0644 deploy/systemd/system/user-engine-off.conf "$d/90-system-unit.conf"
for u in ulei-inbox ulei-tg ulei-memory ulei-garden ulei-night ulei-uncertain; do
  install -d -m 0755 "${d%/*}/$u.service.d"
  install -m 0644 deploy/systemd/system/client-no-userns.conf \
    "${d%/*}/$u.service.d/90-system-engine.conf"
done
systemctl --user daemon-reload
systemctl --user stop ulei-engine.service
```

and as root `systemctl daemon-reload && systemctl enable --now ulei-engine.service`, then as
`<ULEI_USER>` `systemctl --user restart ulei-inbox.service ulei-tg.service` (and
`try-restart ulei-memory.service ulei-garden.service`, which starts nothing while the archive and
the garden are not enabled; `ulei-night.service` and `ulei-uncertain.service` are oneshots their
timers start and need no restart). Without the
clients' drop-in `ulei status` reads `channel: down` and the kernel log shows `apparmor="DENIED"
operation="connect" ... disconnected path` for `c6.sock`. The engine's
log is then `journalctl -u ulei-engine` (as root or a member of `adm`), not `--user`; `ulei
status` reads the same socket. `deploy/install.sh` keeps upgrading the binary; restart the engine
as root after an upgrade.

Roll back: as root `systemctl disable --now ulei-engine.service`, remove the unit and its
`.d` directory, `systemctl daemon-reload`; as `<ULEI_USER>` remove `90-system-unit.conf` and the
six `90-system-engine.conf`, `systemctl --user daemon-reload && systemctl --user start
ulei-engine.service && systemctl --user restart ulei-inbox.service ulei-tg.service`.

### 6.4 Check from inside the engine's unit

Run the probe in the engine's own namespace and cgroup, as the engine's terminal would. Case (a),
as `<ULEI_USER>`: `pid=$(systemctl --user show -p MainPID --value ulei-engine.service)`; case (b):
`pid=$(systemctl show -p MainPID --value ulei-engine.service)`. Then as root:

```sh
for p in <LIVE_BOT_TOKEN_FILE> <LIVE_ROOT>/producer.key <HERMES_HOME> <ULEI_HOME>/.ssh \
    <ULEI_HOME>/.config/qb <RUNTIME_DIR>/bus <RUNTIME_DIR>/systemd/private; do
  nsenter -t "$pid" -m -S <ULEI_UID> -G <ULEI_UID> -- sh -c 'cat -- "$1" >/dev/null 2>&1 \
    || ls -- "$1" >/dev/null 2>&1 && echo "READABLE $1" || echo "denied $1"' _ "$p"
done
```

Pass: every line reads `denied`. Fail: any `READABLE`; stop the engine until it reads `denied`.
`ulei status` exits 0 again, and a turn with a tool (step 14.2 of
`docs/runbooks/engine-live-smoke.md`) still runs.

## Part 7. The hive's reminder tools and its stall watchdog

After 2.6 (ulei delivers from the live root) and 4.6 (the hive tools), as `<ULEI_USER>`, with
the release and the hive at the same commit. The secretary then claims, sends or withdraws a due
reminder or a watchdog alert with `hive_reminder` (ADR-0101), and `ulei-hive-watchdog.timer` checks
the hive every 5 minutes.

### 7.1 The hive publishes over C1

The kinds `reminder_due`, `reminder_fallback` and `watchdog_alert` need the hive's `ulei` delivery
adapter; the old inbox library of `drop` refuses them. In `<HIVE_CONFIG>`, replace `delivery =
"drop"` and set, keeping the other keys:

```toml
delivery = "ulei"
inbox_socket = "<RUNTIME_DIR>/ulei/inbox.sock"        # inbox.socket of ulei.toml
inbox_producer_key_file = "<LIVE_ROOT>/producer.key"

[reminders]
notify_timeout_s = 60   # a hive_reminder send ends within hive_mcp.timeout (120)
```

Then:

```sh
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> health --json >/dev/null; echo "exit $?"
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> health | grep '^outbox'
```

Expected: `exit 0`; after the dispatcher's next tick (it reads the config every tick)
`outbox unpublished: 0`.

Pass: both hold. Fail: `config: inbox_producer_key_file ...` (the key's path or mode) or a growing
unpublished count (`hive.py health --json` names each row's error).

Roll back: `delivery = "drop"` again; the fallback still delivers every reminder.

### 7.2 `[watchdog]` of the hive's config

Append to `<HIVE_CONFIG>` (every other key keeps the default of `hive-py/config.example.toml`):

```toml
[watchdog]
qb_providers = ["<VENDOR_KEY>"]
escalations_command = <ESCALATIONS_COMMAND>
bridge_unit = "<BRIDGE_UNIT>"
```

Then one run by hand:

```sh
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> watchdog; echo "exit $?"
```

Expected: one JSON line with `"errors": []` and `exit 0`; `fired` lists the conditions that hold
now, each a card `watchdog: <condition>` that reaches the secretary as a `watchdog_alert`.

Pass: `exit 0`. Fail: `exit 1` with `hive: watchdog: <condition>: <error>` lines: fix that source
or leave its key out, which turns its condition off.

Roll back: remove `[watchdog]`.

### 7.3 Start the timer

```sh
umask 077
printf 'HIVE_CLI=%s\nHIVE_CONFIG=%s\n' '<HIVE_CLI>' '<HIVE_CONFIG>' \
  > <CONFIG_HOME>/ulei/hive-watchdog.env
mkdir -p <CONFIG_HOME>/systemd/user/ulei-hive-watchdog.service.d
printf '[Service]\nReadWritePaths=%s\n' '<HIVE_DB_DIR>' \
  > <CONFIG_HOME>/systemd/user/ulei-hive-watchdog.service.d/10-hive-root.conf
systemctl --user daemon-reload
systemctl --user start ulei-hive-watchdog.service
systemctl --user show -p Result -p ExecMainStatus ulei-hive-watchdog.service
journalctl --user -u ulei-hive-watchdog.service -n 5 --no-pager -o cat
systemctl --user enable --now ulei-hive-watchdog.timer
systemctl --user list-timers ulei-hive-watchdog.timer --no-pager
```

Expected: `Result=success`, `ExecMainStatus=0`; the journal's last line is the run's JSON summary;
the timer's next start is at most 5 minutes away.

Pass: all hold. Fail: `Result=exit-code` with `hive: watchdog: ...` (7.2), or `Permission denied`
or `readonly database` on the store (the drop-in does not name `<HIVE_DB_DIR>`).

Roll back: `systemctl --user disable --now ulei-hive-watchdog.timer`, remove the env-file and the
drop-in, `systemctl --user daemon-reload`.

### 7.4 A reminder through the secretary

Send: `Remind me in 5 minutes: synthetic check of the reminder path`.

Must come back: a reply that the reminder is set, its turn calling `mcp__hive__hive_create` with
`reminder`; 5 minutes later the text in the owner chat once, and no second message from the
assistant's own reply. Then:

```sh
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> list --creator secretary --limit 3
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_CLI> show --json <CARD> | grep -E '"(state|sent_by)"'   # its card
```

Expected: the card's `reminder_state` reads `"state": "delivered"` and `"sent_by": "secretary"`; the turn of the `reminder_due` event called
`mcp__hive__hive_reminder` (`claim`, then `send`) and ended `NO_REPLY_DONE`.

Pass: one message, sent by `secretary`. Fail: sent by `dispatcher` or `watchdog` (the secretary did
not claim in 3 minutes: check 7.1 and the engine's tools), or two messages (the turn also posted
the text: the policy of ADR-0101 section 2 did not reach the turn).
