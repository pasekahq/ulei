# ADR-0068: The Python task hive moves into `hive-py/`, the Rust rule for it and its board reform, its CI lane, data rule and compatibility promise

Status: accepted
Date: 2026-11-23
Affects: crates/hive, hive-py, xtask, .github/workflows/ci.yml, dep:py:cronsim, dep:py:pytest

## Context

The task hive (cards, heads, gate, schedules and series, steer, delivery of results) runs today
from the private secretary repository: the Python package `hive/`, its CLI `scripts/hive.py` and
its tests `tests/hive/`. Its only third-party runtime dependency is `cronsim`; its tests run
under `pytest`. It keeps its state in one SQLite store (every card transition a compare-and-swap
inside `BEGIN IMMEDIATE`), reads one TOML config (`HIVE_CONFIG`, `HIVE_DB`), and writes under a
live root: the store, the card workspaces, the run directories with `job.json`,
`last-message.txt` and the `REPORT.md` or `REPORT-<label>.md` a head leaves, the result files,
the watch continuity files and their locks. It delivers results by publishing the C1 packet of
an outbox row into the owner inbox through the inbox library of the secretary repository.

The owner decided to move the hive into this repository and to reform its board in the same
sprint. Root `AGENTS.md` says all code here is Rust and Python is reached only over the wire
(hard rule 5); ADR-0001 decision 5 limits Python here to stdlib CI tools under `ci/`;
`crates/hive` is the S0 skeleton of C3; the roadmap puts a Rust hive next to the Python one at
step P6. The hive source holds deployment specifics and personal data in code, comments, tests
and its config; this repository is public.

## Decision

### 1. Where the hive lives

