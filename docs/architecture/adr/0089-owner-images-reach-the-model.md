# ADR-0089: The owner's images reach the model as image blocks

Status: accepted
Date: 2026-11-26
Affects: contract:C6, crates/channels, crates/engine

## Context

`ulei-tg` saves every photo and image document the owner sends and, until now, put a note in
the prompt instead of the image: `[The user sent an image. It is saved at: <path>. You cannot
view images yet, so do not describe its content.]`. The engine had no way to take an image on
`submit_turn`, so a vision-capable model never saw the picture. The reference agent attaches
user images natively when the main model takes them (`agent/image_routing.py`,
`build_native_content_parts`): the user's text, or `What do you see in this image?` when there
is none, then one `[Image attached at: <path>]` line per image, then the images as inline
base64 parts at their native size. C4G already carries `image` blocks with `base64`, and qb
forwards them to the vendor.

## Decision

1. C6 0.10.0 (additive): `SubmitTurnRequest` gains optional `images`, 1 to 10 absolute paths of
   local image files in arrival order.
2. `ulei-tg` builds the prompt as the reference does: the prepared text (or `What do you see in
   this image?` when empty), then one `[Image attached at: <path>]` per image, so tools that take
   a path still reach the file; it sends the paths in `images`.
3. The engine reads each file (at most 5 MiB raw, so its base64 stays within the vendors' per
   image limit), names its media type by signature (PNG, JPEG, GIF, WebP), and appends an
   `image` block with `base64` after the turn's input in the copy each request of the turn
   sends, like the turn context. The journal keeps the prompt without the blocks, so history
   does not grow by the image bytes and later turns are not billed for them again. An image it
   cannot attach becomes a text line saying so in that copy.
4. A pre-flight token count takes an image block at a flat 1500 tokens instead of the length of
   its base64.

## Consequences

- `SubmitTurnRequest` is closed: `ulei-tg` and `ulei-engine` deploy together for C6 0.10.0.
- A model without vision gets the image block too; qb's vendor routes for the owner's assistant
  are vision-capable. A per-model text fallback, as the reference's `image_input_mode`, can be
  added later without a contract change.
- Later turns see the `[Image attached at: …]` pointer, not the pixels.
