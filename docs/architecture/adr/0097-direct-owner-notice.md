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
  local users can read in the process list. It is UTF-8. Surrounding whitespace is dropped
  and what remains is at most 32 KiB (`notify::MAX_TEXT_BYTES`, at most nine messages); the
  library and the CLI apply this one rule. The CLI reads at most 64 KiB of input and refuses a
  longer one, so an unbounded stdin is never read whole.
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
  ids are missing or invalid, the input is unreadable, not UTF-8 or too long, or the text is
  empty or too long (section 2). Stderr is one line naming the key or the reason.
- No line carries the token (Bot API errors are redacted, `bot_api::redact`) or the text.

### 5. The `uncertain` alerter

The alert on `uncertain > 0` runs as a one-shot subcommand of `ulei-tg` started by a user timer
of its own, not as a thread of the daemon (which would wake an idle daemon) and not inside
`ulei-inbox` (which would have to reach Telegram from another module). It talks to the inbox over
C1 only and never to the engine, and sends its notice in process through `notify::notify`
(`ulei_channels::alert`).

- `ulei-tg uncertain [--config PATH] --state PATH` reads the keys of `tg::Transport` (section 1)
  and the existing key `inbox.socket`; it needs no `engine.c6_socket`. Each run makes one C1
  `uncertain` call with `limit` 100, C1's largest, and at most one notice.
- `ulei-uncertain.timer` starts the oneshot `ulei-uncertain.service` every 10 minutes
  (`OnCalendar=*:0/10`, up to 30 s of random delay). The service has the hardening and the
  `Slice=ulei.slice` of `ulei-tg.service`, writes only its `StateDirectory=ulei/uncertain` and
  passes `--state %S/ulei/uncertain/state.json`; no C9 key names the state. `deploy/install.sh`
  installs both and prints the timer in the enable line of the inbox and the channel.
- The state file is one line of JSON, `{"reported":[<id>,...],"notice_at":<secs>|null,
  "fault_at":<secs>|null}` with Unix seconds: the ids of the uncertain events the owner was told
  of, the last notice of them and the last notice that the inbox could not be read. A missing
  file is the empty state; its directory must exist. It is written atomically: a temporary file
  of mode 0600 beside it, synced, renamed over it, then the directory synced. A run whose state
  is unchanged does not write it.
- The rules of one run at time `now`:
  - The inbox answers no events: no notice; `reported` and `notice_at` are cleared.
  - An event's id is not in `reported`: one notice of all the listed events.
  - No new id and `notice_at` is 24 h or more before `now`: the same notice again.
  - Otherwise no notice. After any answer `reported` holds the listed ids, so an event that left
    `uncertain` is never named again and comes back as new.
  - Any non-200 answer, an unreachable inbox, a timeout, a peer of another uid or an answer that
    is not an `UncertainList` within the limit: one "cannot read the inbox" notice when
    `fault_at` is absent or 24 h or more before `now`, then `fault_at = now`; `reported` and
    `notice_at` stay. One malformed stored row, which makes C1 answer `internal`, therefore
    cannot silence the alert. `fault_at` is not cleared by a good answer, so a flapping inbox
    gives at most one such notice every 24 h.
  - A clock before the last notice makes that notice due again.
  - A failed send leaves the state as it was, so the next run sends the notice again.
- The notice is plain text. The events notice starts `ulei inbox: <count> uncertain events, <k>
  new. They may not have reached the agent.` (`1 uncertain event`; at 100 events `at least 100
  uncertain events, the oldest listed`), then, after an empty line, one line per event, oldest
  first: `<id> | <kind> | from <source> | <reason> | <yyyy-mm-dd HH:MM> UTC`, with ` | new` on a
  new one. The fault notice is `ulei inbox: cannot read the inbox (<reason>); uncertain events,
  if any, are not known. This notice repeats every 24 h while it lasts.`, the reason naming the
  status and C1 error code or the socket failure. Neither carries a payload or the message of a
  C1 error, which may quote a stored row.
- Exit codes, as section 4:
  - `0`: the run did its duty: no notice was due, or the notice went out and the state was
    written. Stdout is the receipt line of a notice sent, else empty. When the inbox could not
    be read, stderr is one line, `ulei-tg uncertain: <reason>`, with or without a notice.
  - `1`: the notice failed after the retries (`sent <k> of <n> messages, then telegram <kind>:
    <description>`), or the state could not be written; a notice may have gone out before.
  - `2`: a usage error.
  - `3`: refused before any C1 call or send: the config is not found or invalid, the token file,
    the owner ids or `inbox.socket` are missing or invalid, or the state file is unreadable or
    malformed or its directory is missing.

## Consequences

- One reusable path to the owner that needs only the Bot API and the config file: the uncertain
  alerter, the direct fallback of reminders, the hive stall watchdog (over the CLI) and the
  offload of the per-call tool budget build on it.
- A caller that sees exit `1` decides whether to resend; a resend may duplicate the messages
  that went out before the failure.
- With more than one owner id, a failure on one chat leaves the later chats without the notice.