1. The hive moves into the top-level directory `hive-py/`, which keeps the relative layout of
   the source so its imports and its own `sys.path` root (`parents[1]` of an entry point) work
   unchanged: `hive-py/hive/` (the package, still imported as `hive`), `hive-py/scripts/hive*.py`
   (the CLI and the hive's other scripts), `hive-py/tests/hive/` (the tests). It also holds
   `hive-py/AGENTS.md`, `hive-py/module.toml`, `hive-py/requirements.lock`,
   `hive-py/requirements-test.lock`, `hive-py/config.example.toml` and `hive-py/SOURCE`
   (section 6). The move set is these three source paths less the profile pack of section 2.3
   and the files the table below keeps private; it is also the carry set of section 6.2.

   Every file outside `hive/`, `scripts/hive.py` and `tests/hive/` that the hive or its tests
   reach has this place:

   | Source file | Reached by | Place |
   |---|---|---|
   | `scripts/hive_steward.py` | `test_steward.py`; the daily steward unit | Moves to `hive-py/scripts/` with `hive/steward.py` and its tests. It is a daily one-shot run, not a long-lived observer: it proposes cancellations of stale cards, cancels through the CLI like any caller and writes a digest. It never decides "done" or "rework"; those verdicts belong to the judge of reform item 3 only. The judge card moves the steward's model call onto the judge's C4G client by a tier, so no model call of the hive bypasses qb. Its `alert` publishes a `task_result` packet through the inbox-library loader of section 2.5; when reform item 5 lands, the item-5 card reroutes that alert through the delivery interface like every other result. |
   | `scripts/hive_worktree_rotation.py` | `test_worktree_rotation.py`; its timer unit | Moves to `hive-py/scripts/` with its test: it works on the card workspaces of the live root only. |
   | `scripts/hive_migrate_cancellations.py`, `scripts/hive_migrate_classes.py` | `test_cancel.py` (the first) | Move to `hive-py/scripts/`: audited, idempotent migrations of the hive store, kept for the store they wrote. |
   | `deploy/hive/claude-hive` and its test `tests/hive/test_claude_hive.py` | the config's head binary list; the steward | Stay private with the deployment: the launcher holds the deployment's logins and fallback policy. The hive starts it only as a configured binary; item 4 hands it the tags through that launch. |
   | The other `deploy/hive/*` files (dispatcher, steward and rotation units and timers, their drop-ins, the install scripts, the space-prep unit) | the deployment | Stay private with the deployment; the cutover step decides the units and the install of the moved hive. |
   | `brain/event_inbox` (the inbox library) | the dispatcher's drop adapter; the steward's `alert` in `scripts/hive_steward.py`; `test_hive.py` and the tests that import its fixtures; `test_steward.py`, which reads the inbox's `events.sqlite3` and `Inbox.payload()` | Stays private with the deployment. In production the drop adapter and the steward alert load it through the one config-keyed loader of section 2.5. In `hive-py/tests/` the fake inbox of section 2.2 replaces it. |
   | `hive/config.toml` (the live config) | `store.load_config` by default; four tests: two in `test_hive.py` (one asserts `executors.dots.timeout_s == 3600`) and two in `test_queue_admission.py` | Stays private (section 4.5). All four tests load `hive-py/config.example.toml`, which carries the values they assert, such as `executors.dots.timeout_s = 3600`. |
2. `hive-py/` is not under `crates/`: `crates/` holds only cargo packages, and every directory
   there with a `Cargo.toml` must stay a workspace member. `hive-py/` is no cargo package and no
   workspace member.
3. `crates/hive` stays the Rust module of C3 and the place of step P6. The two share no code,
   no build and no files; `crates/hive` never imports, links or spawns anything from
   `hive-py/`, and `hive-py/` never calls Rust code except over a wire of section 2.5. This ADR
   does not change C3. At P6 the Rust hive ports the behaviour of `hive-py/` as it stands then,
   including the reform of section 2.6, runs next to it in shadow and takes over by a flag
   (ADR-0001 consequences); P6 gets its own ADR, which derives C3 from the hive's store, report
   and CLI formats.
4. `cargo xtask arch` treats `hive-py/` as follows. The crate-graph rules do not see it, as they
   read `cargo metadata`. The source bans that are not Rust syntax also scan
   `hive-py/**/*.py` and `hive-py/**/*.toml`: no personal paths (`/home/`, `~/.`), no mention of
   Hermes, no model-vendor hosts or vendor credential paths. `hive-py/module.toml` declares
   `role` and `contracts` like `[package.metadata.ulei]`; `arch` fails on a contract id that is
   not in `contracts/index.json`, and on a package in `requirements.lock` other than those
   admitted in section 3.5.
5. `cargo xtask module-map` renders a table "Python trees" with one row for `hive-py`: path,
   role and contracts from `module.toml`, packages from `requirements.lock`. The contracts table
   lists `hive-py` among the users of the contracts it names. The crate graph is unchanged.

### 2. "New code is Rust, Python only over the wire" applied

1. Moved code as is. `hive-py/` is the one exception to "all code is Rust", granted to the hive
   until P6 replaces it; ADR-0001 decision 5 is amended to allow it next to the stdlib tools of
   `ci/`. No other Python component moves into `hive-py/` or into the repository on its
   strength; a new module of the ecosystem is Rust. The ADR-0001 budgets bind Rust binaries
   and do not apply to `hive-py/`; the Rust hive of P6 carries them.
2. Edits at the move. The move changes the code only where this ADR requires it: the scrub and
   the config path of section 4, the profile-pack cut below, import paths, and the test edits
   here. The tests stop importing the real inbox library: a fake inbox in `hive-py/tests/hive/`
   gives the three calls the hive and its tests make (`packet`, `publish`, `get`) and keeps the
   packets it receives for the tests to read. The tests' reads of `Inbox.payload()` become
   reads of the fake's kept packets, and the steward test reads those kept packets instead of
   the inbox's `events.sqlite3`. The golden fixture of section 5 pins the real
   owner-inbox packet format the fake must match. A test that asserts on a key named after the
   old runtime asserts the neutral property instead (the head environment holds only the keys
   the config grants). The move changes no behaviour otherwise; the moved tests pass on the
   moved code.
3. Profile pack. A module whose subject is one of the owner's own sources or the migration of
   the owner's jobs off the old runtime (today the collectors of one issue and of the delegation
   watchdog, and the install and replay tools of that migration) is profile pack (ADR-0002) and
   stays in the secretary repository with its tests. It reaches the moved hive as today: run by
   the watch tick as a script card from its configured path, answering with its
   `{"signals": [...]}` stdout. The generic watch modules (tick, gate, shared collector
   plumbing) move.
