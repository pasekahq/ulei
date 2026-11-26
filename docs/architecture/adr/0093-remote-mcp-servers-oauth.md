# ADR-0093: Remote MCP servers over HTTPS with OAuth 2.1: the `ulei-mcp-remote` daemon

Status: accepted
Date: 2026-11-26
Affects: crates/engine

## Context

The engine's MCP client (ADR-0026) reaches only loopback `http://` servers with static header
values from an env-file. ADR-0015 section 3 recorded the gap: "Remote https servers and OAuth
would need TLS in the engine or a long-lived helper session, which calls for a separate ADR".
This is that ADR. It decides the mechanism only; the implementation is a later change of its own.

Why it is needed now:

- Remote MCP servers are the common case for hosted services: an `https://` endpoint protected
  by the MCP authorization spec (OAuth 2.1 with PKCE, RFC 9728 protected resource metadata,
  RFC 8414 authorization server metadata, RFC 8707 resource indicators, client identification
  by a Client ID Metadata Document or RFC 7591 dynamic registration). Every owner of the paseka
  who adds such a service meets the same gap, so the mechanism is general, not a per-service
  shim.
- The first such server in live use is a freelance marketplace's MCP endpoint (OAuth 2.1,
  rotating refresh tokens, 133 tools). It runs today through a temporary stdlib Python bridge
  outside this repository: a loopback HTTP proxy that the engine reaches as an ordinary
  loopback server with a local bearer header, which adds the OAuth access token, refreshes it,
  turns the server's `text/event-stream` answers into JSON bodies and passes 33 allowed tools.
  The bridge is the prototype this record generalises. What it showed:
  - The server answers every `POST` with `text/event-stream`; the engine's reader refused the
    bridge's first relay of such a stream with a `Content-Length` head (`event stream with a
    Content-Length`), so a relay must keep the stream chunked or answer JSON.
  - The server rotates its refresh token on every refresh. The first refresh by the bridge
    invalidated the copy held by the reference's token store; two holders of one grant keep
    invalidating each other. One writer per grant is a correctness requirement, not a nicety.
  - The bridge seeded its tokens from an existing login. It has no authorization flow of its
    own, so it cannot serve an owner who has never logged in elsewhere.
  - The engine's startup preflight answered by the bridge with 501 still connects.
- The reference (`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3) implements this inside
  its agent process: `tools/mcp_oauth.py` (the token storage class, the loopback callback
  server, the paste fallback `_paste_callback_reader`, `build_oauth_auth`, client metadata,
  Client ID Metadata Document handling `_maybe_use_cimd`), `tools/mcp_oauth_manager.py`
  (`MCPOAuthManager`: one provider per server, reload of tokens another process refreshed
  by file mtime, one recovery for concurrent 401s) and the SDK it pins (`mcp` 2.0.0,
  `mcp/client/auth/oauth2.py` `OAuthClientProvider.async_auth_flow`, `mcp/client/auth/utils.py`,
  `mcp/shared/auth_utils.py`).

Constraints that rule out a straight port into the engine:

- TLS in `ulei-engine` was measured in ADR-0015 section 4: the `ring` + `webpki-roots` stack
  adds 1,227,264 B and lifts idle `VmHWM` to 4,396 to 4,488 kB. ADR-0053 raised the engine's
  static size limit to 4 MiB, but its idle RSS limit stays 4,096 kB (ADR-0010 section 4), and
  the idle rise comes from the larger binary alone. TLS still cannot enter the engine.
- `ulei-engine run` lives for one turn and `serve` reconnects the MCP servers every turn
  (ADR-0026). Short-lived processes that each refresh a rotating grant race each other exactly
  as the bridge and the reference's store did. A process per call (the `ulei-web` route of
  ADR-0015 section 2) has the same race and also loses the MCP session between calls; ADR-0015
  rejected route (iii) for that reason.
- The engine's tools run terminal commands. OAuth refresh tokens are long-lived credentials and
  must not be readable from inside the engine's sandbox (ADR-0069).

## Decision

### 1. A long-lived relay daemon owns TLS and OAuth

A new binary `ulei-mcp-remote`, a `[[bin]]` of `crates/engine` behind a crate feature
`mcp-remote = ["dep:ureq", "dep:ring"]`, as `ulei-web` is behind `web` (ADR-0015 section 2).
It uses the TLS stack and features fixed in ADR-0015 section 1 (`ureq` 3 with `rustls`,
`ring`, `webpki-roots`; no `gzip`, no platform verifier). Both crates are already optional
dependencies of `crates/engine`, so this record adds no dependency and no licence; the engine
build without the feature stays as it is, and the arch gate keeps every `ureq` use in the bin
target.

- It runs as a user unit `ulei-mcp-remote.service` beside `ulei-engine.service` (ADR-0042),
  hardened as the other units, with its own exposure threshold in the `systemd-units` job.
