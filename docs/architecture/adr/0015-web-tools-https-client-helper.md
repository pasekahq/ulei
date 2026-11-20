# ADR-0015: HTTP clients of the web and MCP tools: ureq 3 with rustls in a `ulei-web` helper, loopback MCP on the gateway client

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:ureq, dep:rustls, dep:ring, dep:webpki-roots

## Context

Two consumers in the engine need an HTTP client:

- The tools `web_search` and `web_extract`, ported from the reference implementation
  (`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3) with the Exa backend only:
  - `tools/web_tools.py`: `web_search_tool`, `web_extract_tool`, `_truncate_with_footer`,
    `_store_full_text`, the tool schemas and the URL pre-checks;
  - `tools/url_safety.py`: `is_safe_url`, the SSRF filter;
  - `plugins/web/exa/provider.py`: `search`, and `extract` through the SDK's `get_contents`.

  Both tools are HTTPS JSON POSTs to one Exa API host; Exa fetches the pages, ulei does not.
  The reference SDK (`exa_py/api.py`, exa-py 2.10.2 as pinned by the reference) sends
  `POST /search` with `{"query", "numResults", "contents": {"highlights": true}}` and
  `POST /contents` with `{"urls", "text": true}`, the key in an `x-api-key` header.
- The engine's MCP client over Streamable HTTP, reference `tools/mcp_tool.py`. It talks plain
  http to loopback servers. A POST is answered with `application/json` or with
  `text/event-stream`, and the answer carries `Mcp-Session-Id`.

std has no TLS. `deny.toml` bans hyper, reqwest, tokio and openssl-sys. ADR-0010 holds
`ulei-engine` to 2,621,440 B static, 4,096 kB idle RSS and 0 idle wake-ups, and compaction
and the C6 server still have to fit into that binary. A later `ulei-tg` binary also speaks
HTTPS and takes the TLS stack fixed here.

## Decision

### 1. TLS stack

| Crate | Features | Why |
|---|---|---|
| `ureq` 3 | `default-features = false`, `rustls` | Blocking HTTP/1.1 client without an async runtime. Its `rustls` feature brings rustls with TLS 1.2 and 1.3, the `ring` provider and the `webpki-roots` store. |
| `rustls` 0.23 | via `ureq` | TLS in Rust; no OpenSSL. |
| `ring` 0.17 | via `ureq` | Crypto provider. 778,112 B smaller than `aws-lc-rs` (section 4). Its C and assembly build with the `cc` toolchain that the musl build already needs for bundled SQLite. |
| `webpki-roots` 1 | via `ureq` | Mozilla roots compiled in. No file read at run time, the same trust on every host and in a static musl binary, pinned by `Cargo.lock`. |

Not enabled: `gzip` (+45,184 B measured), `brotli`, `charset`, `json`, `cookies`,
`multipart`, `socks-proxy`, `platform-verifier`, `native-tls`. No request sends
`Accept-Encoding`, so the body limits hold on the wire. `serde_json` is already a dependency
of `crates/engine`.

The implementing change adds these licences to `[licenses].allow` in `deny.toml`:

- ISC (`ring`, `rustls-webpki`, `untrusted`);
- BSD-3-Clause (`subtle`);
- CDLA-Permissive-2.0 (`webpki-roots`).

Any other ulei binary that needs outbound TLS takes the same stack and features under the
release profile of ADR-0001. Its own ADR names its crate, `dep:ureq` and its budget row.

### 2. Placement

Rule: a client goes into `ulei-engine` only if both of these hold:

- The engine with a call path through the client stays at or below 2,097,152 B. That keeps
  524,288 B (512 KiB) of the 2,621,440 B budget for compaction and the C6 server.
- Its idle numbers stay within ADR-0010 section 4.

Otherwise the client goes into a helper binary. The measured numbers are in section 4.

| Consumer | Route | Engine size | Idle `VmHWM` | Result |
|---|---|---|---|---|
| Exa (HTTPS) | TLS stack in the engine | 3,062,008 B | 4,396 to 4,488 kB | Fails: over the full budget, and over 4,096 kB idle. |
| Exa (HTTPS) | `ulei-web` helper, one process per call | unchanged | unchanged | Chosen. |
| MCP (loopback http) | (i) ureq in the engine, http only, `rustls` not compiled in | 2,091,000 B | 4,096 to 4,232 kB | Fits the size rule, but adds the dependency and idle reaches the 4,096 kB limit. |
| MCP (loopback http) | (i) ureq in the engine, http only, `rustls` unified in by the helper sharing the crate | 3,080,056 B | 4,308 to 4,592 kB | Fails: the plain-http agent links all of TLS. |
| MCP (loopback http) | (ii) the HTTP/1.1 client of `gateway.rs` over a loopback `TcpStream` | 1,853,688 B | 3,944 to 3,980 kB | Chosen: +18,944 B, idle as base, no new dependency. |
| MCP (loopback http) | (iii) through the helper | unchanged | unchanged | Rejected: a process per call, the session id handed back and forth, and a TLS binary carrying plain http. |

#### Exa: the `ulei-web` helper

`ulei-web` is a second `[[bin]]` of `crates/engine`. It is the same module, so its request
and reply are internal to the crate and not a contract.

What the helper does, and what stays in the engine:

- The helper ports `plugins/web/exa/provider.py`, the URL pre-checks and SSRF filter of
  section 5, and the result shaping of `web_extract_tool` (`_truncate_with_footer`,
  `_store_full_text`, the base64 image placeholders). An Exa reply of up to 16 MiB is parsed
  only in the helper, and the engine receives the finished tool result text.
- The engine keeps the tool schemas, the argument handling of the reference handlers (the
  `limit` and `char_limit` clamps, the first five `urls`), the untrusted-data frame of the
  result and the call. It applies no per-result cap: the per-result persistence of the
  reference's tool registry is not ported (ADR-0016 section 3).

How the engine calls it:

- One process per tool call, in its own process group. The engine starts the helper from the
  path in the setting `engine.web_helper` with an empty environment. The engine requires
  `ulei.toml` and reads it through `ulei_contracts::config` (ADR-0040); the variable
  `ULEI_ENGINE_WEB_HELPER` wins over the key, as each `engine.web_*` variable wins over its key. The arch gate forbids a literal `Command::new("ulei...")`.
- The engine writes one JSON request to stdin and closes it. The request holds the operation
  and its arguments, plus the configured Exa base URL (`engine.web_base_url`, `https://`
  only), env-file path (`engine.web_env_file`), full-text store directory
  (`engine.web_store`) and the 300 s Exa call limit.
