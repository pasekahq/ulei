# ADR-0054: Engine core tier of the system prompt: safeguards and meta-skills as data

Status: accepted
Date: 2026-11-22
Affects: crates/engine, contract:C9

## Context

The owner's design of the secretary's prompt is a thin core plus retrieval: every request
carries only the personality (how to talk), the safeguards and the meta-skills (how to find
everything else). Facts about the owner, preferences, skills and tools are retrieved or looked
up by the agent. The reference has no such tier, so it is not a port; it changes two parts of
the ported prompt of ADR-0020, the skills index (section 4) and, through ADR-0022, the turn
reminder. This record states the core tier, its data and the defaults of those two parts.

## Decision

### 1. The tier

The core opens the stable tier of ADR-0020 section 1, before every part that can change: SOUL.md
(the personality, `engine.soul_file`, unchanged), then the safeguards under `# Safeguards`, then
the meta-skills under `# Meta-skills`, one item per line in shipped order, each text stripped. A
heading with no enabled item is left out. The prompt is built once per session and stored, so the
core is byte-identical on every turn of a session and sits in the cached prefix (ADR-0020
section 5).

### 2. Data shape

The shipped items are data in the engine binary: an id, a kind (safeguard or meta-skill) and a
text. `ulei.toml` (C9 0.5.0) overrides them by id in item sections:

| Key | Variable | Kind | Meaning |
|---|---|---|---|
| `engine.core.<id>.file` | `ULEI_ENGINE_CORE_<ID>_FILE` | string | The UTF-8 file whose stripped text replaces the shipped text of `<id>`. A missing or unreadable file, an empty value or a blank file is an error naming the key. |
| `engine.core.<id>.enabled` | `ULEI_ENGINE_CORE_<ID>_ENABLED` | switch | Off leaves `<id>` out. Default on. |
| `engine.task_board_file` | `ULEI_ENGINE_TASK_BOARD_FILE` | string | The file telling how to reach the task board: its address and commands. The task-delegation meta-skill names it; the engine does not read it. |

A key or section header under `engine.core.` whose id is not shipped is an error at start:
`ulei-engine run` fails before the journal opens and `ulei-engine serve` refuses to start, with
`config <path> line <n>: <key> names an unknown id; known: <ids>`. The decoder already refuses a
key other than `file` and `enabled`. Only the shipped ids have variables. Users add no items:
a new item is a shipped default.

### 3. Ids and default texts

| Id | Kind | Default text |
|---|---|---|
| `no_unasked_messages` | safeguard | Never write to other people without a direct command from the owner: no email, no messenger message, no issue in someone else's public repository, no blog post, nothing of the kind. |
| `no_unfiled_promises` | safeguard | Never say that you will do something later unless a task for it is actually filed. |
| `no_irreversible_deletion` | safeguard | Never delete data irreversibly without a direct command from the owner. The exception is a routine the owner has already agreed to, such as the weekly compaction of correspondence. |
| `search` | meta-skill | Search and retrieval: what this prompt does not hold, facts about the owner, notes, preferences, skills and tools, you find yourself. Find files and text with search_files and read them with read_file. List the skills with skills_list and load the one a task needs with skill_view before you start. Use web_search and the retrieval tools of your MCP servers when you have them. Look things up before you answer from memory or ask the owner. |
| `terminal_and_files` | meta-skill | Terminal and files: run commands with terminal; read files with read_file and change them with patch or write_file. Read a file before you change it, and check what a command really returned before you report it. |
| `task_delegation` | meta-skill | With `engine.task_board_file` set: Task delegation: to file a task or hand one out, read `<path>` with read_file. It gives the address of the task board and the commands to use. Unset: Task delegation: no task board is configured, so you cannot file a task or hand one out. Say so instead. |

Logins are not a safeguard (the agent enters them itself), and money is not in the default list.
Remembering is not a meta-skill: the agent's only write to long-term memory is `garden_propose`
for the owner's "remember" and corrections.

### 4. Defaults off: the skills index and the turn reminder

- The skills index of ADR-0020 section 4 is in the prompt only when `engine.skills_index`
  (`ULEI_ENGINE_SKILLS_INDEX`, a switch) is on. Default off: the path to skills is the search
  meta-skill (`skills_list`, `skill_view`, `search_files`). Switched on, the index renders and
  sits as before.
- The turn reminder of ADR-0022 is on only when `engine.turn_reminder_file` is set, so it is
  off by default and stays as it is.

### 5. Token budget

The engine has no tokenizer; the budget uses its rough estimate (`compress::estimate_tokens_rough`,
four characters a token, one per CJK character). The shipped core, both kinds without a task
board, measures 308 tokens; its ceiling is 384 tokens, about a quarter of room. The test
`the_core_tier_fits_its_token_ceiling` of `crates/engine/tests/prompt.rs` asserts the ceiling
and prints the figures, with a synthetic SOUL.md and a task board added, as a `::notice` in the
`test` CI job. A change of a default text that crosses the ceiling is a change of this record.

## Consequences

- An override or a switch reaches only sessions started after it, as any brain file
  (ADR-0020).
