# Live steer delivery

New ordinary worker runs keep a bidirectional CLI connection open. The runner polls
SQLite for clarifications, so dispatcher restarts do not sever the input channel.
Panels and external/script executors keep their existing contracts.

- Codex: `app-server` over stdio, initialized once, one thread and turn. Each steer
  uses `turn/steer` with `expectedTurnId`; only its matching successful RPC response
  records `steer_live`.
- Claude: `-p --input-format stream-json --output-format stream-json --verbose
  --replay-user-messages`; input UUIDs correlate with replayed user messages.
  Queued input stays in the same process and session. Only a matching replay records
  `steer_live`; stdin remains open while queued input is outstanding.
- Neither a pipe write nor a heartbeat proves receipt. A missing acknowledgement
  after `live_steer_grace_s` (default 90 seconds, `LIVE_STEER_GRACE_S`), protocol refusal, EOF, or a legacy
  run without a live channel uses stop/handoff. `steer_restart` records why.
- Set `live_input = false` in a vendor section to use legacy one-shot runs. Existing
  active runs retain their original transport and fall back when steered. No schema
  migration or termination of other running cards is needed when deploying.

`hive.py show CARD` and the JSON API expose `pending-live`, `delivered-live`, or
`restarted` during delivery. The final report must still quote every clarification;
its `acknowledged`/`unacknowledged` status is separate from `method`, which retains
`delivered-live`/`restarted`, and `reason`, which explains a fallback. Crash recovery
can replay input whose CLI acknowledgement was lost before the journal committed;
this is an at-least-once delivery contract, not exactly-once execution.

The card tool policy applies to both transports. Codex thread startup preserves
sandbox and network policy and disables search for browser/network restrictions;
Claude retains denied tools and strict MCP config. Timeouts and cancellation still
terminate the complete head process group. Raw protocol output stays in `head.log`;
usage and fallback reports retain the existing runner format.

Protocol references:
- https://learn.chatgpt.com/docs/app-server (initialize, thread/start, turn/steer)
- https://code.claude.com/docs/en/cli-reference (stream input and replay flags)
- https://code.claude.com/docs/en/agent-sdk/streaming-vs-single-mode (persistent queued input)

## Wait nudge

A head can sit for an hour in one command that will never end (a wait loop whose exit condition cannot become
true, a hung process) while nothing in the hive notices. The dispatcher's insurance is a nudge, not a stop
(`hive/wait_nudge.py`, `Dispatcher.nudge_waits`):

- At most once a minute (`wait_nudge.CHECK_S`) the dispatcher reads the cgroup and `/proc` of every running worker
  unit, the same view `hive.py inspect` shows under `waiting_on`, and takes the command the head has run longest. It
  looks at nothing else: not what the command is, not what it waits for.
- When that command has run `[head_wait_nudge] interval_s` (default 1200) or longer, the dispatcher journals
  `wait_nudge` on the card: run, pid, command line (redacted, cut), how long it ran, `count`, whether the run has a
  live channel, and the text, the config's `text` with `{minutes}` and `{command}` filled in. The next nudge for the
  same command (same pid and command line) is due `interval_s` after the last one; another command starts at
  `count` 1 once it has itself run `interval_s`. `interval_s = 0` turns the nudge off; an absent section means the
  defaults.
- The runner hands each `wait_nudge` of its run to the head once, over the live channel, as a user message
  (Claude) or `turn/steer` (Codex), and journals `wait_nudge_live` on the vendor's acknowledgement or
  `wait_nudge_failed` when it is refused. A nudge is not a steer: the report need not quote it, an unacknowledged
  nudge holds no exit, disables no channel and brings no stop or restart. A run without a live channel (legacy
  one-shot, or a runner started before this change) gets the journal event only.
- A Claude head receives the message when its running tool call returns, within the same turn, or at once when its
  turn has ended while a background task runs; a Codex head through `turn/steer` during its turn.

Claude Code's own guards are timeouts that end or background a command, not a reminder: the Bash tool's
`BASH_DEFAULT_TIMEOUT_MS` (2 minutes) and `BASH_MAX_TIMEOUT_MS` (10 minutes), after which a foreground command moves
to the background, and a time limit on background commands in unattended sessions (30 minutes by default, at most 2
hours), https://code.claude.com/docs/en/tools-reference. A head can still wait for longer (a raised limit, a
background task it keeps waiting for, a Codex head), and a stopped command tells the head nothing about why it
hung; the nudge asks the head to look, and leaves the decision to it.
