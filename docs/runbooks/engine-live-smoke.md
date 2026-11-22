# Runbook: end-to-end live smoke of `ulei-tg`, `ulei-engine` and `qb gateway`

A manual procedure, never run in CI: CI and tests reach only fakes (`crates/engine/AGENTS.md`,
ADR-0042 section 9). The owner runs it on the deployment host. One scenario, in order: qb built
from the pin of `ecosystem.lock`, the ulei release installed with `deploy/install.sh`, one
`ulei.toml`, the three user units, then turns through Telegram on a route with a subscription
login and an API-key login, and inbox events. Every step names its command, what Telegram,
`ulei status`, the engine journal and qb show, how to tell pass from fail, and how to roll the
step back. A step that fails stops the run: record the command, its output and exit code, the
`ulei status` lines, the journal rows of the turn and `journalctl --user -u <unit> -n 50` of each
ulei unit, then roll back from that step upwards.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<ULEI_USER>`, `<ULEI_UID>` | The account that runs the ulei units, and its uid. |
| `<QB_USER>`, `<QB_UID>` | The account that owns the qb credentials and runs `qb gateway`, and its uid. |
| `<QB_GROUP>` | A group of `<QB_USER>` that `<ULEI_USER>` joins to reach the gateway socket. |
| `<QB_SOCKET_DIR>` | A directory for the gateway socket, owned by `<QB_USER>` and `<QB_GROUP>`, mode 2750, outside `<QB_USER>`'s runtime directory (which is 0700) and outside `/tmp` and `/var/tmp`, which PrivateTmp hides from the engine unit. Its setgid bit gives the socket the group `<QB_GROUP>`, which need not be `<QB_USER>`'s primary group. |
| `<QB_GATEWAY_SOCKET>` | `<QB_SOCKET_DIR>/gateway.sock`. |
| `<QBD_SOCKET>`, `<QBD_CONFIG>` | Socket and config file of the system `qbd` (qb's defaults: `docs/config.md` and `README.md` at the pin). |
| `<QB_REPO>`, `<QB_COMMIT>` | `repo` and `commit` of `[products.qb]` in `ecosystem.lock`. |
| `<QB_CHECKOUT>` | An empty directory for the qb build. |
| `<QB_CREDENTIALS_DIR>` | Directory of the gateway's credential files, mode 0700, owned by `<QB_USER>`. |
| `<QB_LOGIN_SUB>` | The qbd and gateway login of the subscription (an OAuth login). |
| `<QB_LOGIN_KEY>` | The qbd and gateway login of the API key. |
| `<VENDOR_SUB>`, `<VENDOR_KEY>` | qbd vendors of these two logins, one each. |
| `<SUB_WINDOWS>` | The quota windows of `<VENDOR_SUB>` in qbd's `[[vendors]]` form. |
| `<MODEL_SUB>`, `<MODEL_KEY>` | Model ids each vendor serves. |
| `<QB_ROLE>` | The qbd role of the gateway's routes. |
| `<ROUTE_MAIN>` | Route alias: the ladder of `<QB_ROLE>`, subscription first, key last. |
| `<ROUTE_KEY>` | Route alias: a pin of `<VENDOR_KEY>` and `<MODEL_KEY>`. |
| `<KEY_ENV_FILE>`, `<KEY_VAR>` | An env-file holding the API key, and the name of its line. |
| `<COMMIT>` | The ulei commit under test. |
| `<ULEI_CHECKOUT>` | A checkout of `<COMMIT>`. |
| `<RELEASE_DIR>` | An empty directory for the release files of `<COMMIT>`. |
| `<CONFIG_HOME>`, `<STATE_HOME>`, `<RUNTIME_DIR>` | `$XDG_CONFIG_HOME` (default `$HOME/.config`), `$XDG_STATE_HOME` (default `$HOME/.local/state`) and `$XDG_RUNTIME_DIR` of `<ULEI_USER>`, written out as absolute paths. |
| `<BOT_TOKEN_FILE>` | File holding the token of a Telegram bot that no other process polls, mode 0600, owned by `<ULEI_USER>`. |
| `<OWNER_CHAT_ID>` | The owner's Telegram user id, which is also the id of the owner's direct chat with the bot. |
| `<HERMES_HOME>/profiles/<PROFILE>` | The reference agent's profile whose `SOUL.md` the smoke copies, read only. |
| `<SMOKE_DIR>` | A fresh directory of `<ULEI_USER>` for the `ulei-engine run` checks. |
| `<BACKUP_DIR>` | A fresh directory of `<ULEI_USER>` for a copy of an earlier ulei deployment (step 6). |
| `<EVENT_ID_IDLE>`, `<EVENT_ID_BUSY>` | Two event ids not used before on this inbox, matching `^[a-zA-Z0-9:_-]{1,96}$`. |
| `<MCP_URL>`, `<MCP_TOOL>` | A loopback `http://` URL of a Streamable HTTP MCP server run for the smoke, and a tool of it that has no side effects. |

Shell helpers, as `<ULEI_USER>`, used from step 7 on:

```sh
J=<STATE_HOME>/ulei/engine/journal.sqlite3
S='agent:main:telegram:dm:<OWNER_CHAT_ID>'
c1() { curl -sS --unix-socket <RUNTIME_DIR>/ulei/inbox.sock -H 'Content-Type: application/json' -d "$2" "http://localhost/v1/$1"; echo; }
steps() {
  sqlite3 -readonly "$J" "SELECT turn, step, json_extract(route, '$.login_label'),
    json_extract(route, '$.vendor'), json_extract(route, '$.model') FROM steps
    WHERE session = (SELECT max(id) FROM sessions WHERE key = '$S') ORDER BY turn DESC, step DESC LIMIT 6"
}
```

Read the units' journal only while `ulei status` shows the session `idle`: a read holds a shared
lock that the engine's next append waits for.

## 1. Accounts, groups and the socket directory

As root:

```sh
id <ULEI_USER>; id <QB_USER>
loginctl show-user <ULEI_USER> -p Linger; loginctl show-user <QB_USER> -p Linger
usermod -aG <QB_GROUP> <ULEI_USER>
install -d -o <QB_USER> -g <QB_GROUP> -m 2750 <QB_SOCKET_DIR>
```

Expected: both accounts exist, both print `Linger=yes`, and `<QB_SOCKET_DIR>` is
`drwxr-s---` of `<QB_USER>:<QB_GROUP>`. `<ULEI_USER>`'s user manager has the new group only when
it starts after the `usermod`: `systemd-run --user --wait --pipe id -G`, run as `<ULEI_USER>`,
must list `<QB_GROUP>`; until it does, restart that manager (`systemctl restart
user@<ULEI_UID>.service`), which restarts every user unit of `<ULEI_USER>`. A directory under
`/run` is gone after a reboot; create it again before step 7.

Pass: the group shows in the manager's list. Fail: it does not after the restart.

Roll back: `gpasswd -d <ULEI_USER> <QB_GROUP>`, `rmdir <QB_SOCKET_DIR>`.

## 2. qb from the pin

As `<QB_USER>`, on a host with Rust, its `x86_64-unknown-linux-musl` target and `musl-tools`,
the build of the CI job `qb-contract` (ADR-0038 section 6):

```sh
grep -E '^(repo|commit) = ' <ULEI_CHECKOUT>/ecosystem.lock
git init -q <QB_CHECKOUT>
git -C <QB_CHECKOUT> fetch -q --depth 1 <QB_REPO> <QB_COMMIT>
git -C <QB_CHECKOUT> checkout -q FETCH_HEAD
cd <QB_CHECKOUT>
RUSTFLAGS= CC_x86_64_unknown_linux_musl=gcc nice \
  cargo build --release --locked -p qb --bin qb --target x86_64-unknown-linux-musl
git rev-parse HEAD
file target/x86_64-unknown-linux-musl/release/qb
install -D -m 0755 target/x86_64-unknown-linux-musl/release/qb ~/.local/bin/qb
~/.local/bin/qb version
```

`qbd` runs the same build. As root, keeping the binary it replaces:

```sh
cp -p /usr/local/bin/qb /usr/local/bin/qb.before-smoke   # only when one is there
install -m 0755 <QB_CHECKOUT>/target/x86_64-unknown-linux-musl/release/qb /usr/local/bin/qb
```

Expected: `git rev-parse HEAD` prints `<QB_COMMIT>`, `file` shows a static binary with no
interpreter, `qb version` prints the version of `[products.qb]`. The C4G that ulei vendors is
qb's `docs/openapi-gateway.json` at this commit byte for byte (ADR-0038 sections 1 and 2):
`sha256sum <QB_CHECKOUT>/docs/openapi-gateway.json` equals the sum ADR-0038 section 1 gives.

Pass: all four hold. Fail: another commit, a dynamic binary, or another gateway contract sum.

Roll back: `rm ~/.local/bin/qb`; as root `mv /usr/local/bin/qb.before-smoke /usr/local/bin/qb`
(or `rm /usr/local/bin/qb` when there was none) and restart `qbd` as in step 3;
`rm -r <QB_CHECKOUT>`.

