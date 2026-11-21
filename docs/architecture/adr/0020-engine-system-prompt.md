# ADR-0020: Engine system prompt: the ported assembly and what stays different

Status: accepted
Date: 2026-11-18
Affects: crates/engine

## Context

`ulei-engine` ports the system prompt of the reference implementation
(`THIRD_PARTY_NOTICES.md#ref-agent`, pinned 76f6ba3): `build_system_prompt_parts` and
`build_system_prompt` of `agent/system_prompt.py`, `load_soul_md`,
`build_context_files_prompt` and `_truncate_content` of `agent/prompt_builder.py`,
`load_from_disk` and `_render_block` of `tools/memory_tool.py`, `build_skills_system_prompt`
of `agent/prompt_builder.py` with `parse_frontmatter` and `iter_skill_index_files` of
`agent/skill_utils.py`, and `_restore_or_build_system_prompt` of `agent/conversation_loop.py`. AGENTS.md allows a
difference from the reference only when it is recorded with the upstream module and the
reason. This record states what the engine assembles and the differences that stay.

## Decision

### 1. The prompt

The first turn of a session builds the prompt, stores it as C8 `Session.system_prompt`
(ADR-0014) and sends it; every later turn, also after a restart, sends the stored text
unchanged. A turn rebuilds and stores it when the session has no history yet, when the stored
prompt is absent or empty, or when the host block of the stored prompt names another working
directory, as `_restore_or_build_system_prompt` and `_stored_prompt_matches_runtime` do: the
working directory is the first `Current working directory:` line among the three after a
`User home directory:` line, trying each such line in turn.

Three tiers, each joining its non-blank parts stripped and separated by a blank line, joined
the same way:

| Tier | Parts in order |
|---|---|
| stable | SOUL.md; the index pointer of section 3; `TASK_COMPLETION_GUIDANCE`; `PARALLEL_TOOL_CALL_GUIDANCE`; the local host block of `build_environment_hints` (`Host:`, `User home directory:`, `Current working directory:`) |
| context | the project context of the working directory: the AGENTS.md chain from the git root down, else CLAUDE.md, else the cursor rules, under `# Project Context`, each source cut at 20,000 chars to its 70 % head and 20 % tail around the reference marker |
| volatile | the skills index of section 4, the MEMORY block, the USER PROFILE block, the `Conversation started:` line with the date in the zone of section 3, the IANA name, the abbreviation and the UTC offset |

The MEMORY and USER PROFILE blocks are those of `_render_block`: the entries of the file split
at `\n§\n`, stripped, empty and repeated ones dropped, under a frame of 46 `═` and a header with
the usage of the store's char limit (defaults 2200 and 1375). A file over its limit renders in
full with the usage at 100 %, as the reference snapshot does; the limit binds only the memory
write tool, which the engine does not have. An absent file renders no block. The settings and
their environment variables are listed in `docs/runbooks/engine-live-smoke.md`.

### 2. Parts that are not ported

