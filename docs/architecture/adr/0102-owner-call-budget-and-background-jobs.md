# ADR-0102: A hard per-call budget in owner turns, with offload to engine-owned background jobs whose results reach the owner inbox (C1 1.7.0, C6 0.14.0, C8 0.11.0, C9 0.16.0)

Status: accepted
Date: 2026-11-27
Affects: crates/engine, crates/core, contract:C1, contract:C6, contract:C8, contract:C9

## Context

A tool call of an owner turn runs as long as the tool lets it: the terminal waits for the
timeout the model chose (up to 600 s), an MCP call for its server's timeout, and the parallel
segment for `engine.concurrent_tool_timeout`. The session runs one turn at a time, so a message
the owner sends meanwhile waits behind the call. The reference has no such budget; its terminal
promises a `background=true` its schema lacks. This is an owner-designed addition on top of the
reference's tool loop.

## Decision

### 1. The budget

- `engine.owner_call_budget` (env `ULEI_ENGINE_OWNER_CALL_BUDGET`), seconds as an integer or a
  decimal string, default 45; 0 or less turns the budget off. `serve` reads it at start, so a
  test sets it to half a second.
- It applies to every tool call of a turn whose origin is `Origin::TopLevelOwner` (the owner's
  input on `engine.main_channel`) in `ulei-engine serve`. Other origins, and the one-shot
  `ulei-engine run`, are unchanged.
- It bounds the wait, not the work: a model-supplied `timeout` of the terminal never extends it.
  The call's own limit still bounds the work, as section 4 says.
- A call refused by the pre-tool policies is answered as before; the budget starts when the call
  starts.

### 2. The parallel segment

The calls of a parallel segment share one deadline, the segment's start plus the budget; each
call meets it as in section 3, and a call the segment's workers had not started by then is not
started and gets the `timeout` result with `reason` `not_started`. So a segment of an owner turn
ends within the budget plus the time a stopped call takes to end. `concurrent_tool_timeout`
keeps applying on top; for owner turns the budget is normally the shorter one.

### 3. What a call gets at the budget

- Offloadable: the tools whose `Tool::detach` hands over work that owns everything it runs with.
  Only `terminal` is offloadable: its work is a process group with its own timeout. Such a call
  runs on a thread of its own from its start; when it is still running at the budget it goes on
  as a background job (section 4) and the turn gets the `offloaded` result at once (section 5).
- Every other tool (MCP tools, `web_search`, `web_extract`, the memory, garden and archive tools,
  file tools) is not offloadable: their state belongs to the turn (an MCP session is opened and
  closed per turn). At the budget the call's own interrupt fires, which stops a running process,
  helper or MCP socket as the turn's interrupt does, and the turn gets the typed result
  `{"status": "timeout", "tool", "budget_s", "reason": "stopped", "error"}` with `is_error`.
  Whatever the call did before it was stopped stays done; the text tells the model so.
- A tool that checks no interrupt (local file reads) finishes on its own; its late result is
  dropped. Such tools are bounded by their own limits.
- The turn's `interrupt` reaches a call until it is offloaded.

### 4. The job

- Id: `<the engine's start in microseconds>-<n>`, C6 `JobId` (`^[a-zA-Z0-9_-]{1,64}$`), never
  reused across restarts. Owner: the engine process; the job serves the owner session.
- Record: a C8 `Job` row in the journal's table `jobs` (schema version 10), written when the call
  is offloaded, given its `outcome` (`ok` or `error`, from the tool's result), `ended` and capped
  `output` when the work ends, and removed when the inbox confirmed it.
- Lifetime cap: the call's own limit, the terminal's `timeout` (default 180 s, at most 600 s).
  An offloadable tool must end its work within its limit; a terminal past it is killed and the
  job ends `error` with the terminal's timeout result, so a job past its limit is never silent.
- `interrupt` does not touch a job: it stops the running turn only (C6), and an offloaded call is
  no longer part of a turn.
- An engine stop or restart stops the job's work with the engine (the unit's processes go with
  it). At its next start the engine ends every job the journal still holds running as `lost`
  with a fixed output, and that result is delivered like any other.
- At most 4 jobs run at once (`MAX_JOBS`). Past it, an offloadable call is stopped at the budget
  like any other, with `reason` `jobs_full`.
- Without `inbox.socket` nothing can deliver a result, so no call is offloaded: an offloadable
  call is stopped at the budget like any other.

