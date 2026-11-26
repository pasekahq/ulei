# ADR-0088: The agent sets emoji reactions in the owner's chat

Status: accepted
Date: 2026-11-26
Affects: contract:C6, crates/channels, crates/engine

## Context

The owner's assistant reacts to the owner's Telegram messages and sees the owner's reactions.
On the reference agent this is three pieces:

- the owner's reactions: the profile plugin `reactions-log` logs each one and shows the new ones
  to the agent on its next turn with the reacted-to text, and a reaction wakes the agent after
  15 s. `ulei-tg` already ports this (ADR-0018, ADR-0032 section 7);
- the agent's reactions: the deployment's helper `tg-react.sh` calls `setMessageReaction` with
  the bot token for the message the turn answers (the session context's message id) or for an
  owner message it finds by text in the session store (`--find`). It refuses a guessed id. The
  reference's own `react_to_message` (`tools/react_to_message_tool.py`) and
  `send_message(action="react")` (`tools/send_message_tool.py`) cover other surfaces;
- the owner's own texts: `reactions-log` reads the owner's messages from the session store, so
  a reaction to the owner's own message shows its text.

`ulei` had none of the last two. The engine's tools run in a sandbox without the bot token or
the old runtime's store (ADR-0069), so a terminal helper cannot call the Bot API. The engine
never got the Telegram id of the owner message a turn answers, so neither the model nor a tool
had any id to react to. `tg::SentTexts` kept only the bot's messages (ADR-0032 section 7), so a
reaction to the owner's own message read "text not found".

## Decision

1. **C6 0.11.0.** `SubmitTurnRequest` gains optional `message_id`, the channel's id of the owner
   message the prompt answers (the one its answer replies to); `ulei-tg` sends it for every owner
   turn and none for a reaction wake-up. `TurnEventType` gains `reaction`: a `TurnEvent` with
   `message_id` and the emoji in `text`, after any `interim` and before `final`.
2. **Engine tool `react_to_message`**, the reference's tool name and `emoji` parameter, with a
   Telegram `message_id` target as `send_message(action="react")` has. Without `message_id` it
   reacts to the turn's `message_id`, as `tg-react.sh` falls back to the session context's
   message id; a turn without one (an inbox event, a wake-up) needs an explicit id and gets the
   helper's error text "No source message id; ...". At most 16 per turn. It is registered for
   `serve` turns only: `ulei-engine run` has no channel to hand a reaction to.
3. **The owner message's id is in the journaled user message.** For a turn with `message_id` the
   engine sends and journals the prompt followed by a blank line and `[message_id: N]`. The
   model sees which id to pass, and a later turn (an inbox event about a task the owner asked
   for) finds the original request and its id with `session_search`, which is what `tg-react.sh
   --find` did against the session store.
4. **The channel sets the reaction.** The engine records each successful call; the turn's
   ending hands the record to `serve`, which appends one `reaction` event per call before
   `final`. `ulei-tg` sets each one with `setMessageReaction` on the chat's own message, for
   owner turns (`follow`) and inbox turns (`inbox`) alike, in order with the chat's deliveries.
   A failed call is one stderr line, like the reference adapter's reaction calls. An
   interrupted turn still hands over the reactions it recorded.
5. **`SentTexts` keeps the owner's messages too.** The ingress records the plain text of every
   owner message in the private chat (an edit records the new text), in the same ring of 256
   messages and 300 characters, so a reaction to the owner's own message shows its text as
   `reactions-log` shows it.

## Deviations from the reference

| Reference | ulei | Why |
|---|---|---|
| `tg-react.sh` calls the Bot API at once and returns Telegram's answer to the agent | The tool returns success when the reaction is recorded; the channel sets it when the turn ends, and a Telegram failure is a stderr line of `ulei-tg` | The engine holds no bot token by design (ADR-0069); C6 events of a turn are the one engine-to-channel path, and a reaction with the answer is what the owner sees either way. |
| `tg-react.sh --find --search TEXT` looks the owner message up in the session store | The id is a line of the journaled prompt, found with `session_search` | The engine's journal is the session store; one line in the prompt costs a few tokens and needs no new tool or index. |
| `reactions-log` reads owner texts from the session store on disk | `SentTexts` keeps them in memory, lost on restart | `ulei-tg` has no store of its own (ADR-0032 section 7); after a restart a reaction to an older owner message shows its id only, as before. |
| `tg-react.sh` accepts only the owner chat | The channel sets the reaction in the chat of the turn's session, which is the owner's private chat | `ulei-tg` serves only the owner's chat; the guest receiver also pins `setMessageReaction` to it. |

## Consequences

`ulei-tg` and `ulei-engine` deploy together for C6 0.11.0: an engine before it refuses
`message_id`, a channel before it refuses the `reaction` event type. The 👍/👎 evaluation of
proactive pushes and the 🗿 quality capture of `reactions-log` stay deployment tooling, as
ADR-0018 recorded; the owner's reactions reach the agent as context and wake it as before.