| Part | Upstream | Why |
|---|---|---|
| The default identity when SOUL.md is absent or blank | `agent/prompt_builder.py` `DEFAULT_AGENT_IDENTITY`; `agent/system_prompt.py` | It names the reference product, which the engine is not. A missing or blank SOUL.md fails the turn with an error naming the file. |
| The help guidance about the reference product and its documentation | `agent/prompt_builder.py` `HERMES_AGENT_HELP_GUIDANCE` | Same reason; the engine carries no such skill or documentation. |
| Guidance of tools the engine does not have: memory, user profile, session search, skill management, kanban, computer use | `agent/prompt_builder.py` `MEMORY_GUIDANCE`, `USER_PROFILE_GUIDANCE`, `SESSION_SEARCH_GUIDANCE`, `SKILLS_GUIDANCE`, `KANBAN_GUIDANCE`, `computer_use_guidance` | The reference adds each only with its tool; each comes with its tool here. |
| The mid-turn steering note | `agent/prompt_builder.py` `STEER_CHANNEL_NOTE` | Steering belongs to the C6 server; the note comes with it. Its text also names the reference product. |
| The subscription prompt | `agent/prompt_builder.py` `build_nous_subscription_prompt` | Provider-specific; providers live in qb (ADR-0009). |
| Guidance gated on the model name: tool-use enforcement, the Google operational guidance, the execution guidance, the model-name line of one provider; their settings `agent.tool_use_enforcement` and `agent.execution_guidance` | `agent/system_prompt.py`; `agent/prompt_builder.py` `TOOL_USE_ENFORCEMENT_MODELS`, `EXECUTION_GUIDANCE_MODELS` | The engine sends a route alias and qb picks the model (ADR-0009), so no model name is known when the prompt is built. The secretary's model matches neither default list, so its reference prompt carries none of them either. |
| Host notes beyond the local host block: the WSL line and hint, the remote backend probe, the Windows notes, the embedder hint of the environment variable or `agent.environment_hint` | `agent/prompt_builder.py` `build_environment_hints` | The engine runs its tools on the local Linux host only; no deployment sets an embedder hint. |
| The coding posture: operating brief, workspace snapshot, trailing guidance | `agent/coding_context.py` `coding_system_prompt_parts` | The reference applies it only on interactive coding surfaces (`INTERACTIVE_CODING_PLATFORMS`); the secretary's surface is a messenger, where it is off. |
| The Python toolchain probe line | `tools/env_probe.py` `get_environment_probe_line` | It runs interpreter and package-manager probes in a background thread; the engine starts no threads at idle (ADR-0010). |
| The Bot Mode protocol, the capability epoch and the timeless prompt | `tools/bot_mode_probe.py`; `agent/conversation_loop.py` | The engine runs no bots. |
| The active profile line | `agent/system_prompt.py` | The engine has no profiles. |
| Platform hints, the Telegram rich-message hint, the `platform_hints` overrides and the `Platform:` line | `agent/prompt_builder.py` `PLATFORM_HINTS`, `TELEGRAM_RICH_MESSAGES_HINT`; `agent/system_prompt.py` `_resolve_platform_hint` | `ulei-engine run` has no platform; the channel will name one over C6 together with the per-session route. |
| The `Session ID:`, `Model:` and `Provider:` lines and the stale check on model, provider and platform | `agent/system_prompt.py`; `agent/conversation_loop.py` `_stored_prompt_matches_runtime` | The session id line is off by default (`pass_session_id`); the model and provider are qb's choice (ADR-0009). The working-directory check is ported. |
| The caller-supplied system message and the ephemeral system prompt | `agent/system_prompt.py` `system_message` | The engine has no caller that supplies system text. |
| The `.hermes.md` and `HERMES.md` project context and its frontmatter strip | `agent/prompt_builder.py` `_find_hermes_md`, `_load_hermes_md`, `_strip_yaml_frontmatter` | The file names carry the reference product name, which engine code may not name (AGENTS.md hard rule 8). The AGENTS.md chain, CLAUDE.md and the cursor rules follow in the reference order. |
| The threat scan of SOUL.md, context files and memory entries and its `[BLOCKED: ...]` placeholders | `tools/threat_patterns.py`; `agent/prompt_builder.py` `_scan_context_content`; `tools/memory_tool.py` `_sanitize_entries_for_snapshot` | Its NFKC normalisation needs Unicode tables the engine binary does not carry (ADR-0010 budgets, as for `read_file` in ADR-0016), and the library is shared with the memory write tool and the tool-result delimiters, which the engine does not have; it comes with the memory write tool. The leading byte order mark the scan removes is removed. |
| The external memory provider block and plugin prompt sections | `agent/system_prompt.py` `_memory_manager`, `_frozen_plugin_prompt_sections` | The engine loads no memory providers or plugins. |
| The context-file cap from the model's window and `context_file_max_chars` | `agent/prompt_builder.py` `_dynamic_context_file_max_chars`, `_get_context_file_max_chars` | The C4G routes do not report a window, so the cap is the 20,000 chars the reference uses without one. With the deployment's model the reference cap is larger: a SOUL.md or context file between 20,000 chars and that cap is cut here and whole there. |
| Truncation warnings on the status channel and in the log | `agent/prompt_builder.py` `_record_truncation_warning`, `drain_truncation_warnings` | The engine has no status channel or log; the marker in the prompt stays. |
| The rebuild after compaction, the static prefix for a two-block cache layout, the prompt table deduplicated by hash | `agent/system_prompt.py` `invalidate_system_prompt`, `reconstruct_static_prefix`; `hermes_state.py` `_store_system_prompt` | The engine does not compact yet; the rebuild comes with compaction. Cache layout is qb's (ADR-0009): the engine sends only the cache mode of section 5. The prompt is stored in its session row. |
| The `on_session_start` hook and the credits seed | `agent/conversation_loop.py` | The engine loads no plugins; credits are qb's. |

### 3. Differences in what is ported

| Behaviour | Upstream | Why |
|---|---|---|
| The pointer `Knowledge base index: <path>. Read it with the read_file tool to find the notes a task needs.` after SOUL.md, for `engine.index_file`; the file is not read | none | The owner's definition of done asks the prompt to point to INDEX.md of the brain at a configured path. The reference has no such part: the deployment writes the pointer into its SOUL.md. |
| SOUL.md, MEMORY.md and USER.md come from three configured paths | `agent/prompt_builder.py` `load_soul_md`; `tools/memory_tool.py` `get_memory_dir` | The reference reads `SOUL.md` and `memories/` of its profile home; the deployment keeps the brain files in different directories, and explicit paths need no links. |
| A brain or context file that exists but is not readable UTF-8 text fails the turn with an error naming it | `agent/prompt_builder.py` `load_soul_md`, `_load_agents_md`, `_load_claude_md`, `_load_cursorrules`; `tools/memory_tool.py` `_read_file` | The reference skips such a context file and reads such a memory file as empty, with a debug log line. The prompt is frozen for the session, so the notes would be missing from every turn of it, and the engine has no log for the line. A directory where a context file is looked up is skipped, as there. |
| The zone of the `Conversation started:` line: `engine.timezone`, read from the first of `/usr/share/zoneinfo`, `/usr/lib/zoneinfo`, `/usr/share/lib/zoneinfo` and `/etc/zoneinfo` holding it; unset, the zone file `/etc/localtime`, UTC without it. An unknown or malformed zone fails the turn | `hermes_time.py` `get_timezone`, `now`; Python `zoneinfo` | The setting is the reference's `timezone`. The reference reads local time through libc, which honours `TZ`; the engine reads the zone file. It logs a warning for a bad zone and uses local time; the engine has no log, and a wrong date would stay in the prompt for the whole session. |
| `HOME` unset or empty fails the turn | `agent/prompt_builder.py` `build_environment_hints` (`os.path.expanduser`) | Python falls back to the password database; the engine reads no user database. |

