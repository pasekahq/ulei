# Results, acceptance, loop detection and owner capacity

Worker and synthesis reports start with a single sentence, for example:

    RESULT: done — nothing is needed from the owner.
    RESULT: partial — owner: confirm the remaining scope.
    RESULT: needs-owner — owner: grant access to the project.

The first line is the first payload field (`result_line`), and `report_excerpt`
contains that line plus the last 2500 characters for long reports. Short reports
are preserved whole. This is a report contract, not a new card state; historical
reports remain readable without a RESULT line.

Every outbox row reaches the secretary through the delivery interface `hive/delivery.py`. The
dispatcher's publish loop picks the row's destination (`owner-inbox` or `local`) and hands an
`owner-inbox` row to the adapter the required config key `delivery` names as one event: the row's
kind, event id and source time, and its payload with the card's `card_class` and `owner_visible`,
`result_line` cut to 2000 and `report_excerpt` to 2500 characters, and the last worker report as
`text` (cut to 8000 characters, with `text_truncated`). The steward's model-error alert goes the
same way, once per error. The adapter `drop` publishes the C1 packet through the deployment's
inbox library (the module `inbox` in the directory `inbox_library` names, opened on `inbox`) and
reads it back through `get`. The adapter `ulei` signs the same packet bytes with the 32-byte key in
the owner-only file `inbox_producer_key_file` names and sends one C1 `publish` on the owner inbox's
socket `inbox_socket` names; an event counts as delivered when C1 `get` returns its row. Only the
selected adapter's keys are required, and a config naming any other adapter is refused at load.
An adapter failure (for `ulei` also a non-2xx C1 `Error` such as `conflict`, an unreachable socket
or an answer outside C1) leaves the row unpublished with `error` set to the exception's class and
message, and the next publish tries it again. A delivered row gets a `secretary_event` event, a
`local` row a `result_delivered` one.

`api.validate` rejects bare `test -s REPORT.md` (also quoted paths and the `[ ]`
form) without review or judge criteria. Use `--accept-review CRITERIA` (the judge
checks the report; no reviewer), `--review`, or write a command that checks the
deliverable. Existing historical cards are not rewritten.

Review is off by default: only `--review` (`review=True`, hive-mcp `review: true`)
turns on the independent reviewer. `--accept-review`, the card's kind, class, step
and the config never do; the card's author decides.

The dispatcher persists a gate claim and detaches `python -m hive.gate`. Both
initial acceptance and scheduled rechecks execute outside the tick process.
`--gate-timeout SECONDS` / API `gate_timeout` overrides `gate_timeout_s` for a
card and is inherited by series instances. On deadline the complete shell process
group is killed. A stale worker result cannot change a cancelled or newer card
version. Already running acceptance is allowed to finish after cancellation, so
acceptance commands should observe results rather than mutate external systems.
Claims survive dispatcher restarts and expire after timeout + 90s startup grace.

An acceptance exits 0 for true, any other code for false, and 75 (EX_TEMPFAIL) for «do not
know»: the external system it asks is unreachable, or its answer is unreadable. Unknown is no
verdict. The gate keeps the card in `check` with `retry_at` one `recheck_every` (900 s without
it) ahead, and a due recheck keeps the card `done` with `recheck_due` moved one `recheck_every`
on and the attempt not spent. Either records a `gate_unknown` event (`round` or
`recheck_attempt`, `unknowns` in a row, the next time, the output tail) and no gates row, no
`retry`, no `blocked` and no `task_blocked`. Unknown is decided as a failure only at the 96th
unknown in a row of the same round or attempt, or when the next ask would fall at or past
`recheck_until`.

A failed gate fails the round with its output as feedback and calls no model. A head card whose
gate passed goes to its reviewer if it has `--review`, then to the one-shot judge
(`hive/judge.py`); a script card and an epic finish on the gate, and a round an external
executor ran finishes on the gate or, with `--review`, on its reviewer's PASS or FAIL. The judge
runs detached under a `gate_jobs` claim of mode `judge` (the gate worker whose gate passed takes
it over in the same process; otherwise the next tick detaches it) and sends one C4G turn
(`hive/c4g.py`) to the qb gateway socket `gateway_socket`, on the route alias `[judge] routes`
maps from the card's step, with `[judge] max_tokens` and `[judge] timeout_s` for the whole turn.
The turn carries the card's user tag in the field `tag` when the card has one (`create --tag`,
`hive.py tag`), and has no such field otherwise. Its prompt holds the task (title, goal,
details, acceptance command, `judge_criteria`), the worker report of the round, the gate output,
and the reviewer's verdict and text. The first `VERDICT:` line of the answer decides: `done`
finishes the card (a steer that arrived meanwhile queues a steer round instead); `rework`
followed by a reason fails the round through the normal retry path with the reason as the next
worker's feedback, spending `max_rounds` and taking backoff. A failed turn is no verdict, and
nothing else is tried: no other route, vendor or CLI, and never the request without its tag. A
retryable class keeps the card in `check` with `retry_at` at the gateway's `resets_at`, else
now + `retry_after`, else now + `[judge] retry_s`, and the judge runs again from then with the same
route and tag. A non-retryable class (among them `no_login_for_tag`, and `bad_request` from a qb
that does not know the `tag` field), or an answer without a verdict (or a `rework` without a
reason), blocks the card with `wait_reason` `judge` and a `block_reason` naming the class
(`unparseable` for the answer) and, for a tagged card's failed turn, the tag. A transport or
protocol failure of the gateway counts as retryable `unavailable`. Every judgement writes one
`judge` event with the route, the tag if any, and the verdict or the failure.