- The helper answers with one reply on stdout: the tool result text with exit 0, or the
  reference error text with exit 1.
- The engine reads at most 16 MiB of reply. It kills the helper's process group 5 s after the
  300 s overall limit or on an interrupt, as it kills a `terminal` command.
- Without `engine.web_helper` the web tools are not registered and the other web settings are
  not read.

How TLS stays out of the engine binary:

- `ureq` is an optional dependency behind the crate feature `web`.
- The helper's `[[bin]]` has `required-features = ["web"]`, and all code that uses `ureq`
  lives in that bin target, never in the library. The helper's request handling and result
  shaping live in the library module `web`, compiled only with `web`.
- The engine and the budgets job build `-p ulei-engine` without `web`, so `ureq` is not even
  compiled there. Release packaging builds `--features web --bin ulei-web` separately.
- Measured: with `ureq` as a dependency that the engine does not call, the engine is the
  base size. A plain-http `ureq` agent in the engine with `rustls` unified in links the whole
  TLS stack (+1,245,312 B).

Budget row of `ulei-web`, added to `ci/budgets.sh` by the implementing change:

| Measure | Limit | Environment variable |
|---|---|---|
| Static size | ≤ 2,097,152 B (2 MiB), statically linked | `ULEI_WEB_MAX_BINARY_BYTES` |
| Peak RSS of one `web_extract` whose Exa reply is at the 16 MiB cap | ≤ 81,920 kB | `ULEI_WEB_MAX_RSS_KB` |

The helper has no idle state: it exists only during a call.

- Size: the limit leaves 451,544 B over the gnu measurement. The margin covers the musl
  difference and the ported shaping code.
- RSS: the limit leaves 13,148 kB over the 68,772 kB measured at a 16,700,000-byte reply.
- CI: `ci/web_rss.py` serves the reply from a fake Exa on loopback plain http to the helper
  built with the `web-loopback-http` test switch (ADR-0021) and reads the helper's peak RSS
  with GNU `time`. The shipped `web` build takes only `https://` base URLs and trusts only the
  bundled roots.

#### MCP: the gateway client over loopback TCP

The engine's HTTP/1.1 client in `gateway.rs` serves both the qb gateway (Unix socket) and
MCP (loopback `TcpStream`):

