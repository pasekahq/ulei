# ADR-0097: `ulei-tg notify`, a direct technical notice to the owner, and where the uncertain alerter runs

Status: accepted
Date: 2026-11-27
Affects: crates/channels
Amends: ADR-0032 section 1 (the usage of `ulei-tg`)

## Context

Some notices must reach the owner when the path through the engine and the owner inbox is the
thing that failed: an inbox event that ended `uncertain` (ADR-0037), a reminder whose turn
failed, a stalled hive, a tool call over its budget. A notice sent as an inbox event or an
engine turn can itself end `uncertain`. `ulei-channels` already holds the Bot API client, the
sender's retry ladders, the splitter at Telegram's limit and the settings of the bot.

## Decision

### 1. The path

- `ulei_channels::notify::notify(sender, owner_ids, text)` sends a notice and returns its
  `Receipt` or a `NotifyError`. It makes Bot API `sendMessage` calls and nothing else: no
  `getMe`, no `getUpdates`, no C6 call to the engine, no C1 call to the inbox. Code inside
  `ulei-channels` calls it in process; any other module or process uses the CLI.
- `ulei-tg notify [--config PATH] [--file PATH]` is a one-shot wrapper of it. It finds the
  config file as the daemon does (ADR-0040 section 2) and reads only the keys of the Bot API
  client (`telegram.token_file`, `telegram.base_url`, `telegram.file_base_url` and the
  `telegram.*_timeout` keys) and `telegram.owner_ids` (`tg::Transport`); it needs no
  `engine.c6_socket` and creates no directory. Its `sendMessage` does not disturb a running
  daemon's `getUpdates`. The daemon's start, `ulei-tg [--config PATH]`, is unchanged.

### 2. Input and form

- The text comes from the file `--file` names, else from stdin; never from argv, which other
  local users can read in the process list. It is UTF-8 of at most 32 KiB
  (`notify::MAX_TEXT_BYTES`, at most nine messages); surrounding whitespace is dropped.
- It is sent as plain text, with no `parse_mode`: Telegram cannot reject its markup and shows
  it as written. A text over 4,096 UTF-16 units is split by `render::split_message`, with its
  ` (i/n)` indicators. The messages ring (no `disable_notification`).
- Each message goes through `Sender::send_message` (its 3 attempts) inside
  `Sender::send_with_retry` (2 more deliveries, 2 s and 4 s apart plus jitter, or the server's
  `retry_after`); a response timeout and a too-long message are not retried. The daemon's
  "delivery failed" text is not sent to the chat: the caller gets the error.

### 3. Owner chats

Every id of `telegram.owner_ids`, in config order, each as its direct chat (a private chat's id
is the user's id, as for the daemon's inbox turns, ADR-0036). Each chat gets every part before
the next chat gets any. The first message that still fails after the retries ends the notice;
the chats after it get nothing.

### 4. Receipt and exit codes

- `0`: every message was sent. Stdout is one line of JSON,
  `{"chats":[{"chat_id":<id>,"message_ids":[<id>,...]},...]}`, the chats in the order of
  `owner_ids` and the ids in part order. Stderr is empty.
- `1`: a message failed after the retries, or the receipt could not be written. Messages may
  have gone out; a `Timeout` failure may itself have arrived. No receipt; stderr is one line,
  `ulei-tg notify: sent <k> of <n> messages, then telegram <kind>: <description>`.
- `2`: a usage error.
- `3`: refused before any send: the config is not found or invalid, the token file or the owner
  ids are missing or invalid, the input is unreadable, not UTF-8, empty or over 32 KiB. Stderr
  is one line naming the key or the reason.
- No line carries the token (Bot API errors are redacted, `bot_api::redact`) or the text.

### 5. The `uncertain` alerter

The alert on `uncertain > 0` is built separately. It runs as a one-shot subcommand of `ulei-tg`
started by a user timer of its own, not as a thread of the daemon (which would wake an idle
daemon) and not inside `ulei-inbox` (which would have to reach Telegram from another module).
It learns the uncertain rows through a new read-only C1 request that lists the events with
status `uncertain` (id, source, kind, time and reason, no payload), oldest first and bounded,
added to C1 as an additive change by that work, and sends its notice in process through
`notify::notify`. It talks to the inbox over C1 only and never to the engine.

## Consequences

- One reusable path to the owner that needs only the Bot API and the config file: the uncertain
  alerter, the direct fallback of reminders, the hive stall watchdog (over the CLI) and the
  offload of the per-call tool budget build on it.
- A caller that sees exit `1` decides whether to resend; a resend may duplicate the messages
  that went out before the failure.
- With more than one owner id, a failure on one chat leaves the later chats without the notice.
