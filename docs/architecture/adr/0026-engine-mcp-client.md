# ADR-0026: Engine MCP client: session, discovery, registration and recovery over loopback HTTP

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The reference implementation (`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3) connects its
MCP servers in `tools/mcp_tool.py` through the MCP SDK it pins (`mcp` 2.0.0,
`mcp/client/session.py` `ClientSession`, `mcp/client/streamable_http.py`). The deployment uses
one Streamable HTTP server on loopback with its own `timeout` and `connect_timeout`; its OAuth
servers are outside this record. ADR-0015 fixed the transport: the HTTP/1.1 client of
`gateway.rs` over a loopback `TcpStream`, loopback `http://` only, no redirects, header values
from an env-file, and the head, line and body limits.

## Decision

`crates/engine/src/mcp.rs` with `mcp/client.rs`, `mcp/recovery.rs`, `mcp/schema.rs` and
`mcp/tools.rs` port the startup, discovery, registration, call and recovery path of the
reference's default HTTP path:

- Settings: `engine.mcp_servers` names the servers; each has `engine.mcp.<server>.url`,
  `env_file`, `timeout` (default 300 s, `_DEFAULT_TOOL_TIMEOUT`), `connect_timeout` (default
  60 s, `_DEFAULT_CONNECT_TIMEOUT`), `tools.include`, `tools.resources` and `tools.prompts`
  (both on by default, `_select_utility_schemas`). Each has its environment variable
  `ULEI_ENGINE_MCP_<SERVER>_<FIELD>`, which wins over the key of the required `ulei.toml`; the
  engine reads the file through `ulei_contracts::config` (ADR-0040). A section
  `[engine.mcp.<server>]` whose server `engine.mcp_servers` (or `ULEI_ENGINE_MCP_SERVERS`, which
  wins over it) does not list fails the start with an error naming the section and its line. A URL off loopback fails the run with a settings error
  naming the key (ADR-0015 section 3); so do two server names whose settings read the same
  environment variable, such as `a-b` and `a_b`, or `a` and `a_connect`, naming both servers
  and the variable.
- Startup (`register_mcp_servers`, `_discover_and_register_server`, `MCPServerTask.run`): every
  server is connected at the start of `ulei-engine run`, all at once, each within its
  `connect_timeout`: `_preflight_content_type` once, then `_negotiate_session` on the handshake path
  (`ClientSession.initialize` with protocol `2025-11-25`, empty capabilities, client info
  `mcp` `0.1.0`, the answer's version checked against the handshake versions, then
  `notifications/initialized`), then `_discover_tools` with `_advertises_tools` and
  `_paginate_full_list` (50 pages). A failed handshake or listing is retried up to 3 times
  (`_MAX_INITIAL_CONNECT_RETRIES`) after jittered waits of 1, 2 and 4 s; a non-MCP content type
  found by the preflight is not retried. The tools of the servers that connected are registered
  in configuration order after the built-in tools.
- Transport (`StreamableHTTPTransport._prepare_headers`, `_handle_post_request`,
  `_handle_json_response`, `_handle_sse_response`, `_handle_sse_event`, `terminate_session`): the
  `mcp-session-id` of the initialize answer and the negotiated `mcp-protocol-version` go on every
  later request; an `application/json` answer or a `text/event-stream` read until the JSON-RPC
  response whose `id` is the request's (ADR-0015 section 3), skipping notifications, requests,
  priming events and responses to other ids; the 202, 404 and other error
  statuses mapped to the SDK's JSON-RPC errors; `DELETE` of the session when the run ends,
  bounded by 10 s as the reference's shutdown.
- Registration (`_register_server_tools`, `_should_register`, `matches_name_filter`,
  `_convert_mcp_schema`, `_normalize_mcp_input_schema` with `tools/schema_sanitizer.py`
  `strip_nullable_unions` and `collapse_const_unions`, `_build_utility_schemas`,
  `sanitize_mcp_name_component`, `mcp_prefixed_tool_name`): names `mcp__<server>__<tool>` with
  `[^A-Za-z0-9_]` as `_`; names several entries share are skipped, a listed tool winning over a
  utility; a name another server registered stays with it.
- Calls (`_make_tool_handler`, `_make_list_resources_handler`, `_make_read_resource_handler`,
  `_make_list_prompts_handler`, `_make_get_prompt_handler`, `_run_on_mcp_loop`,
  `_interrupted_call_result`, `_sanitize_error`, `_truncate_mcp_text_result`,
  `_strip_reserved_meta_keys`, `_render_mcp_resource_block`; `tools/ansi_strip.py`
  `strip_unicode_tags`): an MCP tool is a registry tool, so every call passes the policy chain;
  `isError` is a tool error with the reference text, any other answer `{"result": text}` with its
  structured content and metadata; the per-server `timeout` ends a call with
  `MCP call failed: TimeoutError: MCP call timed out after <s>s (configured timeout: <s>s)`; an
  interrupt shuts the call's socket down and answers
  `MCP call interrupted: user sent a new message`. The results go through the reference's
  `agent/tool_dispatch_helpers.py` `_maybe_append_elision_notice` and `_maybe_wrap_untrusted`,
  which cover the `mcp_` prefix.
- MCP tools are not parallel-safe: `is_mcp_tool_parallel_safe` is false unless a server sets
  `supports_parallel_tool_calls`, which the engine does not read (ADR-0016 section 3).
- Recovery (`MCPServerTask.run`, `_mark_session_proven`, `_signal_reconnect_and_wait`,
  `_wait_for_server_session_ready`, `_handle_session_expired_and_retry`,
  `_is_session_expired_error`, `_SESSION_EXPIRED_MARKERS`, `_bump_server_error`,
  `_reset_server_error`, the breaker of `_make_tool_handler`, `_jittered`): the constants are
  the reference's: 5 reconnect retries, backoff from 1 s doubling to at most 60 s with +/-20 %
  jitter, a parked probe every 300 s, a breaker of 3 consecutive failures and 60 s.

### Recovery without the background loop

The reference runs each server in a task of its asyncio MCP loop (`_run_on_mcp_loop`), and that
task's `run` loop reconnects in the background. The engine is synchronous (port map R3) and
`ulei-engine run` lives for one turn, so the calls of the turn drive the same state machine,
held per server in `mcp/recovery.rs`, and every wait happens inside a call on the injected
clock:

| Reference | Engine |
|---|---|
| A call fails with a session-expired error (`_is_session_expired_error`) or the stream errors `ClosedResourceError`, `EndOfStream`, `BrokenResourceError` | A failure whose text holds a marker of `_SESSION_EXPIRED_MARKERS`, or a transport failure of a refused, reset or closed connection (`ConnectError`, `ReadError`, `WriteError`, a disconnect without an answer), which the reference meets as those stream errors |
| `_signal_reconnect_and_wait` wakes `run`, which tears the session down (`terminate_session`) and rebuilds it at once, charging the budget only when the session never served a call; the caller polls 15 s for the new session, then retries once | The call sends `DELETE` for the lost session and reconnects at once (handshake, `notifications/initialized`, `tools/list`, no preflight), with the same charge; it drives the ladder below for at most 15 s, then retries once. A retry that answers without an error resets the breaker and is the result; otherwise the call answers `MCP call failed: <kind>: <text>` of the first failure |
| A failed reconnect: `_reconnect_retries` + 1, a sleep of `_jittered(backoff)`, backoff doubled up to 60 s; past 5 the server parks and self-probes every 300 s with one attempt | The same counters and schedule: the immediate rebuild, then 5 reconnects after waits of 1, 2, 4, 8 and 16 s (each +/-20 %); the failure of the fifth parks the server until 300 s later, when one probe is allowed |
| A tool call that finds no session waits `min(5, timeout)` s for one, then answers `transport is down; reconnect requested` and bumps the breaker | The call waits for the ladder's next attempt when it falls within `min(5, timeout)` s and makes it; otherwise it answers the same text and bumps the breaker. A later call continues the ladder where its schedule stands |
| A parked server's tools are deregistered (`_deregister_tools`); its utilities answer `is not connected` | The tools of a turn are fixed (the tool list is part of every model request), so they stay registered and answer `MCP server '<server>' is not connected`, bumping the breaker as the reference's handler does for a server it does not hold; a call within 5 s of the probe waits for it and makes it |
| A new session clears the breaker; a call whose round trip completed proves the session and clears the reconnect budget | The same |
| The resource and prompt utilities: no breaker, no wait for a session (`is not connected`), the session-expired retry | The same |

Every wait of a call (the backoff, the parked wait, the wait for the next attempt) is bounded by
the 15 s or `min(5, timeout)` s window above and ends at once when the turn's `Interrupt`
fires; the call then answers `MCP call interrupted: user sent a new message`. A reconnect's
handshake and listing take the same `Interrupt`. The startup waits are bounded by
`connect_timeout` and have no interrupt, as the turn has not started. Between calls nothing
runs: no thread, no timer, no wake-up (ADR-0010).

### Differences from the reference

These parts of the reference's default HTTP path differ:

| Behaviour | Upstream | Why |
|---|---|---|
| The keepalive: a `ping` (or `tools/list` when the server lacks `ping`) every 180 s while a session is idle, a failure triggering a reconnect, a success proving the session | `tools/mcp_tool.py` `_DEFAULT_KEEPALIVE_INTERVAL`, `_MIN_KEEPALIVE_INTERVAL`, `_keepalive_probe`, `_wait_for_lifecycle_event` | `ulei-engine run` lives for one turn, and `ulei-engine serve` connects the MCP servers at the start of each turn and closes them at its end as `run` does, so no session idles between turns; a timer would break the 0 idle wake-ups of ADR-0010. A session the server dropped is found by the next call and rebuilt as above. |
| Failures of a reconnect that a call requested count against the 5 reconnect retries; the reference clears `_ready` before signalling, so its `run` charges such failures to the 3 retries of a first connect | `tools/mcp_tool.py` `_signal_reconnect_and_wait`, `MCPServerTask.run` | Port map R3 names the reconnect ladder of `run` for a server that had connected; the first-connect ladder stays with the startup. |
| A retry wait of the startup that would end past `connect_timeout` is not slept: the connect fails at once with the last attempt's failure, where the reference sleeps until `asyncio.wait_for` cancels it and reports `CancelledError` | `tools/mcp_tool.py` `_discover_and_register_server`, `_format_connect_error` | The wait could only end in the timeout; the last failure names the cause. |
| A server that failed its startup is not kept parked for revival in the run | `tools/mcp_tool.py` `_discover_and_register_server` (the recoverable park), `register_mcp_servers` (`stale_cached`) | Its tools were never registered in this turn's tool list; the next run connects it again. |
| The tools a reconnect lists are not registered again | `tools/mcp_tool.py` `_register_discovered_tools_if_needed` | The tool list of a turn is fixed; a changed list takes effect at the next run. |
| The connect cooldown across discovery passes, OAuth recovery with its retry, the permanent-failure park of a 401 or 403, the immediate reconnect after a transport task group drop, the stream resumption with `Last-Event-ID`, and the lazy reconnect of a recycled stdio server | `tools/mcp_tool.py` `_record_connect_failure`, `_connect_cooldown_active`, `_handle_auth_error_and_retry`, `_classify_mcp_failure`, `_reconnect_or_reraise_group`, `_wait_for_lazy_reconnect`, `_request_lazy_reconnect`, `_RECYCLED_RECONNECT_TIMEOUT`; `mcp/client/streamable_http.py` `_handle_reconnection` | One process connects once per run; OAuth and stdio servers are not in this deployment step; an error status reaches the engine as the SDK's JSON-RPC error, never as an `HTTPStatusError`, so the only permanent failure is the preflight's; the engine has no transport task group and no standing stream. |
| An interrupt during the 15 s wait for a rebuilt session ends the call with the interrupt answer | `tools/mcp_tool.py` `_wait_for_server_session_ready` | That poll ignores the interrupt; every wait inside a turn must end at an interrupt. |
| `tools.resources` and `tools.prompts` read as every engine switch: `1`, `true`, `yes` or `on` in any case is on, any other value off; `_parse_boolish` also takes `0`, `false`, `no` and `off` after stripping and keeps the default with a warning for anything else | `tools/mcp_tool.py` `_parse_boolish`, `_select_utility_schemas` | One reading for every switch of the engine's config (`Config::switch`); a value it does not know turns the utilities off, which offers the model fewer tools, never more. |
| The stateless 2026-07-28 era: `server/discover` after an initialize the server rejects as modern-only, and the `protocol` key choosing the era | `tools/mcp_tool.py` `_negotiate_session`, `_handshake_rejected_as_modern`; `mcp/client/session.py` `discover`, `_make_modern_stamp` | The deployment's server speaks the handshake era. The modern era needs its per-request envelope and `Mcp-Name` and `Mcp-Param-*` headers, a second protocol path. A rejected initialize fails the connect. |
| The standing `GET` event stream, and the handling of server notifications (`tools/list_changed` refreshing the tools, logging) | `mcp/client/streamable_http.py` `handle_get_stream`; `tools/mcp_tool.py` `_make_message_handler`, `_refresh_tools`, `_make_logging_callback` | One `ulei-engine run` serves one turn, so a changed tool list takes effect at the next run. Notifications in an answer stream are read and dropped. |
| Sampling and elicitation (both on by default): their capabilities in `initialize`, and answers to requests the server sends during a call, `ping` included | `tools/mcp_tool.py` `SamplingHandler`, `ElicitationHandler`; `mcp/client/session.py` `_build_capabilities` | Sampling would run a model turn on the server's behalf, which needs its own qb route decision (ADR-0009); elicitation needs an owner approval surface. The engine advertises neither, and a request from the server inside an answer stream is skipped. |
| The validation of structured content against the tool's `outputSchema`, and the SDK's model validation of each result | `mcp/client/session.py` `validate_tool_result`, `send_request` (`validate_server_result`) | The engine carries no JSON Schema validator. It checks only what it reads: the handshake version and capabilities, list fields that must be lists, and a string `name` on every listed tool. |
| Image and audio blocks and embedded blobs: the reference caches them and answers a `MEDIA:` tag or the saved path | `tools/mcp_tool.py` `_cache_mcp_image_block`, `_cache_mcp_audio_block`, `_render_mcp_resource_block` (`cache_document_from_bytes`) | The engine has no media or document cache. It answers as the reference does without one: an image renders empty, audio empty or as the too-large note, a blob as `[MCP embedded resource received (<n> bytes, <type>) but document cache unavailable in this process]`. |
| Non-text content of a `prompts/get` message renders as its JSON text | `tools/mcp_tool.py` `_make_get_prompt_handler` (`str(content)` of an SDK model) | The engine has no SDK models; the JSON carries the same fields. |
| Objects inside structured content, `_meta` and the list results keep their keys in sorted order | `tools/mcp_tool.py` `_make_tool_handler`, `json.dumps` | The engine's JSON maps are sorted; the top-level fields keep the reference's order. |
| Transport failure texts: `ConnectError`, `ConnectTimeout`, `ReadTimeout`, `ReadError`, `WriteError`, `WriteTimeout` and `RemoteProtocolError` with the engine's text, and `HTTPStatusError: redirect not followed: status code <n>`; a connect past its `connect_timeout` is reported as `TimeoutError` | `tools/mcp_tool.py` `_make_tool_handler`, `_format_connect_error`; `httpx` | Another HTTP client (ADR-0015). The shapes stay the reference's: `MCP call failed: <kind>: <text>` and `Failed to connect to MCP server '<server>': <text>`. |
| The preflight `POST` names its client `ulei-probe` | `tools/mcp_tool.py` `_preflight_content_type` | The reference's probe name contains the product name, which engine sources may not (AGENTS.md hard rule 8). |
| The elision notice opens with `[note: ` | `agent/tool_dispatch_helpers.py` `_UPSTREAM_ELISION_NOTICE` | The same label change as the guardrail notice (ADR-0016 section 12). |
| Every result and error text of an MCP tool and every connect warning has the credential header values, each word of 8 characters or more of them, and every session id of the run replaced with `[REDACTED]`; a credential header is one whose name holds `auth`, `token`, `key`, `secret`, `cookie`, `password` or `session` in any case, so `X-Flag: 1` stays readable. The reference only runs `_sanitize_error` on error texts | `tools/mcp_tool.py` `_make_tool_handler`, `_sanitize_error` | Credential values and session ids never reach a tool result, the journal or an error line (ADR-0015 section 3), even when a server echoes them. |
| An env-file header the transport writes itself (`Host`, `Content-Length`, `Connection`, `Transfer-Encoding`, `Accept`, `Content-Type`, `Mcp-Session-Id`), or a name or value HTTP cannot carry, fails that server's connect; `mcp-protocol-version` replaces the seeded handshake version as in the reference | `tools/mcp_tool.py` `_run_http`; `mcp/client/streamable_http.py` `_prepare_headers` | The reference lets the request-level header win silently; the engine writes each header once and names the misconfigured one. |
| The schema cache with lazy servers, the cross-process discovery lock and the suspicious-entry filter | `tools/mcp_tool.py` `_register_from_cache_sync`, `_resolve_server_lazy`, `_try_acquire_mcp_discovery_lock`, `_filter_suspicious_mcp_servers` | One process connects per run and its servers are loopback URLs from the settings; there is no cache to reuse and no stdio command to inspect. |
| The warnings of the startup go to stderr as `ulei-engine: <text>`: a failed connect, a name collision, a suspicious tool description; the info and debug lines and the pagination warning are not written | `tools/mcp_tool.py` `register_mcp_servers`, `_register_server_tools`, `_scan_mcp_description`, `_paginate_full_list` | The engine has no log; stderr is its only diagnostic channel. |
| A `timeout` or `connect_timeout` of 0 or less waits without a deadline; a value that is not a number fails the run naming the key | `tools/mcp_tool.py` `MCPServerTask.run` (`config.get("timeout", ...)`), `_run_on_mcp_loop` | The engine reads every timeout this way (ADR-0016 section 3). |
| The keys `enabled`, `tools.exclude`, `supports_parallel_tool_calls`, `trust`, `idle_timeout_seconds`, `max_lifetime_seconds`, `ssl_verify`, `client_cert`, the identity header, `strict_redirect_headers`, `transport`, `skip_preflight`, `protocol`, `auth`, and `${VAR}` interpolation of the config | `tools/mcp_tool.py` `register_mcp_servers`, `_register_server_tools`, `_record_tool_trust_metadata`, `_get_lifecycle_seconds`, `_run_http`, `_apply_identity_header`, `_interpolate_env_vars` | Each is off or empty by default and the deployment sets none; header values come from the env-file (ADR-0015). OAuth servers are not in this deployment step. |

## Consequences

- An MCP server's tools exist for a run only when it connected at the start of that run; a
  server lost during the turn is reconnected by its calls, parks after 5 failed reconnects and
  is probed again after 300 s.
- `ulei-engine` gains no dependency; the transport stays the `gateway.rs` reader over loopback
  TCP (ADR-0015).
- Tests drive the recovery on an injected `mcp::Clock` that records its waits and supplies the
  jitter; the engine's `SystemClock` waits on the turn's `Interrupt`.
