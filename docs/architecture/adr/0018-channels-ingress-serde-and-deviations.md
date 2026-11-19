# ADR-0018: Telegram ingress of `ulei-channels`: serde dependencies and deviations from the reference

Status: accepted
Date: 2026-11-17
Affects: crates/channels, dep:serde, dep:serde_json

## Context

The owner's Telegram channel (`crates/channels`, roadmap U3) needs an inbound half: Bot API
updates in, turns for the engine out. AGENTS.md ("Reference implementation") makes the
reference agent (`ref-agent` in `THIRD_PARTY_NOTICES.md`) the source of its behaviour: the
Telegram adapter of the reference, the turn-coalescing, forward-label, rich-message and
custom-emoji patches of the owner's deployment, the gateway's preparation of the user message
and the profile's reaction plugin. The reference is asynchronous Python; ulei is synchronous
Rust with zero idle wake-ups. Any difference that is not forced by that shape needs a record.

## Decision

### 1. Dependencies

| Crate | Features | Why |
|---|---|---|
| `serde` (workspace) | `std`, `derive` | Typed model of the Bot API `Update` and `Message` with only the fields the ingress reads; unknown fields ignored. |
| `serde_json` (workspace) | `std`, `float_roundtrip` | Parses `getUpdates` results (bytes or `Value`) and keeps rich messages and unrecognised message fields as values. |

Both are already in the workspace (ADR-0001, ADR-0007); no new third-party code enters the
lockfile. Time zones are read from the system zone database (`TZif` with its POSIX footer) by
a small std-only reader instead of a time crate.

### 2. Module `ingress`

`Ingress` is a state machine over caller-supplied instants: `accept(update, now)`,
`poll(now)` and `next_deadline()`; the daemon sleeps until the deadline, so an idle channel
has no timer. The reference's per-batch sleeping tasks become deadlines of the same lengths
(text batch 0.18/0.24/0.3 s and 1 s after a split-length chunk, photo burst and album 0.8 s,
turn coalescing 2 s quiet capped at 7 s). Downloads, STT and the engine are seams
(`FileSource`, `Transcriber`, `MessageTexts`, `Engine`) for the transport, the STT client,
the renderer/journal and the C6 client.

### 3. Deviations from the reference

| Reference behaviour (module) | Here | Why |
|---|---|---|
| Commands are parsed after `@bot` mentions are stripped from the text (telegram adapter `_handle_command`, `_clean_bot_trigger_text`) | Owner commands are parsed from the raw text; a slash command for the engine first loses the `@bot` suffix of its command word, as `get_command` reads the name, then mentions are cleaned from the rest | Stripping turns `/model@bot fast` into `/modelfast` and `/skill@bot arg` into `/skillarg`; a known failure. |
| Reaction wake-up waits up to 5 min while the agent is busy (profile plugin `reactions-log`, `_wake`) | The wake-up turn is submitted at once; the engine's turn queue (C6) orders it | No busy polling; the queue already serialises turns. |
| Reactions are appended to a JSONL file with a cursor; 🗿 starts a capture script (`reactions-log`) | Unseen reactions are kept in memory until the next turn | The file and the script are deployment tooling, not channel behaviour; a restart loses at most one unseen batch. |
| Wake-up text and context header name the owner and deployment files (`reactions-log` `WAKE_TEXT`) | `ReactionConfig` from config, with neutral defaults | Hard rule 7: no personal names or deployment specifics in code. |
| The reaction time stamp is the processing time (`reactions-log` `_now`) | The reaction's own `date` in the configured zone | Deterministic, and the same minute in practice. |
| Static stickers are described by a vision call with a cache (`_handle_sticker`) | The reference's fallback text for a failed description | Vision goes through qb (ADR-0009) and is not in the engine yet. |
| Images are described or attached by the gateway (`_prepare_inbound_message_text`, image routing) | Each image is a note with its saved path in the turn's text, no description (ADR-0033 section 8) | No image input through the gateway yet (issue:8329caafa3601f025ea9). |
| Failed STT falls back to a local whisper (`transcribe_audio_local_fallback`) | No local fallback; the reference's failure note | Not used by the owner's profile. |
| Voice duration comes from probing the file (`_probe_audio_duration`) | Telegram's `duration` | No audio decoder in the channel; same `M:SS` format. |
| Download failures name the Python exception class | The I/O error kind (`TimedOut`, ...) or `ValueError` for non-image data | No exception classes in Rust; the note keeps its wording. |
| `/new` banner shows provider, endpoint and a random tip; `/new <title>` sets a title (`_handle_reset_command`) | Banner with model alias and context only; arguments ignored | Models are qb aliases (port map R1); titles and tips are not used. |
| `/status` header names the product; `/model` lists providers and a picker | `📊 **Состояние канала**`; `/model` shows the current alias and usage | Hard rule 8; providers are behind qb. |
| Unknown slash commands are answered by the gateway or resolved as skills | They reach the engine as a turn that is never coalesced | Skills live in the engine. |
| Command chunks of 4000+ characters go through the text batcher | Commands are always immediate | The four commands take short arguments. |
| Group chats, forum topics, pairing, allow-all and channel posts | Only private chats with an owner id are processed | Out of scope for the owner's single private chat. |

Everything else follows the reference, including texts the model and the owner see.

## Consequences

- The binary wiring (`ulei-tg`) owns the loop: poll updates, call `accept`, sleep until
  `next_deadline`, call `poll`, hand `Output`s to the engine client and the renderer.
- Reaction context reaches the model only through the turn's `context` field; the C6
  `submit_turn` request must carry it to the API copy of the user message.
- A later STT client, vision step or C6 client changes no ingress code, only the seams.