A tagged card's heads carry the tag the same way, through the configured launcher, by the
launcher contract of ADR-0068 section 2.6, item 4. Every head of the card (worker, reviewer,
panel member, synthesis) runs only on a vendor whose `[vendors.<v>] tag_launcher` is true, in
every pick: worker, reviewer, provider failover, the assignee's `on_unavailable` fallback and
`review_same_vendor_fallback`; it gets the tag in `HIVE_TAG`, which an untagged card's heads
never get. When none of the vendors a head may choose is enabled and tag-capable, the card
blocks with `wait_reason` `tag` and a `block_reason` naming the tag and the vendors; a
tag-capable vendor held back by budget, cooldown or limits keeps the card waiting for it. A
launcher refusal blocks the card with `wait_reason` `tag` and a `block_reason` of the form
`launcher <class> for tag <tag>: <message>`; it is no provider failure, and no other vendor,
retry or run without the tag follows. Any other failure of a tagged head takes the normal
retry path (stuck rule included) on tag-capable vendors. `hive.py cancel CARD` ends such a wait.

Two consecutive identical gate failures (exit and full-output fingerprint) or
fresh worker/synthesis reports stop the card in failed `blocked`, with a
`loop_stopped` audit event. No round cap is introduced. A new steer between
attempts resets the comparison. The normal `task_blocked` event includes
`loop_stopped=true` and goes to the secretary inbox even for local-delivery cards.
The normal audited cancellation still closes such cards.
Cards with `recheck_every`, recurring containers, and their instances are exempt
from both identical-result stops: unchanged results are expected while waiting.
Normal retry limits, recheck bounds, cancellation and timeouts still apply.

A run is active while its process lives, whatever its heartbeat: `hive/liveness.py` reads
`cgroup.events` of the run's unit under the user manager (systemd launcher) or checks the
runner's pid (popen). The board shows a `running` or `review` card in progress only while
such a run lives, and a `check` card only while its gate job runs.
A run whose process is gone before its runner wrote the exit record
and report has no result; the reaper ends it at once (a `starting` run after
`spawn_grace_s`). A run `heartbeat_stale_s` (at least 120 s) past its step timeout is
killed and ended as a `timeout` without a result, even when its runner caught the stop and
recorded an exit; a head the runner itself stopped at the step timeout (status `timeout`)
has no result either. Such a head run fails its round through the normal retry path, and
the next worker's feedback is the break context: the reason, the run id and the last 20
lines (at most 2000 characters each) of its `head.log` and of its last message. The second
such round in a row, with no round that produced a result between them, blocks the card
with `wait_reason` `stuck` instead, and its one `task_blocked` event carries `stuck`: the
two runs with their reasons. `hive.py cancel CARD` takes a card out of `stuck`.

A worker head of a card without a panel that exits on its own may leave processes running,
for example a long build. Its runner stays the main process of the run's unit until the
unit's cgroup holds only the runner, so systemd, which kills the cgroup only when the main
process exits or the unit is stopped, leaves them running, and no step timeout applies to
them. Only a head that handed its work to such a process waits on it: by the workspace rule
for a long step, its report has no `RESULT:` first line (case, leading markdown marks and a BOM
aside), or one whose status right after `RESULT:` is a placeholder (`RESULT: running in the
background`); a finished line that names a process further on (`RESULT: done — the browser is
left running`) is a result. Leftovers of a head that wrote its result, such as
the browser of its browser tool, MCP servers or tool daemons, are not waited on. A stop of the
unit is queued with `systemctl --user stop --no-block`, which ends every process in its cgroup,
the runner included, and recorded as a `process_stopped` event with the run, the round, the
processes (pids and command lines) and `gate_passed`; a stop that could not be queued is a
`process_stop_failed` event with the error instead, and the gate tries again. When the
dispatcher handles a run's exit record while the unit's `cgroup.procs` lists other processes:

- a report with a `RESULT:` first line: the unit is stopped (`gate_passed` null) and the round
  goes on as if none were left: exit 0 to the gate, any other exit to the retry path;
