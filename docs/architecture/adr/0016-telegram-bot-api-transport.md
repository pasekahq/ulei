# ADR-0016: Telegram Bot API transport of `ulei-channels`: ureq with rustls, serde_json, `ulei-tg` budget row

Status: accepted
Date: 2026-11-17
Affects: crates/channels, dep:ureq, dep:serde_json

## Context

U3 builds the owner's Telegram channel `ulei-tg` in `crates/channels`. Its transport talks to
the Telegram Bot API: long polling for updates, sending text, chat actions and reactions,
downloading attachments and uploading files. The public Bot API is HTTPS; in production the
channel talks plain `http://` to a self-hosted Bot API server on loopback, so both schemes
must work and both base URLs come from config.

The behaviour is ported from the reference agent (`THIRD_PARTY_NOTICES.md#ref-agent`): its
Telegram adapter, the shared send ladder of its platform base, and the polling loop of the
Telegram library it runs on. ADR-0015 fixed the TLS stack for every ulei binary and asks
each binary that takes it for its own ADR naming its crate, `dep:ureq` and its budget row.

## Decision

### 1. Dependencies of `crates/channels`

| Crate | Features | Why |
|---|---|---|
| `ureq` 3 | `default-features = false`, `rustls` | The ADR-0015 stack unchanged: blocking HTTP/1.1, rustls with `ring` and bundled `webpki-roots`. No `gzip`, `json` or `multipart`. |
| `serde_json` (workspace) | workspace features | JSON request bodies and the Bot API envelope; updates stay `serde_json::Value` for the ingress. |

`rustls`, `ring` and `webpki-roots` come through `ureq` as in ADR-0015; the licences that
ADR lists (ISC, BSD-3-Clause, CDLA-Permissive-2.0) are added to `deny.toml` with this change.

### 2. Transport shape

- Module `crates/channels/src/bot_api/`: `Client` (one call per Bot API method), `Poller`
  (long-poll loop), `Sender` (send ladders, flood limiter, typing cooldown), `Config`, `Error`.
- A method URL is `{base_url}{token}/{method}`, a file URL `{file_base_url}{token}/{path}`,
  as in the reference's Telegram library. Defaults are the public Bot API; a non-default
  `base_url` makes `file_base_url` follow it and raises the download limit from 20 MiB to
  2 GiB, as the reference does for a self-hosted server.
- The token lives only in request paths. Errors carry Telegram's `description` or a short
  local text, never ureq's own message (it may contain the URL), and every text passes the
  reference's redaction (the configured token and any `<8+ digits>:<30+ chars>` become
  `<digits>:***`). `Config`'s `Debug` hides the token. Redirects are off.
- Timeouts of the reference: connect 10 s, read 20 s, long poll `timeout` 10 s on top of the
  read timeout, uploads 60 s per write and 60 s for the response.
- Downloads stream into a caller's writer or a new file with mode 0600 and stop at the limit;
  uploads stream a hand-written `multipart/form-data` body from the file with an exact
  `Content-Length`.

### 3. Measured size and idle

The host lacks `x86_64-linux-musl-gcc` (needed by `ring` and bundled SQLite), so as in
ADR-0010 section 3 and ADR-0015: rustc 1.100.0, `x86_64-unknown-linux-gnu`, release profile
of ADR-0001, dynamically linked to glibc, stripped. Locked versions: ureq 3.5.2,
rustls 0.23.46, ring 0.17.14, webpki-roots 1.0.12, serde_json 1.0.154.

A throwaway example binary (not committed) built a `Client` from a base URL given on argv,
so the scheme is decided at run time and the linker keeps the TLS stack; it called `getMe`
and then blocked in `Poller::next_batch` with a 600 s poll timeout.

```sh
cargo build --release --locked -p ulei-channels --example probe
stat -c %s target/release/examples/probe
```

| Build | Size |
|---|---|
| Probe: `Client`, `Poller`, TLS stack, `serde_json` | 1,648,696 B |

Idle: a loopback fake answered `getMe` and read the `getUpdates` request without answering.
Two seconds after start the driver read `voluntary_ctxt_switches` from `/proc/<pid>/status`,
again 10 s later, then `VmHWM`.