- Its connection reader becomes generic over the stream.
- The head parser also returns `Content-Type` and `Mcp-Session-Id`.
- A chunked `text/event-stream` answer is read line by line with the existing chunk and line
  reassembly.
- A `Content-Length` `application/json` answer is read as a body.

No dependency is added to the engine, and TLS never enters it.

### 3. MCP HTTP transport

| Item | Value | Source |
|---|---|---|
| Servers | `http://` URLs whose host is `127.0.0.1`, `[::1]` or `localhost` resolving only to loopback; any other URL is refused at config load | Deviation, see below |
| Connect | 60 s per server (`connect_timeout`), also the bound of the initialize handshake | `_DEFAULT_CONNECT_TIMEOUT` 60 |
| Tool call | 300 s per call (`timeout` per server), overall | `_DEFAULT_TOOL_TIMEOUT` 300 |
| Read | Idle timeout between bytes: each socket read waits at most 300 s or the time left of the call, whichever is shorter | httpx `Timeout(connect, read=300.0)` of the reference's HTTP client |
| Streamed answer | `text/event-stream` is read synchronously and incrementally. Each `data:` line is parsed as it arrives, and the call returns at the JSON-RPC response with the request's `id`. | Streamable HTTP |
| Session | `Mcp-Session-Id` from the initialize answer is sent on every later request of the session | Streamable HTTP |
| Headers | Per server, values read from an env-file at a configured path; sent only to that server; never logged, journaled, put in a tool result or an error line | Header values are secrets; they stay out of the config file, the environment and the logs (section 6) |
| Redirects | None; a 3xx answer is an error | Deviation, see below |
| Limits | Head 8 KiB; one SSE line 4 MiB; `application/json` body 4 MiB | R3 budget refinement (reference has none) |

Deviations from `tools/mcp_tool.py`:

- Only loopback http: the owner's servers are loopback (PO entry 6 on the sprint). Remote
  https servers and OAuth would need TLS in the engine or a long-lived helper session, which
  calls for a separate ADR.
- No redirects (the reference follows them and strips auth headers when a redirect leaves
  the origin): a loopback server at a configured port has no reason to redirect.
- Header values come from an env-file, not from `${VAR}` interpolation of the config.

### 4. Measurements

The host has no `x86_64-linux-musl-gcc` (needed by bundled SQLite and by `ring`) and no root
to install it, so the measurements use gnu, as ADR-0010 section 3 did:

- rustc 1.100.0, `x86_64-unknown-linux-gnu`, release profile of ADR-0001;
- dynamically linked to glibc and stripped;
- locked versions: ureq 3.5.2, ureq-proto 0.7.1, rustls 0.23.46, rustls-webpki 0.103.17,
  ring 0.17.14, webpki-roots 1.0.12, aws-lc-rs 1.19.0, serde_json 1.0.154.

The rule in section 2 keeps its margin because the TLS route fails even without the reserve.

Each engine variant is a throwaway copy of the base commit:

- `crates/engine` gets optional dependencies and features for the variant.
- `main` gets an argument that performs one call through the client under test, so the
  linker keeps it.
- TLS variants: one HTTPS GET of `https://example.com/` (577 bytes, fetched).
- MCP variants: one `tools/call` POST with `Authorization` to a local fake. The fake answers
  with a chunked `text/event-stream` of a progress notification and the response, and an
  `Mcp-Session-Id`. Every variant read the session id and both messages.
- Variant (ii) makes the `Conn` of `gateway.rs` generic over the stream, adds `Content-Type`
  and `Mcp-Session-Id` to the parsed head, and adds a call that connects a `TcpStream` with
  `connect_timeout`, sets the read and write timeouts, writes the request and reads `data:`
  lines through `next_line`.

```sh
cargo build --release --locked -p ulei-engine
cargo build --release -p ulei-engine --features <variant>
stat -c %s target/release/ulei-engine
python3 ci/engine_idle.py target/release/ulei-engine "$(mktemp -d -p /var/tmp)"
/usr/bin/time -f %M target/release/ulei-engine <call argument> <target>
```

