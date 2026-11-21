# ADR-0052: outgoing media of `ulei-tg`: the `MEDIA:` files of a final reply

Status: accepted
Date: 2026-11-21
Affects: crates/channels

## Context

The agent hands the owner a file by writing `MEDIA:<path>` into its final reply. In the
reference (`THIRD_PARTY_NOTICES.md#ref-agent`) the gateway takes such tags out of the reply
text and sends each file as its own Telegram message: `gateway/platforms/base.py`
(`extract_media` with `[[audio_as_voice]]` and `[[as_document]]`,
`filter_media_delivery_paths` with `validate_media_delivery_path`,
`should_send_media_as_audio`, `_notify_media_delivery_failure`, the routing in the reply
delivery of `_process_message_background`) and `plugins/platforms/telegram/adapter.py`
(`send_image_file`, `send_multiple_images`, `send_voice`, `send_video`, `send_document`).
The same delivery code runs for the owner-inbox turns of the reference's owner inbox patch,
whose `delivery_acked` follows the text send.

`ulei-tg` renders replies in `crates/channels/src/render` (ADR-0019) and sends them on the
`outbound` thread (ADR-0032, ADR-0034, ADR-0036); its transport has uploads since ADR-0017.

## Decision

### 1. Which replies

A reply delivered with `notify` (the final answer of a followed turn, ADR-0034) and the
answer of an inbox turn (ADR-0036 section 6) are split by `render::extract_media`; interim
texts and echoes are sent as they are. The text without the tags goes first, through the
same plan, retry and fallbacks as before; the files follow it, also when the text is empty
or failed.

### 2. The tags

`render::extract_media` ports `extract_media` with the patterns it uses, matched by hand
(no regex crate) with the same semantics, offsets in characters:

- A tag is `MEDIA:` in any case, optional spaces, then a path in backticks or quotes, or a
  bare path starting with `~/`, `/` or a drive letter that may hold single-line spaces and
  ends in one of the reference's 58 deliverable extensions (`MEDIA_DELIVERY_EXTS`), followed
  by a space, a quote or emphasis mark, `,;:)]}[`, full-width punctuation, the next `MEDIA:`,
  a sentence-final period or the end. Up to three quote or emphasis marks around the tag and
  a final period belong to it.
- A tag whose path has no deliverable extension counts only when the path passes the checks
  of section 3; a failing path is extended across single spaces, up to 8 tokens, before it is
  left in the text.
- Tags inside fenced code, inline code (unless the whole tag is in inline code and its path
  passes the checks), blockquote lines and JSON string values holding a bare path are text.
- `[[audio_as_voice]]` marks the audio files of the reply as voice; `[[as_document]]` sends
  its images as documents; both are removed from the text. A file named twice goes once.
  `~` is `$HOME` of the process.
- When a file was found, the tags are cut out, runs of three or more line feeds become two
  and the text is trimmed.

### 3. Checks before a send

Ported from `validate_media_delivery_path` in its default (non-strict) mode, then the two
checks the reference leaves to Telegram:

1. The path after `~` expansion is absolute, resolves to a regular file, and lies outside
   `/etc`, `/proc`, `/sys`, `/dev`, `/root`, `/boot`, `/var/log`, `/var/lib`, `/var/run` and
   `$HOME/{.ssh,.aws,.gnupg,.kube,.docker,.config,.azure,.gcloud,Library/Keychains}`
   (`$HOME` itself is never denied as a whole).
2. The file opens for reading.
3. Its size is at most 50,000,000 bytes; an image routed as a photo over 10,000,000 bytes goes
   as a document.

Bot API facts, checked on 2026-11-21 against the Bot API page (Bot API 10.4):

| Fact | Source text |
|---|---|
| Multipart upload limits | "10 MB max size for photos, 50 MB for other files" (Sending files) |
| `sendPhoto` | "The photo must be at most 10 MB in size. The photo's width and height must not exceed 10000 in total. Width and height ratio must be at most 20." |
| `sendVoice` | "your audio must be in an .OGG file encoded with OPUS, or in .MP3 format, or in .M4A format (other formats may be sent as Audio or Document). Bots can currently send voice messages of up to 50 MB in size" |
| `sendAudio` | ".MP3 or .M4A format"; up to 50 MB |
| `sendVideo`, `sendDocument` | up to 50 MB |
| `sendMediaGroup` | `media` "must include 2-10 items" |

