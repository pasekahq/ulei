# ADR-0086: Read-only snapshots of the hive: `hive_inspect`, `hive_health`, `hive_file`

Status: accepted
Date: 2026-11-26
Amends: ADR-0071 (section 4, the tools; section 5, the argv; section 7, "no field names a path").
Affects: crates/hive-mcp, contract:C2, hive-py

## Context

On ulei the secretary reaches the hive only through `ulei-hive-mcp` (ADR-0071). Its `hive_show`
answers a card's id, state, round, title and two first lines; the engine's sandbox (ADR-0069)
hides the hive's store, CLI and card workspaces, by design. The secretary was therefore blind to
everything the owner asks about the hive's work:

- whether a running card works or hangs: the head's systemd unit, the command it waits on and
  for how long, the units it started (a swarm unit, a `systemd-run`), the pull request or CI run
  it waits on and the state of its checks. The secretary had to walk the unit's cgroup and
  `/proc/<pid>/cmdline` by hand, which it cannot do from its sandbox at all;
- why a round was sent back: the gate's result, the judge's or reviewer's verdict and reason;
- the history of a card: every round's report, start and end, who ran it (worker or reviewer,
  model, effort);
- which run and unit belong to which card;
- why a queued card does not start: the slot limit and its urgent reserve, who holds each slot
  (series runs and reviewers too, which the board does not show), the queue position, holds,
  backoff, blockers and budget waits;
- series: their rules, next starts, runs, and whether one holds a slot;
- chains: parent, children, `followup_of` and followups, so that a new card does not duplicate a
  running one;
- the card's workspace: `REPORT.md`, `ev/`, the results files its goal names;
- the hive's limits and health: concurrency, the ladder, whether the dispatcher ticks;
- a fast, filtered list and a text search;
- the delivery of results: which events for the secretary are published, which the owner inbox
  holds, and the secretary's own ACK (`success`, `silent`, ...);
- each steer's text next to its delivery status.

All of it is in the hive's store, the user manager's cgroups and `/proc`, which the hive's CLI
reads as the hive's user. None of it needs a write, a stream or a new daemon.

## Decision

### 1. The hive's CLI: `inspect`, `health`, `file`, list filters (`hive-py/hive/inspect.py`)

- `hive.py inspect CARD --json`: one snapshot of the card at the time of the call (version 1):
  `card`, `links`, `rounds` (per round its runs with role, vendor, model, effort, unit, run
  directory, start, end, outcome, verdict, error and report cut to 6000 characters; the gate;
  the judge's verdict and reason; the round's `retry`, `blocked`, `done` and similar events),
  `heads`, `queue`, `steers` (text cut to 4000 characters, delivery, method), `deliveries` and
  `files`.
