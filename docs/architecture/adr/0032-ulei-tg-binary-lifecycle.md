# ADR-0032: the `ulei-tg` binary: place, threads, cold start, config and signals

Status: accepted
Date: 2026-11-19
Affects: crates/channels, dep:signal-hook

## Context

ADR-0017 and ADR-0018 leave the wiring of the owner's Telegram channel to the binary: poll
updates, feed the ingress, sleep until its deadline, hand its outputs on, read the config file.
The transport left two choices open: whether a cold start drops the updates Telegram queued, and
which of the two typing cooldowns (the transport's `Sender` or the renderer's `TypingRefresh`)
governs. The reference (`THIRD_PARTY_NOTICES.md#ref-agent`) runs the channel as an adapter task
inside its asynchronous gateway: `connect`, `_start_polling_resilient`, `disconnect`, and the
gateway's adapter start and stop. ulei runs it as its own synchronous daemon (ADR-0001).

## Decision

### 1. Place and kind

`ulei-tg` is a `[[bin]]` of the package `ulei-channels` (`crates/channels/src/main.rs`), a
daemon under the "every daemon" budget row of ADR-0001 with the row ADR-0017 section 4
proposes. The binary only parses its arguments, registers the signals and prints its lines; the
lifecycle (`ulei_channels::tg`) and the settings are library code tested through that seam.

Usage: `ulei-tg [--config PATH]`, the path else from `ULEI_CONFIG` as `ulei-engine` reads it;
`ulei-tg --version`. Exit codes: 0 after a signal stop, 1 for a startup or polling failure,
2 for a usage error.

### 2. Threads

- `poller`: `Poller::start`, then `Poller::next_batch` in a loop; each batch goes to the loop
  thread over a channel of capacity one, so a busy loop thread holds the next `getUpdates`
  back instead of piling batches up. The thread only polls.
- The main thread is the loop: it takes a batch and calls `Ingress::accept` for each update,
  else sleeps in the channel's receive until `Ingress::next_deadline` (a plain receive with no
  deadline, so an idle channel has no timer), then calls `Ingress::poll`. Attachment downloads
  (`FileSource`) and speech to text (`Transcriber`) run inside `accept` on this thread, never on
  the poller thread.
- `signals`: SIGTERM and SIGINT write a byte to a socket pair through `signal-hook`, as
  `ulei-engine run` does (ADR-0029); the thread blocked on the other end requests the poller's
  `Stop`.
- Every `Output` goes, in order, to one seam, `tg::Dispatch`, on the loop thread. The
  implementations own the threads they block on; `Sender` is shared under `Arc` between them.
- `outbound`: `tg::Outbound` is the binary's `Dispatch`. It sends a notice to the owner as a
  reply to the message it concerns and counts turns and commands without content. Its thread
  runs the jobs of `tg::Outbox` handles in order: `deliver` (chat, reply target, text, final or
  interim) executes the `render::plan_reply` plan through `Sender` (section 7);
  `turn_started` and `turn_ended` drive the typing indicator of a chat (section 4). With no turn
  running the thread sleeps in its channel's receive with no deadline. The stop waits until
  every queued job is done.

### 3. Lifecycle

- Start: settings are read and checked, the attachments directory is created, `getMe` gives
  the bot's user name for mention cleaning; any failure exits 1 with one stderr line. Then
  `ulei-tg ready` on stdout.
- Cold start keeps the updates Telegram queued: `Poller::start(false)`.
- Stop: a signal requests the stop; the poller returns after its in-flight long poll (at most
  the poll timeout plus the read timeout, ADR-0017 section 5) without confirming a batch
  fetched after the request. The loop thread accepts every batch already handed out, runs every
  pending ingress deadline at once so coalesced turns and reaction wake-ups reach the dispatch
  seam, and only then `Poller::shutdown` confirms the offset.
- Guarantee: a clean SIGTERM or SIGINT stop hands every accepted update on before it confirms
  the offset, so the next start neither re-reads handled updates nor loses accepted ones. The
  poller's next long poll already confirms a batch it handed to the loop thread, so a crash
  while that batch waits in a coalescing window or a reaction delay loses it.
- A fatal polling error (`Poller::start` or `Poller::next_batch`) ends the poller; the loop
  thread flushes the ingress as above, the offset is not confirmed, and the process exits 1 for
  its supervisor (ADR-0017 section 5).
- A second signal changes nothing.

### 4. Typing cooldown

The renderer's `TypingRefresh` is the one per-chat typing cooldown. It ports the reference's
`send_typing` cooldown with the 1.5 s cancel of `_keep_typing` (a timed-out call starts no
cooldown) and carries the deadline from turn to turn (ADR-0019 section 3). The transport keeps
`Client::send_chat_action` and no cooldown of its own.

The `outbound` thread keeps one `TypingRefresh` per chat: `turn_started` starts it with the
cooldown deadline the chat's previous turn left, `turn_ended` stops it and keeps the entry
while that cooldown lasts. Cadence calls go through a client whose connect and read timeouts
are `TYPING_SEND_TIMEOUT`, so a call that times out is abandoned as the reference cancels it.
The `typing` after an interim reply goes through the shared client, is skipped during the
chat's cooldown and records its outcome, as the reference's `send` calls `send_typing`.

### 5. Config

