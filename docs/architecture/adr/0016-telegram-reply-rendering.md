# ADR-0016: Telegram reply rendering: `MarkdownV2` port and its recorded deviations

Status: accepted
Date: 2026-11-17
Affects: crates/channels

## Context

U3 needs the outbound half of the owner's Telegram channel: the agent's Markdown reply turned
into the `sendMessage` calls Telegram accepts. The product plan says "Markdown -> HTML with
plain fallback, split at 4096". The reference agent (`ref-agent`, `THIRD_PARTY_NOTICES.md`)
already does this, and AGENTS.md makes it the reference: its Telegram adapter converts the
reply to `MarkdownV2`, splits it at 4096 UTF-16 code units and resends a chunk as plain text
when Telegram rejects its markup. Its other outbound path, Bot API 10.1 rich messages, is an
opt-in (`rich_messages`) that the owner's profile leaves off; streaming edits and drafts are
U5 candidates.

`crates/channels` is synchronous and must not wake up when idle (ADR-0001); the reference's
send and typing code are async loops around HTTP calls.

## Decision

1. `crates/channels/src/render/` ports, std only and without I/O:
   - `format_message` with its table rewrite, escaping and placeholder passes ->
     `to_markdown_v2`; `_strip_mdv2` -> `strip_markdown_v2`;
   - `truncate_message` (UTF-16 measure, fence carry, inline-code guard, `(i/n)` indicators),
     `utf16_len`, the indicator escaping and `_separate_chunk_indicator_from_fence` of `send`
     -> `split_message`, `split_markdown_v2`;
   - the per-chunk decisions of `send` (`parse_mode`, `_should_thread_reply`, link previews,
     silent delivery, typing re-trigger after intermediate replies, the plain-text fallback
     predicate, the "message to be replied not found" predicate) -> `plan_reply`,
     `OutgoingText::plain_fallback`, `is_markup_rejection`, `is_reply_target_missing`;
   - `_keep_typing` / `_stop_typing_refresh` with the adapter's typing cooldown ->
     `TypingRefresh`.
2. The wire format is `MarkdownV2`, as in the reference, not HTML as the plan text says. The
   plan predates the port directive; the reference uses HTML on no path the owner's assistant
   runs.
3. Shape differences forced by the synchronous design (behaviour unchanged):
   - `send` becomes a plan (`SendPlan` of `OutgoingText`) that the transport executes; retries,
     flood control and HTTP stay in the transport;
   - the typing loop becomes a schedule over an injected clock: the channel thread sleeps until
     `next_due`, calls `poll`, sends the chat action and reports `sent`; the next tick is one
     interval after the call returned, as in the reference. A call that times out within
     `TYPING_SEND_TIMEOUT` is reported with `abandoned`, which reschedules without a cooldown
     (the reference cancels it and records nothing). The extra chat action after an
     intermediate reply reports its outcome with `record`, which touches only the cooldown.
     The cooldown deadline lives per chat in the reference; the channel carries it from one
     turn's schedule to the next with `cooldown_until` / `with_cooldown_until`.
4. Deviations in edge semantics, each unreachable or harmless on the owner's traffic:
   - only the UTF-16 measure of `truncate_message` is ported; the Telegram adapter passes no
     other `len_fn`;
   - custom emoji fallbacks are unescaped for `&amp; &lt; &gt; &quot; &apos;` and numeric
     references only, not the full HTML5 table: the inbound side escapes exactly those;
   - `\d` in custom emoji ids and chunk indicators matches ASCII digits only (Telegram ids and
     generated indicators are ASCII);
   - NUL in a reply becomes U+FFFD before conversion. NUL delimits the reference's
     placeholder keys, and literal look-alikes in a reply make its restore step expand them
     again and again: a few hundred bytes grow to gigabytes (known failure of the reference,
     fixed here). Output differs from the reference only on replies that contain NUL;
   - the word-character test of the italic rule in `strip_markdown_v2` uses Unicode
     `Alphabetic`/`Numeric` instead of the Unicode categories behind `str.isalnum`; they
     differ only on some combining marks.

## Consequences

- Golden fixtures produced by the reference functions pin the behaviour
  (`crates/channels/tests/fixtures/render/`); a differential run on random inputs found no
  difference.
- HTML output, rich messages, streaming edits and drafts are not ported; adding one later is
  a port from the reference with its own record.