- It is synchronous: one thread per accepted connection, at most 16 at once (the 17th is
  answered 503), and no thread, timer or wake-up while no connection is open (ADR-0010).
- It is the only process that reads or writes the OAuth state of a server (section 4), so a
  rotating refresh token has exactly one writer on the host. The login command (section 5)
  writes under a lock the daemon honours.

### 2. Engine to daemon: MCP over the daemon's Unix socket

- A server whose `engine.mcp.<server>.url` is `https://` is no longer refused at config load.
  The engine never opens a TCP connection off the host itself: it speaks the unchanged MCP
  transport of ADR-0026 (session id, protocol header, JSON or chunked event stream, `DELETE`
  at the end, recovery ladder, interrupt by socket shutdown) to the daemon's socket
  `engine.mcp_remote_socket` (default `$XDG_RUNTIME_DIR/ulei/mcp-remote.sock`, directory 0700,
  socket 0600), request path `/<server>/mcp`. The `gateway.rs` connection is already generic
  over the stream (ADR-0015 section 2), so the change is the choice of stream plus the path.
- The daemon checks the peer uid of every connection with `rustix` `socket_peercred` and
  accepts only its own uid, as the garden does (ADR-0048). No shared bearer between engine and
  daemon, no TCP port another local account could reach.
- `engine.mcp.<server>.env_file` keeps its meaning for a remote server: static headers, for
  services that use an API key over HTTPS instead of OAuth. The engine does not send them to
  the socket; the daemon reads that env-file and adds the headers upstream. With
  `engine.mcp.<server>.auth = "oauth"` the daemon adds `Authorization: Bearer <access token>`
  instead; both at once fail that server's connect, naming the keys.
- The daemon relays each request to the server URL and relays the answer: status, the
  `Content-Type`, `Mcp-Session-Id` and `mcp-protocol-version` heads, and the body. An
  `application/json` body is relayed with its length; a `text/event-stream` is relayed chunked,
  event by event as it arrives, never buffered whole and never with a `Content-Length`, so the
  engine's id matching and limits of ADR-0015 section 3 apply as for a local server. When the
  engine shuts the socket down (interrupt) the daemon drops the upstream connection.
- Upstream rules: `https://` only; certificates checked against the bundled roots; redirects
  not followed (a 3xx is relayed as the error status ADR-0026 maps); connect 60 s, read idle
  300 s; the engine's per-call `timeout` stays the overall bound. Head 8 KiB, one event line
  4 MiB, a JSON body 4 MiB, as ADR-0015 section 3. A test switch `mcp-remote-loopback-http`
  (like `web-loopback-http`, ADR-0021) lets CI point the daemon at plain-http fakes on loopback;
  the shipped build refuses `http://` upstreams.
- The daemon answers `GET /status` on the socket with one JSON line per server
  (`ok`, `needs_login`, `refresh_failed`, `unreachable`, with the time of the last success).
  `ulei status` prints one `mcp-remote` line from it, last, as ADR-0074 added the
  `hive-mcp` line; a stopped daemon reads `down`.

### 3. The OAuth flow, ported from the reference's SDK path

On the request path, per server, as `OAuthClientProvider.async_auth_flow`:

1. With a stored access token whose `expires_at` has passed and a refresh token, refresh first
   (`grant_type=refresh_token`, `client_id`, `resource`). A refresh answer is persisted
   (section 4) before its access token is used; a refresh token absent from the answer keeps
   the stored one.