4. Fixes. A fix of existing behaviour is Python, in the `hive-py/` module it fixes, with its
   test in `hive-py/tests/`. A fix is never a Rust rewrite of a module. A fix the live hive
   needs before the cutover follows section 6.3.
5. Wires out of `hive-py/`. The hive reaches ulei and qb only over their contracts: C1 `publish`
   on `inbox.socket` and the C4G turn on the qb gateway socket, both clients in the standard
   library. One out-of-tree import remains: one loader of the deployment's inbox library, keyed
   by the config key that names its directory, used by the drop adapter of item 5 below and by
   the steward's `alert`; the moved code computes no second path to it. It goes away when item 5
   moves both onto the delivery interface. No other `sys.path` entry leaves `hive-py/`.
6. The reform. Each item lands in Python in `hive-py/`. None is ported to Rust in this step:
   every item reads and moves cards in the one store, whose writers keep one discipline (a
   compare-and-swap per transition inside `BEGIN IMMEDIATE`); a Rust piece would be a second
   writer of a schema no contract describes, ahead of P6, which ports the whole hive at once.

| Item | Language and place | Why there |
|---|---|---|
| 1. Board: columns "In progress", "Waiting" with a reason badge (another card, an external dependency, a queue for a slot) and "Done" for about the last 2 days; the rest archived; schedules and series off the board, their runs in the rule's history | Python: the board projection `api.board` in `hive/api.py` and the CLI `board`, its mapping in `hive-py/AGENTS.md`; the archive is computed from the state and the finish time, with no stored flag; the rule history is the rule's `instances` in `show` | The columns are a projection of the existing states, `wait_reason`, holds, classes and links; the reader that already queries them is the API the CLI and the secretary call. |
| 2. "In progress" while the card's process (its runner's unit or cgroup) lives; agent exited leaving a long process means "Waiting" with a wake when that process ends; no process and no result is a failure with a new round carrying the break context; 2 failures in a row mean "Waiting/stuck" and an event to the secretary | Python: the dispatcher's reap and advance step (`hive/dispatcher.py`) with the process probe `hive/liveness.py`, and the runner (`hive/runner.py`), the event through item 5; the stuck state is `wait_reason` `stuck` with one `task_blocked` event | The dispatcher already owns `pid`, `unit`, heartbeat and round per run and is the only component that moves cards; liveness is a read of the unit or cgroup files of the user manager. |
| 3. A one-shot judge moves statuses: the mechanical check, then one model call through qb by a tier, verdict "done" or "rework" with a reason; no long-lived observer | Python: a judge module in `hive-py/hive/` called by the dispatcher after the gate; its model call is one C4G turn on the qb gateway socket whose `route` is the tier alias from the hive config | One request without tools or a session needs no agent loop; the C4G contract is vendored and language-neutral, so a stdlib client is the wire use hard rule 5 allows. No provider or model name enters hive code or config; qb resolves the alias. The long-lived observer no longer moves statuses. |
| 4. The card's user tags go to qb in the request; qb's "no login for this tag" refusal is not bypassed by another route; the card goes to "Waiting" with the reason | Python: the judge's C4G request and the head launch, which hands the tags to the configured launcher that reaches qb; the refusal maps to "Waiting" in the dispatcher | The hive owns the card and its tags and makes or launches every request. The request field and the refusal class come from qb's contract at a pin: a qb pin bump with the vendored C4G (hard rule 4) before this item lands. The qb side is not in this ADR. |
| 5. Results reach the secretary through an adapter: today's owner-inbox drop, later ulei events, switched by config without editing the hive core | Python: one delivery interface in `hive-py/hive/` with two adapters, `drop` (today's publish through the inbox library) and `ulei` (C1 `publish` on `inbox.socket`), chosen by one config key | The outbox and its publish loop are the dispatcher's; the core hands an outbox row to the interface and never names an adapter. The `ulei` adapter is a wire client, so no Python copy of the inbox enters the repository. This card also reroutes the steward's `alert` through the interface, which retires the inbox-library loader of section 2.5. |