- no `RESULT:` first line and exit 0: the card goes on to its gate, which reads the leftovers
  before and after the acceptance. A passed gate stops the unit (`gate_passed` true) and
  closes the card as usual. A gate that does not pass, while the processes ran when it
  started, records only its `gate` event, not a `gates` row, and the card waits as below; if
  they ended during the acceptance, the next tick wakes it as after any wait. A card that
  does not reach its gate (a loop stop, a retry for unquoted steers, an unknown send outcome)
  has the unit stopped right after the exit is handled;
- no `RESULT:` first line and another exit: the card waits as below.

A waiting card is `blocked` with `wait_reason` `process` and a `process_wait` event with their
pids and command lines (each cut to 500 characters). Once the unit holds none of them, the next tick
queues the card for a new worker round with a `process_ended` event; its feedback names
the processes, when the run exited, when they were seen ended, and the last 20 lines of
the head's last message. That round spends no `max_rounds`, takes no backoff, and the
stuck count skips the waited run. Cancel of a waiting card, or of a card whose last worker run
exited and still has its unit, stops the unit and with it the processes. A card waiting on the
leftovers of a run whose report has a `RESULT:` first line (parked so by an older dispatcher)
goes to its gate in the same round on the next tick when that run exited 0, its unit stopped
if they still run; after another exit only the unit is stopped, and the card wakes as above
once they end. The popen launcher has no unit, so its heads' processes are neither waited on
nor stopped.

A script run's exit code is its result. Exit 0 moves the card to `check`, where
the acceptance command, if any, decides. Exit 75 (EX_TEMPFAIL) defers it: the card
returns to `queued` with `retry_at` = now + `script_defer_s` (default 900), spends
no failure round, and gets a `deferred` event with the run, the deferral count and
the last 500 characters of its stderr. The script is not launched again before
`retry_at`, and a start window that ends first cancels the occurrence as usual. Once
a card has `script_defer_max` (default 6) deferrals, its next exit 75 blocks it with
the count in the reason. Every other exit, a timeout or a crash blocks the card.

Direct tasks whose `created_by` is the config's `owner_actor` and `source=msg:…` default
to priority 100 (explicit priority is retained). Series instances are never owner tasks,
even if the owner created their container.

Admission launches candidates by class, highest first, across all lanes before
the next class: external-executor sends, then head lanes, then the script pool.

- `urgent`: an `owner_task` at step `[admission] urgent_max_step` (default 2) or
  below in `queued` or `review`, or a card whose source is in
  `[admission] urgent_sources` or is such a source with `:conditional`.
- `owner`: any other `owner_task`.
- `followup`: card class `followup`.
- `priority`: source in `[resources] priority_sources` or its `:conditional`.
- `background`: everything else.

`[admission] urgent_sources` has no default; the other keys default to step 2,
an urgent reserve of 1 and an urgent floor of 768 MiB. Within a class, reviews
go before queued work, then higher `priority`, then older `created_at`. The
`claimed` event of a head, script or external run records `admission_class`, and
`hive.py list` prints it.

`[admission] urgent_reserve` (R) holds room for urgent work on every local
ceiling: the shared `[resources] max_local_runs` (heads, scripts and detached
gates), each lane's `slots` and `script_concurrency`. A non-urgent launch or gate
needs active < ceiling − R on each ceiling it counts against; an urgent one needs
active < ceiling + R, so at most R urgent runs exceed a full ceiling. The resource
gate closes every class on IO pressure felt by the protected services: `some avg10`
of `io.pressure` at or above `[resources] protected_io_some_pct` (default 10) in any
cgroup of `[resources] psi_cgroups` (paths under `/sys/fs/cgroup`, required when
`[resources]` is enabled, as is `disk`, the path whose filesystem the free-space
floor samples), with a reason naming that cgroup. A missing or
unreadable cgroup file does not close the gate and is listed in
`sample.missing_cgroups` of Hive meta `resource_gate`. Host IO PSI at
`io_some_pct`/`io_full_pct`, CPU iowait at `iowait_pct`, RAM reserve, disk, cooldown
and the missing second CPU sample close it too; the host IO thresholds are the
emergency stop. An urgent head or script still starts
through a closed gate while three hard floors hold: the host sample was read, free
disk is at least `[resources] min_free_mb`, and MemAvailable minus starting runs ×
`per_run_mb` is at least `[admission] urgent_floor_mb` (this floor replaces the
`reserve_mb` arithmetic for urgent work). At most R runs admitted this way are
starting or running at once, and detached gates never pass a closed gate.
Running work is never stopped to make room. The `claimed` event of a head or
script carries `bypass`: `"pressure"` when the launch passed the closed host gate,
else `"cap"` when it fit only through the reserve or overflow, otherwise `null`.
Hive meta `resource_gate` keeps the last urgent admission under `last_urgent`:
card, run, bypass, the gate reasons it bypassed and the time. Owner tasks above `urgent_max_step` keep their rank
but get no reserve. Panels count each head, including synthesis; external
executors keep their own limits.