2. Send the request with the access token.
3. On 401: refresh once and retry once. Concurrent 401s with the same access token share one
   refresh (`MCPOAuthManager`'s in-flight deduplication): the second caller waits on the
   server's lock and reuses the token the first one stored. If there is no refresh token, or the
   token endpoint answers `invalid_grant` or any 4xx, the server turns `needs_login` and the
   relay answers the engine's request with a JSON-RPC error whose text is
   `MCP server '<server>' needs a new login: run ulei-mcp-remote login <server>`; the engine
   shows it as an MCP call error (ADR-0026) and its recovery ladder treats it as any server
   error. The daemon never starts an interactive flow by itself (the reference's
   `OAuthNonInteractiveError` path for a background process).
   A token endpoint that answers 5xx, times out or cannot be reached leaves the stored grant
   untouched. The server turns `refresh_failed`, and the relay answers with a JSON-RPC error
   whose text is `MCP server '<server>' could not refresh its token (<host>: <status>)`. The
   next request tries again, and a success turns the server `ok`. Section 11 says how both
   states reach the engine's callers.
4. On 403 with `WWW-Authenticate: ... error="insufficient_scope"`: the server turns
   `needs_login` with the union of the stored and the challenged scopes recorded, so the next
   login asks for them (the reference's step-up, SEP-2350); no automatic re-authorization.

The interactive part runs only in `ulei-mcp-remote login <server>` (section 5):

- Discovery: the `resource_metadata` of the 401's `WWW-Authenticate`, then the path-based and
  root `/.well-known/oauth-protected-resource` URLs (`build_protected_resource_metadata_discovery_urls`);
  the metadata's `resource` must match the server URL (`_validate_resource_match`); then the
  authorization server metadata URLs in the SDK's order
  (`build_oauth_authorization_server_metadata_discovery_urls`), whose `issuer` must equal the
  discovered one (`validate_metadata_issuer`). Every discovery and OAuth endpoint is `https://`.
- Scopes: as `get_client_metadata_scopes` (challenge, then protected resource metadata, then
  server metadata), unioned with a recorded step-up scope and an optional
  `engine.mcp.<server>.oauth.scope`.
- Client identification, in the reference's order: a pre-registered
  `engine.mcp.<server>.oauth.client_id` (with `client_secret_file` for a confidential client);
  else a Client ID Metadata Document when the server advertises
  `client_id_metadata_document_supported` and `engine.mcp.<server>.oauth.client_metadata_url`
  is set (the paseka publishes no shared document in this step); else RFC 7591 dynamic
  registration with client name `ulei`. Stored client credentials are bound to the issuer that
  issued them and dropped with their tokens when the issuer changes (SEP-2352,
  `credentials_match_issuer`).
- Authorization code with PKCE `S256`, a random `state` checked in constant time, the `iss`
  parameter checked (RFC 9207, `validate_authorization_response_iss`), `resource` on the
  authorization, token and refresh requests (RFC 8707). Randomness from
  `ring::rand::SystemRandom`, the digest from `sha2` already in the crate.
- Redirect: `http://127.0.0.1:<port>/callback` with a one-shot listener on loopback (port from
  `engine.mcp.<server>.oauth.redirect_port`, else a free one, kept for the next login as the
  reference's `_cached_redirect_port`). The command prints the authorization URL and also reads
  one pasted line from stdin (the full redirect URL or its query, or `skip`), as
  `_paste_callback_reader`: the owner's browser is often on another machine than a headless
  host, where the redirect cannot reach the listener but its URL can be copied back. It opens
  no browser itself.

### 4. Token store

- Per server under `mcp_remote.state_dir` (default `$XDG_STATE_HOME/ulei/mcp-remote/<server>/`,
  directories 0700, files 0600): `tokens.json` (the token answer plus `expires_at`),
  `client.json` (client information with its issuer) and `meta.json` (the discovered metadata),
  the three files of the reference's token storage class with the same fields, so a grant held
  elsewhere can be moved by copying the files and stopping the old holder (runbook step, since a
  rotating grant must have one holder).
- Writes are atomic (temporary file, `fsync`, rename, `fsync` of the directory) under an
  exclusive `flock` of `<server>/lock`, taken by the daemon around a refresh and by `login`
  around its whole exchange. The daemon re-reads the files when their mtime changed, checked
  with one `stat` per request (`MCPOAuthManager`'s disk watch), so a login takes effect without
  a restart and without a watcher thread.
- No token, code, verifier, client secret or session id is written to stderr, the journal, a
  tool result or `GET /status`; failure lines name the server, the endpoint's host and the
  status only. Header redaction of ADR-0026 stays in force in the engine.
- The store and the daemon's env-files are added to `ulei-engine.service.d/10-deny-secrets.conf`
  (`InaccessiblePaths=`), so the engine's tools cannot read them (ADR-0069). The engine itself
  never holds an OAuth token.

### 5. The `login` and `logout` commands

`ulei-mcp-remote login <server>` runs in the owner's terminal, reads the same `ulei.toml`,
performs section 3's interactive part, writes the store under the lock and exits 0; it tells
the running daemon nothing beyond the files. `ulei-mcp-remote logout <server>` deletes the
store of that server under the lock (`remove_oauth_tokens`). A login from the owner chat (the
URL sent by the assistant, the redirect pasted back) is a later step with its own record; it
needs an owner approval surface the engine does not have yet (ADR-0026, elicitation).

### 6. Tool policy

The engine's `engine.mcp.<server>.tools.include` (ADR-0026) stays the policy of what the model
sees. The daemon adds an optional `engine.mcp.<server>.relay_allow`, a list of tool names or
`*` patterns read only by the daemon: when set, a `tools/list` answer is filtered to it and a
`tools/call` of any other name is answered with a JSON-RPC error
`tool <name> is not enabled on this relay` before it leaves the host. It exists because the
daemon holds the credential: an owner can grant a paying or publishing account only a read
subset even if the engine's configuration were changed. Absent, everything is relayed.

### 7. Configuration (C9)

New keys, added to the C9 catalogue with its version bump by the implementing change (ADR-0041):
`engine.mcp_remote_socket`; `engine.mcp.<server>.auth` (`oauth` or absent);
`engine.mcp.<server>.oauth.client_id`, `.client_secret_file`, `.client_metadata_url`,
`.scope`, `.redirect_port`; `engine.mcp.<server>.relay_allow`; `mcp_remote.state_dir`. Each
engine key has its `ULEI_ENGINE_MCP_<SERVER>_<FIELD>` variable as in ADR-0026. Example:

```toml
[engine]
mcp_servers = "hive,market"

[engine.mcp.market]
url = "https://mcp.example.com/mcp"
auth = "oauth"
relay_allow = "whoami,list_*,get_*"
```

### 8. Budget row of `ulei-mcp-remote`

Added to `ci/budgets.sh` by the implementing change:

| Measure | Limit | Environment variable |
|---|---|---|
| Static size, statically linked | ≤ 2,621,440 B (the daemon row of ADR-0001) | `ULEI_MCP_REMOTE_MAX_BINARY_BYTES` |
| Idle `VmHWM` after start, 10 s | ≤ 4,096 kB | `ULEI_MCP_REMOTE_MAX_RSS_KB` |
| Voluntary context switches in 10 s idle | 0 | `ULEI_MCP_REMOTE_MAX_IDLE_WAKEUPS` |
| Peak RSS of one relayed call whose event line is at the 4 MiB cap | measured by the implementing change and fixed there with its margin | `ULEI_MCP_REMOTE_MAX_CALL_RSS_KB` |

ADR-0015 section 4 measured a standalone helper with this stack at 1,592,424 to 1,653,368 B and
a peak RSS of 3,840 to 3,968 kB for small calls; the relay adds the socket server and the OAuth
code. If a limit above does not hold, the implementing change brings measurements and the
owner's word, as ADR-0053 did; it does not raise the limit to make CI green (ADR-0001).

### 9. Tests the implementing change brings

All in CI, none on the owner's host: a fake OAuth authorization server and a fake MCP server on
loopback (built with `mcp-remote-loopback-http`) covering discovery from `WWW-Authenticate` and
from the well-known URLs, a resource or issuer mismatch refused, dynamic registration, PKCE and
`state` and `iss` checks, refresh before expiry, a 401 with one refresh for concurrent calls, a
rotated refresh token persisted before use and the old one never sent again, `invalid_grant`
turning `needs_login` with the error text the engine relays, the 403 step-up, the event stream
relayed chunked with notifications before the response, a `GET` answered 405 and a legacy-only
server (its `initialize` `POST` answered 405) reading `unreachable` (section 10), an interrupt
dropping the upstream call, the peer-uid refusal, `relay_allow` on list and call, no secret in
any output (a scan of stderr, journal and status for the fakes' token values), the budgets of
section 8, and the unit in the `systemd-units` job.

### 10. Transports

The MCP specification (2025-06-18) names two HTTP transports. Each case is decided here.

| Transport or part of one | Decision | Reason |
|---|---|---|
| Streamable HTTP, `POST` answered with `application/json` | Supported, relayed with its length (section 2) | The base case of ADR-0026. |
| Streamable HTTP, `POST` answered with `text/event-stream` (notifications and requests before the response) | Supported, relayed chunked event by event (section 2) | The prototype's server answers every `POST` this way. The engine reads the stream up to the response with the request's `id` and drops notifications (ADR-0015 section 3, ADR-0026). |
| Streamable HTTP, `DELETE` of the session | Supported, relayed | The engine ends every session this way (ADR-0026). |
| Streamable HTTP, the server-initiated `GET` event stream (a standing stream for server notifications and server-to-client requests) | Not supported. The engine never opens it (ADR-0026, differences table). The daemon answers a `GET /<server>/mcp` on its socket with `405 Method Not Allowed`, which the specification gives as the answer of a server without such a stream. It never opens one upstream by itself. | A standing stream is an open connection, and the daemon's reads on it would be wake-ups while the engine is idle (ADR-0010: no wake-ups when idle). A changed tool list takes effect at the next run because the engine reconnects every turn (ADR-0026). Server-to-client requests (sampling, elicitation, roots) need an owner approval surface the engine does not have (section 5). So the stream would carry nothing the engine can use. Resumption with `Last-Event-ID` is not supported for the same reasons. |
| The legacy HTTP+SSE transport (protocol 2024-11-05: a `GET` stream that first sends an `endpoint` event, then `POST`s to that endpoint answered `202`, with all responses arriving on the stream) | Rejected. The daemon makes no backwards-compatibility fallback. When a server answers the `initialize` `POST` with `400`, `404` or `405`, that status is relayed to the engine as a connect failure. The server reads `unreachable` in `GET /status`, with the detail `legacy HTTP+SSE (2024-11-05) is not supported`. | Every response would arrive on one long-lived stream shared by all requests. The daemon would have to hold that stream, keep it alive and match responses to requests across connections, which breaks one request per engine connection and the zero-idle-wake-up rule. The specification deprecated this transport in 2025-03-26. Hosted OAuth servers, the prototype's server among them, speak Streamable HTTP. A server that speaks only the legacy transport needs its own record with measurements. |

### 11. Contracts: C6 unchanged, C9 extended

- **C9 (`ulei.toml`)** gets the keys of section 7, with a version bump and a `CHANGELOG.md` entry
  in `contracts/`, in the implementing change (ADR-0041). It is the only contract this
  mechanism changes.
- **C6 (core ↔ runtime adapter, ADR-0005 and ADR-0008) stays unchanged: no new method, field,
  enum value or error code.** Each remote-MCP state already reaches C6's callers through a
  field C6 has:
  - `needs_login` and `refresh_failed` during a call. The daemon answers the engine's MCP
    request with a JSON-RPC error whose text names the state and the command (section 3,
    step 3). The engine turns it into an MCP tool error (ADR-0026), and the model gets it as
    the tool's result. The model then tells the owner in its reply, which C6 carries as
    `interim`/`final` turn events (ADR-0031), like any other tool failure. The turn's
    `TurnOutcome` does not change: a failed tool call leaves it `completed`, as it does for a
    local MCP server. So C6 `status` and `turn_events` need nothing new.
  - The relay socket is absent, or the daemon is down, at connect. The engine writes the usual
    startup warning naming `engine.mcp_remote_socket` to stderr (ADR-0026, Consequences). It
    offers the turn no tools of that server. Nothing crosses C6.
  - Server status (`ok`, `needs_login`, `refresh_failed`, `unreachable`, `down`). `ulei status`
    reads the daemon's own `GET /status` on the relay socket, as it reads `ulei-hive-mcp`
    directly (ADR-0074). Status therefore does not pass through the engine or C6. C6
    `SessionStatus` describes sessions and turns, not tool servers. Adding a per-server status
    there would make every C6 runtime, foreign adapters included, report a daemon it does not
    own.
  - C6 `Error` and `ErrorCode` classify failures of the C6 wire itself (ADR-0008). A remote-MCP
    failure happens inside a turn and never fails a C6 request, so it gets no C6 error code.
  - C6 `mcp_register` with `McpTransport` (`stdio`, `uds`) registers the core's C2 facade in a
    foreign runtime. Remote servers are configured for the engine in C9 and never registered
    over C6, so `McpTransport` gets no `https` value. A foreign runtime behind C6 brings its own
    MCP client and its own OAuth.
- C6 changes, with its own record, only when the owner can be asked through the channel. That
  covers a login from the owner chat and a structured `needs_login` notice instead of the
  model's words (open questions).

### 12. Migration from the prototype bridge `contra-mcp-bridge` (card h_e0e84034)

The bridge runs as the user unit `ulei-contra-mcp.service`. Its code is
`~/.local/lib/contra-mcp-bridge/bridge.py`, and its state is in
`~/.local/state/contra-mcp-bridge/`: `tokens.json`, `client.json`, `meta.json`, `allow.txt`,
`local.token` and `bridge.json`. The engine reaches it as `[engine.mcp.contra]` with
`url = "http://127.0.0.1:8745/mcp"` and an `env_file` holding the local bearer. The grant
rotates, so it must have exactly one holder at every moment. The steps therefore stop the old
holder before the new one starts, in both directions. The implementing change ships this plan
as a runbook. The owner's host runs it after the released `ulei-mcp-remote` is installed and
its unit is enabled.

1. **Stop the bridge**: `systemctl --user stop ulei-contra-mcp.service`. From here until step 6,
   the `contra` tools answer the connect failure. Nothing else is affected.
2. **Copy the token files**: with `ulei-mcp-remote.service` stopped, copy the bridge's three
   files into the new store `$XDG_STATE_HOME/ulei/mcp-remote/contra/` (directory 0700, files
   0600, the name is the server key of `engine.mcp.<server>`). Format mapping:

   | Bridge file | Store file | Fields | Handling |
   |---|---|---|---|
   | `tokens.json` | `tokens.json` | `access_token`, `token_type`, `expires_in`, `refresh_token`, `refresh_token_expires_in`, `scope`, `expires_at` | Same names, copied as is. `expires_at` is absolute Unix time in both. If `expires_at` is missing (a file never refreshed by the bridge), the daemon treats the access token as expired and refreshes first (section 3, step 1). Unknown fields are kept. |
   | `client.json` | `client.json` | `client_id`, `client_name`, `client_id_issued_at`, `redirect_uris`, `grant_types`, `response_types`, `token_endpoint_auth_method`, `scope`, `issuer` | Same names. `issuer` binds the client to its authorization server (section 3). If it is missing, it is set from `meta.json`'s `issuer`. |
   | `meta.json` | `meta.json` | The RFC 8414 authorization server metadata: `issuer`, `authorization_endpoint`, `token_endpoint`, `registration_endpoint`, `introspection_endpoint`, `scopes_supported`, ... | Copied as is. The daemon checks that its `issuer` equals `client.json`'s. |
   | `allow.txt` | none | The tool allowlist | Goes to `relay_allow` (step 4). |
   | `local.token`, `bridge.json` | none | The engine's local bearer and the bridge's listen config | Not copied. |

   The bridge seeded its files from the reference's token store, whose names are
   `<server>.json`, `<server>.client.json` and `<server>.meta.json`. Those names map to the
   same three files, so a grant held by the reference moves the same way, after the reference
   stops using it.
3. **Disable the bridge**: `systemctl --user disable ulei-contra-mcp.service`. The unit file,
   the code and the state directory stay in place until step 7, for a rollback.
4. **Change the server's config** in `[engine.mcp.contra]`:
   - set `url = "https://contra.com/mcp"` and `auth = "oauth"`;
   - remove `env_file` (a static header plus OAuth fails the connect, section 2) and delete
     `~/.local/state/ulei/engine/contra-mcp.env`;
   - **map the allowlist to `relay_allow`**: take the non-comment, non-empty lines of
     `allow.txt` (text after `#` is a comment) and join them with `,`. At card h_e0e84034 the
     file held 33 tool names. It may have grown since, and the count of names is the check
     in step 6. The bridge matched with Python `fnmatchcase`, and `relay_allow` takes names
     and `*`. The bridge's file holds plain names only, so the mapping is one to one. An
     entry using `?` or `[...]` must be rewritten as names before the move;
   - keep `[engine.mcp.contra.tools]` (`tools.include`, `resources`, `prompts`) as it is.
5. **Start**: `systemctl --user start ulei-mcp-remote.service`, then restart the engine by the
   host's usual procedure.
6. **Check**:
   - `ulei status` prints `mcp-remote: ... contra ok`;
   - one turn calls `whoami`;
   - the engine's `tools/list` for `contra` has as many tools as `relay_allow` has names;
   - a call of a tool outside `relay_allow` is answered `tool <name> is not enabled on this
     relay`;
   - the daemon's journal shows no token.
7. **Clean up**, after a week without a rollback:
   - delete the bridge's state directory, which holds a refresh token the first rotation has
     made stale;
   - remove the unit file and `~/.local/lib/contra-mcp-bridge/`;
   - `systemctl --user daemon-reload`.

**Rollback** (at any point before step 7, again with one holder at a time):

1. Stop `ulei-mcp-remote.service`.
2. Copy the store's current `tokens.json`, `client.json` and `meta.json` back to
   `~/.local/state/contra-mcp-bridge/`. The daemon has rotated the refresh token, so the
   bridge's old copy is no longer valid. The format is the same in both directions.
3. Restore `url = "http://127.0.0.1:8745/mcp"` and `env_file`, recreating it from
   `local.token`. Remove `auth` and `relay_allow`.
4. `systemctl --user enable --now ulei-contra-mcp.service`, then restart the engine.

If the grant is already invalid (`needs_login`), the bridge cannot recover it, because it has
no login. In that case, run `ulei-mcp-remote login contra` with the daemon stopped, then copy
the new files back as in rollback step 2.

### Differences from the reference

| Behaviour | Upstream | Why |
|---|---|---|
| OAuth and TLS live in a separate daemon that relays MCP; the agent process never holds a token | `tools/mcp_oauth.py` `build_oauth_auth`, `tools/mcp_oauth_manager.py`, `mcp/client/auth/oauth2.py` (an `httpx.Auth` in the agent process) | TLS does not fit the engine's idle RSS (ADR-0015 section 4, ADR-0010); short-lived engine processes would race on a rotating refresh token; tokens stay out of the engine's sandbox (ADR-0069). |
| The interactive flow runs only in `ulei-mcp-remote login`; a server needing a login answers an error naming the command | `tools/mcp_oauth.py` `_is_interactive`, `_raise_if_non_interactive`, `force_interactive_oauth`, the browser start in `_make_redirect_handler` | The engine and the daemon run under systemd without a terminal or a browser; the reference's non-interactive path also refuses to start a flow. |
| No browser is opened; the URL is printed and a pasted redirect is accepted beside the loopback listener | `tools/mcp_oauth.py` `_can_open_browser`, `webbrowser` | Headless hosts are the normal case. |
| The step-up on `insufficient_scope` records the scopes and waits for a login instead of re-authorizing in the request | `mcp/client/auth/oauth2.py` `async_auth_flow` (403 branch) | Re-authorization is interactive (row above). |
| The reference's own published Client ID Metadata Document, its fixed CIMD ports and its per-vendor client defaults are not used; CIMD only with an owner-configured document URL, else dynamic registration | `tools/mcp_oauth.py` `_CIMD_CLIENT_METADATA_URL`, `_CIMD_PORTS`, `apply_oauth_provider_defaults` | A metadata document is a URL the publisher controls; the paseka publishes none in this step, and the reference's document names another product. Vendor-specific defaults are not ported (port only what the owner uses). |
| The `redirect_uri` and `redirect_host` overrides, the dashboard OAuth flow and the GUI-driven forced interactivity are not ported | `tools/mcp_oauth.py` `_resolve_redirect_uri`, `tools/mcp_dashboard_oauth.py` | No GUI; the loopback redirect plus paste covers the terminal case. |
| Token files are guarded by `flock` as well as mtime reload | `tools/mcp_oauth_manager.py` (mtime reload only) | Two writers of a rotating grant invalidate each other (seen with the prototype); the lock makes the login and the daemon's refresh exclusive. |
| An optional relay-side tool allowlist | none (the reference filters only in the agent, `tools.include`) | The process that holds the credential enforces the owner's grant, as the prototype did. |

The port map has no entry for the OAuth modules yet; the implementing card adds one for
`tools/mcp_oauth.py`, `tools/mcp_oauth_manager.py` and the SDK's `mcp/client/auth/` with the
rows above as its recorded changes (AGENTS.md, reference implementation section).

## Threat model

### Assets

- **Refresh tokens**: long-lived, rotating, one per grant. Whoever holds one acts as the owner
  on the service until it is revoked. It is the main asset.
- **Access tokens**: short-lived (the prototype's server: one hour), sent on every upstream
  request.
- **Client secrets**: `client_secret_file` of a confidential client, and a `client_secret` or
  registration access token returned by dynamic registration (`client.json`). Static API-key
  headers in the daemon's env-files have the same value.
- Transient secrets of a login: the authorization code, the PKCE verifier and `state`. Session
  ids, which ADR-0026 already redacts.
- The owner's account on the service itself, through tool calls. A paying or publishing tool
  called against the owner's will is a harm even when no token leaks.

### Adversaries and the measures against them

| Adversary and threat | Measures (section) | What remains |
|---|---|---|
| **A prompt-injected engine tool** (a terminal command the model was led to run) reads the tokens or the client secret | The engine never holds an OAuth token (section 1). The store and the daemon's env-files are in `InaccessiblePaths=` of the engine's unit (section 4, ADR-0069). The engine's `ProtectHome=read-only` keeps `ulei.toml` unwritable from its tools. | Only where the unit's namespace applies. Where the user manager cannot create it, the system unit of ADR-0069 is required. |
| The same tool calls a dangerous tool of the service, directly on the relay socket or through the model | `relay_allow` is enforced in the daemon on `tools/list` and `tools/call`, before a request leaves the host (section 6). Its config is read only by the daemon, from a file the engine cannot write. | The relay socket is in `%t/ulei`, which the engine may write, and it serves the engine's own uid. A tool can therefore call any tool inside `relay_allow`, bypassing `tools.include`. It could also replace the socket and feed the engine false tool results, though it gets no secret that way. `relay_allow` is the boundary, so it must hold only what the owner accepts the model calling. |
| The same tool runs `ulei-mcp-remote login` or `logout` | `login` needs the owner's browser and consent and writes to a store the sandbox cannot reach. `logout` fails for the same reason. | None beyond the phishing row below. |
| **Another local uid** connects to the relay socket, reads the store, or answers the login's loopback callback | The socket is 0600 in a 0700 directory, and the daemon checks `SO_PEERCRED` uid equality (section 2). The store is 0700/0600 (section 4). The callback checks `state` in constant time and `iss`, and PKCE `S256` makes a captured code useless without the verifier (section 3). | Another uid can take the redirect port first, or send junk to the listener. That makes the login fail, a denial of service, and the paste fallback still works. root is out of scope. |
| **A malicious or compromised MCP server** tries to get a token for another resource, steer discovery to an endpoint of its choice, or exhaust the daemon | `resource` must match the server URL (`_validate_resource_match`). `resource` is sent on every authorization, token and refresh request, so the token is bound to that server (RFC 8707). Discovery and OAuth endpoints are `https://` only. Redirects are not followed (section 2). Limits: head 8 KiB, event line and body 4 MiB, connect 60 s, idle 300 s, 16 connections. Token values are redacted from every output (section 4, ADR-0026). The engine's description scan (ADR-0026) runs on what `relay_allow` lets through. | The server legitimately receives its own access token. Its tool descriptions and results can still carry prompt injection to the model; `relay_allow` narrows that surface but does not remove it. It can name any authorization server in its resource metadata, so the owner trusts the server they configured. Discovery can reach any `https://` host the server names, a narrow request forgery that cannot carry a token. |
| **A malicious or compromised authorization server** mixes up issuers, or swaps metadata to capture codes or client credentials | The metadata `issuer` must equal the discovered one (`validate_metadata_issuer`). The `iss` parameter is checked (RFC 9207). Client credentials are bound to their issuer and dropped when it changes (SEP-2352). `https://` only. | An authorization server compromised at the service itself controls that grant anyway. That is outside this host's control, and the service's revocation is the remedy. |
| **A network MITM** reads or alters the traffic, or downgrades it | `https://` only in the shipped build. Certificates are checked against the bundled `webpki-roots`, with no platform verifier (ADR-0015 section 1). No redirects. The `http://` test switch exists only in CI builds (section 2). | A mis-issued certificate from a trusted CA. There is no pinning, since hosted services rotate certificates. |
| **A phishing authorization URL or pasted redirect**: an attacker sends the owner a lookalike login link, or gets the owner to paste a redirect from another session (code injection, login CSRF) | `login` builds the URL only from metadata discovered over `https://` for the configured server, and prints it itself; it never accepts a URL from outside. A pasted redirect is accepted only with this login's `state` and `iss`, and the code only works with this login's PKCE verifier (section 3). The daemon never starts a flow by itself (section 3, step 3), so no automatic process sends the owner a login link. | The owner can still be fooled by a link that did not come from `login`. The rule for the owner is to log in only from a URL their own `login` printed. A login from the owner chat would widen this surface and needs its own record (open questions). |

### Residual risks, in short

- A tool inside `relay_allow` can be called by a prompt-injected command through the relay
  socket. `relay_allow` must therefore list only what the owner accepts the model calling.
- Prompt injection through the remote server's tool descriptions and results.
- No certificate pinning. A compromised CA, authorization server or service is out of scope.
- Isolation of the tokens from the engine's tools depends on the unit's namespace (ADR-0069).
- Denial of service of a login by another local uid that takes the redirect port.

## Consequences

- Any owner can add a remote MCP server with one `[engine.mcp.<server>]` section, an `https://`
  URL and `auth = "oauth"` (or an env-file for an API key), then `ulei-mcp-remote login`.
- The engine gains no dependency and no TLS; its change is the socket route, the request path
  and the lifted `https://` refusal. ADR-0015 section 3's "only loopback http" now reads
  "only loopback http or the relay socket".
- One more daemon and unit on hosts that use remote servers; it costs no wake-ups when idle.
  Hosts without remote servers need not install it. When the socket is absent, an `https://`
  server fails its connect at the start of the run with the usual startup warning (ADR-0026),
  naming `engine.mcp_remote_socket`; the other servers connect as before.
- A grant moves between holders only by moving its files and stopping the old holder. The
  move from the prototype bridge, with its rollback, is the plan of section 12.
- A refresh token that expires while the server is unused (lazy refresh, no timer) surfaces as
  `needs_login` on the next call and in `ulei status`; whether to report it earlier is an open
  question.

## Open questions

Each is left for a later record or for the implementing change. None blocks the mechanism
above.

- **Login from the owner chat.** The assistant would send the authorization URL, and the owner
  would paste the redirect back. That needs an owner approval surface in the channel, C6 fields
  to carry it (section 11), and a rule against phishing, since the chat would then carry login
  links (threat model). Until then, login is the terminal command only (section 5).
- **A published paseka Client ID Metadata Document.** The paseka publishes none in this step,
  so CIMD works only with an owner-configured `client_metadata_url` (section 3). A shared
  document would spare each owner the dynamic registration. It would also need a URL that the
  paseka controls and keeps stable, a review of its `redirect_uris` (loopback ports), and a
  decision on who may change it.
- **The call-RSS budget is not measured yet.** The peak RSS of one relayed call whose event line
  is at the 4 MiB cap (`ULEI_MCP_REMOTE_MAX_CALL_RSS_KB`, section 8) has no number. The
  implementing change measures it and fixes it with its margin. If it or the idle limits do not
  hold, it comes back with the measurements and the owner's word (ADR-0053).
- **A refresh token that expires while the server is idle.** The daemon refreshes only on the
  request path and has no timer (ADR-0010), so a grant whose refresh token lapses during a long
  pause reads `needs_login` only at the next call. The prototype's server reports
  `refresh_token_expires_in`. Still open:
  - whether `GET /status` should report `needs_login` ahead of time from that field, without a
    wake-up;
  - whether `ulei status` should warn some days before expiry;
  - or whether a rare scheduled refresh (a timer unit, not a thread) is worth its wake-ups.
- Whether the relay socket should move out of the engine's writable `%t/ulei`, or the engine
  should check more than the peer uid, to close the socket replacement in the threat model.