| `ulei-engine` build | Size | Over base | Idle `VmHWM` per run | Peak RSS of the call |
|---|---|---|---|---|
| Base: `main` at ea66841 (engine sources as at 21eb99a) | 1,834,744 B | | 3,944; 3,980; 3,980; 3,980; 4,004; 4,004 kB | |
| `ureq` with `rustls` as a dependency, not called | 1,834,744 B | 0 | | |
| TLS: `ring` + `webpki-roots`, GET called | 3,062,008 B | 1,227,264 B | 4,396; 4,428; 4,488 kB | |
| TLS: `ring` + system bundle (`/etc/ssl/certs/ca-certificates.crt` parsed with `ureq::tls::parse_pem`) | 3,006,456 B | 1,171,712 B | 4,384 kB | |
| TLS: `aws-lc-rs` + `webpki-roots` | 3,840,120 B | 2,005,376 B | | |
| TLS: `ring` + `webpki-roots` + `gzip` | 3,107,192 B | 1,272,448 B | | |
| MCP (i): `ureq` http only, no `rustls` | 2,091,000 B | 256,256 B | 4,232; 4,096 kB | 3,456 kB |
| MCP (i): `ureq` http only, `rustls` unified in | 3,080,056 B | 1,245,312 B | 4,592; 4,308 kB | 3,712 kB |
| MCP (ii): `gateway.rs` client over loopback TCP | 1,853,688 B | 18,944 B | 3,980; 3,944 kB | 2,944 kB |
| `gateway.rs` with the generic `Conn` only, no MCP call | 1,835,768 B | 1,024 B | 4,004; 3,972 kB | |

Idle wake-ups were 0 in every run. Idle is measured as in ADR-0010 section 4: work directory
on ext2/ext3 under `/var/tmp`, 10 s. The idle path never touches the linked client, so the
idle rise comes from the larger binary alone.

Helper builds are a standalone crate with `ureq` and `serde_json` under the same profile:

- It reads the JSON request on stdin, applies an approximation of the SSRF check of section 5
  with std `ToSocketAddrs`, and reads `EXA_API_KEY` from the env-file.
- It POSTs to `<base_url>/search` or `<base_url>/contents` with https only, no redirects,
  connect 60 s, overall 300 s and the 16 MiB body cap.
- It parses the reply into `serde_json::Value`.

| Helper build | Size | Run | Peak RSS |
|---|---|---|---|
| GET with `webpki-roots` | 1,592,424 B | `https://example.com/` | 3,968 kB |
| GET with the system bundle | 1,534,024 B | `https://example.com/` | 4,352 kB |
| Exa POST with `webpki-roots` | 1,645,608 B | Local fake Exa: refused with `invalid peer certificate: UnknownIssuer`, as its test CA is not a bundled root | |
| Exa POST with the fake's CA as root | 1,652,248 B | `search`, limit 5: 5 results, `x-api-key` sent | 3,840 kB |
| | | `extract` of 5 URLs, one private: 4 sent, `http://10.0.0.1/c` blocked; reply with 15,000,000 text bytes | 62,088 kB |
| | | `extract` of 5 URLs; reply with 16,700,000 text bytes, two runs | 68,696; 68,772 kB |
| | | Reply with 17,500,000 text bytes: refused at the 16,777,216 B cap | 20,608 kB |
| Exa POST + MCP over http, route (iii) | 1,653,368 B | MCP fake: session id and both messages relayed | 3,028 kB |

Bundled roots cost 58,400 B over parsing the system bundle. In exchange they need no file
read and no parse at each call, and save 384 kB of peak RSS.

### 5. Limits of the web tools

Reference values, from `tools/web_tools.py` unless noted:

| Limit | Value |
|---|---|
| `web_search` results | `limit` default 5, clamped to 1..100 |
| `web_extract` URLs | At most 5 per call (`urls[:5]`, schema `maxItems` 5) |
| Characters per page | `char_limit` default 15000 (`DEFAULT_EXTRACT_CHAR_LIMIT`), clamped to 2000..500000 |
| Truncation | `_truncate_with_footer`: whole page up to `char_limit`. Beyond it, 75 % head and 25 % tail, each snapped to a line boundary, plus the reference's footer. The full text is stored up to 2,000,000 characters (`MAX_STORED_TEXT_CHARS`). |
| URL pre-checks | Refuse URLs that contain a secret prefix or a credential-like query parameter. Then refuse private and internal targets with `is_safe_url` of `tools/url_safety.py`, before any backend call: scheme http or https, blocked hostnames, every resolved address outside private, loopback, link-local, metadata and reserved ranges, and fail-closed on DNS errors. |
| Tool result | 100,000 characters per result (`max_result_size_chars` of both tools in the tool registry) |
| Time | `exa_py` 2.10.2 sends with `requests` and no `timeout`, so the reference has no time limit on an Exa call. Its only bound is the interrupt check before the call. |

R3 budget refinements (limits the reference lacks):

