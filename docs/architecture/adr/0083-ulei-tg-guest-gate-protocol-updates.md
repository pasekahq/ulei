# ADR-0083: `ulei-tg` hands the owner's guest-gate traffic to the gate first

Status: accepted
Date: 2026-11-26
Affects: crates/channels, contract:C9, docs/runbooks/mvp-pilot-cutover.md

## Context

The guest gate is a separate process on the deployment host that lets other people write to the
owner's assistant: it previews each guest request to the owner in Telegram, with inline buttons
(approve `ap:`, block `bi:`, postpone `po:`, request `rq:`), and takes the owner's consent from
the button presses, from the command `/requests`, from `отменить <id>` and `ответить <id>`, from
the words `шли`, `отправь`, `отправить` and `не надо`, and from replies to its previews. Guests
reach it by a lane of its own; only the owner's traffic passes the assistant's bot.

On the old runtime a local plugin of the reference agent, `guest-gate-ingress`, sat in front of
the agent's Telegram handlers and posted those owner updates to the gate's
`http://127.0.0.1:8792/internal/telegram` with a bearer token; when the gate answered
`{"handled": true}` the update stopped there (the gate answers the owner itself, including
`answerCallbackQuery`), otherwise it went on to the agent. `ulei-tg` has no such step:
`Ingress::accept` handles messages and reactions only, a `callback_query` is counted as skipped,
and `/requests`, the consent words and replies to previews go to the model. After the cutover
the owner could not answer a guest at all.

## Decision

### 1. The rules, ported unchanged

`crates/channels/src/ingress/guest.rs` classifies the raw `getUpdates` object, before the
ingress parses it and before any coalescing, exactly as the plugin's `intercept`:

- only the owner (`telegram.owner_ids`), by `callback_query.from` or `message.from`;
- a `callback_query` whose `data` starts with `ap:`, `bi:`, `po:` or `rq:` (a button);
- a `message` whose `text` before the first `@` is `/requests`, or whose trimmed text matches
  `^(?:отменить|ответить)\s+[0-9a-f]{24}(?:\s|$)` case-insensitively, or is, lower-cased, one of
  `шли`, `отправь`, `отправить`, `не надо` (a command);
- any other `message` with `reply_to_message` (a reply: the gate decides by its preview's id
  whether it is the answer to one).

`edited_message` is never forwarded, as in the plugin. Whitespace is Python's (`str.isspace`,
`re`'s `\s`), case folding Unicode lower case, which equals `casefold` on these Cyrillic words.

The payload is the plugin's `payload_for`: `update_id` and either `message` or `callback_query`
(`id`, `from`, `data` or `""`, `message`), the message reduced to `message_id`, `from`, `chat`,
`text`, `caption`, `document`, `voice` and `audio` where present, and of `reply_to_message` only
its `message_id`. No media is downloaded, nothing of the update is stored.

The routing is the plugin's: `handled: true` drops the update; a failed call (no connection, a
status outside 2xx, an answer that is not a JSON object, the timeout) drops a button or a
command, so it never reaches the model, and lets a reply go on; `handled` anything but `true`
lets it go on. A button that goes on reaches the ingress as the new `UpdateKind::Callback` and is
counted in `skipped_updates`, as every button was before.

One check is added: the update must come from a private chat (`chat.type` `private`, for a
button the chat of its message), the rule the rest of the ingress already applies to the owner.
A button without its message (an inline message) is not forwarded, as `payload_for` returns
nothing for it.

### 2. The call does not stop the channel

The plugin awaited the gate inside the agent's sequential update handler, so a slow gate held up
everything for up to 145 s. In `ulei-tg` the classified updates go to one worker thread
(`tg::guest_gate::GateWorker`) in arrival order; it posts them one at a time with ureq (the
ADR-0015 stack, no new dependency): `Authorization: Bearer <token>`, JSON body, 145 s for the
whole call, no proxy from the environment (the plugin's `trust_env=False`), no redirect. An
update the gate does not take comes back to the loop thread on the same channel as the poller's
batches and enters `Ingress::accept` then. So polling and the owner's other messages go on
during a gate call; the protocol updates keep their order among themselves; an update that comes
back may reach the engine after owner messages that arrived later (the deliberate difference from
the plugin). A stop does not wait for a call in flight: its update is lost, as it is when the old
runtime restarts mid-call; the gate may still have acted on it.

Each call writes one stderr line: the update id, the category (`button`, `command`, `reply`)
and the outcome (`handled`, `not handled` or `failed (<timeout|unreachable|status N|unreadable
answer>)`, then `dropped` or `usual way`). Never the text, the URL or the token.

### 3. Configuration (C9 0.10.0, additive)

`[telegram]` gains `guest_gate_url` and `guest_gate_token_file`, both or neither; neither keeps
today's behaviour, with no gate call. The URL must be plain `http://` on `127.0.0.1`, `[::1]` or
`localhost`, or `https://`, with no user info in it, so the token never crosses a network in
clear. The token file must be mode 0600 (no group or other bits), is read once at start, trimmed,
must be one word of visible ASCII and is hidden from `Debug`. A refused setting is a start error
naming the key, never the URL or the token. The live values are in the cutover runbook, 2.5.

The user unit `ulei-tg.service` already allows `AF_INET` to loopback; nothing in it changes.

## Consequences

- After the cutover the owner answers guests as before: buttons, `/requests`, the consent words
  and replies to previews reach the gate, and only what the gate does not take reaches the model.
- The plugin's rules are now code in `crates/channels`; a change of the gate's prefixes, words or
  payload is a change here, with its tests (`ingress::guest`, `tg::guest_gate`, `tests/tg.rs`).
- `ulei-tg` holds one more thread while the gate is configured.
