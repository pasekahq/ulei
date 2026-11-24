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