## 3. qbd: vendors, logins, role and the gateway as a client

When `qbd` is absent, install it as qb's `README.md` at the pin says ("Install as a service",
the group `qb-clients`, `deploy/systemd/qbd.socket` and `qbd.service`), and add `<QB_USER>` to
`qb-clients` (`usermod -aG qb-clients <QB_USER>`, as root). `<QB_USER>`'s user manager, which
runs the gateway of step 4, has the new group only when it starts after the `usermod`:
`systemd-run --user --wait --pipe id -Gn`, run as `<QB_USER>`, must list `qb-clients`; until it
does, restart that manager (`systemctl restart user@<QB_UID>.service`), which restarts every user
unit of `<QB_USER>`. Then, as root, keep the config and add the smoke's items (format: qb
`docs/config.md`, sections `[[vendors]]` to `[[clients]]`):

```sh
cp -p <QBD_CONFIG> <QBD_CONFIG>.before-smoke
```

```toml
[[vendors]]
name = "<VENDOR_SUB>"
windows = <SUB_WINDOWS>

[[vendors]]
name = "<VENDOR_KEY>"
windows = []

[[logins]]
name = "<QB_LOGIN_SUB>"
vendor = "<VENDOR_SUB>"

[[logins]]
name = "<QB_LOGIN_KEY>"
vendor = "<VENDOR_KEY>"

# Subscription first, the key as the last tier.
[[roles]]
name = "<QB_ROLE>"

[[roles.ladder]]
difficulty = 1
candidates = [
  { vendor = "<VENDOR_SUB>", model = "<MODEL_SUB>", tier = 1 },
  { vendor = "<VENDOR_KEY>", model = "<MODEL_KEY>", tier = 2 },
]

[[clients]]
name = "smoke-gateway"
uid = <QB_UID>
role = "<QB_ROLE>"
```

A `uid` is unique among qbd's clients: when `<QB_UID>` is one already, identify the gateway by
bearer instead (qb `docs/runbooks/hermes-secretary-cutover.md`, "qbd, if absent").

```sh
systemctl restart qbd.service
qb --socket <QBD_SOCKET> state          # as <QB_USER>
```

Expected: `qbd` restarts without a config line on stderr (`journalctl -u qbd -n 20`), and
`state` lists `<QB_LOGIN_SUB>` and `<QB_LOGIN_KEY>`. Note `[outcomes] rate_limit_backoff_s` of
`<QBD_CONFIG>` (default 60): step 10 runs within it.

Pass: both logins listed. Fail: exit code 2 of `qbd` with a field path, or a 401 to `state`
(`<QB_USER>` is not a client).

Roll back: `mv <QBD_CONFIG>.before-smoke <QBD_CONFIG>`, `systemctl restart qbd.service`. A
restart replays qbd's journal, so leases and marks of the other logins survive it (qb
`docs/journal.md`).

## 4. qb gateway: credentials, config and unit

As `<QB_USER>`, who must run no `qb-gateway.service` yet (`systemctl --user is-active
qb-gateway.service` prints `inactive`; on a host where it runs one, take another `<QB_USER>`).
The subscription login gets a sign-in of its own, never a credential another client refreshes;
the key comes from its env-file (qb `docs/config.md`, "Signing in" and "Importing credentials").
`~/.config/qb/gateway.toml`:

```toml
socket = "<QB_GATEWAY_SOCKET>"
socket_mode = 0o660
qbd_socket = "<QBD_SOCKET>"

# The engine runs as another uid.
[[clients]]
name = "ulei-engine"
uid = <ULEI_UID>

[[logins]]
name = "<QB_LOGIN_SUB>"
kind = "oauth_anthropic"
credentials = "<QB_CREDENTIALS_DIR>/<QB_LOGIN_SUB>.json"

[[logins]]
name = "<QB_LOGIN_KEY>"
kind = "api_key"
api = "anthropic"
credentials = "<QB_CREDENTIALS_DIR>/<QB_LOGIN_KEY>.json"

[[routes]]
alias = "<ROUTE_MAIN>"
role = "<QB_ROLE>"
difficulty = 1
expected_minutes = 2

[[routes]]
alias = "<ROUTE_KEY>"
role = "<QB_ROLE>"
vendor = "<VENDOR_KEY>"
model = "<MODEL_KEY>"
expected_minutes = 2
```

A ChatGPT subscription is `kind = "chatgpt_oauth"`, and a key of another API family takes its
`api` (qb `docs/config.md`, "Gateway config").

```sh
install -d -m 0700 <QB_CREDENTIALS_DIR>
qb login --config ~/.config/qb/gateway.toml <QB_LOGIN_SUB>
qb login import --config ~/.config/qb/gateway.toml --from env --source <KEY_ENV_FILE> \
  --var <KEY_VAR> <QB_LOGIN_KEY>
install -D -m 0644 <QB_CHECKOUT>/deploy/systemd/qb-gateway.service \
  ~/.config/systemd/user/qb-gateway.service
systemctl --user daemon-reload
systemctl --user enable --now qb-gateway.service
qb --socket <QB_GATEWAY_SOCKET> health
curl -sS --unix-socket <QB_GATEWAY_SOCKET> http://localhost/v1/model/routes; echo
```

Then as `<ULEI_USER>`:

```sh
curl -sS --unix-socket <QB_GATEWAY_SOCKET> http://localhost/v1/model/routes; echo
```

Expected: `qb login` prints an authorize URL and, after the pasted code, one line naming the
file; neither command prints a token. `journalctl --user -u qb-gateway -n 20` holds no
`gateway:` error. `ls -l <QB_GATEWAY_SOCKET>` shows `srw-rw----` of `<QB_USER>:<QB_GROUP>`.
Both `routes` calls answer `{"routes": [...]}` with `<ROUTE_KEY>` `ready` and its vendor and
model, and `<ROUTE_MAIN>` `ready` (C4G `GET /v1/model/routes`, ADR-0038).

Pass: the second `routes` call, as `<ULEI_USER>`, answers. Fail: exit code 2 of the gateway
with a field path; `Permission denied` on the socket (step 1); a 401 `auth` (`<ULEI_UID>` is not
in `[[clients]]`).

Roll back: `systemctl --user disable --now qb-gateway.service`, `rm
~/.config/systemd/user/qb-gateway.service ~/.config/qb/gateway.toml`, `systemctl --user
daemon-reload`, `rm -r <QB_CREDENTIALS_DIR>`. The sign-in of `<QB_LOGIN_SUB>` stays valid at the
vendor until revoked there.

## 5. The ulei release of `<COMMIT>`

As `<ULEI_USER>`, from the artifact `ulei-x86_64-unknown-linux-musl` of the CI run of `<COMMIT>`
(README, "Release and install"; ADR-0042 section 10):

```sh
unzip ulei-x86_64-unknown-linux-musl.zip -d <RELEASE_DIR>
(cd <RELEASE_DIR> && sha256sum -c SHA256SUMS)
<ULEI_CHECKOUT>/deploy/install.sh <RELEASE_DIR>
~/.local/bin/ulei --version
```

Expected: `sha256sum -c` prints `OK` for the five binaries and `SHA256SUMS` equals the
`SHA256SUMS` notice of the run's `release` job. The script prints `installed` or `unchanged`
for each binary in `$HOME/.local/bin` and each unit in `<CONFIG_HOME>/systemd/user`, the line
`write <CONFIG_HOME>/ulei/ulei.toml before the first start` and the `systemctl --user` commands;
it starts nothing.

Pass: exit 0 and the five binaries in place. Fail: `install.sh: verify: ...` (a file does not
match `SHA256SUMS`; nothing was copied) or another `install.sh: <step>: <reason>` line.

Roll back: remove the files the script reported `installed`, or run `deploy/install.sh` of the
release that was in place before; `systemctl --user daemon-reload`.

## 6. `ulei.toml`, the brain files and the inbox root

As `<ULEI_USER>`. The roll back of this step and step 15 remove `<CONFIG_HOME>/ulei` and
`<STATE_HOME>/ulei` whole, so `<ULEI_USER>` must have no ulei deployment there:

```sh
ls -d <CONFIG_HOME>/ulei <STATE_HOME>/ulei
```

Expected: `No such file or directory` for both. When either exists, the account has a ulei
deployment: take another `<ULEI_USER>`, or stop its units (`systemctl --user stop
ulei-tg.service ulei-inbox.service ulei-engine.service`) and keep a copy first, then copy it back
after the roll back:

```sh
install -d -m 0700 <BACKUP_DIR>
cp -a <CONFIG_HOME>/ulei <BACKUP_DIR>/config-ulei
cp -a <STATE_HOME>/ulei <BACKUP_DIR>/state-ulei
```

`ulei.toml` is written from `docs/ulei.example.toml` (ADR-0040 sections 2 and 3). The written
paths follow the layout of ADR-0042 section 3, which the units' `ReadWritePaths` allow; every
other key of the example stays out:

```sh
umask 077
mkdir -p <CONFIG_HOME>/ulei <STATE_HOME>/ulei/engine/brain/skills <STATE_HOME>/ulei/inbox
cp <HERMES_HOME>/profiles/<PROFILE>/SOUL.md <STATE_HOME>/ulei/engine/brain/SOUL.md
touch <STATE_HOME>/ulei/inbox/ENABLED
"${EDITOR:-vi}" <CONFIG_HOME>/ulei/ulei.toml
```

```toml
[inbox]
root = "<STATE_HOME>/ulei/inbox"
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
token_file = "<BOT_TOKEN_FILE>"
owner_ids = "<OWNER_CHAT_ID>"
attachments_dir = "<STATE_HOME>/ulei/telegram/attachments"
```

- `owner_session` is the key `ulei-tg` gives the owner's direct chat (ADR-0033 section 3); with
  another key the engine refuses the channel's `inbox_turns` and no inbox event is delivered
  (ADR-0036 section 5).
- `inbox/ENABLED` exists before `ulei-inbox` starts: the delivery loop checks it at start
  (ADR-0036 section 1).
- The SOUL.md is a copy; nothing is written under `<HERMES_HOME>`. The memory files start
  absent, so the system prompt has no MEMORY and USER PROFILE blocks (ADR-0020).
- No `ULEI_*` setting variable may reach the units: each wins over its key in `ulei-engine`
  (ADR-0040 section 2), and no unit sets one (ADR-0042 section 2).

```sh
systemctl --user show-environment | grep '^ULEI_'
stat -c '%a %U' <CONFIG_HOME>/ulei/ulei.toml <BOT_TOKEN_FILE>
printf %s <RUNTIME_DIR>/ulei/c6.sock | wc -c
```

Expected: `grep` prints nothing, both files are `600 <ULEI_USER>`, the socket path has at most
107 bytes.

Pass: all three hold. Fail: a `ULEI_` line (remove it with `systemctl --user unset-environment`
or from `environment.d`), a wider mode, a longer path.

The `ulei-engine run` lines of steps 10 and 14 use a config and a journal of their own: write
`<SMOKE_DIR>/brain/SOUL.md`, `<SMOKE_DIR>/brain/memories/MEMORY.md` and
`<SMOKE_DIR>/brain/memories/USER.md` for the smoke, then `<SMOKE_DIR>/ulei.toml`:

```toml
[qb]
socket = "<QB_GATEWAY_SOCKET>"

[engine]
journal = "<SMOKE_DIR>/a/journal.sqlite3"
terminal_output = "<SMOKE_DIR>/a/terminal-output"
soul_file = "<SMOKE_DIR>/brain/SOUL.md"
index_file = "<SMOKE_DIR>/brain/INDEX.md"
memory_file = "<SMOKE_DIR>/brain/memories/MEMORY.md"
user_file = "<SMOKE_DIR>/brain/memories/USER.md"
skills_dir = "<SMOKE_DIR>/brain/skills"
```

Roll back: `rm -r <CONFIG_HOME>/ulei <STATE_HOME>/ulei <SMOKE_DIR>`.

## 7. Start the units and `ulei status`

The four namespace options of the units (`ProtectSystem`, `ProtectHome`, `ReadWritePaths`,
`PrivateTmp`) apply only where the user manager may create an unprivileged user namespace; the
other hardening always applies (ADR-0042 section 8). The steps below give the same results
either way. Whether this host lets the manager create one is part 6 of
`docs/runbooks/mvp-pilot-cutover.md`; this runbook does not check it.

As `<ULEI_USER>`:

```sh
systemctl --user daemon-reload
systemctl --user enable --now ulei-engine.service ulei-inbox.service ulei-tg.service
~/.local/bin/ulei status; echo "exit $?"
```

Expected (ADR-0040 section 5, ADR-0042 sections 4 and 7):

```text
inbox: up <RUNTIME_DIR>/ulei/inbox.sock (ulei-inbox <version>)
queue: pending 0, leased 0, processing 0, deferred 0, uncertain 0
engine: up <RUNTIME_DIR>/ulei/c6.sock
channel: up
session: agent:main:telegram:dm:<OWNER_CHAT_ID> idle since <time>, queue 0, route <ROUTE_MAIN>
last turn: none
qb: up <QB_GATEWAY_SOCKET> (<version>)
route <ROUTE_KEY>: ready <VENDOR_KEY>/<MODEL_KEY>
route <ROUTE_MAIN>: ready
exit 0
```

The two `route` lines come in qb's order of the aliases. `journalctl --user -u ulei-inbox -n 20`
has no `owner inbox delivery off` line, and `journalctl --user -u ulei-engine -n 20` holds
`ulei-engine ready <RUNTIME_DIR>/ulei/c6.sock`.

Pass: exit 0 with `channel: up`. Fail: exit 1 with a `down` or `unknown` line; its reason names
the cause (`connect: ...`, `runs as uid ...`, `401 auth` from qb, `no channel holds
inbox_turns` when `owner_session` or `owner_ids` is wrong). A unit that keeps restarting shows
its one stderr line in its journal (ADR-0042 section 7); exit 2 of `ulei status` is a config it
refused, with the line of ADR-0040 section 2.

Roll back: `systemctl --user disable --now ulei-tg.service ulei-inbox.service
ulei-engine.service`.

## 8. A turn with tools on the subscription

In Telegram, from the owner's account, to the smoke bot:

```text
Use the terminal tool to run `uname -s`, then use write_file to save its output to <STATE_HOME>/ulei/engine/smoke/tool-turn.txt, then reply with that output only.
```

Then, as `<ULEI_USER>` once the reply is there, and as `<QB_USER>`:

```sh
~/.local/bin/ulei status
steps
cat <STATE_HOME>/ulei/engine/smoke/tool-turn.txt
qb --socket <QBD_SOCKET> state          # as <QB_USER>
```

Expected:

- Telegram: typing while the turn runs, interim messages without a reply target when the model
  writes text between tool calls, then the reply `Linux` to the owner's message (ADR-0034
  section 2).
- `ulei status`: `session: ... idle since <time>, queue 0, route <ROUTE_MAIN>` and `last turn:
  <turn> completed at <time>`.
- Journal: at least two steps of the turn, each with `<QB_LOGIN_SUB>`, `<VENDOR_SUB>` and
  `<MODEL_SUB>` (C4G `RouteInfo`, ADR-0038); the file holds `Linux`.
- qb: `<QB_LOGIN_SUB>` is `ok` and its windows, where the vendor sent them, show `source`
  `stream` and an `observed_at` of the turn.

Pass: the reply, the file and the subscription login on every step. Fail: a step on
`<QB_LOGIN_KEY>` (the subscription login was not admissible: read its status in `state`), or a
`The request failed: ...` reply naming the C4G class (ADR-0034 section 2).

Roll back: `rm -r <STATE_HOME>/ulei/engine/smoke`.

## 9. Fallback to the key with the subscription login dead

The login is made dead in qbd only: two 401 reports by login, as qb `docs/api.md` describes
`POST /v1/report` at the pin (`http_401` makes a login `auth_stale`, a second one `auth_dead`,
which no pick admits). No credential file is touched. As `<QB_USER>`, first record the
subscription login's windows for the roll back:

```sh
qb --socket <QBD_SOCKET> state          # note name, used and resets_at of each window of <QB_LOGIN_SUB>
qb --socket <QBD_SOCKET> report --login <QB_LOGIN_SUB> --outcome http_401
qb --socket <QBD_SOCKET> report --login <QB_LOGIN_SUB> --outcome http_401
curl -sS --unix-socket <QB_GATEWAY_SOCKET> http://localhost/v1/model/routes; echo
```

Expected: the reports answer `{"accepted": true, "login_status": "auth_stale"}`, then
`"login_status": "auth_dead"`; `routes` still shows `<ROUTE_MAIN>` `ready` (the key login is
`ok`). When `<QBD_CONFIG>` has `[hooks] on_dead`, that hook runs once for `<QB_LOGIN_SUB>`.

In Telegram:

```text
Use the terminal tool to run `date -u +%Y`, then reply with its output only.
```

Then `ulei status` and `steps` as `<ULEI_USER>`.

Expected: the year as the reply, with typing as in step 8; `last turn: <turn> completed`; every
step of the turn on `<QB_LOGIN_KEY>`, `<VENDOR_KEY>`, `<MODEL_KEY>`. qbd no longer admits the
dead login, so the gateway's pick lands on the key's tier; the engine reaches models only
through the gateway and runs an ordinary turn, with no error event (ADR-0009, ADR-0038).

Pass: the key login on every step and no failure text in Telegram. Fail: a step on
`<QB_LOGIN_SUB>` (the reports did not reach the qbd the gateway asks), or `The request failed:
...` naming `no_route` or `auth` (the key login is not admissible either).

Roll back: an `ok` report that carries a declared window clears `auth_dead` (qb `docs/api.md`,
`POST /v1/report`). Send back the values recorded above, one entry per window, leaving out
`resets_at` where `state` showed none:

```sh
curl -sS --unix-socket <QBD_SOCKET> -H 'Content-Type: application/json' \
  -d '{"login": "<QB_LOGIN_SUB>", "outcome": "ok", "windows": [{"name": "<WINDOW>", "utilization": <USED>, "resets_at": <RESETS_AT>}]}' \
  http://localhost/v1/report; echo
qb --socket <QBD_SOCKET> state
```

Expected: `"login_status": "ok"` and `state` shows `<QB_LOGIN_SUB>` `ok`; a `[hooks]
on_recover` runs once. A restart of `qbd` does not clear the mark: it replays it from its
journal.

## 10. Quota exhaustion: one line

Both logins of `<ROUTE_MAIN>` are made to wait with a 429 report each, which marks a login
`rate_limited` for `rate_limit_backoff_s` (step 3); qbd then answers the route's pick with
`wait` and the gateway ends the turn `quota_exhausted` with `resets_at` (qb `docs/api.md`,
"Errors"). Send the Telegram message and run the `ulei-engine run` line within that time. As
`<QB_USER>`:

```sh
qb --socket <QBD_SOCKET> report --login <QB_LOGIN_SUB> --outcome http_429
qb --socket <QBD_SOCKET> report --login <QB_LOGIN_KEY> --outcome http_429
```

In Telegram:

```text
Reply with the word: pong
```

As `<ULEI_USER>`:

```sh
~/.local/bin/ulei status
~/.local/bin/ulei-engine run --config <SMOKE_DIR>/ulei.toml --session quota \
  --route <ROUTE_MAIN> --prompt hi; echo "exit $?"
```

Expected (port map R5 as ADR-0034 section 2 renders it):

- qb: each report answers `"login_status": "rate_limited"`.
- Telegram: within seconds and without retries, exactly one message, the reply to `pong`:
  `quota exhausted on route <ROUTE_MAIN>; nearest reset YYYY-MM-DD HH:MM:SS UTC`.
- `ulei status`: `route <ROUTE_MAIN>: exhausted, resets <time>` and `last turn: <turn>
  quota_exhausted at <time>: quota exhausted on route <ROUTE_MAIN>; nearest reset ...`, the
  reset in both the same instant.
- `ulei-engine run`: stdout empty, stderr the same one line for `<ROUTE_MAIN>`, `exit 3`
  (section "ulei-engine run" below).
- Journal: the turn's prompt is the session's last message; no assistant message follows it.

Pass: one line, at once, naming the route and the reset. Fail: a delay of minutes before it,
more than one message, `The request failed: ...`, or a reply `pong` (a mark ended before the
turn: repeat both reports and send at once).

Roll back: none can lift the marks early ("Marks end by time only", qb `docs/api.md`); wait
until the reset in the line, then `curl -sS --unix-socket <QB_GATEWAY_SOCKET>
http://localhost/v1/model/routes` shows `<ROUTE_MAIN>` `ready` again.

## 11. `/model`: switch the route alias

In Telegram, one message each, waiting for the answer (ADR-0033 section 4, ADR-0028 section 5):

```text
/model
/model <ROUTE_KEY>
Reply with the word: switched
/model nosuch-route
Reply with the word: never
/model <ROUTE_MAIN>
```

Run `ulei status` and `steps` as `<ULEI_USER>` after the third and the sixth message.

Expected:

- `/model`: `Текущая: ` and `<ROUTE_MAIN>` in code.
- `/model <ROUTE_KEY>`: `Модель изменена на ` and `<ROUTE_KEY>` in code, then the line about the
  session scope; `ulei status` reads `route <ROUTE_KEY>` in its `session:` line.
- `switched`: the reply; its step on `<VENDOR_KEY>` and `<MODEL_KEY>`. The running turn keeps
  its route; every later turn reads the stored alias (ADR-0028 section 5).
- `/model nosuch-route`: the switched text with `nosuch-route`: `set_route` checks only the
  alias syntax.
- `never`: `The request failed: ` and a line naming `bad_request`, then `Try again or use
  /reset to start a fresh session.`; `ulei status` reads `last turn: <turn> failed at <time>:
  ...`.
- `/model <ROUTE_MAIN>`: the switched text; `ulei status` reads `route <ROUTE_MAIN>`.

Pass: each answer as above. Fail: `Ошибка: ` and an engine error on a valid alias, a step of
`switched` on `<VENDOR_SUB>`, or a `never` turn that ran.

Roll back: the last message is the roll back; `/new` also returns the key to
`engine.default_route` (ADR-0028 section 5).

## 12. An inbox event while the session is idle

The event is a C1 packet signed with the inbox's `producer.key` (C1, ADR-0007); ulei has no
publisher command, so a stdlib Python signs it and curl publishes it. As `<ULEI_USER>`, with
`ulei status` showing the session `idle` and `channel: up`:

```sh
publish() {
  python3 -I - "$1" "$2" <STATE_HOME>/ulei/inbox/producer.key <<'EOF' |
import hashlib, hmac, json, sys, time
event_id, text, key_path = sys.argv[1:4]
def canonical(value):
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
packet = {"version": 1, "kind": "task_result", "id": event_id, "payload": text,
          "source_ts": int(time.time()), "trust": "untrusted-source-data"}
with open(key_path, "rb") as f:
    packet["mac"] = hmac.new(f.read(), canonical(packet), hashlib.sha256).hexdigest()
sys.stdout.buffer.write(canonical({"packet": packet}))
EOF
  curl -sS --unix-socket <RUNTIME_DIR>/ulei/inbox.sock -H 'Content-Type: application/json' \
    --data-binary @- http://localhost/v1/publish; echo
}
publish <EVENT_ID_IDLE> 'Smoke check: the nightly build finished green. Tell the owner in one line.'
```

After the Telegram message:

```sh
c1 get '{"id": "<EVENT_ID_IDLE>"}'
~/.local/bin/ulei status
sqlite3 -readonly "$J" "SELECT event_id, attempt, delivery FROM deliveries"
```

Expected (ADR-0036 sections 2, 5, 6 and 7):

- `publish` answers `{"id": "<EVENT_ID_IDLE>"}`; the loop claims at once (the publish wakes
  it) and the engine runs the event as a quiet turn of the owner session.
- Telegram: within seconds, one message about the build, sent with sound, replying to nothing,
  with no typing before it.
- `get`: `status` `done`, `outcome` `success`, `attempts` 1, a `session_id`.
- `ulei status`: the queue all 0, `last turn: <turn> completed at <time>`.
- Journal: `<EVENT_ID_IDLE>|1|sent`.

Pass: one message and `success`. Fail, by the row: `pending` with `attempts` 0 (no claim:
`ENABLED` missing or `consumer.lock` held, see the `owner inbox delivery off` line in
`journalctl --user -u ulei-inbox`; or `channel: down`); `uncertain` (the turn gave no answer or
the send failed: `no_answer`, `send_failed`); `done` with `silent`, or `deferred` (the model
answered with a silence marker: republish under a new id with a payload that asks for an
owner-facing line).

Roll back: none; C1 keeps the settled row, and its id cannot be published again with other
content (`conflict`).

## 13. An inbox event published during a turn

In Telegram:

```text
Use the terminal tool to run `sleep 60; echo slept`, then reply with its output only.
```

While typing shows, as `<ULEI_USER>`:

```sh
~/.local/bin/ulei status
publish <EVENT_ID_BUSY> 'Smoke check: the second build finished green. Tell the owner in one line.'
c1 get '{"id": "<EVENT_ID_BUSY>"}'
~/.local/bin/ulei status
```

After both Telegram messages: `c1 get '{"id": "<EVENT_ID_BUSY>"}'` and `ulei status` again.

Expected (ADR-0036 sections 2, 3 and 6):

- While the turn runs: `session: ... running since <time>`; `publish` answers the id; the row
  is `pending` with `attempts` 0 and the queue line reads `pending 1`. The loop asks `is_idle`
  every 2 s and claims nothing while the session runs.
- Telegram: `slept` as the reply to the owner's message, then, within seconds after it, the
  event message as in step 12.
- After it: the row `done`, `outcome` `success`, `attempts` 1; the queue all 0.

Pass: the event message after `slept`, and one claim. Fail: the event message before `slept`
or between parts of the turn, `attempts` above 1, or the row still `pending` a minute after
`slept` (as step 12).

Roll back: none, as step 12.

## 14. Engine checks with `ulei-engine run`

These run the engine's own turns against the same gateway, outside the units, on a journal of
their own; they check what the fakes cannot show of qb's answers. Run them while the session
of the units is idle, in a shell of their own as `<ULEI_USER>`, and close that shell after
them. Every check writes only under `<SMOKE_DIR>`, which step 15 removes; a check that changes
more names its roll back. A result other than the one named is a failure: record the command,
its stdout, stderr and exit code, and the journal rows of the turn.

The config is `<SMOKE_DIR>/ulei.toml` of step 6.

```sh
export ULEI_CONFIG=<SMOKE_DIR>/ulei.toml PATH=$HOME/.local/bin:$PATH
mkdir -p <SMOKE_DIR>/work && cd <SMOKE_DIR>/work
```

### 14.1 Plain turn