Sources: <https://core.telegram.org/bots/api> and <https://core.telegram.org/bots/faq>
("files of any type of up to 50 MB"). Telegram does not say whether MB is 10^6 or 2^20 bytes;
the decimal reading holds under both. The reference checks no photo dimensions; a photo
Telegram refuses for them goes as a document (section 5). The larger limits of a self-hosted
Bot API server are not used.

### 4. Routing and order

| File | Method |
|---|---|
| `.jpg`, `.jpeg`, `.png`, `.webp`, `.gif`, without `[[as_document]]` | Photo: albums of up to ten by `sendMediaGroup`; a single photo by `sendPhoto` |
| `.ogg`, `.opus` with `[[audio_as_voice]]` | `sendVoice` |
| `.mp3`, `.m4a` | `sendAudio` |
| `.mp4`, `.mov`, `.avi`, `.mkv`, `.webm`, `.3gp` | `sendVideo` |
| Anything else, `.ogg`/`.opus` without the directive included | `sendDocument` |

Order: the text, the photo albums, then photos over the photo limit (as documents) and the
notices of refused photos, then every other file in tag order. Uploads carry the chat, no
reply target, no caption, and ring.

### 5. Failures

- Each upload is one call without a retry. A photo Telegram refuses goes again as a document;
  an album Telegram refuses goes again photo by photo, each with that fallback.
- A refused file (section 3) or an upload that still failed gives the owner one ringing
  message, sent once without the delivery retry, worded as `_notify_media_delivery_failure`:
  `⚠️ Couldn't deliver the audio attachment.` for audio routes and voice,
  `⚠️ Couldn't deliver the video attachment.` for videos, otherwise
  `⚠️ Couldn't deliver the file attachment (<file name>).`
- A refusal or failure writes one stderr line with the path (control and line-separator
  characters as `?`, at most 200 characters) and the reason or Bot API error; file contents
  are never logged.

### 6. No file twice

- The files of a reply are sent after its text and outside the text's retry
  (`Sender::send_with_retry`), so a retried or resent text never resends a file.
- A response timeout on an upload ends that file without a fallback: it may have arrived.
- Inbox answers keep the ADR-0036/0037 guarantee: the engine hands a turn over once
  (`inbox_turns`), a redelivered event is answered with its recorded turn and starts nothing,
  and the files are extra parts of that one answer. `Delivery::Sent` needs a message id for
  every part, each text message and each file; a refused or failed file makes it
  `send_failed`. No second record is kept.

### 7. Deviations from the reference

| Behaviour | Reference | Why |
|---|---|---|
| A refused path gives the owner the failure line | `filter_media_delivery_paths` drops it with a log line only | The owner learns that a promised file did not come. |
| Readability and size are checked before the upload; a photo over 10,000,000 bytes goes straight to `sendDocument`; a file over 50,000,000 bytes is refused | No check; Telegram's refusal leads to the fallbacks and the notice | One short line instead of an upload Telegram refuses. |
| A single photo goes by `sendPhoto` | A media group of one, which Telegram refuses (2 to 10 items), then the per-photo fallback | Same result without the doomed call. |
| A response timeout ends the file without the document or per-photo fallback | Any failure falls back | An upload that may have arrived is not sent again. |
| Every failure line of a non-audio, non-video file names the file | A document upload that raises sends the line without the name | One wording per file kind. |
| An inbox answer is `sent` only when its files went out too, and a files-only answer can be `sent` | `delivery_acked` follows the text send alone | Files are parts of the answer (section 6). |
| Not ported: bare local paths without `MEDIA:` (`extract_local_files`), Markdown image URLs (`extract_images`), the container path translation, strict mode with its allow-list roots and recency window, the reference's own credential files under its home, `~user`, `human_delay`, the voice duration probe, topic and reply-anchor metadata | `gateway/platforms/base.py`, `plugins/platforms/telegram/adapter.py` | Outside this tag path or without a counterpart: `ulei-tg` has no containers, profiles, topics or pacing delay; ulei's own config lives under the denied `~/.config` by default. The duration probe needs an audio container parser the crate does not have; Telegram then shows no duration on a long voice clip. |

## Consequences

- `bot_api` gains `sendAudio`, `sendVideo` and `sendMediaGroup`; its multipart form streams
  any number of files.
- `ulei-tg` starts no thread or timer for this; the idle budget of ADR-0034 is unchanged.