| Limit | Value | Why |
|---|---|---|
| Exa reply body | ≤ 16,777,216 B; a larger reply fails the call with the reference's failure shape | Bounds the helper's RSS (section 2 budget row). It holds five ASCII pages at the 2,000,000-character store cap. |
| Exa connect | 60 s | A hung connect must not hold the turn. The value is the reference's MCP connect constant. |
| Exa call | 300 s overall; the engine kills the helper at 305 s | The same reason. The value is the reference's MCP tool-call constant. |
| Helper reply read by the engine | ≤ 16 MiB | Bounds the engine's memory during a call. |

Other deviations from the reference, each deliberate:

| Deviation | Reason |
|---|---|
| No redirects on Exa calls; a 3xx answer is an error. `requests` follows up to 30, and does not strip `x-api-key` when a redirect leaves the host. | A fixed API host needs none, and following one could send the key to another host. |
| Exa base URL from config, required; scheme `https` only | Deployment hosts come from config (AGENTS.md hard rule 7). |
| No `x-exa-integration` header; `User-Agent: ulei-web/<version>` instead of `exa-py/<version>` | The integration header is vendor analytics and names the upstream product, which `.rs` files may not. |
| `EXA_API_KEY` from an env-file at a configured path, not the process environment | The key never enters an environment or argv (section 6). |
| An env-file that cannot be read answers `cannot read the env-file <path>: <error>` where the reference's missing-key error stands | The reference has no env-file of its own; the path is not secret and names the fault. |
| Keyless free tier (`plugins/web/keyless_mcp.py`) and the keyless rescue of failed calls not ported; without a key the tools return the reference's missing-key error | Exa with a key is the deployment's backend, and the other backends are on the do-not-port list of the port map. |
| No HTTP proxy, and no delegation of DNS to a proxy in the SSRF check | The helper runs with an empty environment and the deployment reaches Exa directly. A proxy would come from config with its own record. |
| No `security.allow_private_urls` toggle and no trusted private-address host list (`_TRUSTED_PRIVATE_IP_HOSTS`): every private, loopback, link-local, metadata, multicast and reserved target is refused | The deployment reads public pages only; the one trusted host of the list serves a messenger outside the port map. |
| IDNA of a host outside ASCII: each such label is lowercased and stripped of the characters nameprep maps to nothing, then Punycode-encoded; nameprep's NFKC step and the NFKC check of `urlsplit` on a netloc outside ASCII are not ported | The helper carries no Unicode normalisation tables. A host that changes only under NFKC gets another name, which fails the DNS check or names another public host. |
| Transport failure texts: ureq's (connect, TLS, timeout, size limit) and serde_json's parse errors replace those of `requests` and `json`; a 3xx answer reads `redirect not followed: status code <n>` | Another HTTP client. The shapes stay the reference's: `Exa search failed: ...`, the per-page `Exa extract failed: ...`, and the SDK's `Request failed with status code <n>: <body>` alone. |
| No per-result cap: a result over 100,000 characters reaches the model whole instead of as a stored file with a preview | The engine ports no per-result persistence for any tool (ADR-0016 section 3). |
| Full texts of truncated pages go to the directory of `engine.web_store`, not `cache/web` under the agent home | Deployment paths come from config (AGENTS.md hard rule 7). |

### 6. Secrets and vendor hosts

The helper reads `EXA_API_KEY` from the env-file at each call; the engine passes the path,
never the value. The key goes only into the `x-api-key` header of the request to the
configured Exa base URL. It never appears in any of these places:

- argv or an environment;
- a log, the journal or a tool result;
- an error line, which names the failure and the host, never request headers.

No logger is installed in the helper, so the `log` records of ureq and rustls go nowhere.
MCP header values follow the same rules (section 3).

Vendor hosts stay excluded by the `cargo xtask arch` rule of ADR-0009, unchanged:

- No host appears in the sources: the Exa base URL and the MCP server URLs come from config.
- The helper's environment is empty, and it sends no model credential.

## Consequences

- `ulei-engine` keeps its ADR-0010 budgets and never links TLS. The MCP transport costs it
  18,944 B (gnu), which leaves 767,752 B of the static budget for compaction and the C6
  server.
- Each web call pays a process start and a full TLS handshake; no connection is reused
  between calls.
- The root store changes only with a `webpki-roots` update in `Cargo.lock`. No private CA is
  trusted.
- A remote or https MCP server is outside this decision.
- ADR-0021 adds `ureq` and `ring` to `crates/engine` behind the `web` feature, with the test
  switch the tests and the RSS driver use.