```sh
ulei-engine run --session smoke --route <ROUTE_MAIN> --prompt 'Answer with the single word: pong'
echo "exit $?"
```

Expected: stdout `pong` (in any case), `exit 0`, stderr empty. The journal directory
`<SMOKE_DIR>/a` is mode 0700 and `journal.sqlite3` is 0600 (`stat -c %a`). The journal holds
the step `s1-t1-0-<tag>` (12 hex digits) with the route and usage qb reported:

```sh
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 'SELECT request_id, route, usage FROM steps'
```

### 14.2 Tool turn

```sh
ulei-engine run --session smoke --route <ROUTE_MAIN> \
  --prompt 'Use the write_file tool to create smoke.txt containing ok, then reply done'
```

Expected: exit 0, stdout contains `done`, `<SMOKE_DIR>/work/smoke.txt` contains `ok`, and the
journal holds at least two steps of turn 2 (`s1-t2-0`, `s1-t2-1`, ...) and a `tool_result`
message.

### 14.3 Continuation after a restart

Each invocation is a new process, so the next one is the restart.

```sh
ulei-engine run --session smoke --route <ROUTE_MAIN> --prompt 'Which word did you answer first?'
```

Expected: exit 0 and stdout names `pong`: qb received the stored history of turns 1 and 2.
The step id is `s1-t3-0`. Edit `<SMOKE_DIR>/brain/SOUL.md` and run the check again: the session
keeps the system prompt its first turn stored, so the edit does not reach it.

```sh
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 'SELECT id, length(system_prompt) FROM sessions'
```

### 14.4 `--new`

```sh
ulei-engine run --session smoke --route <ROUTE_MAIN> --new --prompt 'Which word did you answer first?'
```

Expected: exit 0; the model says it has no earlier answer. The step id is `s2-t1-0`, session 2
stores a system prompt built from the edited SOUL.md, and session 1 keeps its rows:

```sh
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 'SELECT id, key FROM sessions'
```

### 14.5 Two user messages in a row after an interrupted turn

A turn that ends without an assistant reply leaves its prompt as the last message, so the
next turn sends two user messages in a row. The quota turn of step 10 is one such turn; an
interrupted one is made by cutting the gateway while it streams:

```sh
ulei-engine run --session cut --route <ROUTE_MAIN> --prompt 'Count slowly from 1 to 300, one per line'
# while it runs, as <QB_USER>: systemctl --user restart qb-gateway.service
ulei-engine run --session cut --route <ROUTE_MAIN> --prompt 'Reply with the word: resumed'
ulei-engine run --session quota --route <ROUTE_MAIN> --prompt 'Reply with the word: resumed'
```

Expected: the cut run exits 1 with `ulei-engine: ...` naming the cut or the socket error.
Both later runs exit 0 with `resumed`: qb accepts a history whose last two messages are user
messages. The journal shows them:

```sh
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT seq, turn, json_extract(message, '$.role') FROM messages
   WHERE session = (SELECT max(id) FROM sessions WHERE key = 'cut') ORDER BY seq"
```

The restart also cuts a turn the units run; run this check while the owner session is idle.

### 14.6 Repeated request id numbers

The request id of a step is `s<session>-t<turn>-<step>-<tag>`: the numbers come from journal
state, and the tag is drawn at random for each run of a turn. qb sees the same numbers again in
two cases.

Crash before the append: start a long turn and kill the process with SIGKILL while it
streams (the turn is not appended; Ctrl-C would append it), then run again.

```sh
ulei-engine run --session crash --route <ROUTE_MAIN> --prompt 'Count slowly from 1 to 300, one per line' &
# while it streams
kill -KILL $!
ulei-engine run --session crash --route <ROUTE_MAIN> --prompt 'Reply with the word: again'
```

Two journal files: point a second journal at the same qb.

```sh
ULEI_ENGINE_JOURNAL=<SMOKE_DIR>/b/journal.sqlite3 \
  ulei-engine run --session other --route <ROUTE_MAIN> --prompt 'Reply with the word: second'
```

Expected: both runs exit 0 with their word. The killed turn and the rerun both send
`sNN-tMM-0-<tag>` with the same numbers and different tags (the rerun's id in the journal's
`steps` is the next turn after the last appended one, so the killed request carried its numbers
too), and the run on journal `b` sends `s1-t1-0-<tag>`, the numbers of check 14.1 under another
tag. qb answers each as a new turn: no `bad_request` for an id already running, and no replay
of the earlier answer.

### 14.7 SIGINT during a tool call

```sh
ulei-engine run --session stop --route <ROUTE_MAIN> --prompt 'Run this shell command and report its output: sleep 300; echo done' &
# once `pgrep -f 'sleep 300'` prints a pid
kill -INT $!; wait $!; echo "exit $?"
pgrep -f 'sleep 300' || echo gone
ulei-engine run --session stop --route <ROUTE_MAIN> --prompt 'Did the last command finish? Reply in one line.'
```

Expected: exit 130 with the one stderr line `ulei-engine: the turn was interrupted`, then
`gone`: the tool's process group did not outlive the engine. The journal holds the interrupted
turn's prompt, tool call and `[Command interrupted]` result, and the second run exits 0 with an
answer about the interrupted command: the next turn runs in the same history.

### 14.8 Cached tokens in `usage`

The engine takes `input` + `cache_read` + `cache_write` of a step's `usage` as the size of its
prompt (the compaction threshold, the empty-reply guard), so the three counts must not overlap:
C4G `Usage.input` counts the input tokens not read from the cache. With `engine.prompt_cache`
at its default the requests ask for one-hour breakpoints. Run the two turns within the cache
lifetime, while `<QB_LOGIN_SUB>` is ready, so both run on one login:

```sh
ulei-engine run --session cache --new --route <ROUTE_MAIN> --prompt 'Reply with the word: one'
ulei-engine run --session cache --route <ROUTE_MAIN> --prompt 'Reply with the word: two'
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT request_id, json_extract(usage, '$.input'), json_extract(usage, '$.cache_read'),
     json_extract(usage, '$.cache_write'), json_extract(usage, '$.input')
     + json_extract(usage, '$.cache_read') + json_extract(usage, '$.cache_write')
   FROM steps WHERE session = (SELECT max(id) FROM sessions WHERE key = 'cache') ORDER BY turn"
```

Expected: the second step reads `cache_read` above 0 and an `input` far below its sum, and its
sum is at least the first step's and larger by about the tokens of the first reply and the
second prompt. A second sum below the first, or an `input` that alone is about the whole
prompt while `cache_read` is large, means the counts overlap: a defect against C4G. With
`cache_read` and `cache_write` 0 in both steps nothing was cached and the check is
inconclusive; record that.

### 14.9 Compaction on the compression route

Start a session and read the prompt size of its step, the last column:

```sh
ulei-engine run --session compact --new --route <ROUTE_MAIN> --prompt 'Reply with the word: start'
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT request_id, json_extract(usage, '$.input') + json_extract(usage, '$.cache_read')
     + json_extract(usage, '$.cache_write')
   FROM steps WHERE session = (SELECT max(id) FROM sessions WHERE key = 'compact') ORDER BY turn"
```

Add to `<SMOKE_DIR>/ulei.toml`, with `<threshold>` that size plus 2000 and the aliases written
out:

```toml
[engine.compression]
route = "<ROUTE_KEY>"
protect_last_n = 3
threshold_tokens = <threshold>

[engine.routes.<ROUTE_MAIN>]
context_length = 200000
```

With these settings the threshold is `<threshold>`: the share of the window,
(200000 - 16384) * 0.75 = 137712 tokens, is above it. Then run four filler turns of 1000
tokens or more each (every number is at least one token) and a last turn:

```sh
for n in 1 2 3 4; do
  ulei-engine run --session compact --route <ROUTE_MAIN> \
    --prompt "Filler $n: $(seq -s ' ' 1 1000). Reply with the word: noted"
done
ulei-engine run --session compact --route <ROUTE_MAIN> --prompt 'Reply with the word: done'
```

Expected: every run exits 0 and the last one prints `done`. A turn compacts before its first
step when the last step's prompt size reached `<threshold>`; a first compaction also needs
more than 7 messages, the 3 kept at the head, at least 3 kept at the tail and one between.

- The step of the third or the fourth turn reaches `<threshold>`. The fourth turn starts with 7
  messages and does not compact.
- The fifth turn (filler 4) compacts: it keeps `start`, its reply and filler 1 at the head,
  at least filler 3, its reply and filler 4 at the tail, and replaces the messages between
  with one summary message. Its step's prompt size stays above `<threshold>`, since the kept
  head and tail alone hold three fillers.
- So the last turn compacts again. A summary now opens the history, so nothing is kept at the
  head: the earlier summary, `start` and the messages up to the last 3 or more go into one new
  summary, which updates the earlier one. That summary may open the first kept message rather
  than stand in a message of its own.

```sh
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT seq, turn, active, compacted, instr(message, '[CONTEXT COMPACTION') > 0,
     instr(message, 'deterministic fallback') > 0
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'compact') ORDER BY seq"
```