| Run | `VmHWM` | Wake-ups in 10 s |
|---|---|---|
| 1 | 3,304 kB | 0 |
| 2 | 3,244 kB | 0 |
| 3 | 3,348 kB | 0 |

### 4. Proposed budget row of `ulei-tg`

`ulei-tg` takes the "every daemon" row of ADR-0001 unchanged. The change that adds the
binary adds the row to `ci/budgets.sh` for the musl release build:

| Measure | Limit | Environment variable |
|---|---|---|
| Static size | ≤ 2,621,440 B (2.5 MiB), statically linked | `ULEI_TG_MAX_BINARY_BYTES` |
| Idle RSS | ≤ 4,096 kB (4 MiB) | `ULEI_TG_MAX_RSS_KB` |
| Idle wake-ups | 0 voluntary context switches over 10 s idle | `ULEI_TG_MAX_IDLE_WAKEUPS` |

Idle is the binary blocked in `getUpdates` with no turn in flight; the driver's fake Bot API
reads the long poll and never answers, as the fake gateway of ADR-0010 section 4 does. The
transport alone leaves 972,744 B of the static limit (gnu) and about 750 kB of idle RSS for
ingress, rendering, the C6 client and speech-to-text. A real server answers an empty long
poll after the poll timeout; that is an I/O wake-up of the server's choosing, not a timer
of the client.

### 5. Differences from the reference

The reference is asynchronous; ulei uses blocking calls on std threads (ADR-0001). Where the
shape changes for that reason:

- One blocking loop replaces the library's polling task and the adapter's recovery tasks.
  The ladders keep the reference's numbers: flood control waits `retry_after` + 0.5 s; a
  response timeout polls again at once; an invalid token (401/404) is fatal; a conflict
  waits 10 s + 10 s × attempt (20 … 60 s), then re-bootstraps with
  `deleteWebhook(drop_pending_updates=true)`, fatal after 5; a network error waits
  5 s × 2^(n−1) capped at 60 s, then re-bootstraps with `deleteWebhook(false)`, fatal after 10;
  any other error waits 1 s, then 1.5× up to 30 s. Progress resets the counters, except that
  the first success after a conflict restart keeps the conflict count.
- While the reference waits in its network ladder, the library's own task keeps retrying
  every 1–30 s in parallel. A blocking loop has one poller, so only the ladder's wait applies.
- Dropped as async-pool workarounds with no blocking counterpart: the periodic `getMe`
  heartbeat and the polling-progress verifier (they detect a long poll wedged on a dead
  socket; here the read timeout ends it and the network ladder runs, and a heartbeat would
  be a timer in idle), the 60 s cold-start progress gate, the stop/drain watchdogs, and the
  "send path degraded" fast failure of sends while polling recovers (sends use their own
  connections and their own ladder).
- Fatal polling errors are returned from `Poller::next_batch`; the binary exits and its
  supervisor restarts it, where the reference marks the adapter fatal for its gateway runner.
- A stop request ends any back-off wait at once and is checked between polls; an in-flight
  long poll is not interrupted, so a stop takes at most one poll timeout plus the read
  timeout. A batch fetched after the stop is not confirmed, as in the library.
- Attachments are streamed to disk with the reference's limits instead of being read into
  memory.

Other differences:

- Flood limiter: the reference sends without pacing. `Sender` waits so that at most one
  message per second goes to a chat and 30 per second in total, Telegram's documented
  limits, so bursts of split replies do not run into 429. It only delays; nothing is
  dropped.
- `Sender::send_with_retry` honours a `retry_after` carried by the failure once, as the
  platform base intends; the reference's Telegram adapter never fills that field.
- The platform base's plain-text fallback after a non-network delivery failure, the re-sent
  typing action after an intermediate message, and the skip of whitespace-only text are not
  in the transport: the rendering module owns them.
- `Sender::send_media` returns a failure without the reference's fallback to a text
  message; the caller decides.

## Consequences

- `crates/channels` links TLS only into binaries that call the client; other code of the
  crate is unaffected.
- The wiring card adds the `ulei-tg` binary, its config keys (`base_url`, `file_base_url`,
  token from a secret file, timeouts) and the budget row above.
- A new Bot API method is a new `Client` method over the same envelope parsing; no new
  dependency.
