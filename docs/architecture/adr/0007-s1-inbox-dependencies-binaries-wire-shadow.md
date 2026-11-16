# ADR-0007: S1 owner inbox: dependencies, two binaries, budgets, wire and shadow

Status: accepted
Date: 2026-11-16
Affects: dep:serde, dep:serde_json, dep:rusqlite, dep:hmac, dep:sha2, dep:rustix, contract:C1

## Context

S1 moves the owner inbox into Rust over the same `events.sqlite3` file and the same C1 packet
as the Python inbox (`canonical()`, `Inbox.publish/get/recover/claim/start/renew/retry/ack`,
table `events`, `PRAGMA journal_mode=DELETE`, `synchronous=FULL`, `DEFER_SECONDS=3600`,
`MAX_OFFERS=3`). The S1 cards build on this record: the store (d2), the golden database (d3),
the daemon and its budgets (d4), shadow mode (d5) and the C6 client (d6). A dependency that a
later card needs and that is not listed here needs its own ADR.

## Decision

### 1. Two binaries

- `ulei` (`crates/core`, `[[bin]] ulei`) stays the small CLI. Its four S0 limits in
  `ci/budgets.sh` and its row in ADR-0001 are unchanged. It links no SQLite.
- `ulei-inbox` (`crates/core`, `[[bin]] ulei-inbox`, added with the daemon) holds everything
  that opens `events.sqlite3`: `ulei-inbox serve` (the daemon) and `ulei-inbox shadow`. It
  takes the "every daemon" row of ADR-0001 with the budgets of section 4; the static limit
  covers the whole binary, serve and shadow together.
- `ulei inbox <args>` replaces the process (`exec`) with `ulei-inbox <args>` and passes the
  remaining arguments unchanged. The sibling is taken only from the directory of the running
  `ulei` executable (`std::env::current_exe()`), never from `PATH`. A missing sibling is a
  non-zero exit with a message that names the expected path. The forward is chosen on the first
  argument, so `ulei --version` does no extra work. A service unit may start
  `ulei-inbox serve` directly; both spellings are the same daemon.

### 2. Dependencies

All are declared in `[workspace.dependencies]` and used by `ulei-core` only. Licenses are in the
`deny.toml` allow list, and `deny.toml` has no advisory `ignore`. Versions are pinned by
`Cargo.lock`; every locked version declares a `rust-version` of at most 1.89.

The workspace MSRV is 1.89; this replaces the MSRV of ADR-0001 decision 1. The first versions
free of two RustSec advisories need it:

- RUSTSEC-2026-0459: bundled SQLite before 3.54.0 can return uninitialized page-cache memory
  when a database is opened with a `-wal` file of a different page size. It affects
  `libsqlite3-sys` up to 0.38.2 (SQLite 3.53.2) and is fixed in 0.39.0; the locked
  `libsqlite3-sys` 0.39.1 bundles SQLite 3.54.1 and declares Rust 1.88. The `rusqlite` that
  uses it, 0.41, declares Rust 1.89.
- RUSTSEC-2026-0385: `ctutils` before 0.4.4 can branch in its constant-time mask functions.
  `hmac` 0.13 reaches it through `digest` 0.11; the locked `ctutils` 0.4.8 declares Rust 1.87.

| Crate (locked) | Features | Why | std alternative considered | Size delta |
|---|---|---|---|---|
| `serde` 1.0.229, `serde_json` 1.0.154 (workspace deps of ADR-0001) | `std`; `serde_json` adds `float_roundtrip` | Canonical JSON, wire bodies, divergence records. | No JSON in std. | `ulei` +0 B. `float_roundtrip` +16,384 B on the prototype. |
| `rusqlite` 0.41.1 (`libsqlite3-sys` 0.39.1, SQLite 3.54.1) | `bundled`, no default features | SQLite in a static musl binary, same file and SQL as Python. | No SQLite in std; `libsqlite3-sys` alone needs `unsafe` FFI, which is denied. | +1,122,208 B (SQLite and the store SQL). |
| `hmac` 0.13.0, `sha2` 0.11.1 | no default features | HMAC-SHA256 of the packet, SHA-256 digest, constant-time MAC compare. | A hand-written SHA-256 and HMAC: crypto code to own and test for 16 KiB saved. | +16,416 B together. |
| `rustix` 1.1.6 | `std`, `net`, no default features | `SO_PEERCRED` uid of a socket peer (`net::sockopt::socket_peercred`). | `UnixStream::peer_cred` is unstable; `getsockopt` through `libc` needs `unsafe`. | +0 B. |

New transitive crates: `libsqlite3-sys`, `fallible-iterator`, `fallible-streaming-iterator`,
`digest`, `crypto-common`, `hybrid-array`, `typenum`, `block-buffer`, `cpufeatures`, `cmov`,
`ctutils`, `linux-raw-sys`, `errno`, `windows-sys` (non-Linux targets only), and the build-only
`cc`, `shlex`, `find-msvc-tools`, `pkg-config`, `vcpkg`.

