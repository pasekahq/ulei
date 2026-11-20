# ADR-0035: speech to text of owner voice notes in `ulei-tg`

Status: accepted
Date: 2026-11-19
Affects: crates/channels
Amended by: ADR-0040 (one env-file reader in `ulei-contracts`).

## Context

The ingress turns an owner voice note into turn text through its `Transcriber` seam and already
ports the reference's notes (`THIRD_PARTY_NOTICES.md#ref-agent`, `gateway/run.py`
`_enrich_message_with_transcription`): the quoted transcript, the note for an empty transcript,
and the file note when no transcriber is set or it fails. The reference transcribes with
`tools/transcription_tools.py` `transcribe_audio`, whose cloud Whisper path `_transcribe_groq`
posts the file to an OpenAI-compatible `/audio/transcriptions` endpoint. The port map lists
speech to text only for Telegram voice notes, as part of the channel and not of the engine.

## Decision

### 1. Place

`ulei_channels::stt::Stt` implements the `Transcriber` and `ulei-tg` plugs it into `Ports.stt`.
It is a channel service, not a model route through qb: qb carries the engine's chat model calls
(port map R1, ADR-0009), while transcription is one file upload that only the Telegram channel
makes, before the turn exists, as the reference's gateway makes it outside its agent loop.

### 2. Endpoint and request

One `POST <base_url>/audio/transcriptions` per voice note, `multipart/form-data` with the fields
of `_transcribe_groq`: `model`, `response_format` `text`, then `file` named as the saved file and
typed by its extension as the reference's HTTP client guesses it (`.ogg` is `audio/ogg`). The key
goes in `Authorization: Bearer`. The answer body, stripped, is the transcript. A model that only
the OpenAI endpoint serves (`OPENAI_MODELS`) is replaced by the default, as `_transcribe_groq`
does. No retry (`max_retries=0`); a redirect is not followed and counts as an error status, so
the key and the upload reach only the configured endpoint.

| Key | Default | Meaning |
|---|---|---|
| `telegram.stt_env_file` | none | Env-file holding `GROQ_API_KEY`. Unset: no transcriber, voice notes reach the agent as file and note. |
| `telegram.stt_base_url` | `https://api.groq.com/openai/v1` | `http://` or `https://` base of the endpoint (`GROQ_BASE_URL`). |
| `telegram.stt_model` | `whisper-large-v3-turbo` | Model sent with each file (`DEFAULT_GROQ_STT_MODEL`). |

### 3. Key

Each call reads `GROQ_API_KEY` from the env-file, its last assignment winning, as the reference
resolves the key at call time; a rotated key needs no restart. The env-file is read by
`ulei_contracts::env_file`, the one reader of the env-files `ulei.toml` names (ADR-0040). The
key is in no setting, `Debug` output, log line or error: an error body that echoes it shows `***`
in its place (`bot_api::redact`).

### 4. Limits

- Size: a file over 25 MiB (`MAX_FILE_SIZE`) fails before the key is read and nothing is sent.
- Time: 30 s bound each resolve, connect, read and write of the call, as the reference client's
  `timeout=30` bounds each phase.
- Formats: the reference's `SUPPORTED_FORMATS`, matched on the lower-cased extension.

### 5. Errors and fallback

Every failure is an error with the reference's text; the ingress logs
`ulei-tg: voice transcription failed for <path>: <error>` to stderr and puts the reference's file
note in the turn, keeping the cause out of the prompt as the reference does.

| Case | Text |
|---|---|
| Symbolic link | `Path is a symbolic link: <path>` |
| Missing file | `Audio file not found: <path>` |
| Not a regular file | `Path is not a file: <path>` |
| Unreadable metadata | `Failed to access file: <reason>` |
| Unsupported extension | `Unsupported format: <ext>. Supported: .aac, .caf, ...` |
| Over 25 MiB | `File too large: <size>MB (max 25MB)` |
| `.caf` | `CAF audio could not be converted to WAV.` |
| Unreadable env-file | `cannot read the env-file <path>: <reason>` |
| Key missing or blank | `GROQ_API_KEY not set` |
| File cannot be opened | `Permission denied: <path>`, else `Transcription failed: <reason>` |
| Timeout | `Connection error: Request timed out.` |
| Any other transport failure | `Connection error: Connection error.` |
| Error status | `API error: Error code: <status> - <body as Python repr, keys in server order>`, the body text when it is not JSON, `Error code: <status>` when it is empty |

An empty or blank transcript is a success: the ingress gives the reference's empty-note text,
not the file note, as `_enrich_message_with_transcription` does.

### 6. Thread

Transcription runs inside `Ingress::accept` on the loop thread, after the download, under
ADR-0033 section 6: the poller and engine threads never wait for it beyond the one batch the
poller hands over, Telegram keeps later updates queued, and the turn text needs the transcript
before the chat's next update. The 30 s phase bounds of section 4 cap how long one voice note
holds the loop.

### 7. Differences from the reference

| Reference (module) | Here | Why |
|---|---|---|
| Secret-store read guard before any check (`agent/file_safety.py` `get_read_block_error`) | Absent | The transcriber only gets files the ingress saved in the attachments dir. |
| `.silk` conversion (`_prepare_audio_for_transcription`) | Absent; `.silk` is an unsupported format | A WeChat format; other platforms are not ported (port map section 4). |
| CAF to WAV with ffmpeg (`_convert_caf_to_wav`) | A `.caf` file fails with the reference's conversion error | No subprocess or ffmpeg in the channel; Telegram voice notes are Ogg. The text is what the reference gives without ffmpeg. |
| Silence trim before a cloud upload (`_trim_silence_for_cloud_stt`, on by default) | The original file is uploaded | Needs ffmpeg and ffprobe; the reference uploads the original when they are missing. |
| `stt.enabled`, provider choice and auto-detection (`_get_provider`), other providers, the local retry (`transcribe_audio_local_fallback`) | One provider; the env-file setting is the switch | Other providers are out of the port map. |
| Language hint, `stt.prompt`, `pre_transcription` hook (`_dispatch_stt_provider`) | Never sent | The request is the one `_transcribe_groq` sends with none of them set. |
| Key from config, environment or credential pool (`resolve_provider_secret`) | Env-file only | Secrets stay out of `ulei.toml` (AGENTS.md hard rule 7). |
| Redirects followed by the HTTP client | Not followed; the 3xx is an error status | Section 2. |
| Each non-empty transcript echoed as `🎙️ "<transcript>"` through a plain `send` (silent, no reply target, a failure only logged) before the agent's turn, switched by `stt_echo_transcripts` (default on); a note queued behind a busy session is echoed when it drains (`_transcribe_and_echo_pending_voice`) | The same echo, one per transcript in order, switched by `telegram.stt_echo_transcripts` (default `true`; a value that is not a boolean is a startup error naming the key); the engine thread sends it and waits for it before the turn's `submit_turn`, also for a turn the engine then queues | The channel transcribes each note when it arrives (section 6) and hands every turn to the engine, which owns the queue (ADR-0033). |
| Success log line (`Transcribed ... via Groq API`) | None | `ulei-tg` logs failures only. |

## Consequences

- Owner voice notes reach the engine as their transcript when `telegram.stt_env_file` is set.
- The upload adds no dependency: ureq and rustls (ADR-0015, ADR-0017) and the multipart form of
  the Bot API client.