Each compaction archives the live rows (`active` 0, `compacted` 1), and the rows a turn
writes live carry that turn's number. After the last turn:

- one archived row of turn 5 reads 1 in the fifth column: the first summary;
- every live row (`active` 1, `compacted` 0) is of turn 6, exactly one of them reads 1 in the
  fifth column, and the last one is the reply `done`;
- both summary rows read 0 in the sixth column: the summary turns answered on `<ROUTE_KEY>`.

In qb, each summary turn picks with its own `run_id` `compact-<16 hex>` (it has no
`session_key`), while the session's steps pick with `run_id` `compact`. A sixth column of 1
means the summary turn failed and the engine wrote the deterministic fallback summary, for
example because qb refused the alias with `bad_request`; a summary turn that failed on `auth`,
`quota_exhausted`, its timeout or the connection leaves the session uncompacted instead.

Roll back: remove the two added sections.

### 14.10 A skill

Write `<SMOKE_DIR>/brain/skills/smoke/smoke-skill/SKILL.md`:

```markdown
---
name: smoke-skill
description: Holds the code word of the live smoke.
---

# Smoke skill

The code word is amber-lantern-42.
```

```sh
ulei-engine run --session skill --new --route <ROUTE_MAIN> \
  --prompt 'Load the skill smoke-skill with skill_view and reply with its code word only'
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT instr(system_prompt, 'smoke-skill') > 0 FROM sessions WHERE key = 'skill'"
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT seq, json_extract(message, '$.role'), json_extract(message, '$.content[0].type'),
     json_extract(message, '$.content[0].name'), json_extract(message, '$.content[0].input.name')
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'skill') ORDER BY seq"
```

Expected: exit 0 and stdout `amber-lantern-42`; the session's system prompt indexes the skill
(1); the journal holds a `tool_use` of `skill_view` with the name `smoke-skill`, then its
`tool_result`. With `ULEI_ENGINE_DISABLED_SKILLS=smoke-skill` and `--new` the new session's
system prompt leaves the skill out, a `skill_view` call of it gets an error result naming it
disabled, and no code word is printed.

### 14.11 An MCP tool

Run a Streamable HTTP MCP server on `<MCP_URL>`. Add to the `[engine]` section of
`<SMOKE_DIR>/ulei.toml` `mcp_servers = "smoke"`, and append:

```toml
[engine.mcp.smoke]
url = "<MCP_URL>"
```

When the server needs request headers, put them into an env-file of mode 0600 with
`env_file = "<SMOKE_DIR>/mcp-smoke.env"` in that section; use a synthetic value such as
`Authorization=Bearer smoke-token-0001`.

```sh
ulei-engine run --session mcp --new --route <ROUTE_MAIN> \
  --prompt 'Call the tool mcp__smoke__<MCP_TOOL> once and report its result in one line'
echo "exit $?"
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 \
  "SELECT seq, json_extract(message, '$.content[0].type'), json_extract(message, '$.content[0].name'),
     substr(json_extract(message, '$.content[0].content'), 1, 80)
   FROM messages WHERE session = (SELECT max(id) FROM sessions WHERE key = 'mcp') ORDER BY seq"
sqlite3 <SMOKE_DIR>/a/journal.sqlite3 .dump | grep -c smoke-token-0001
```

Expected: exit 0, stderr empty, stdout reports the tool's answer; the journal holds a
`tool_use` of `mcp__smoke__<MCP_TOOL>` (every character outside `[A-Za-z0-9_]` of the tool name
as `_`) and its `tool_result` with the server's answer; the header value appears nowhere in the
journal (count 0). Stop the server and run the prompt again: stderr holds the one line
`ulei-engine: Failed to connect to MCP server 'smoke': <reason>`, the turn runs without the
server's tools and exits 0.

Roll back: stop the server, remove the MCP settings.

## 15. Stop and roll back

As `<ULEI_USER>`:

```sh
systemctl --user stop ulei-tg.service ulei-inbox.service ulei-engine.service
systemctl --user show -p Id -p ActiveState -p Result ulei-engine.service ulei-inbox.service ulei-tg.service
~/.local/bin/ulei status; echo "exit $?"
```

Expected: each unit `ActiveState=inactive` with `Result=success`: SIGTERM ends each daemon
cleanly and none restarts (ADR-0042 sections 6 and 7). `ulei status` prints `inbox: down ...:
connect: ...`, `engine: down ...`, `qb: up ...` and exits 1. Telegram: a message sent now gets
no answer; `ulei-tg` was not polling.

Pass: all three inactive with `success`. Fail: a unit `failed` or active again (its journal
holds the cause).

Roll back the whole smoke, in this order, each step's own roll back:

1. ulei: `systemctl --user disable ulei-tg.service ulei-inbox.service ulei-engine.service`,
   then steps 6 and 5 (state, config, binaries and units), `systemctl --user daemon-reload`,
   and `rm -r <RELEASE_DIR>`. Keep a copy of `<STATE_HOME>/ulei/engine/journal.sqlite3` first
   when the run is to be reviewed.
2. qb: steps 4, 3 and 2, as `<QB_USER>` and root.
3. Accounts: step 1.
4. Telegram: revoke the smoke bot's token with BotFather when the bot is not kept, and remove
   `<BOT_TOKEN_FILE>`.

## Reference: `ulei-engine run`

```sh
ulei-engine run --session S --route ALIAS --prompt TEXT [--new] [--channel NAME] [--event] [--config PATH]
```

Runs one turn of the current session of key `S` on the route `ALIAS`, whatever route the
session stores, with the built-in tools in the current directory, appends the turn to the journal and prints the final assistant text to stdout. A
turn that reaches its step limit asks the model once more, without tools, for a summary and
prints it. A reply without visible text is retried; one that stays empty prints `(empty)`. `--new` first starts an empty session for `S`; the older sessions stay in the
journal. The first turn of a session builds its system prompt from the prompt files and the
host and stores it with the session; every later turn sends it unchanged. `--channel NAME`
names the channel the prompt came on and `--event` marks it as an event rather than the
owner's message; a turn is a top-level owner turn only when the owner's prompt comes on the
channel `engine.main_channel` names (ADR-0022).

| Exit | Meaning | stderr |
|---|---|---|
| 0 | The model stopped, or the step limit was reached. | nothing |
| 1 | Any other failure: settings, journal, gateway error, transport. | `ulei-engine: <reason>` |
| 2 | Usage error. | the usage line |
| 3 | Every login of the route is out of quota. | `quota exhausted on route <route>; nearest reset <UTC time>` |
| 130 | SIGINT, SIGTERM or SIGHUP interrupted the turn: its tool process groups were killed and its gateway turn cancelled (ADR-0029). | `ulei-engine: the turn was interrupted` |

Every turn that reached the gateway is appended, whatever its exit code: the prompt, the
messages the turn produced and the request ids of its steps. The first SIGINT, SIGTERM or
SIGHUP ends the turn and the process exits once the turn is appended; later ones change
nothing. A process killed before the append, by SIGKILL or by a signal that arrives while the
settings are read, stores nothing of its turn.

## Reference: `ulei-engine serve`

```sh
ulei-engine serve [--config PATH]
```

Reads its settings once, opens the journal, binds the C6 socket `engine.c6_socket` with mode
0600, prints `ulei-engine ready <socket>` to stdout and serves C6 per ADR-0008 until `accept`
fails: `inject` and `is_idle` on the session `engine.owner_session`, `status`, `submit_turn`,
`interrupt` and `set_route` (ADR-0028). `interrupt` ends the running turn of a session as a
signal ends the turn of `run` and drops the turn queued next; the turn reports `interrupted`
once it is appended. Each submitted turn is a turn of `run` with `--route` set to the alias
`set_route` stored with the session, else `engine.default_route`, its `channel` as `--channel`
and its `new` as `--new`; a session runs one turn at a time and queues up to 32 more, and
different sessions run at the same time. An injected event is such a turn of the owner session
with its payload as `--prompt` and `--event`, started only while that session runs and queues
no turn. The turn settings come from the config file read at the start and from
the environment when each turn starts, as `run` reads them; one that would fail a `run` fails
that turn with outcome `failed`. A missing directory of the socket is created 0700; a directory that is
not the user's or that other users can access fails the start, as does any setting below. A
failed start or a failed `accept` exits 1 with one stderr line `ulei-engine: <reason>`.
SIGINT and SIGTERM end `serve` at once: running turns are not appended, and their tool process
groups end only with the service unit's control group (ADR-0029).

## Reference: settings