Bundled SQLite is compiled by `cc` for the target: a musl release build needs
`x86_64-linux-musl-gcc` (Debian and Ubuntu package `musl-tools`); the `budgets` CI job
installs it. Glibc builds use the system `cc`.

Kept on std, no crate: HTTP/1.1 (section 5; a `tiny_http` 0.12 variant of the prototype was
+159,824 B, starts threads and hides the socket descriptor, so no peer check), the 32-byte
producer key and the 16-byte lease token (read from `/dev/urandom` with `std::fs`), Unix
sockets (`std::os::unix::net`), clocks (`SystemTime`). No signal handling: every state change
is one SQLite transaction with `synchronous=FULL`, so `SIGTERM` with its default action is safe.

`serde_json` parses floats with `float_roundtrip`. Without it, about one in eight `time.time()`
values (17 significant digits) parses one ulp off (123,090 of 10^6 random samples), so the
canonical bytes and the HMAC change; with it, 0 of 10^6.

### 3. Measured sizes

`x86_64-unknown-linux-musl`, release profile of ADR-0001, rustc 1.100.0. The prototype
(`serve` over HTTP/1.1 on a Unix socket with the peer check, publish with field, HMAC and
digest checks, claim, start and ack on bundled SQLite, plus a load client) is not committed.

| Build | Static size | Budget |
|---|---|---|
| `ulei` on the S0 base | 381,624 B | 524,288 B |
| `ulei` with the dependencies of section 2 declared | 381,624 B | 524,288 B |
| prototype: JSON, Unix socket, HTTP/1.1 server path | 508,592 B | |
| + `rustix` peer check | 508,592 B | |
| + `hmac`, `sha2` | 525,008 B | |
| + `rusqlite` bundled: prototype `ulei-inbox` | 1,647,216 B | 2,621,440 B |

The same prototype after 10^4 cycles of publish, claim, start and ack on one connection, with
the database on local ext4: peak RSS 3,956 kB with the default SQLite page cache and 2,292 kB
with `PRAGMA cache_size=-512`; one thread; 0 voluntary context switches in 10 s idle; publish
round trip with `cache_size=-512` p50 4.9 ms, p99 18.8 ms; about 77 KB passed to `write(2)` per
cycle.

### 4. `ulei-inbox` budgets

The daemon adds its own section to `ci/budgets.sh`, next to the unchanged `ulei` section:

| Measure | Limit | Method |
|---|---|---|
| Static size | ≤ 2,621,440 B, statically linked | `stat` of the release binary. |
| Peak RSS | ≤ 4,096 kB | `VmHWM` of the daemon after 10^4 cycles of publish, claim, start and ack. |
| Ready | ≤ 10 ms | Spawn of `ulei-inbox serve` on a fresh root to the first `200` of `health`. |
| Idle wake-ups | 0 per s | `voluntary_ctxt_switches` of the daemon unchanged over 10 s idle after the run. |
| Publish latency | p99 ≤ 50 ms | 10^4 `publish` requests of distinct ids, one connection each, database on the runner disk (not tmpfs); client wall time from connect to the last response byte; p99 is the 9,900th of the sorted samples. |
| Write bytes per event | ≤ 128 KiB | `wchar` of the daemon (`/proc/<pid>/io`) over the 10^4 cycles, divided by 10^4. |

The store sets `PRAGMA journal_mode=DELETE`, `synchronous=FULL` and `cache_size=-512` and
uses a busy timeout of 10 s, as the Python inbox uses `timeout=10`.

### 5. Canonical JSON rules

The golden vectors in `contracts/fixtures/c1/golden/` cite these rules.