### 3. CI lane

1. Job `hive-py` in `.github/workflows/ci.yml`, on `ubuntu-24.04`, on every push and pull
   request. Interpreter: the image's `/usr/bin/python3`, CPython 3.12, the version the hive's
   tests run on in the secretary repository; the job fails when `sys.version_info[:2]` is not
   `(3, 12)`. A version change needs an ADR.
2. Packages. `hive-py/requirements.lock` holds the runtime package, `cronsim` at `==2.7`, with
   the `--hash=sha256:` of its distribution file; it is the only lock the live host installs.
   `hive-py/requirements-test.lock` holds `pytest` and the packages it requires at exact versions
   with hashes; they never reach the live host. The job creates a venv under `$RUNNER_TEMP` and
   runs `pip install --require-hashes --no-deps` on both files; that step is the only one with
   network.
3. Tests run with no network: `python -m pytest hive-py/tests -q -p no:cacheprovider` inside a
   network namespace with loopback only (`sudo unshare --net`, back to the runner's uid and gids
   with `setpriv`), so a test that reaches outside the host fails. Tests need no inbox library,
   qb or vendor CLI: the drop adapter runs against a fake library in `hive-py/tests/`, the
   judge against a scripted fake gateway on a unix socket, heads as fake head scripts.
4. `python3 -m ci.broad_check` stays stdlib only and does not run the hive suite;
   `hive-py/AGENTS.md` names the local command, the same two installs and the same pytest call.
5. Admission of a package. A runtime or test package enters a lock only with an ADR whose
   `Affects:` names `hive-py` and `dep:py:<name>`, giving version, license and why the standard
   library does not do it; the packages an admitted test runner requires come with it. Its license must be in the allow list of `deny.toml`; the change that
   adds it writes the license id next to the pin. `cronsim` and the test packages are admitted
   here: `cronsim` computes the cron cursor of schedules and series (`hive/schedule.py`), and
   `pytest` runs the moved tests unchanged.

### 4. Personal data

Hard rule 7 holds for every file under `hive-py/`:

1. Code, comments and docstrings hold no home paths, host names, user ids or uid-bound cgroup
   paths, unit names of the deployment, chat ids, account or login names, owner names or
   quotes, real card or work-item ids, or dates. Comments that tell an incident are cut to the
   reason the code needs (code style rules 1 to 3).
2. Every deployment value that is a constant in the source becomes a hive config key without a
   default: the directories heads and scripts read, the owner time zone, the cgroups the
   admission samples, the urgent sources of admission, vendor homes and binaries, the
   environment of script cards, the inbox library directory and socket. A missing key is a
   config error.
3. Text the hive gives a head or the secretary that addresses the owner by name or carries
   deployment rules (the workspace context, the report request, the series news note) comes
   from files the config names; the repository holds a neutral sample of each.
4. Tests and fixtures use synthetic values only: paths under the test's temporary directory,
   ids of the fixture's own shape, time zones such as `UTC`.
5. `hive-py/config.example.toml` names every key with a synthetic value. The live config is not
   in the tree; the hive reads it from `HIVE_CONFIG` and fails without it.
6. The files are copied after the scrub, never merged with their history: the source history
   holds the raw values, and gitleaks scans this repository's history. The first commit under
   `hive-py/` holds scrubbed content only.

