# Render golden fixtures

Inputs and expected outputs of the reply renderer (`ulei_channels::render`). The expected
outputs were produced by running the reference agent's own functions (`ref-agent` in
`THIRD_PARTY_NOTICES.md`: the Telegram adapter's `format_message`, `_strip_mdv2`,
`_separate_chunk_indicator_from_fence` and the chunk post-processing of `send`, the base
adapter's `truncate_message` and `utf16_len`, and the shared table helpers). Their source was
extracted into an isolated interpreter (`python3 -I`, temporary `HOME`); no adapter, gateway
or network was started. All texts are synthetic.

Record format: a line starting with `-----8<----- ` opens a record and names it; the body is
everything up to the next such line, minus its final newline.

| File | Content |
|---|---|
| `NAME.md` | one agent reply (Markdown) |
| `NAME.out` | for each chunk of its send plan: `chunk i/n markdownv2` (text sent with `MarkdownV2`), then `chunk i/n plain` (the plain-text fallback of that chunk); empty when nothing is sent |
| `cases.txt` | short inputs: `case`, then its `markdownv2` conversion, then `plain` (fallback of that conversion) |
| `splits.txt` | `split N` with an input, followed by the `chunk` records of splitting it at N UTF-16 units |

Covered: long Russian text, code blocks longer than a chunk, nested lists, links with
parentheses, tables (also inside code), emoji and surrogate pairs at the 4096 boundary, every
`MarkdownV2` special character, custom emoji, unclosed fences, headers spanning blank lines,
empty and blank replies.
