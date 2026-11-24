# Runbook: hive cutover to `hive-py/` and rollback

A manual procedure, never run in CI and never run by an agent: the owner runs it on the deployment
host. It moves the live task hive from the code of the secretary repository (the old tree) to
`hive-py/` of this repository (ADR-0068). The store, the live root, the config file and the
delivery into the owner inbox stay; only the code that reads and writes them changes. It has five
parts, in this order:

1. Preconditions: the commit, the carry check, the moved hive on the host, the config additions,
   a dry run on a copy of the live store, a store backup.
2. Switch: the config, qb, the launcher, every unit and caller moved to `hive-py/`, start and
   checks.
3. Rollback: the old tree again on the same store and config.
4. Retiring the old tree.
5. Later and optional: `delivery = "ulei"`.

Every step names its command, what it must show, how to tell pass from fail, and how to roll it
back. A step that fails stops the run: record the command, its output and exit code, and
`journalctl --user -u <DISPATCHER_UNIT> -n 50`, then roll back from that step upwards. From 2.4 on
the way back is part 3.

## Relation to `mvp-pilot-cutover.md`

`docs/runbooks/mvp-pilot-cutover.md` moves the consumer of the live owner inbox and the owner bot
to ulei; this runbook moves only the hive. Neither touches the other's units, and part 3 of
either leaves the other's state as it is.

- Order. Run this runbook before part 2 of `mvp-pilot-cutover.md` or after the 7 days of its 2.7,
  never between them, so that a failure in the days after a cutover has one cause. The pilot of
  its part 1 does not touch `<LIVE_ROOT>` and may run at the same time as this runbook.
- Inbox root. The hive delivers with `delivery = "drop"`: it publishes through the inbox library
  into the root its config key `inbox` names, which is `<LIVE_ROOT>` before, during and after the
  MVP cutover, never `<PILOT_ROOT>`. Whichever consumer holds `<LIVE_ROOT>/consumer.lock` delivers
  the hive's `task_result`: the old runtime until step 2.3 of that runbook, `ulei-inbox` from its
  2.6 on (in between the events wait `pending`), the old runtime again after its part 3. Its
  rollback needs no change of the hive.
- The `ulei` adapter. `delivery = "ulei"` sends C1 `publish` to ulei's `inbox.socket`, which
  serves `[inbox] root` of `ulei.toml`. In the pilot that is `<PILOT_ROOT>`, so the adapter becomes
  an option only after step 2.6 of `mvp-pilot-cutover.md` and its 7 days; part 5 below.
- Item 4.6 of `mvp-pilot-cutover.md`, run after this cutover, hands the task to the moved hive.

## Placeholders

Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<HIVE_USER>`, `<HIVE_UID>` | The account that runs the live hive's user units, and its uid. It runs the moved hive too. |
| `<HIVE_UNIT_DIR>` | The user unit directory of `<HIVE_USER>` that holds the hive's units, written out as an absolute path. |
| `<DISPATCHER_UNIT>` | The user service of the hive dispatcher (`hive.py daemon`). |
| `<STEWARD_UNIT>`, `<STEWARD_TIMER>` | The daily steward's service (`hive_steward.py`) and its timer. |
| `<ROTATION_UNIT>`, `<ROTATION_TIMER>` | The worktree rotation's service (`hive_worktree_rotation.py`) and its timer. |
| `<SECRETARY_CHECKOUT>` | The checkout of the secretary repository the live hive runs from: the old tree. |
| `<LIVE_REV>` | The commit of `<SECRETARY_CHECKOUT>` the live hive runs. |
| `<OLD_PYTHON>` | The interpreter the old hive's units run `scripts/hive.py` with. |
| `<PROFILE_PACK_EXCLUDES>` | One pathspec `':(exclude)<path>'` per file of the profile pack of ADR-0068 section 2.3 inside `hive/`, `scripts/` and `tests/hive/` of the old tree. |
| `<INBOX_LIBRARY_DIR>` | The directory of the secretary repository that holds the owner-inbox library's module `inbox` (ADR-0068 section 1.1). |
| `<HIVE_CONFIG>` | Absolute path of the live hive config: the one file both hives read. |
| `<HIVE_DB>` | The value of `db` in `<HIVE_CONFIG>`: the live store. |
| `<OWNER_ACTOR>`, `<OWNER_TZ>`, `<STEWARD_DIR>`, `<ROTATION_LOG>` | The values of `owner_actor`, `owner_tz`, `steward_dir` and `rotation_log` in `<NEXT_CONFIG>`. |
| `<COMMIT>` | The ulei commit to deploy; it holds this runbook. |
| `<ULEI_CHECKOUT>` | A checkout of `<COMMIT>` on the host that `<HIVE_USER>` reads and nobody edits. |
| `<HIVE_PY>` | `<ULEI_CHECKOUT>/hive-py`. |
| `<SOURCE>` | The commit id in `<HIVE_PY>/SOURCE`. |
| `<HIVE_VENV>` | An empty directory of `<HIVE_USER>` for the moved hive's virtual environment. |
| `<NEXT_CONFIG>` | `<HIVE_CONFIG>` with the additions of 1.4, a file of `<HIVE_USER>` outside both trees, mode 0600. |
| `<HIVE_TEXTS_DIR>` | A directory of `<HIVE_USER>` outside both trees for the texts `[texts]` names. |
| `<DRY_DIR>`, `<BACKUP_DIR>` | Fresh directories of `<HIVE_USER>`, mode 0700, outside both trees and outside the live root. |
| `<CALLER_PATHS>` | The files and directories that hold callers of the hive besides its units: the secretary's skills and instructions that run `hive.py`, the heads' instruction files, other units and scripts of `<HIVE_USER>`. |
| `<QB_GROUP>` | As in `docs/runbooks/engine-live-smoke.md`. |
| `<JUDGE_ROUTE_1>` to `<JUDGE_ROUTE_5>` | The qb route aliases of the judge for card steps 1 to 5; several steps may share one. |
| `<STEWARD_ROUTE>` | The qb route alias of the steward. |
| `<STEWARD_TIMEOUT>` | A start timeout of the steward's service: at least twice `[steward] timeout_s`, plus a margin for the cancellations it runs (each up to 180 s). |
| `<TAG_LAUNCHER>` | The private head launcher: the binary of the vendor whose `tag_launcher` is `true`. |
| `<LAUNCHER_PROBE_ARGS>` | Arguments that make `<TAG_LAUNCHER>` run one short prompt and exit, as the launcher's own documentation gives them. |
| `<TEST_CARD>`, `<RUN_UNIT>`, `<ROLLBACK_CARD>` | The id 2.7 prints, the `unit` of its worker run, and the id 3.5 prints. |
| `<LIVE_ROOT>`, `<PILOT_ROOT>`, `<ULEI_USER>`, `<RUNTIME_DIR>` | As in `docs/runbooks/mvp-pilot-cutover.md`. |
| `<QB_USER>`, `<QB_GATEWAY_SOCKET>`, `<QB_ROLE>` | As in `docs/runbooks/engine-live-smoke.md`. |

Shell helpers, as `<HIVE_USER>`:

```sh
hp() { HIVE_CONFIG=<HIVE_CONFIG> <HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive.py "$@"; }
oh() { HIVE_CONFIG=<HIVE_CONFIG> <OLD_PYTHON> <SECRETARY_CHECKOUT>/scripts/hive.py "$@"; }
ids() { python3 -c 'import json, sys; print("\n".join(sorted(r["id"] for r in json.load(sys.stdin))))'; }
db_ids() {
  sqlite3 -readonly "$1" "SELECT id FROM cards WHERE state != 'cancelled'
    ORDER BY created_at DESC LIMIT 200" | sort
}
```

`hp` is the moved hive on the live config and store, `oh` the old one; `ids` and `db_ids` give the
ids `list --json` prints and the ids `list` selects from a store. Every `hive.py` command opens
the store, and the moved hive's first open adds the nullable column `tag` to `cards`. Before 2.6,
run `hp` only where a step says so.

## Part 1. Preconditions

### 1.1 The commit and its CI

Anywhere with access to the repository:

```sh
gh run list --commit <COMMIT> --json databaseId,workflowName,conclusion
gh run view <RUN_ID> --json jobs --jq '.jobs[] | "\(.name) \(.conclusion)"'
```

Expected: the CI run of `<COMMIT>` with `conclusion` `success` (its `databaseId` is `<RUN_ID>`),
and every job `success`, among them `hive-py` (ADR-0068 section 3). `<COMMIT>` is on `main`.

Pass: all hold. Fail: a job not `success`, or a run of another commit. Deploy another commit.

Roll back: none.

### 1.2 The carry check (ADR-0068 section 6.4)

As `<HIVE_USER>`, after `<ULEI_CHECKOUT>` is at `<COMMIT>`:

```sh
git -C <ULEI_CHECKOUT> rev-parse HEAD
git -C <ULEI_CHECKOUT> status --porcelain
cat <HIVE_PY>/SOURCE
cd <SECRETARY_CHECKOUT>
git rev-parse HEAD
git status --porcelain -- hive scripts tests/hive
git cat-file -e <SOURCE>^{commit}; echo "exit $?"
git diff --stat <SOURCE> <LIVE_REV> -- hive ':(glob)scripts/hive*.py' tests/hive \
  ':(exclude)hive/config.toml' ':(exclude)tests/hive/test_claude_hive.py' <PROFILE_PACK_EXCLUDES>
