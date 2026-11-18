# ADR-0022: Engine turn origin and the turn-reminder policy

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

The owner's profile plugin `turn-reminder` returns a fixed text from its `pre_llm_call` hook
on the turns of the main platform in a session without a parent. The reference core
(`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3) appends the contexts of that hook to the
API copy of the current user message (`agent/turn_context.py` `build_turn_context`,
`compose_user_api_content`); the stored message and the system prompt stay as they are. The
engine ports this behaviour. Policies that act only on a top-level owner turn of the main
channel need to know where a turn's input comes from, before the C6 server exists.

## Decision

### 1. Turn origin

- `ulei-engine run` takes `--channel NAME`, the channel the prompt came on, and `--event`, which
  marks the prompt as an event (a schedule, a system notice, another agent) instead of the
  owner's message. Without `--event` the prompt is the owner's; without `--channel` it has no
  channel.
- The setting `engine.main_channel` (`ULEI_ENGINE_MAIN_CHANNEL`) names the main channel. Unset,
  no turn qualifies.
- A turn is a top-level owner turn (`policy::Origin::TopLevelOwner`) when the owner's prompt
  comes on the main channel in a session without a parent. Sessions have no parent until
  session lineage exists, so every current session is top-level; the parent check joins
  `Origin::of` with lineage. An event or a sub-session never qualifies.
- The loop takes the origin as `Agent::origin` and hands it to the policy chain. The C6 server
  maps its turn request onto the same `Sender` and channel inputs of `Origin::of`.

### 2. turn-reminder

- `engine.turn_reminder_file` (`ULEI_ENGINE_TURN_REMINDER_FILE`) names a UTF-8 file. Set, the
  policy is on and the file is read once when the turn is built, before the journal opens; its
  bytes are the text for the life of the process, as the plugin's text is a constant of its
  loaded module. A missing or unreadable file fails the start with an error naming the setting.
  Unset, the policy is off and no file is read. An empty file is off, as an empty text is in the
  plugin. The engine carries no text of its own.
- `Policy::turn_context` is the `pre_llm_call` context. The chain is consulted only when the
  turn's input is a user message of one text block (the reference's string content): it asks
  every policy once before the first step, skips empty contexts and joins the others in chain
  order with `\n\n`, as `build_turn_context` joins the hook results. `TurnReminder` gives its
  text on a `TopLevelOwner` turn and nothing otherwise. An input of another shape is sent as it
  is and the chain is not asked.
- The joined context is appended in place to the text of that block as
  `compose_user_api_content` appends it: `content + "\n\n" + context`. Every request of the turn,
  the summary request and the requests after an `on_stop` continue included, sends that copy;
  the journal row of the input and the stored system prompt are the bytes of a run with the
  policy off.

### 3. Differences from the reference

| Behaviour | Upstream | Why |
|---|---|---|
| The composed copy is persisted as the `api_content` sidecar of the user row and replayed on later turns | `agent/turn_context.py` `compose_user_api_content`, `substitute_api_content`; `agent/conversation_loop.py` | The journal does not change with the policy on. Earlier owner messages are replayed as stored, so the last owner message of turn N is sent with the reminder in turn N and without it from turn N+1 on. The provider prompt-cache prefix therefore ends before that message on each later turn: the system prompt, the tools and the history before it are reused, the rest is written to the cache again. |
| Memory-provider prefetch and the gateway's per-turn notes ride the same append | `agent/turn_context.py` `compose_user_api_content`, `consume_gateway_turn_context_notes` | The engine has no external memory provider and no gateway notes; the policies' contexts are the only source. |
| An oversized hook context is spilled to a file | `agent/turn_context.py` `build_turn_context`, `tools/hook_output_spill.py` | The only context is the owner's configured text; a spill would hide it from the model. |
| The scope is a platform constant of the plugin and the agent's parent session id; the plugin is switched off by disabling it or by an empty constant | profile plugin `turn-reminder` `on_pre_llm_call`, `register` | The scope is the origin rule of section 1; the switch is the setting, and the text comes from the configured file. |
| A failing hook is logged and skipped | `agent/turn_context.py` `build_turn_context` | The text is read at start, so a missing or unreadable file is a start error naming the setting, never a silent off; the engine has no log. |

## Consequences

- The no-ask-guard policy scopes on the same `Origin` and needs no further run input.
- The C6 server builds `Origin::of` from its own fields; the CLI flags stay for the one-turn run.
- With the policy on, each turn pays the cache write of the previous turn's last owner message
  and what follows it.