### 5. The offloaded result

The turn gets, without `is_error`:
`{"status": "offloaded", "job_id": "<id>", "tool": "terminal", "elapsed_s": 45.0, "note": ...}`.
The note tells the model that the call goes on and was not stopped, that its final result
reaches the owner later as an inbox event naming the `job_id`, not to run the call again or wait
for it, to tell the owner it goes on in the background and to end the turn. The terminal's
description states the budget and the offload and promises no parameter its schema lacks.

### 6. How the result reaches the owner inbox

The inbox daemon publishes, so the engine holds no producer key and no new secret is readable
by tools (the engine's tools share its sandbox):

1. The job's end is committed to the journal, then the engine calls the new C1 operation `pull`
   (`PullRequest`, no arguments) on `inbox.socket`. It carries no content.
2. `ulei-inbox serve` answers `{"ok": true}` when it runs its delivery loop and wakes it; without
   `inbox.deliver` it answers `{"ok": false}` and nothing pulls.
3. The loop asks C6 `job_results` (at most 16, oldest first), publishes each result into its own
   store as kind `job_result`, id `engine:job:<job id>`, `source_ts` the job's end, payload the
   JSON object `{"source": "engine", "job_id", "tool", "outcome", "started", "ended", "output"}`,
   signed with the producer key, and confirms them with C6 `jobs_done`; the engine then forgets
   them. A failed step keeps the results in the engine.
4. The engine calls `pull` again at once after every new end and otherwise every 15 s, doubling to
   300 s, while ended jobs wait; with none waiting no thread runs and nothing wakes. So a result
   survives an engine restart after the job ended (the next start pulls) and an inbox that was
   down (a later pull reaches it). The inbox calls the engine only after a `pull`, so its idle
   stays without engine calls (ADR-0036 section 9).
5. A republished result under the same id and content changes nothing (C1 idempotence), so a
   result pulled twice before its confirmation is one event.

`output` is the tool's result as the model would have seen it, saved to a spill file and
replaced by a preview with its path past the tool's spill threshold or 8000 characters, then cut
to 8000 characters: the payload stays within C1's 65,536 bytes.

From the store on, the event is an ordinary inbox event: delivered at least once into the owner
session when it is free (busy, it waits and is retried), deferred or `uncertain` by the
delivery report, and seen by the uncertain alerter `ulei-tg uncertain` (ADR-0097, ADR-0099).
`crates/core/src/prompt.rs` gives `job_result` a policy of its own: the event is the final result
of a call the turn offloaded earlier, its `job_id` matches that `offloaded` result, and the model
tells the owner the result of that earlier request; it never claims done without evidence in
`output` and does not run the call again unasked.

### 7. Owner input during a long call

No new lane: the call ends within the budget (offloaded or stopped), the turn takes one more
model step and ends, and the owner's message queued meanwhile starts as the session's next turn.
A wall-clock budget per whole turn and a read-only side responder are not part of this decision.

### 8. Differences from the reference

| Behaviour | Reference module | Why |
|---|---|---|
| The owner call budget, the offload into a job and the `timeout` result at the budget | `agent/tool_executor.py` `execute_tool_calls_concurrent`, `execute_tool_calls_segmented`; `run_agent.py` `_execute_tool_calls` | The owner directive of this decision: no call keeps the owner waiting past the budget; the reference waits for every call up to its batch deadline. |
| The description and the `timeout` text of `terminal`: the budget and the offload, without the `background=true` the schema lacks | `tools/terminal_tool.py` `TERMINAL_TOOL_DESCRIPTION`, `TERMINAL_SCHEMA` | The engine has no background parameter (ADR-0016); the text promises only what the engine does. |

## Consequences

- C1 1.7.0 (`job_result`, `pull`), C6 0.14.0 (`job_results`, `jobs_done`), C8 0.11.0 (`Job`) and
  C9 0.16.0 (`engine.owner_call_budget`; `inbox.socket` also read by the engine) are additive.
- The engine and the inbox deploy together: an older inbox answers `pull` with `not_found`, and
  the engine keeps the results and retries until a newer inbox takes them.
- A job's thread and its process exist only while it runs; an idle engine has no job thread.
- The release needs nothing on the host besides the new binaries and a restart of the engine and
  the inbox; `inbox.socket` is already in the deployed `ulei.toml`.