`run` and `serve` require `ulei.toml`, found from `--config PATH`, else `ULEI_CONFIG`, else
`$XDG_CONFIG_HOME/ulei/ulei.toml` when that is absolute, else `$HOME/.config/ulei/ulei.toml`
(ADR-0040 section 2); with no file there the start fails with
`ulei-engine: config <path>: <reason>`. The file is read through `ulei_contracts::config`,
which checks it whole against C9. Each environment variable wins over its key, so an empty
file with every setting in the environment runs. Only
`engine.concurrent_tool_timeout`, `engine.max_steps`, `engine.prompt_cache`,
`engine.hardline_malformed_mode`, `engine.empty_response_guard.enabled`,
`engine.memory_char_limit`, `engine.user_char_limit`, `engine.web_extract_char_limit`,
`engine.no_ask_guard.timeout`, the `engine.tool_loop_guardrails.*` and
`engine.compression.*` settings and `security.redact_secrets` have a default, and
`engine.service_unit`, `engine.timezone`, `engine.disabled_skills`, `engine.web_helper`,
`engine.main_channel`, `engine.turn_reminder_file`, `engine.no_ask_guard.route`,
`engine.mcp_servers` and the `engine.routes.<alias>.*` settings may stay unset.
`engine.c6_socket`, `engine.owner_session` and `engine.default_route` are read by `serve`
only, which requires them. The other `engine.web_*` settings are read only when
`engine.web_helper` is set, the other `engine.no_ask_guard.*` settings only when
`engine.no_ask_guard.route` is set, the `engine.mcp.<server>.*` settings only for the
servers `engine.mcp_servers` names; of these only `url` is required.