cd -
```

The pathspecs are the move set of ADR-0068 section 1.1: `hive/`, `scripts/hive*.py` and
`tests/hive/`, less the live config, the launcher's test and the profile pack.

Expected: `<COMMIT>`, then nothing; `<SOURCE>`; `<LIVE_REV>`, then nothing (the live hive runs
committed code); `exit 0`; and `git diff --stat` prints nothing.

Pass: the diff is empty. Fail, and the cutover does not start:

- `git status` prints a path: the old tree holds uncommitted hive code. Commit it in the secretary
  repository first, then carry it as below.
- `git cat-file` exits 1: `<SECRETARY_CHECKOUT>` lacks `<SOURCE>`; fetch it and run the check again.
- The diff names files and `git merge-base --is-ancestor <LIVE_REV> <SOURCE>` exits 0: the live
  checkout is behind `<SOURCE>`. Bring it to `<SOURCE>` the way the secretary repository deploys,
  then run the check again.
- The diff names files otherwise: the live hive has changes `hive-py/` lacks. Carry them (ADR-0068
  section 6.2: the diff from `<SOURCE>` to `<LIVE_REV>` on these paths, applied with the scrub of
  section 4, its tests with it, `SOURCE` advanced in the same commit), merge the carry green, take
  that merge as `<COMMIT>` and start again at 1.1.

From 2.4 on no commit of the secretary repository touches these paths (part 4).

Roll back: none.

### 1.3 The moved hive on the host

As `<HIVE_USER>` (ADR-0068 section 3.2: `requirements.lock` is the only lock the live host
installs):

```sh
python3.12 -m venv <HIVE_VENV>
<HIVE_VENV>/bin/pip install --require-hashes --no-deps -r <HIVE_PY>/requirements.lock
<HIVE_VENV>/bin/python -c 'import sys, cronsim; print(sys.version_info[:2])'
```

Expected: pip installs `cronsim` only; the last line prints `(3, 12)`.

Pass: both hold. Fail: a hash mismatch (the lock or the download is wrong; stop), no `python3.12`.

Roll back: `rm -r <HIVE_VENV>`.

### 1.4 The config additions

As `<HIVE_USER>`:

```sh
umask 077
cp <HIVE_CONFIG> <NEXT_CONFIG>
install -d -m 0700 <HIVE_TEXTS_DIR>
cp <HIVE_PY>/texts/* <HIVE_TEXTS_DIR>/
"${EDITOR:-vi}" <NEXT_CONFIG>
```

Add every key below that `<NEXT_CONFIG>` lacks, and keep every key it has with its value
(ADR-0068 section 5: the old hive ignores unknown keys, so one file serves both).
`<HIVE_PY>/config.example.toml` is the full list with a comment on each key. A value the old tree
holds as a constant in its code at `<LIVE_REV>` (ADR-0068 section 4.2) is taken from there; the
texts are the old tree's texts written into the files of `<HIVE_TEXTS_DIR>`. Top-level keys go
above the first `[table]` line.

```toml
delivery = "drop"
inbox = "<LIVE_ROOT>"
inbox_library = "<INBOX_LIBRARY_DIR>"
gateway_socket = "<QB_GATEWAY_SOCKET>"
owner_actor = "..."
owner_tz = "..."
results_dir = "..."
card_show_command = "HIVE_CONFIG=<HIVE_CONFIG> <HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive.py show"
script_env_file = "..."
steward_dir = "..."
rotation_log = "..."
script_defer_s = 900
script_defer_max = 6

[head_env]
HIVE_CONFIG = "<HIVE_CONFIG>"
# and the other keys a head's environment gets

[gate_env]
HIVE_CONFIG = "<HIVE_CONFIG>"
# and the rest of an acceptance command's environment

[script_env]
HIVE_CONFIG = "<HIVE_CONFIG>"
# and the rest of a script run's environment

[texts]
workspace_context = "<HIVE_TEXTS_DIR>/workspace-context.md"
report_request = "<HIVE_TEXTS_DIR>/report-request.txt"
series_news_note = "<HIVE_TEXTS_DIR>/series-news-note.txt"
result_note = "<HIVE_TEXTS_DIR>/result-note.txt"
missed_note = "<HIVE_TEXTS_DIR>/missed-note.txt"
steward_prompt = "<HIVE_TEXTS_DIR>/steward-prompt.txt"
steward_alert = "<HIVE_TEXTS_DIR>/steward-alert.txt"

[judge]
max_tokens = 2048
timeout_s = 300
retry_s = 300
routes = { 1 = "<JUDGE_ROUTE_1>", 2 = "<JUDGE_ROUTE_2>", 3 = "<JUDGE_ROUTE_3>", 4 = "<JUDGE_ROUTE_4>", 5 = "<JUDGE_ROUTE_5>" }

[steward]
route = "<STEWARD_ROUTE>"
max_tokens = 4096
timeout_s = 900
```

and inside the existing tables: `disk` and `psi_cgroups` in `[resources]` (required while
`enabled = true`), `limits_home` in `[budget]` (required with `limits_command`), `urgent_sources`
in `[admission]`, and `home` and `tag_launcher` in every `[vendors.<v>]`: `tag_launcher = true`
only for the vendor whose binary is `<TAG_LAUNCHER>`. `inbox` is the root the old hive publishes
into today. The `[texts]` paths are absolute, so a copy of the config elsewhere reads the same
files. Then, as `<HIVE_USER>`:

```sh
HIVE_CONFIG=<NEXT_CONFIG> <HIVE_VENV>/bin/python -P -c 'import sys
sys.path.insert(0, sys.argv[1])
from hive import store
c = store.load_config()
print("config ok", c["delivery"], c["inbox"], sorted(c["judge"]["routes"].items()), c["steward"]["route"])' \
  <HIVE_PY>
diff <HIVE_CONFIG> <NEXT_CONFIG> | grep -c '^<'
realpath <LIVE_ROOT>
stat -c '%a %U %n' <NEXT_CONFIG>
```

The first command loads the config with the moved hive (`store.load_config`, which opens no
store) and checks every key it needs.

Expected: `config ok drop <LIVE_ROOT> [(1, '<JUDGE_ROUTE_1>'), ..., (5, '<JUDGE_ROUTE_5>')]
<STEWARD_ROUTE>`; `0`: no line of the live config is changed or removed; the root the old hive
publishes into; `600 <HIVE_USER>`.

Pass: all hold. Fail: `ValueError: config: missing key <key>` or another `config:` line names the
key to add or fix; a count above 0 (a live key was changed: restore it); `FileNotFoundError` of a
text file.

Roll back: `rm <NEXT_CONFIG>`, `rm -r <HIVE_TEXTS_DIR>`.

### 1.5 Dry run on a copy of the live store

As `<HIVE_USER>`, while the old hive runs. The copy is taken by SQLite's online backup through a
read-only connection; nothing under the live root is written. Every command below names the copy
in `HIVE_DB`, which overrides `db` of the config, and runs only commands that move no card.

```sh
umask 077
install -d -m 0700 <DRY_DIR>
sqlite3 -readonly <HIVE_DB> ".backup <DRY_DIR>/hive.db"
sqlite3 -readonly <DRY_DIR>/hive.db 'PRAGMA integrity_check'
schema() {
  sqlite3 -readonly "$1" 'SELECT type, name, tbl_name, sql FROM sqlite_master ORDER BY type, name'
}
schema <DRY_DIR>/hive.db > <DRY_DIR>/schema.before
dnew() { HIVE_CONFIG=<NEXT_CONFIG> HIVE_DB=<DRY_DIR>/hive.db <HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive.py "$@"; }
dold() { HIVE_CONFIG=<NEXT_CONFIG> HIVE_DB=<DRY_DIR>/hive.db <OLD_PYTHON> <SECRETARY_CHECKOUT>/scripts/hive.py "$@"; }
dnew list --json > <DRY_DIR>/new.json; echo "exit $?"
dnew board; echo "exit $?"
dnew ladder; echo "exit $?"
card=$(sqlite3 -readonly <DRY_DIR>/hive.db "SELECT id FROM cards ORDER BY created_at DESC LIMIT 1")
dnew show "$card"; echo "exit $?"
schema <DRY_DIR>/hive.db > <DRY_DIR>/schema.after
diff <DRY_DIR>/schema.before <DRY_DIR>/schema.after
dold list --json > <DRY_DIR>/old.json; echo "exit $?"
dold show "$card"; echo "exit $?"
db_ids <DRY_DIR>/hive.db > <DRY_DIR>/db.ids
ids < <DRY_DIR>/new.json | diff <DRY_DIR>/db.ids - && echo "new lists the store"
ids < <DRY_DIR>/old.json | diff <DRY_DIR>/db.ids - && echo "old lists the store"
```

Expected: `ok`; each `exit 0`; `board` prints `In progress (n)`, `Waiting (n)` and `Done (n)`
with the copy's cards; `ladder` prints the step table of the config; `show` prints the newest
card. The schema diff is the `cards` table only, its `CREATE TABLE` gaining `, tag TEXT` before
the closing parenthesis (ADR-0068 section 5: additive only). The old hive reads the copy after the moved one touched
it: `list` and `show` exit 0, and both lines `new lists the store` and `old lists the store`
print.

Pass: all hold. Fail: any other schema change (stop: the compatibility promise is broken; file
it), a `config:` error (1.4), a traceback of the old hive on the copy (stop and file it: part 3
could not work), an id diff.

Roll back: `rm -r <DRY_DIR>`. Remove it after the pass in any case: it holds every card.

### 1.6 Store backup

As `<HIVE_USER>`:

```sh
umask 077
install -d -m 0700 <BACKUP_DIR>
sqlite3 -readonly <HIVE_DB> ".backup <BACKUP_DIR>/before.db"
sqlite3 -readonly <BACKUP_DIR>/before.db 'PRAGMA integrity_check'
sqlite3 -readonly <BACKUP_DIR>/before.db 'SELECT count(*) FROM cards'
cp -p <HIVE_CONFIG> <BACKUP_DIR>/config.toml
```

Expected: `ok` and a count of cards.

Pass: both. Fail: anything but `ok`: the live store is damaged; stop and repair it first.

A restore is for a damaged store only, never part of a rollback (part 3 runs the old hive on the
store as the moved one left it), and loses every change after the backup. With every hive unit
and timer stopped and no `hive-run-*` unit active, as `<HIVE_USER>`:
`sqlite3 <HIVE_DB> ".restore <BACKUP_DIR>/<file>.db"`, then `PRAGMA integrity_check`.

Roll back: none; keep `<BACKUP_DIR>` until part 4.

## Part 2. Switch

Preconditions: part 1 passed on `<COMMIT>`; a quiet hour of the owner with no steward run due;
the relation to `mvp-pilot-cutover.md` above holds.

### 2.1 Install the config

The old hive keeps running: it re-reads its config every tick and ignores the new keys. As
`<HIVE_USER>`:

```sh
cp -p <HIVE_CONFIG> <BACKUP_DIR>/config.before
install -m 0600 <NEXT_CONFIG> <HIVE_CONFIG>
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_VENV>/bin/python -P -c 'import sys
sys.path.insert(0, sys.argv[1])
from hive import store
print("config ok", store.load_config()["delivery"])' <HIVE_PY>
oh list > /dev/null; echo "exit $?"
journalctl --user -u <DISPATCHER_UNIT> --since '-2 min' -o cat
```

Expected: `config ok drop`; `exit 0`; after two minutes the dispatcher's journal has no traceback
and no config error.

Pass: all hold. Fail: an error of either hive; roll back.

Roll back: `install -m 0600 <BACKUP_DIR>/config.before <HIVE_CONFIG>`.

### 2.2 qb: the hive as a client and its route aliases

As `<QB_USER>`, in the gateway config of `docs/runbooks/engine-live-smoke.md` step 4:

- unless `<HIVE_UID>` is already a client there, one `[[clients]]` entry with `name = "hive"` and
  `uid = <HIVE_UID>`, and `<HIVE_USER>` a member of `<QB_GROUP>` (step 1 there);
- one `[[routes]]` entry per distinct alias of `<JUDGE_ROUTE_1>` to `<JUDGE_ROUTE_5>` and
  `<STEWARD_ROUTE>`, each a ladder of `<QB_ROLE>` or a pin of a vendor and model, in the form of
  that step.

```sh
cp -p ~/.config/qb/gateway.toml ~/.config/qb/gateway.toml.before-hive
"${EDITOR:-vi}" ~/.config/qb/gateway.toml
```

Apply the change as qb `docs/config.md` at the pin says for the gateway config; a restart of
`qb-gateway.service` fails the turns in flight of every client, so make it at a quiet moment. Then
as `<HIVE_USER>`:

```sh
id -nG
curl -sS --unix-socket <QB_GATEWAY_SOCKET> http://localhost/v1/model/routes; echo
```

Expected: `<QB_GROUP>` among the groups; `{"routes": [...]}` with every judge and steward alias
`ready` (C4G `GET /v1/model/routes`).

Pass: all aliases `ready` as `<HIVE_USER>`. Fail: `Permission denied` on the socket (the group), a
401 `auth` (`[[clients]]`), an alias missing or not `ready` (its role or pin).

Roll back: as `<QB_USER>`, `mv ~/.config/qb/gateway.toml.before-hive ~/.config/qb/gateway.toml`,
applied the same way. The old hive does not use the aliases, so they may also stay.

### 2.3 The launcher contract (ADR-0068 section 2.6, item 4)

The private launcher `<TAG_LAUNCHER>` reads `HIVE_TAG`, hands it to qb as the request's tag, and
on a qb refusal prints `tag refused: <class>: <message>` and exits 77. As `<HIVE_USER>`:

```sh
grep -nE '^\[vendors\.|^binary|^tag_launcher' <HIVE_CONFIG>
<TAG_LAUNCHER> <LAUNCHER_PROBE_ARGS> > <BACKUP_DIR>/probe-untagged.out 2>&1; echo "exit $?"
HIVE_TAG=cutover-probe <TAG_LAUNCHER> <LAUNCHER_PROBE_ARGS> > <BACKUP_DIR>/probe-tagged.out 2>&1
echo "exit $?"
grep '^tag refused: ' <BACKUP_DIR>/probe-tagged.out | tail -n 1
```

Expected: `tag_launcher = true` only in the section whose `binary` names `<TAG_LAUNCHER>`; the
untagged run exits 0 as before; the tagged run exits 77 and its last `tag refused:` line names a
class: `bad_request` on a qb without tags (it refuses the unknown request field), or
`no_login_for_tag` on a qb with tags and no login bound to `cutover-probe`.

Pass: all hold. Fail: the tagged run exits 0 (the launcher ignores `HIVE_TAG` or ran without the
tag: a bypass) or another code, or no `tag refused:` line; the launcher is the owner's to fix
before the cutover.

No card carries a tag until the deployed `<COMMIT>`'s `contracts/c4g-qb-gateway.openapi.json`
holds the request field `tag`, that is the qb pin that carries tags: never `create --tag` or
`hive.py tag` before. A tagged card on a qb without tags blocks with `wait_reason` `tag` or
`judge`.

Roll back: none; `rm <BACKUP_DIR>/probe-*.out`.

### 2.4 Stop the old hive

As `<HIVE_USER>`:

```sh
systemctl --user stop <STEWARD_TIMER> <ROTATION_TIMER>
systemctl --user stop <DISPATCHER_UNIT>
systemctl --user show -p Id -p ActiveState \
  <DISPATCHER_UNIT> <STEWARD_UNIT> <ROTATION_UNIT> <STEWARD_TIMER> <ROTATION_TIMER>
systemctl --user list-units --no-legend 'hive-run-*'
sqlite3 -readonly <HIVE_DB> ".backup <BACKUP_DIR>/cutover.db"
sqlite3 -readonly <BACKUP_DIR>/cutover.db 'PRAGMA integrity_check'
```

Expected: each `ActiveState=inactive`; the `hive-run-*` units listed are runs the old dispatcher
started, which keep running in their own units (`launcher = "systemd"`) and which the moved
dispatcher reaps (ADR-0068 section 5: the run directories, `job.json` and reports keep their
format); `ok`.

Pass: all hold. Fail: a unit still active or `failed`; its journal holds the cause.

From here, no commit of the secretary repository touches the move set of 1.2: the old tree stays
at `<LIVE_REV>` for part 3 (ADR-0068 section 6.4).

Roll back: `systemctl --user start <DISPATCHER_UNIT> <STEWARD_TIMER> <ROTATION_TIMER>`.

### 2.5 Every unit and caller to `hive-py/`

As `<HIVE_USER>`. First record the units as they are:

```sh
for u in <DISPATCHER_UNIT> <STEWARD_UNIT> <ROTATION_UNIT>; do
  systemctl --user cat "$u" > "<BACKUP_DIR>/$u.before"
  systemctl --user show -p TimeoutStartUSec "$u"
done
```

Each unit gets a drop-in `50-hive-py.conf` in `<HIVE_UNIT_DIR>/<unit>.d/`, with `<args>` the
arguments the old `ExecStart` passes after the script path, as the recorded `.before` file shows
them:

```ini
# <DISPATCHER_UNIT>.d/50-hive-py.conf
[Service]
Environment=HIVE_CONFIG=<HIVE_CONFIG>
ExecStart=
ExecStart=<HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive.py daemon <args>

# <STEWARD_UNIT>.d/50-hive-py.conf
[Service]
Environment=HIVE_CONFIG=<HIVE_CONFIG>
ExecStart=
ExecStart=<HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive_steward.py <args>
TimeoutStartSec=<STEWARD_TIMEOUT>

# <ROTATION_UNIT>.d/50-hive-py.conf
[Service]
Environment=HIVE_CONFIG=<HIVE_CONFIG>
ExecStart=
ExecStart=<HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive_worktree_rotation.py <args>
```

Leave `TimeoutStartSec=` out of the steward's drop-in when its `TimeoutStartUSec` reads `infinity`.
A `PYTHONPATH` or `WorkingDirectory` of the old unit that names `<SECRETARY_CHECKOUT>` gets an
empty `Environment=PYTHONPATH=` or a `WorkingDirectory=` outside it in the same drop-in. The
timers trigger the same services and need no change.

Then the other callers. Find them:

```sh
grep -rnE 'hive[a-z_]*\.py' <CALLER_PATHS> <HIVE_UNIT_DIR> 2>/dev/null
crontab -l 2>/dev/null | grep -nE 'hive[a-z_]*\.py'
```

For each file to edit, `cp -p <file> <file>.pre-hive-cutover` and
`echo <file> >> <BACKUP_DIR>/callers.list` first; then every call becomes
`HIVE_CONFIG=<HIVE_CONFIG> <HIVE_VENV>/bin/python <HIVE_PY>/scripts/<script> ...` with its
arguments unchanged. That covers the secretary's skills and instructions that run `hive.py` (the
old runtime's and, after the MVP cutover, the copy of `mvp-pilot-cutover.md` 1.4), the heads'
instruction files, the observer's `hive.py observe` and any other unit or crontab line. A caller
inside `ulei-engine.service` reaches `<HIVE_PY>`, `<HIVE_VENV>` and the store only where that
unit's sandbox allows it: item 4.6 of `mvp-pilot-cutover.md` checks it. Heads get `HIVE_CONFIG`
from `[head_env]` of 1.4.

```sh
systemctl --user daemon-reload
systemctl --user show -p ExecStart -p Environment -p TimeoutStartUSec \
  <DISPATCHER_UNIT> <STEWARD_UNIT> <ROTATION_UNIT>
grep -rnE 'hive[a-z_]*\.py' <CALLER_PATHS> <HIVE_UNIT_DIR> 2>/dev/null \
  | grep -vF '<HIVE_PY>/scripts/'
crontab -l 2>/dev/null | grep -nE 'hive[a-z_]*\.py' | grep -vF '<HIVE_PY>/scripts/'
```

Expected: each `ExecStart` names `<HIVE_VENV>/bin/python <HIVE_PY>/scripts/...`, each
`Environment` holds `HIVE_CONFIG=<HIVE_CONFIG>`, the steward's `TimeoutStartUSec` is `infinity` or
at least `<STEWARD_TIMEOUT>`; the two greps print only the `.pre-hive-cutover` copies and the old
units' own files, whose `ExecStart` the drop-ins replace. Every remaining call sets the absolute
`HIVE_CONFIG`: the hive fails without it (ADR-0068 section 5, "CLI").

Pass: all hold. Fail: a caller that names `<SECRETARY_CHECKOUT>/scripts/hive` or a relative
`scripts/hive.py`, or one without `HIVE_CONFIG`; edit it.

Roll back: 3.3.

### 2.6 Start the moved hive

As `<HIVE_USER>`:

```sh
systemctl --user start <DISPATCHER_UNIT>
pid=$(systemctl --user show -p MainPID --value <DISPATCHER_UNIT>)
tr '\0' ' ' < /proc/$pid/cmdline; echo
tr '\0' '\n' < /proc/$pid/environ | grep '^HIVE_CONFIG='
journalctl --user -u <DISPATCHER_UNIT> -n 30 -o cat
hp board; echo "exit $?"
sqlite3 -readonly <HIVE_DB> "SELECT count(*) FROM pragma_table_info('cards') WHERE name = 'tag'"
```

Expected: the command line `<HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive.py daemon ...`;
`HIVE_CONFIG=<HIVE_CONFIG>`; no traceback and no `config:` line in the journal; the board's three
columns with exit 0, the runs of 2.4 in `In progress` while their units live; `1`: the additive
`tag` column, added on the first open.

Pass: all hold. Fail: a `config:` error (1.4), `Permission denied` on `<HIVE_PY>` or `<HIVE_VENV>`
(the unit's sandbox: add the paths in the drop-in), a restart loop. Any of them: part 3.

Roll back: part 3.

### 2.7 A test card through gate and judge to the owner inbox

As `<HIVE_USER>`:

```sh
hp create --title 'Hive cutover check' \
  --goal 'Run the shell command sleep 90, then write the single word cutover into REPORT.md and nothing else.' \
  --acceptance 'grep -qx cutover REPORT.md' --step 1 --source msg:hive-cutover \
  --created-by secretary --json
```

The source `msg:` makes it an owner task, whose result goes to the owner inbox. While its worker
runs (the `sleep 90` leaves time):

```sh
sqlite3 -readonly <HIVE_DB> "SELECT id, role, vendor, status, unit FROM runs WHERE card_id = '<TEST_CARD>'"
cat /sys/fs/cgroup/user.slice/user-<HIVE_UID>.slice/user@<HIVE_UID>.service/app.slice/<RUN_UNIT>.service/cgroup.events
hp board --json | python3 -c 'import json, sys
b = json.load(sys.stdin)
print([c for c, es in b["columns"].items() if any(e["id"] == sys.argv[1] for e in es)])' <TEST_CARD>
```

Expected: one `worker` run `running` with `unit` `<RUN_UNIT>` (`hive-run-<id>`); `populated 1`;
`['in_progress']`. The liveness probe (`hive/liveness.py`) reads that file: the card is in progress
while its unit's cgroup holds a process.

When the run ended, within a few minutes:

```sh
hp show <TEST_CARD>
sqlite3 -readonly <HIVE_DB> "SELECT kind, substr(data, 1, 200) FROM events WHERE card_id = '<TEST_CARD>'
  AND kind IN ('gate', 'judge', 'done', 'secretary_event') ORDER BY id"
sqlite3 -readonly <HIVE_DB> "SELECT event_id, kind, published_at IS NOT NULL, error FROM outbox
  WHERE card_id = '<TEST_CARD>'"
```

Then as the owner of `<LIVE_ROOT>` (`<ULEI_USER>` of `mvp-pilot-cutover.md`):

```sh
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 \
  "SELECT id, status, outcome FROM events WHERE id = 'hive:<TEST_CARD>:done'"
```

Expected: the card `[done]`; the events `gate` with `"passed": true`, `judge` with
`"route": "<JUDGE_ROUTE_1>"` and `"verdict": "done"`, `done`, and `secretary_event` naming
`hive:<TEST_CARD>:done`; the outbox row `hive:<TEST_CARD>:done|task_result|1|` with no error; one
inbox row that reaches `done`, delivered by the consumer that holds `<LIVE_ROOT>/consumer.lock`,
and the secretary tells the owner the result in the chat.

Pass: all hold. Fail:

- the card `blocked` with `wait_reason` `judge`, or waiting in `check` with a `judge` event that
  names a `failure`: the class names the cause (the alias of 2.2, the socket, the client);
- the gate failed: the worker did not write `REPORT.md`; read the run's report with `hp show`;
- an outbox `error`: the drop adapter (`inbox`, `inbox_library`, the root's modes);
- no `in_progress` while `populated 1`: the dispatcher does not run as `<HIVE_USER>`'s user unit,
  or `launcher` is not `"systemd"`.

Roll back: part 3; the done card stays.

### 2.8 The steward and the rotation

As `<HIVE_USER>`, one steward dry run now: a C4G turn on `<STEWARD_ROUTE>` (a second one only after
an unparseable answer) that prints the plan and cancels and writes nothing:

```sh
HIVE_CONFIG=<HIVE_CONFIG> <HIVE_VENV>/bin/python <HIVE_PY>/scripts/hive_steward.py | tail -n 20
echo "exit ${PIPESTATUS[0]}"
systemctl --user start <STEWARD_TIMER> <ROTATION_TIMER>
systemctl --user list-timers --all <STEWARD_TIMER> <ROTATION_TIMER>
```

Expected: the plan's JSON and `exit 0`; both timers with a next run.

After the steward's next scheduled run (it applies its plan only between 07:00 and 12:00 of
`<OWNER_TZ>` and prints `skipped` outside):

```sh
systemctl --user show -p Result -p ExecMainStatus -p ExecMainStartTimestamp <STEWARD_UNIT>
journalctl --user -u <STEWARD_UNIT> -n 50 -o cat
python3 -c 'import json, sys
d = json.load(open(sys.argv[1]))
print(d.get("model_review"), d.get("vendor"), len(d.get("cancel", [])))' \
  <STEWARD_DIR>/$(TZ=<OWNER_TZ> date +%F).json
```

and after the rotation's next run:

```sh
systemctl --user show -p Result -p ExecMainStatus <ROTATION_UNIT>
tail -n 2 <ROTATION_LOG>
```

Expected: `Result=success`, `ExecMainStatus=0` for both; no `steward: no usable model plan` and no
`alert delivery failed` in the steward's journal; `True`, the vendor label and the count of
cancellations; a rotation log line of this run.

Pass: all hold. Fail: a `model_error` with `"alerted": true` (the turn failed and the owner got the
alert: 2.2), a start timeout (`<STEWARD_TIMEOUT>` of 2.5), any other non-zero exit.

Roll back: part 3.

### 2.9 The profile pack and commands stored in cards

The profile-pack collectors stay in the secretary repository and import `hive.schedule` from the
old tree (ADR-0068 section 2.3); a script card runs a `.py` file that is not executable with the
dispatcher's interpreter, from now `<HIVE_VENV>/bin/python`. As `<HIVE_USER>`:

```sh
git -C <SECRETARY_CHECKOUT> grep -lE 'hive\.schedule|from hive import [a-z_, ]*schedule'
<HIVE_VENV>/bin/python -P -c 'import sys
sys.path.insert(0, sys.argv[1])
import hive.schedule
print(hive.schedule.__file__)' <SECRETARY_CHECKOUT>
sqlite3 -readonly <HIVE_DB> "SELECT id, kind, state, script FROM cards
  WHERE state NOT IN ('done', 'cancelled') AND script LIKE '<SECRETARY_CHECKOUT>/%'"
sqlite3 -readonly <HIVE_DB> "SELECT id, state FROM cards
  WHERE state NOT IN ('done', 'cancelled') AND acceptance LIKE '%<SECRETARY_CHECKOUT>%'"
```

Expected: the collectors that import it, beside the old tree's own files;
`<SECRETARY_CHECKOUT>/hive/schedule.py`: the old module imports under the moved hive's
interpreter. Each script row is one of:

- a profile-pack file: it stays. Check the next instance of its rule: the newest entry under
  `instances` of `hp show <rule>` ends as its instances did before the cutover, and its run's
  stderr log in the `run_dir` of `runs` holds no `ModuleNotFoundError` or `ImportError`;
- a file of the move set: the rule would run the old tree's code. Take its fields from
  `hp show <rule> --json`, `hp stop <rule>`, and create it again with the same arguments and the
  file's path under `<HIVE_PY>`; record both ids in `<BACKUP_DIR>/rules.list`.

An acceptance row is a watch child the old tree created; its check runs the old tree's
`hive.watch` on the same store (ADR-0068 section 5). Let it finish; new children name `<HIVE_PY>`.

Pass: every collector's next instance ends as before, and no open rule runs a moved file from the
old tree. Fail: an import error (a package the old interpreter had and the venv lacks: the
collector's own change, or part 3).

Roll back: 3.3.

### 2.10 The first 7 days

The old tree stays in place at `<LIVE_REV>`, unchanged (part 4). Every day, as `<HIVE_USER>`:

```sh
systemctl --user show -p NRestarts <DISPATCHER_UNIT>
sqlite3 -readonly <HIVE_DB> "SELECT event_id, error FROM outbox
  WHERE published_at IS NULL AND created_at < strftime('%s', 'now', '-10 minutes')"
sqlite3 -readonly <HIVE_DB> "SELECT id, wait_reason, substr(block_reason, 1, 120) FROM cards
  WHERE state = 'blocked' AND wait_reason IN ('stuck', 'process', 'judge', 'tag')"
hp board
```

Expected: `NRestarts=0`; no outbox row left unpublished; each blocked card of the new wait reasons
has a reason the owner accepts; the board matches what the owner expects.

Pass: 7 days so. Fail: a restart, a lost result, a card moved wrongly; record it and decide on
part 3.

## Part 3. Rollback

At any time from 2.4 on, as `<HIVE_USER>`. The old hive runs on the store and the config as the
moved one left them (ADR-0068 section 5).

### 3.1 What the old hive cannot carry

```sh
sqlite3 -readonly <HIVE_DB> "SELECT id, state, tag FROM cards
  WHERE tag IS NOT NULL AND state NOT IN ('done', 'cancelled')"
sqlite3 -readonly <HIVE_DB> "SELECT card_id, started_at FROM gate_jobs WHERE mode = 'judge'"
sqlite3 -readonly <HIVE_DB> "SELECT id, wait_reason FROM cards
  WHERE state = 'blocked' AND wait_reason IN ('stuck', 'process', 'judge', 'tag')"
```

- A tagged open card: the old hive knows no tag and would run its heads without it, a bypass of
  the tag. Before 3.2, `hp cancel <id> --reason 'hive rollback: tagged card' --by <OWNER_ACTOR>`,
  or `hp tag <id> --none` when it may run untagged. None exists before the qb pin of 2.3.
- A `judge` gate job: a judge turn in flight. Wait up to `[judge] timeout_s` for it to end; the old
  hive has no judge.
- A card blocked with one of these wait reasons: record it. The old hive has no path that moves it;
  after 3.5 the owner cancels it with `oh cancel` or creates it again.

### 3.2 Stop the moved hive

```sh
systemctl --user stop <STEWARD_TIMER> <ROTATION_TIMER> <DISPATCHER_UNIT>
systemctl --user show -p Id -p ActiveState <DISPATCHER_UNIT> <STEWARD_UNIT> <ROTATION_UNIT>
```

Expected: each `ActiveState=inactive`. Runs in flight keep running in their `hive-run-*` units, and
the old dispatcher reaps them.

### 3.3 Restore the units and callers

```sh
rm -f <HIVE_UNIT_DIR>/<DISPATCHER_UNIT>.d/50-hive-py.conf \
  <HIVE_UNIT_DIR>/<STEWARD_UNIT>.d/50-hive-py.conf <HIVE_UNIT_DIR>/<ROTATION_UNIT>.d/50-hive-py.conf
systemctl --user daemon-reload
for u in <DISPATCHER_UNIT> <STEWARD_UNIT> <ROTATION_UNIT>; do
  systemctl --user cat "$u" | diff "<BACKUP_DIR>/$u.before" - && echo "$u restored"
done
touch <BACKUP_DIR>/callers.list
while read -r f; do mv "$f.pre-hive-cutover" "$f"; done < <BACKUP_DIR>/callers.list
```

Then each rule of `<BACKUP_DIR>/rules.list`: `hp stop` the new one and create the old one again
with `oh create` and its old arguments.

Expected: three `restored` lines; every file of `callers.list` as before 2.5.

### 3.4 Start the old hive

```sh
systemctl --user start <DISPATCHER_UNIT> <STEWARD_TIMER> <ROTATION_TIMER>
pid=$(systemctl --user show -p MainPID --value <DISPATCHER_UNIT>)
tr '\0' ' ' < /proc/$pid/cmdline; echo
journalctl --user -u <DISPATCHER_UNIT> -n 30 -o cat
```

Expected: the old command line under `<SECRETARY_CHECKOUT>`; no traceback.

### 3.5 Check the old hive

```sh
oh list --json | ids > <BACKUP_DIR>/old.ids
db_ids <HIVE_DB> | diff - <BACKUP_DIR>/old.ids && echo "old lists the store"
oh create --title 'Hive rollback check' \
  --goal 'Write the single word rollback into REPORT.md and nothing else.' \
  --acceptance 'grep -qx rollback REPORT.md' --step 1 --source msg:hive-rollback \
  --created-by secretary
```

When the card ended, `oh show <ROLLBACK_CARD>`, and as the owner of `<LIVE_ROOT>`:

```sh
sqlite3 -readonly <LIVE_ROOT>/events.sqlite3 \
  "SELECT id, status, outcome FROM events WHERE id = 'hive:<ROLLBACK_CARD>:done'"
```

Expected: `old lists the store` (a card created between the two commands shows as a diff; run
them again); the card `[done]` through the old gate; one inbox row that reaches `done`, and the
secretary tells the owner the result.

Pass: all hold. Fail: a traceback of the old hive on the store (record it; a restore of 1.6 is the
last resort and loses every change since its backup).

### 3.6 What stays

- The column `tag` of `cards`: the old hive reads past it (ADR-0068 section 5); nothing is dropped.
- The extended config: the old hive ignores the new keys, so a second attempt needs no edit.
- The rows the moved hive wrote: `judge` events, the new wait reasons, run outcomes such as
  `tag_refused`.
- `<ULEI_CHECKOUT>`, `<HIVE_VENV>`, `<HIVE_TEXTS_DIR>`, `<BACKUP_DIR>`, the qb clients and aliases.

Roll back of the rollback: part 2 again from 2.4.

## Part 4. Retiring the old tree

From 2.4 on the hive changes only in `hive-py/` (ADR-0068 section 6.4): no carry flows any more,
`SOURCE` stays as the record of the port, and the move set of the old tree stays at `<LIVE_REV>`
and serves only part 3. A fix lands in `hive-py/`.

The old tree may be retired, and part 3 with it, when all hold:

- 2.10 passed and the owner has closed the window for a rollback (the old runtime of the MVP
  cutover keeps 30 days, `docs/roadmap.md`, "Cutover");
- the two queries of 2.9 return no open card that names a file of the move set;
- the greps of 2.5 find no caller of the old tree's `scripts/hive*.py`;
- `<HIVE_CONFIG>`, `<HIVE_TEXTS_DIR>` and every path the config names lie outside the move set;
  a config inside it is moved first and every absolute `HIVE_CONFIG` of 2.5 changed with it.

Retiring removes the move set from the secretary repository. The profile pack, its imports of
`hive.schedule` and the inbox library `inbox_library` names are not the move set: they stay while
a collector imports the module and while `delivery = "drop"`. Remove `<BACKUP_DIR>` then.

## Part 5. Later and optional: `delivery = "ulei"`

Not part of the cutover; the owner chooses it. Preconditions: part 2 passed, and `ulei-inbox`
serves `<LIVE_ROOT>` after step 2.6 of `mvp-pilot-cutover.md` with its 7 days passed. As
`<HIVE_USER>`:

```sh
stat -c '%a %U %n' <LIVE_ROOT>/producer.key
ls -l <RUNTIME_DIR>/ulei/inbox.sock
curl -sS --unix-socket <RUNTIME_DIR>/ulei/inbox.sock -H 'Content-Type: application/json' \
  -d '{"id": "hive:<TEST_CARD>:done"}' http://localhost/v1/get; echo
```

Expected: `600 <ULEI_USER>`; the socket; the C1 row of 2.7's event. The `ulei` adapter signs with
the 32-byte producer key of the root, which the ulei inbox accepts, and needs the hive's uid able
to open the socket. `<RUNTIME_DIR>` is `<ULEI_USER>`'s runtime directory, mode 0700, so this
holds when `<HIVE_USER>` is `<ULEI_USER>`; otherwise the change is the inbox unit's and not in
this runbook.

Then in `<HIVE_CONFIG>`, `cp -p` it to `<BACKUP_DIR>/config.drop` first:

```toml
delivery = "ulei"
inbox_socket = "<RUNTIME_DIR>/ulei/inbox.sock"
inbox_producer_key_file = "<LIVE_ROOT>/producer.key"
```

`inbox` and `inbox_library` stay for the way back. The key file must be mode 0600 and readable by
`<HIVE_USER>`: the root's own `producer.key` when `<HIVE_USER>` owns `<LIVE_ROOT>`, a copy of it
otherwise. Load the config as in 2.1 (`config ok ulei`), `systemctl --user restart
<DISPATCHER_UNIT>`, and run 2.7 again.

Expected: as 2.7; the dispatcher's journal has no `C1Error`.

Pass: as 2.7. Fail: a load error naming `inbox_socket` or `inbox_producer_key_file`, or an outbox
`error` with a C1 code; roll back.

Roll back: `install -m 0600 <BACKUP_DIR>/config.drop <HIVE_CONFIG>`, `systemctl --user restart
<DISPATCHER_UNIT>`. Outbox rows that failed meanwhile stay unpublished and go out through the drop
adapter on the next ticks; the packet bytes are the same, so a packet both adapters published
dedupes. Part 3 of `mvp-pilot-cutover.md` stops `ulei-inbox` and its socket: roll this part back
with it.