1. `canonical(v)` sorts the object keys of a `serde_json::Value` explicitly by code point at
   every level and writes it compactly with `serde_json`. `serde_json`'s `preserve_order`,
   enabled by `xtask`, is unified into workspace builds, so `Map` order cannot be relied on.
   UTF-8 byte order of the keys equals the code point order of Python `sort_keys`; separators
   `,` and `:`; non-ASCII text, DEL, U+2028, U+2029 and `/` raw; `"`, `\`, `\n`, `\r`, `\t`,
   `\b`, `\f` escaped as two characters and other C0 controls as `\u00XX` with lowercase hex.
   This equals Python
   `json.dumps(v, ensure_ascii=False, sort_keys=True, separators=(',', ':')).encode()`.
2. An integer `source_ts` stays an integer and a float stays a float (`1700000000` and
   `1700000000.0` are different bytes and different MACs); an integral float keeps `.0`.
3. Floats are parsed with `float_roundtrip`; every float from 1e-4 up to the future bound of
   `source_ts` then prints exactly as Python `repr` (shortest round-trip digits, fixed notation).
4. A float `source_ts` with 0 < x < 1e-4 is refused with `invalid_packet`: Python prints it
   with an exponent (`1e-05`), `serde_json` in fixed notation (`0.00001`). `0.0` and `-0.0`
   print alike and are accepted.
5. The `packet` column is `canonical(packet with mac)`; `digest` is the lowercase hex SHA-256
   of `canonical(packet without mac)`; `mac` is the lowercase hex HMAC-SHA256 of the same
   bytes under the 32-byte producer key, compared in constant time.

Publish checks the fields and values in the order of the Python inbox: exactly the seven
fields, the MAC, then version, kind, trust, id, payload (non-blank, at most 65,536 UTF-8
bytes) and `source_ts` (an integer or a finite float, not a boolean, from 0 to now + 300 s).
A float `source_ts` whose magnitude lies outside {0} ∪ [1e-4, 1e16) is refused as
`invalid_packet` before the MAC check: Python prints it with an exponent, so its canonical
bytes, the MAC input, cannot be reproduced (rule 4). `version` must be the integer 1; Python
also accepts `1.0` and `true`, which no producer sends, and the Rust inbox refuses them with
`invalid_packet`.

### 6. Wire

- Transport: HTTP/1.1 with JSON bodies over a `SOCK_STREAM` Unix socket. The socket path comes
  from `--socket` (at most 107 bytes, the `sun_path` limit); the daemon creates it with mode
  0600 in an owner-only directory.
- Operations: C1 `x-methods`; each is `POST /v1/<name>` with the input definition as the body
  and the output definition as the `200` body. The server reads its own clock for every `now`
  of the Python methods.
- Request framing: request line `POST /v1/<name> HTTP/1.1`, CRLF header lines, an empty line,
  then exactly `Content-Length` bytes. `Content-Length` is required; `Transfer-Encoding` is
  refused. Headers are at most 8 KiB, the body at most 512 KiB (a 65,536-byte payload of
  control characters is about 384 KiB as JSON). One request per connection: the answer carries
  `Connection: close` and the server closes. A connection that sends nothing for 5 s is closed,
  so one client cannot hold the single-threaded daemon.
- Response: `HTTP/1.1 <status> <reason>`, `Content-Type: application/json`, `Content-Length`,
  `Connection: close`, canonical JSON body.
- Peer check: after `accept`, the `SO_PEERCRED` uid of the peer must equal the effective uid of
  the daemon, else `403 forbidden` is sent before the request is read. The owner-only directory
  is the first gate; the uid check holds if its mode is ever widened.
- Errors: every non-2xx body is C1 `Error`.

| Condition | Status | `code` |
|---|---|---|
| Malformed request line or headers, body not JSON, body not valid for the input definition | 400 | `bad_request` |
| Packet MAC does not verify | 401 | `unauthenticated` |
| Peer uid differs | 403 | `forbidden` |
| Other path or method | 404 | `not_found` |
| Same id stored with a different digest | 409 | `conflict` |
| Headers or body over the limits | 413 | `too_large` |
| Packet fields or values invalid (section 5) | 422 | `invalid_packet` |
| SQLite or I/O failure | 500 | `internal` |

### 7. Shadow (d5)

- `ulei-inbox shadow` runs once per invocation (a timer starts it), so it adds no idle
  wake-ups. It opens the live Python database read-only as `file:<path>?mode=ro` with
  `SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_URI` and `PRAGMA query_only=1`, reads the `events`
  table in one short read transaction (a rollback-journal reader blocks the writer's commit),
  and never writes the live directory. It reads `producer.key` read-only.
- Replayed for every row: the packet is parsed and `canonical(packet)` must equal the stored
  bytes, the digest and MAC are recomputed (rule 5), and the packet is published into a private
  Rust store under `--state <dir>`; the Rust store must accept it (or refuse it under rule 4)
  and store the same bytes and digest. Against the previous snapshot kept in the same state
  directory, every changed row must follow an edge of the Python state machine (`pending` to
  `leased`; `leased` to `pending`, `processing`; `processing` to `done`, `uncertain`,
  `deferred`; `deferred` to `pending`, `uncertain`), with `packet`, `digest` and `created`
  unchanged and `attempts` not decreasing, and every row must be valid for C1 `Row`.
- Divergence record: one canonical JSON object per line, appended to `--out <file.jsonl>`:
  `{"check": <name>, "detail": <short text>, "id": <event id>, "live": <value>, "rust": <value>, "ts": <unix time>}`,
  where `check` is one of `packet_bytes`, `digest`, `mac`, `publish`, `row`, `transition`,
  `immutable`; byte values are given as `sha256:<hex>` and never as payload text. A run
  with no divergence appends nothing and exits 0; the exit is non-zero only when the shadow
  itself fails.

## Consequences

- `ulei` keeps its S0 budgets; the inbox budgets apply to `ulei-inbox` only.
- Building `ulei-core` compiles SQLite: glibc builds need `cc`, musl builds need `musl-gcc`.
- Until the store lands, cargo reports the S1 dependencies of `ulei-core` as unused.
- Producers that sign a `source_ts` below 1e-4 as a float are refused by the Rust inbox; no
  current producer does.