| Config key | Environment variable | Value |
|---|---|---|
| (the file itself) | `ULEI_CONFIG` | Path of the config file when no `--config PATH` is given. |
| `qb.socket` | `ULEI_QB_SOCKET` | Unix socket of the qb gateway. |
| `engine.c6_socket` | `ULEI_ENGINE_C6_SOCKET` | Path of the C6 socket of `serve`, at most 107 bytes, in a directory only the user can access. |
| `engine.owner_session` | `ULEI_ENGINE_OWNER_SESSION` | Session key of the owner's session, which `inject` runs events in and `is_idle` reports on; 1 to 128 chars. |
| `engine.default_route` | `ULEI_ENGINE_DEFAULT_ROUTE` | Route alias in qb on which the turns of `serve` run in a session that stores none, matching `^[a-z][a-z0-9_-]{0,63}$`. |
| `engine.journal` | `ULEI_ENGINE_JOURNAL` | Absolute path of the journal file. |
| `engine.terminal_output` | `ULEI_ENGINE_TERMINAL_OUTPUT` | Absolute path of the directory for the full output of `terminal` calls whose result was cut. |
| `engine.concurrent_tool_timeout` | `ULEI_ENGINE_CONCURRENT_TOOL_TIMEOUT` | Seconds a parallel batch of tool calls may run; a call still running after that is answered with a timeout error. `0` or less means no deadline. Default 420. |
| `engine.max_steps` | `ULEI_ENGINE_MAX_STEPS` | Steps of one turn before the summary request. `0` or less, `none`, `null`, `unlimited`, `infinite`, `infinity`, `inf` or `∞` means no limit. Default 200. |
| `engine.prompt_cache` | `ULEI_ENGINE_PROMPT_CACHE` | C4G cache mode of every turn, compaction and `no-ask-guard` classifier request: `auto_1h` (qb places its breakpoints with the one-hour lifetime), `auto`, or `off`, which sends no cache field. Any other value, or an empty one, refuses the start naming the key. Default `auto_1h`. |
| `engine.hardline_malformed_mode` | `ULEI_ENGINE_HARDLINE_MALFORMED_MODE` | What the hardline floor of `terminal` does with a command whose grep pattern operands do not parse: `warn` writes `ulei-engine: Hardline downgraded to warning: ...` to stderr, with secrets masked as `security.redact_secrets` says, and runs it unless a hardline pattern or a command substitution in command position refuses it, `block` refuses it (ADR-0016 section 14). Any other value, or an empty one, fails the turn naming the key. Default `warn`. |
| `engine.empty_response_guard.enabled` | `ULEI_ENGINE_EMPTY_RESPONSE_GUARD_ENABLED` | Two empty replies in a row with zero output tokens from one route end the empty-reply retries early; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
| `engine.service_unit` | `ULEI_ENGINE_SERVICE_UNIT` | The systemd service unit running the engine, with or without `.service`; `terminal` refuses a command that would restart, stop or kill it. Unset: under systemd (`INVOCATION_ID` set) the unit is the last `.service` component of the engine's cgroup path, and the engine refuses to start when there is none; outside systemd the guard is off. |
| `engine.soul_file` | `ULEI_ENGINE_SOUL_FILE` | SOUL.md, the identity that opens the system prompt. A missing or blank file fails the turn. |
| `engine.index_file` | `ULEI_ENGINE_INDEX_FILE` | INDEX.md of the knowledge base; the system prompt names it and the engine does not read it. |
| `engine.memory_file` | `ULEI_ENGINE_MEMORY_FILE` | MEMORY.md, rendered as the MEMORY block and written by the `memory` tool under the sibling lock file `MEMORY.md.lock`; absent means no block. |
| `engine.user_file` | `ULEI_ENGINE_USER_FILE` | USER.md, rendered as the USER PROFILE block and written by the `memory` tool (target `user`) under `USER.md.lock`; absent means no block. |
| `engine.memory_char_limit` | `ULEI_ENGINE_MEMORY_CHAR_LIMIT` | Char limit of the MEMORY store: the `memory` tool refuses a write past it, and the MEMORY block header shows it; a larger file is rendered in full at 100%. Default 2200. |
| `engine.user_char_limit` | `ULEI_ENGINE_USER_CHAR_LIMIT` | The same for the USER store and the USER PROFILE block. Default 1375. |
| `engine.skills_dir` | `ULEI_ENGINE_SKILLS_DIR` | Absolute path of the skills directory: one directory per skill with its `SKILL.md`, under category directories. The system prompt indexes it, `skills_list` and `skill_view` read it, and C6 `skills_dir` names it. Absent means no index; `skills_list` creates it. |
| `engine.disabled_skills` | `ULEI_ENGINE_DISABLED_SKILLS` | Comma-separated names (frontmatter `name` or directory name) of the skills the index, `skills_list` and `skill_view` leave out. Unset: none. |
| `engine.timezone` | `ULEI_ENGINE_TIMEZONE` | IANA zone of the `Conversation started` line, read from the system zone database. Unset: the host's local time from `/etc/localtime`, UTC without that file. |
| `engine.web_helper` | `ULEI_ENGINE_WEB_HELPER` | Path of the `ulei-web` helper (`cargo build --release --locked -p ulei-engine --features web --bin ulei-web`). Set: `web_search` and `web_extract` are offered and each call runs the helper once with an empty environment. Unset: no web tools. |
| `engine.web_base_url` | `ULEI_ENGINE_WEB_BASE_URL` | Base URL of the Exa API, an `https://` URL; `/search` and `/contents` are appended to it. A build with the `web-loopback-http` test switch also takes an `http://` URL whose host is a loopback address literal. |
| `engine.web_env_file` | `ULEI_ENGINE_WEB_ENV_FILE` | Absolute path of the env-file with the line `EXA_API_KEY=<key>`. The helper reads it at each call; the key never enters argv, an environment, the journal or a tool result. |
| `engine.web_store` | `ULEI_ENGINE_WEB_STORE` | Absolute path of the directory for the full text of each page `web_extract` cut; the result's footer names the file. |
| `engine.web_extract_char_limit` | `ULEI_ENGINE_WEB_EXTRACT_CHAR_LIMIT` | Characters per page a `web_extract` call without `char_limit` returns whole, clamped to 2000-500000. Default 15000. |
| `engine.mcp_servers` | `ULEI_ENGINE_MCP_SERVERS` | Names of the MCP servers, comma separated. At the start of each turn every server is connected at once, until the turn is interrupted; one that fails is reported on stderr as `ulei-engine: Failed to connect to MCP server '<server>': <reason>` and the run goes on without its tools. Its tools are offered as `mcp__<server>__<tool>`, every character outside `[A-Za-z0-9_]` of both parts as `_`. |
| `engine.mcp.<server>.url` | `ULEI_ENGINE_MCP_<SERVER>_URL` | `http://` URL of the server's Streamable HTTP endpoint on `127.0.0.1`, `[::1]` or `localhost` (resolving only to loopback); any other URL fails the run with an error naming the key. |
| `engine.mcp.<server>.env_file` | `ULEI_ENGINE_MCP_<SERVER>_ENV_FILE` | Absolute path of an env-file whose `NAME=value` lines are the request headers of the server, for example `Authorization=Bearer <token>`. The values never reach argv, an environment, the journal, a tool result or stderr. Unset: no extra headers. |
| `engine.mcp.<server>.timeout` | `ULEI_ENGINE_MCP_<SERVER>_TIMEOUT` | Seconds one call to the server may take before it fails with `MCP call timed out`. `0` or less means no deadline. Default 300. |
| `engine.mcp.<server>.connect_timeout` | `ULEI_ENGINE_MCP_<SERVER>_CONNECT_TIMEOUT` | Seconds connecting to the server and listing its tools may take. `0` or less means no deadline. Default 60. |
| `engine.mcp.<server>.tools.include` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_INCLUDE` | Tools of the server to offer, comma separated exact names or globs with `*`, `?` or `[...]`. Unset: all. |
| `engine.mcp.<server>.tools.resources` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_RESOURCES` | Offers `list_resources` and `read_resource` of a server that advertises resources; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
| `engine.mcp.<server>.tools.prompts` | `ULEI_ENGINE_MCP_<SERVER>_TOOLS_PROMPTS` | Offers `list_prompts` and `get_prompt` of a server that advertises prompts; read as `tools.resources`. Default on. |
| `engine.main_channel` | `ULEI_ENGINE_MAIN_CHANNEL` | The channel whose owner prompts (`--channel` equal to it, no `--event`) are top-level owner turns. Unset: no turn is one. |
| `engine.turn_reminder_file` | `ULEI_ENGINE_TURN_REMINDER_FILE` | Absolute path of the UTF-8 file whose bytes, unchanged, are appended after a blank line to every request copy of the prompt of a top-level owner turn; the journal and the system prompt never carry them. Read when each turn starts; a missing or unreadable file fails that turn, an empty one is off. Unset: off. |
| `engine.no_ask_guard.route` | `ULEI_ENGINE_NO_ASK_GUARD_ROUTE` | Route alias in qb of the no-ask-guard classifier. Set: on a top-level owner turn, a final reply whose last two non-blank lines (code blocks and URLs left out) hold a `?` goes to one tool-less request on this route per part of the reply; on an `extra` verdict the reply does not end the turn, which continues with the note, at most 2 times per turn; a `legit` verdict, a failure, a timeout or a quota answer delivers the reply as it is (ADR-0023). Unset: off, and no other `engine.no_ask_guard.*` setting is read. |
| `engine.no_ask_guard.note_file` | `ULEI_ENGINE_NO_ASK_GUARD_NOTE_FILE` | Absolute path of the UTF-8 continuation note: the whole user message sent after a held-back reply, with one or more `{chain}` placeholders for the 16-hex chain id; `{{` and `}}` stand for braces. Read when each turn starts; unset, missing, unreadable or without `{chain}` fails that turn. |
| `engine.no_ask_guard.instructions_file` | `ULEI_ENGINE_NO_ASK_GUARD_INSTRUCTIONS_FILE` | Absolute path of the UTF-8 classifier instructions with the placeholders `{tag}` (a one-time `⟦DATA-<12 hex>⟧` marker), `{owner}` (the end of the owner's prompt) and `{reply}` (a part of the reply), braces as in the note. The classifier answers with a JSON object whose `verdict` is `extra` or `legit`. Read when each turn starts; unset, missing, unreadable or without one of the placeholders fails that turn. |
| `engine.no_ask_guard.log` | `ULEI_ENGINE_NO_ASK_GUARD_LOG` | Absolute path of the JSON-lines file of the guard's records (`pass`, `pass_loop_cap`, `suppressed_continue`, `continuation_done`); its directory is created. A held-back reply whose record cannot be written is delivered instead. |
| `engine.no_ask_guard.timeout` | `ULEI_ENGINE_NO_ASK_GUARD_TIMEOUT` | Seconds one classifier request may take before it is cancelled and the reply delivered. `0` or less means no deadline. Default 45. |
| `engine.tool_loop_guardrails.warnings_enabled` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARNINGS_ENABLED` | Appends a `[Tool loop warning: ...]` line to the result of a call that failed again with identical arguments, a tool that failed again this turn, or a read-only call (`read_file`, `search_files`, `web_search`, `web_extract`) that returned the same result again (ADR-0016 section 12); on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
| `engine.tool_loop_guardrails.hard_stop_enabled` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_ENABLED` | Blocks a call past its `hard_stop_after` count before it runs, halts the turn at the `same_tool_failure` count, and ends the turn after that batch with a fixed answer naming the tool; on only for `1`, `true`, `yes` or `on` (any case). Default off. |
| `engine.tool_loop_guardrails.warn_after.exact_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_EXACT_FAILURE` | Failures of one call with identical arguments in a turn that warn; a positive integer. Default 2. |
| `engine.tool_loop_guardrails.warn_after.same_tool_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_SAME_TOOL_FAILURE` | Failures of one tool in a turn, with no success of it between, that warn; a positive integer. Default 3. |
| `engine.tool_loop_guardrails.warn_after.idempotent_no_progress` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_WARN_AFTER_IDEMPOTENT_NO_PROGRESS` | Unchanged results in a turn of one read-only call with identical arguments, each the same as its previous one, that warn; a positive integer. Default 2. |
| `engine.tool_loop_guardrails.hard_stop_after.exact_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_EXACT_FAILURE` | With hard stop: failures of one call with identical arguments after which it is blocked; a positive integer. Default 5. |
| `engine.tool_loop_guardrails.hard_stop_after.same_tool_failure` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_SAME_TOOL_FAILURE` | With hard stop: failures of one tool at which the turn halts; a positive integer. Default 8. |
| `engine.tool_loop_guardrails.hard_stop_after.idempotent_no_progress` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_HARD_STOP_AFTER_IDEMPOTENT_NO_PROGRESS` | With hard stop: unchanged results of one read-only call with identical arguments after which it is blocked; a positive integer. Default 5. |
| `engine.tool_loop_guardrails.loop_caps.max_web_searches` | `ULEI_ENGINE_TOOL_LOOP_GUARDRAILS_LOOP_CAPS_MAX_WEB_SEARCHES` | `web_search` calls one turn may make, hard stop on or off; the next one does not run, gets a `loop_web_search_cap` result and the turn ends after its batch with the fixed answer naming the tool. A non-negative integer; `0` is no limit. Default 50. |
| `engine.stall_guards` | `ULEI_ENGINE_STALL_GUARDS` | Within a turn, from the 3rd consecutive call of one tool with identical arguments and an identical result, appends a `[note: this is the 3rd consecutive identical call ...]` line; from the 2nd, replaces a successful result of 512 characters or more with a `[note: this result is byte-identical ...]` reference to the first call of the run (ADR-0016 section 12); on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |
| `engine.compression.route` | `ULEI_ENGINE_COMPRESSION_ROUTE` | Route alias in qb of the compaction summary turn, a tool-less turn without `session_key` whose `request_id` is `compact-<16 hex>`. An alias qb refuses makes every compaction store the deterministic fallback summary. Default `compact`. |
| `engine.compression.timeout` | `ULEI_ENGINE_COMPRESSION_TIMEOUT` | Seconds the summary turn may take before it is cancelled. `0` or less means no deadline. Default 450. |
| `engine.compression.protect_last_n` | `ULEI_ENGINE_COMPRESSION_PROTECT_LAST_N` | Recent messages a compaction keeps verbatim at least, read as 3 to 8; a non-negative integer. Default 20. |
| `engine.compression.target_ratio` | `ULEI_ENGINE_COMPRESSION_TARGET_RATIO` | Share of the threshold kept as the verbatim tail, clamped to 0.10-0.80; a decimal number, in the file a string such as `"0.2"` or an integer. Default 0.20. |
| `engine.compression.threshold` | `ULEI_ENGINE_COMPRESSION_THRESHOLD` | Share of the route's input budget (`context_length` less the step's `max_tokens`) at which a step compacts first, raised to 0.75 for a window under 512000 tokens and floored at 64000 tokens unless that leaves no room; a decimal number as `target_ratio`. Default 0.3. |
| `engine.compression.threshold_tokens` | `ULEI_ENGINE_COMPRESSION_THRESHOLD_TOKENS` | Prompt tokens at which a step compacts first at the latest, whatever the share gives; a non-negative integer, `0` sets no cap. Default 300000. |
| `engine.routes.<alias>.context_length` | `ULEI_ENGINE_ROUTES_<ALIAS>_CONTEXT_LENGTH` | Context length in tokens of the model behind the route alias, a positive integer. Set: a step whose prompt, as the last step's usage reports it (`input` + `cache_read` + `cache_write`), reaches the threshold compacts the session first. Unset: only a `context_too_long` answer compacts, sized for a 256000-token window. |
| `engine.routes.<alias>.max_tokens` | `ULEI_ENGINE_ROUTES_<ALIAS>_MAX_TOKENS` | `max_tokens` of every step on the route alias, which the window reserves for output; a positive integer. Default 16384. A compaction summary turn always reserves 16384. |
| `security.redact_secrets` | `ULEI_SECURITY_REDACT_SECRETS` | Masks secrets in the output of `terminal`, `read_file` and `search_files`; on unless set to a value other than `1`, `true`, `yes` or `on` (any case). Default on. |

The config file accepts only section headers, `key = "string"`, `key = integer`,
`key = true` or `false`, comments and blank lines; any other line fails with
`config <path> line <n>: <reason>`, and so do a section or key C9 does not list
(`unknown section [<name>]`, `unknown key <key>`) and a value of another kind than C9 gives
its key (`<key> must be a string` and the like). A section `[engine.mcp.<server>]` names a
server whose name is a bare key; one whose server the server list does not name fails with
`config <path> line <n>: section [engine.mcp.<server>] is not listed in engine.mcp_servers
(ULEI_ENGINE_MCP_SERVERS)`.