### 5. Compatibility promise

The moved hive runs on the live data unchanged, and the hive of the secretary repository keeps
running on the same data after the moved one touched it, so the cutover can roll back.

| Format | Promise |
|---|---|
| Live root layout | Byte-compatible paths: the store, workspaces, run directories, `job.json`, `last-message.txt`, `REPORT.md` and `REPORT-<label>.md`, result files, watch continuity files and their locks keep their relative paths and names. |
| SQLite store | Schema-compatible: tables, columns, types, constraints, indexes and the meaning of every value stay. A change is additive only, through the existing `CREATE ... IF NOT EXISTS` and add-column path: a new table, index or nullable column. Never a rename, drop, retype or new meaning. A new value in an existing column is allowed only where the old hive reads it safely; otherwise it goes into a new column. |
| TOML config | Every existing key keeps its name, type and meaning. New keys (section 4.2, the delivery adapter, the judge's tier) are added at the top level or in new tables, which the old hive ignores, so one file serves both; the cutover step adds their values. |
| C1 packets of the drop adapter | Byte-compatible: for the same outbox row the moved hive publishes the same canonical packet bytes, so a packet both hives publish dedupes on its digest. A golden fixture in `hive-py/tests/` pins them: the arguments the drop adapter passes for a synthetic outbox row and the packet the real inbox library makes of them, recorded with that library at the revision in `SOURCE`. The fake inbox of section 2.2 must give the same packet, and the fixture validates against C1. |
| JSON files read by both hives (`job.json`, continuity, run records) and the collector stdout `{"signals": [...]}` | Schema-compatible: same keys and types; a new key is optional to the reader. |
| CLI `scripts/hive*.py` | The commands, flags, exit codes and the keys of every `--json` output stay; new flags and keys are additive. Every caller (units, timers, head instructions, the secretary) sets `HIVE_CONFIG`, as the hive fails without it (section 4.5); the cutover step checks this. |

### 6. Source revision and changes before the cutover

1. The port takes one revision of the secretary repository; `hive-py/SOURCE` holds its commit
   id, and the move changes only what section 2.2 allows.
2. Until the cutover, hive changes flow one way, from the secretary repository into
   `hive-py/`. Every change there to the move set of section 1.1 (`hive/`, `scripts/hive*.py`,
   `tests/hive/`, less the profile pack of section 2.3 and the files section 1.1 keeps
   private) after the recorded revision is carried over by a carry change in this repository:
   the source diff from the recorded revision to the new one, limited to those paths, applied
   with the scrub of section 4, its tests with it, and `SOURCE` advanced in the same commit. A
   conflict with reform code is resolved there, keeping both behaviours.
3. A fix the live hive needs lands in the secretary repository first and reaches `hive-py/` by
   a carry. A fix or reform change only the moved hive needs lands in `hive-py/` only and never
   flows back.
4. The cutover step starts only when the diff of those paths from `SOURCE` to the revision the
   live hive runs is empty. From then on the hive changes only in `hive-py/`, and the old tree
   serves only the rollback.

### 7. Order

1. Move: `hive-py/` with the scrub, the locks, `module.toml`, `config.example.toml`, `SOURCE`,
   `hive-py/AGENTS.md`, the `arch` and `module-map` changes of section 1, the CI job of section
   3, and the rows of root `AGENTS.md` and README.
2. The delivery interface with its two adapters (item 5), since items 2 and 4 emit through it.
3. Liveness and failure rounds (item 2), the board projection and archive (item 1), the judge
   (item 3), tags (item 4, after the qb pin that carries the field).

## Consequences

- The repository holds one Python tree besides `ci/`, bounded to the hive and to P6.
- Hive fixes before the cutover cost a carry each; the one-way flow keeps the two trees from
  diverging both ways.
- The live config gains keys at the cutover; the old hive ignores them, so rollback keeps one
  config file.
- The cutover runbook, the hive's service units and the live install are the cutover step's.