`ulei-tg` reads `ulei.toml` with its own reader of the same strict TOML subset `ulei-engine`
accepts (section headers, `key = "string"`, `key = integer`, `key = true|false`, comments),
because `ulei-channels` may depend on no other module crate (ADR-0001) and `ulei-contracts`
holds no logic. The reader is about 100 lines; the shared file stays in the subset both read.
Values come only from the file.

| Key | Required | Meaning |
|---|---|---|
| `telegram.token_file` | yes | File holding the bot token; mode 0600 or stricter (no group or other bits), else a startup error. |
| `telegram.owner_ids` | yes | Owner user ids: an integer or a comma-separated string of integers. |
| `telegram.attachments_dir` | yes | Inbox of saved attachments, created with mode 0700; files are written 0600 (ADR-0018). |
| `engine.c6_socket` | yes | The engine's C6 socket, the same key `ulei-engine serve` binds. |
| `telegram.base_url`, `telegram.file_base_url` | no | `http://` or `https://` bases of ADR-0017 section 2; defaults are the public Bot API, and a set `base_url` makes the file base follow it. |
| `telegram.connect_timeout`, `read_timeout`, `media_read_timeout`, `poll_timeout` | no | Seconds, defaults 10, 20, 60, 10 (ADR-0017 section 2); the poll timeout is whole seconds. |
| `telegram.coalesce_quiet`, `telegram.coalesce_max` | no | Turn coalescing windows in seconds, defaults 2 and 7 (ADR-0018 section 2); 0 quiet disables coalescing. |
| `telegram.stt_env_file` | no | Env-file with the speech-to-text key, for the STT client. |
| `engine.timezone` | no | IANA zone of the dates the agent sees; the host's `/etc/localtime` (UTC without it) when unset, as the engine reads it. |
| `telegram.reaction_wake_delay` | no | Seconds from the first owner reaction to the wake-up turn, default 15 (ADR-0018). |
| `telegram.reaction_wake_text`, `telegram.reaction_context_header` | no | Non-empty wording of the wake-up turn and the first line of the reaction context; the defaults of `ReactionConfig`. |

Seconds are an integer or a decimal string. A missing or invalid required key, or an invalid
optional one, is a startup error naming the key. The token appears in no error, log line or
`Debug` output.

### 6. Dependency

| Crate | Features | Why |
|---|---|---|
| `signal-hook` (workspace, ADR-0030) | none | `low_level::pipe::register` without `unsafe` at the call site; the handler writes one byte. Measured at 9,600 B with its reader thread (ADR-0029 section 1). |

### 7. Delivery

`Outbox::deliver` ports the reference's `_send_with_retry` around `send`. A blank text sends
nothing. Each chunk of the plan goes out as `MarkdownV2` through `Sender::send_message` (flood
limits, attempts, a missing reply target dropped) and once more as
`OutgoingText::plain_fallback` when `is_markup_rejection` holds. `Sender::send_with_retry`
retries the whole reply and ends with its failure notice; any other failure except a first
response timeout resends `(Response formatting failed, plain text:)` and the first 3500
characters once. Every message the bot sends is recorded in `tg::SentTexts` with the plain text
it shows, which the ingress reads as its `MessageTexts`.

### 8. Differences from the reference

| Reference (module) | Here | Why |
|---|---|---|
| A cold first boot drops the queued updates; only a watcher reconnect keeps them (`adapter.py` `connect`, `drop_pending_updates=not is_reconnect`) | Every start keeps them | Every start of `ulei-tg` is a supervisor restart after an exit (ADR-0017 section 5), which is the reference's reconnect case: owner messages sent during a restart or deploy must not be lost. |
| `disconnect` marks the adapter disconnected first, so pending text, photo and coalescing batches are dropped | Pending ingress work is flushed to the dispatch seam before the offset is confirmed | The offset confirmation makes those updates unrecoverable; an accepted update must not be lost. |
| Teardown is bounded by the gateway's 5 s adapter disconnect timeout (`run.py` `_adapter_disconnect_timeout_secs`) and cancels the long poll | The in-flight long poll ends on its own | A blocking poll is not cancelled (ADR-0017 section 5); the bound is the poll timeout plus the read timeout. |
| A failed cold-start connect leaves the gateway running in a degraded mode and retries in the background (`run.py` adapter start) | `getMe` or the first `deleteWebhook` failing exits 1 | The supervisor restarts the process; no background retry task. |
| A failed download is answered with `reply_text`: plain text, the notification on, no retry (`adapter.py` `_surface_media_cache_failure`) | The notice is a final reply through `Outbox::deliver`: `MarkdownV2` with the plain fallback, retries | One delivery path for every text the bot sends, so the notice is paced, retried and indexed like a reply. |
| The adapter keeps no texts of the messages it sent (`adapter.py` `send` returns only their ids) | `tg::SentTexts` keeps the last 256 bot messages with their first 300 characters, in memory; owner messages are not recorded | A reaction to a bot reply shows its text to the agent (ADR-0018); 300 characters is what a reaction keeps, and the bound holds the daemon's RSS budget (ADR-0001). |

## Consequences

- The C6 client submits turns and commands and delivers their replies through an `Outbox`,
  calling `turn_started` and `turn_ended` around each turn; the STT client plugs in as the
  ingress's `Transcriber`.
- `ci/budgets.sh` gets the `ulei-tg` row of ADR-0017 section 4 with its idle driver.
