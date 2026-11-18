# ADR-0026: Engine MCP client: session, discovery and registration over loopback HTTP

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

`crates/engine/src/mcp.rs` with `mcp/client.rs`, `mcp/schema.rs` and `mcp/tools.rs` port the
startup, discovery, registration and call path of the reference's default HTTP path:

- Settings: `engine.mcp_servers` names the servers; each has `engine.mcp.<server>.url`,
  `env_file`, `timeout` (default 300 s, `_DEFAULT_TOOL_TIMEOUT`), `connect_timeout` (default
  60 s, `_DEFAULT_CONNECT_TIMEOUT`), `tools.include`, `tools.resources` and `tools.prompts`
  (both on by default, `_select_utility_schemas`). Each has its environment variable
  `ULEI_ENGINE_MCP_<SERVER>_<FIELD>`. A URL off loopback fails the run with a settings error
  naming the key (ADR-0015 section 3).
- Startup (`register_mcp_servers`, `_discover_and_register_server`, `MCPServerTask.run`): every
  server is connected at the start of `ulei-engine run`, all at once, each within its
  `connect_timeout`: `_preflight_content_type`, then `_negotiate_session` on the handshake path
  (`ClientSession.initialize` with protocol `2025-11-25`, empty capabilities, client info
  `mcp` `0.1.0`, the answer's version checked against the handshake versions, then
  `notifications/initialized`), then `_discover_tools` with `_advertises_tools` and
  `_paginate_full_list` (50 pages). The tools of the servers that connected are registered in
  configuration order after the built-in tools.
- Transport (`StreamableHTTPTransport._prepare_headers`, `_handle_post_request`,
  `_handle_json_response`, `_handle_sse_response`, `_handle_sse_event`, `terminate_session`): the
  `mcp-session-id` of the initialize answer and the negotiated `mcp-protocol-version` go on every
  later request; an `application/json` answer or a `text/event-stream` read until the JSON-RPC
  response, skipping notifications, requests and priming events; the 202, 404 and other error
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

These parts of the reference's default HTTP path differ:

| Behaviour | Upstream | Why |
|---|---|---|
| The reconnect loop: up to 5 reconnects with backoff 1 to 60 s and 20 % jitter, the parked state with its 300 s probe, 3 retries of the first connect, the 180 s keepalive, and the stream resumption with `Last-Event-ID` | `tools/mcp_tool.py` `MCPServerTask.run`, `_MAX_RECONNECT_RETRIES`, `_MAX_BACKOFF_SECONDS`, `_jittered`, `_PARKED_RETRY_INTERVAL`, `_MAX_INITIAL_CONNECT_RETRIES`, `_keepalive_probe`, `_DEFAULT_KEEPALIVE_INTERVAL`; `mcp/client/streamable_http.py` `_handle_reconnection` | The next card of the sprint item ports it. Until then a server that fails its one connect is left out of the run with a warning, and a call on a broken session fails with `MCP call failed: <kind>: <text>`; nothing retries. |
| The retry of a call after a session-expired error, the wait for a session being rebuilt, the `transport is down; reconnect requested` answer, and the circuit breaker (3 consecutive failures open it for 60 s with the `is unreachable after` answer) | `tools/mcp_tool.py` `_handle_session_expired_and_retry`, `_is_session_expired_error`, `_wait_for_server_session_ready`, `_signal_reconnect`, `_CIRCUIT_BREAKER_THRESHOLD`, `_CIRCUIT_BREAKER_COOLDOWN_SEC`, `_bump_server_error`, `_reset_server_error` | They act together with the reconnect loop, which resets the breaker and rebuilds the session; they come with it in the next card. |
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
| Every result and error text of an MCP tool and every connect warning has the server's header values, each word of 8 characters or more of them, and the session id replaced with `[REDACTED]`; the reference only runs `_sanitize_error` on error texts | `tools/mcp_tool.py` `_make_tool_handler`, `_sanitize_error` | Header values and the session id never reach a tool result, the journal or an error line (ADR-0015 section 3), even when a server echoes them. |
| An env-file header the transport writes itself (`Host`, `Content-Length`, `Connection`, `Transfer-Encoding`, `Accept`, `Content-Type`, `Mcp-Session-Id`), or a name or value HTTP cannot carry, fails that server's connect; `mcp-protocol-version` replaces the seeded handshake version as in the reference | `tools/mcp_tool.py` `_run_http`; `mcp/client/streamable_http.py` `_prepare_headers` | The reference lets the request-level header win silently; the engine writes each header once and names the misconfigured one. |
| The schema cache with lazy servers, the cross-process discovery lock and the suspicious-entry filter | `tools/mcp_tool.py` `_register_from_cache_sync`, `_resolve_server_lazy`, `_try_acquire_mcp_discovery_lock`, `_filter_suspicious_mcp_servers` | One process connects per run and its servers are loopback URLs from the settings; there is no cache to reuse and no stdio command to inspect. |
| The warnings of the startup go to stderr as `ulei-engine: <text>`: a failed connect, a name collision, a suspicious tool description; the info and debug lines and the pagination warning are not written | `tools/mcp_tool.py` `register_mcp_servers`, `_register_server_tools`, `_scan_mcp_description`, `_paginate_full_list` | The engine has no log; stderr is its only diagnostic channel. |
| A `timeout` or `connect_timeout` of 0 or less waits without a deadline; a value that is not a number fails the run naming the key | `tools/mcp_tool.py` `MCPServerTask.run` (`config.get("timeout", ...)`), `_run_on_mcp_loop` | The engine reads every timeout this way (ADR-0016 section 3). |
| The keys `enabled`, `tools.exclude`, `supports_parallel_tool_calls`, `trust`, `idle_timeout_seconds`, `max_lifetime_seconds`, `ssl_verify`, `client_cert`, the identity header, `strict_redirect_headers`, `transport`, `skip_preflight`, `protocol`, `auth`, and `${VAR}` interpolation of the config | `tools/mcp_tool.py` `register_mcp_servers`, `_register_server_tools`, `_record_tool_trust_metadata`, `_get_lifecycle_seconds`, `_run_http`, `_apply_identity_header`, `_interpolate_env_vars` | Each is off or empty by default and the deployment sets none; header values come from the env-file (ADR-0015). OAuth servers are not in this deployment step. |

## Consequences

- An MCP server's tools exist for a run only when it connected at the start of that run.
- `ulei-engine` gains no dependency; the transport stays the `gateway.rs` reader over loopback
  TCP (ADR-0015).
- The reconnect behaviour arrives with the next card, which replaces the first two rows.