- `heads` is the pulse of each `starting` or `running` run: its unit's cgroup (populated,
  processes), the commands the head runs (the processes two levels under its runner, oldest
  first, each with its deepest descendant), the other services of the user manager those command
  lines name (a wait loop on a unit the head started), and the pull requests (`gh pr checks N`,
  `gh pr view N`, a `/pull/N` URL) and workflow runs (`gh run watch N`, `gh run view N`) they
  name, at most three, each looked up once with `gh ... --json` within 20 s; `waiting_on` is the
  longest of those commands. Command lines are read from `/proc` (16 KiB scanned, 1500 characters
  shown, a vendor CLI's shell-snapshot preamble before `eval '` dropped). `--no-github` skips the
  lookups. Nothing is signalled, traced or attached to.
- `queue` is the card's board place and wait (`api.placement`, `api.waiting_since`), its
  position in the dispatcher's launch order (admission class, reviewers first, priority, age),
  the slot limit for its class (`concurrency` minus or plus `admission.urgent_reserve`) with every
  run holding a slot, and the dispatcher's last `budget_wait` note while the card still waits.
- `deliveries` is each outbox row with, once published, the owner inbox's row through the
  delivery adapter's new read `status(event_id)` (`drop`: the library's `get`; `ulei`: C1 `get`):
  status, attempts, outcome. The hive reads the inbox only through its adapter, as before.
- `hive.py health --json`: the config's limits without paths, environments, commands or texts;
  the dispatcher's last tick (`meta.dispatcher_tick`) and whether it is younger than 120 s; the
  resource gate; weekly limits; provider cooldowns; cards by state; every run holding a slot,
  scripts, external jobs and gate jobs; the launch queue; every open series with its rule, next
  start, last three runs and the runs holding a slot; the outbox.
- `hive.py file CARD PATH [--offset N] [--limit N]`: one file (JSON, at most 256 KiB per call) or
  directory listing of the card's workspace, or `@out/<name>` for a results file the card's own
  goal, body or acceptance names. Refused: an absolute path, `..`, a symlink anywhere on the way,
  a name that looks like a secret (`.env`, `token`, `key`, `credentials`, ...), a results file
  the card does not name. Another card's workspace cannot be reached by a path: the card id
  picks the root.
- `hive.py list` gains `--class`, `--parent`, `--followup-of`, `--creator`, `--placement
  board|hidden`, `--query` (case-insensitive substring of the title or goal) and `--limit`. The
  store is read through its existing indexes and one bounded query; nothing is migrated.

Each is one store read and no write: the snapshots open the store as `show` does.

### 2. The MCP tools (C2 0.4.0, additive)

| Tool | Input | argv after `hive_command` | Output |
|---|---|---|---|
| `hive_inspect` | `CardRef` | `inspect --json -- <id>` | `HiveInspectOutput`: `card_id`, `state`, `round`, `title`, `result_line`, `report_line`, `waiting_on` (one line, at most 500 characters) and `snapshot`, the CLI's object |
| `hive_health` | `Empty` | `health --json` | `HiveHealthOutput`: `ticking`, `tick_age_s`, `slots_used`, `concurrency`, `queue_length` and `snapshot` |
| `hive_file` | `HiveFileInput` | `file [--offset=N] [--limit=N] -- <id> <path>` | `HiveFileOutput`: `path`, `size`, `offset`, `text`, `truncated` or `entries` |
| `hive_list` | `HiveListInput` with `card_class`, `parent`, `followup_of`, `created_by`, `placement`, `query` | `list --json [--state=S] [--class=C] [--parent=ID] [--followup-of=ID] [--creator=A] [--placement=P] [--query=Q] --limit=N` | `HiveListOutput` as before |

`snapshot` is a free JSON object in C2: its fields are described in the definition and versioned
by the CLI (`version`), so the hive may add a field without a contract release; the summary
fields beside it are typed. `hive_list` now passes its limit to the CLI. `created_by` of
`hive_list` is a filter, passed as `--creator`, never `--created-by`: the server still never sets
a card's creator from an input. `no such card` of `inspect` and `file` is the tool error `no
such card <id>`; the CLI's `hive: <text>` refusal of `file` is the tool error `<text>`.

### 3. Amendment of the threat model (ADR-0071 section 7)

- `hive_file` is the one field that names a path. It is read-only, rooted at the card's
  workspace (or at the results directory for a name the card's own text holds), refuses `..`,
  symlinks and secret-looking names, and reads at most 256 KiB per call; the server also refuses
  an absolute path and `..` before it runs the CLI. The workspaces are the secretary's own
  delegations: what a head wrote for it. The run directories under `.hive/` (prompts, the
  vendor's logs) are listed and readable like the rest; a head that prints a secret into its
  log exposes it to the secretary as it already did to the owner's board.
- `hive_inspect` shows command lines of the heads' processes. They are the hive user's own
  processes started for the secretary's cards; the hive's environment variables are not read,
  and no other user's processes are listed.
- `hive_health` shows the config's limits only: never `head_env`, `gate_env`, `script_env`,
  paths, commands, texts or the delivery's keys.
- No tool writes, signals, steers, starts or stops anything; the server's guarantees of ADR-0071
  section 7 hold for the five earlier tools unchanged.
- The `gh` lookups run as the hive's user with its own credentials, read only (`gh pr view`,
  `gh run view`), for at most three numbers the heads' own command lines name.

## Consequences

- The secretary answers "is it working or stuck", "why was the round sent back", "why does the
  queue not move", "did the result reach me" from one call, without the owner.
- The secretary's sandbox stays closed: no bind mount of the hive's directories into the engine;
  the workspace is read through the same audited door as the other hive tools.
- One CLI run at a time still serialises the tools (ADR-0071): a snapshot takes about 0.3 s on
  the live store, up to 20 s more per looked-up pull request when GitHub is slow.
- The hive's Rust rewrite (P6) keeps the CLI's JSON shapes or bumps `version`.