### 4. The skills index

The index of `build_skills_system_prompt` leads the volatile tier, so a changed index re-reads
only the tail of a cached prefix. It lists the skills of `engine.skills_dir`, the directory the
skill tools read (ADR-0016 section 10), under `## Skills (mandatory)` and `<available_skills>`:
categories in order, each with the `description` of its `DESCRIPTION.md`, then its skills by
name, each with its `description` stripped of quotes and cut to 60 characters ending in `...`.
A skill's category is the path of directories above its own, the directory itself for a skill
at the top. A skill for another platform, or disabled by its frontmatter name or directory name
in `engine.disabled_skills`, is left out; a `SKILL.md` that is not UTF-8 text is listed without a
description. No directory, or no skill in it, leaves the index out.

| Part | Upstream | Why |
|---|---|---|
| The sentence that sends the model to the reference product's own skill for questions about the reference product | `agent/prompt_builder.py` `_build_skills_system_prompt_inner` | It names the reference product and a skill the engine does not carry, as the help guidance of section 2. |
| The sentence `If a skill has issues, fix it with skill_manage(action='patch').` | `agent/prompt_builder.py` `_build_skills_system_prompt_inner` | The engine has no `skill_manage`; the sentence comes with it. |
| The in-process cache of rendered indexes and the snapshot file in the skills directory | `agent/prompt_builder.py` `_SKILLS_PROMPT_CACHE`, `_load_skills_snapshot`, `_write_skills_snapshot`, `_build_skills_manifest` | The engine builds the prompt once per session and stores it (section 1), so the index is scanned only then; the snapshot would write into the skills directory. |
| The names-only demotion of categories outside the coding context | `agent/prompt_builder.py` `build_skills_system_prompt` (`compact_categories`); `agent/coding_context.py` | Part of the coding posture of section 2, which is off on the secretary's surface. |
| External, project and organisation skill directories, conditional activation, the environment gate, the frontmatter subset | `agent/prompt_builder.py`, `agent/skill_utils.py` | As for the skill tools in ADR-0016 section 10. |

### 5. The prompt cache

Every turn request, the compaction summary request and the `no-ask-guard` classifier request
carry the C4G `cache` mode of `engine.prompt_cache` (`ULEI_ENGINE_PROMPT_CACHE`, C9 0.3.0):
`auto_1h` by default, `auto`, or `off`, which sends no `cache` field. Any other value refuses
the start of `run` and `serve` with `engine.prompt_cache (ULEI_ENGINE_PROMPT_CACHE) must be
auto_1h, auto or off`. qb places the breakpoints: on the last system block, else the last tool,
on the last message and on the user message before it.

Prefix rule: a request sends what stays fixed across the turns of a session before what
changes. The system prompt is the stored text of section 1, the tools are the built-in tools in
their fixed order followed by each MCP server's tools in the order of `engine.mcp_servers` and of
its listing, and the history is the journal's messages as stored. The turn context of ADR-0022
is appended only to the sent copy of the current input, so the next turn repeats the system
text, the tools and the stored history byte for byte, and its prefix differs first at the
previous turn's input.

| Behaviour | Upstream | Why |
|---|---|---|
| The setting is `engine.prompt_cache` with `auto_1h`, `auto` and `off`, default `auto_1h`; an unknown value refuses the start | `agent/agent_init.py` (`prompt_caching.cache_ttl`: `5m` by default, `1h`, a disabling spelling, any other value kept at `5m` silently) | The owner's deployment sets `1h`, the qb tier `auto_1h` names it; models go through qb (ADR-0009), whose C4G names the modes. A silent fallback would hide a typo for the life of the deployment. |
| The breakpoints are placed by qb, not by the engine | `agent/prompt_caching.py` | qb owns the provider request (ADR-0009); its placement covers the system and tools prefix and the latest messages, as the reference's layout does. |

## Consequences

- An edit of a brain file reaches only sessions started after it: `--new` or a new session key.
- Adding a part of section 2 or removing a difference of section 3 is a change of this record.
