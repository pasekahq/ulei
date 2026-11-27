# AGENTS.md: `ulei-engine`

The ecosystem's own synchronous agent loop: tool loop, tools, sessions, the `ulei-engine` binary.

Read the root `AGENTS.md` first. Rules specific to this crate:

- Every model call goes to the qb gateway over its contract and nowhere else (ADR-0009): no
  vendor adapter, vendor host, vendor credential path or vendor SDK crate. The contract is C4G,
  qb's own file vendored at the qb pin of `ecosystem.lock` (ADR-0038).
- Tests use a scripted fake gateway and never reach a vendor. The fake is
  `tests/fake_gateway/`; a test crate includes it with `mod fake_gateway;`. It
  answers in the vendored C4G form only, built from the generated types; a test of a field the
  types do not know sends raw bytes (`Answer::Raw`, `FakeGateway::answer_once`).
- A test of the garden's C7 or the archive's C10 uses the same fake as its socket, answering
  `POST /v1/<method>` through `FakeGateway::respond`.
- The garden reaches the model in three ways, all only with `garden.socket` or `garden.core_file`
  set (ADR-0077): the core profile block that ends the system prompt, re-read every turn and
  stored only when it changed; C7 `pointers` at the start of an owner turn into its ephemeral
  context, within `garden::POINTERS_DEADLINE` (150 ms), a failure one stderr line and never a
  failed or delayed turn; and the tools `garden_search` and `garden_propose`, registered only
  when the socket is set, with `garden_review` (C7 `reconcile_review`, what the night's judge
  changed on its own; it never reports changes unasked). `tests/garden.rs` runs them on the fake
  and once on a real `ulei-garden serve`.
- The one test of a real gateway is `tests/qb_contract.rs`: qb built at the pin, on fake vendors,
  ignored in the ordinary run and run by the CI job `qb-contract` (ADR-0038 section 6).
- MCP servers are reached only on loopback over the `gateway.rs` reader (ADR-0015, ADR-0026);
  tests use the fake Streamable HTTP server `tests/fake_mcp/` with `mod fake_gateway; mod fake_mcp;`.
- Synchronous: no async runtime, no background threads or timers when idle.
- `ulei-engine` is held to the daemon budgets of ADR-0010; a new dependency needs its own ADR.
- Sort through `crate::sort`, not the slice sorts or a collected `BTreeSet` or JSON `Map`:
  each such call site links its own copy of the sort (ADR-0027).
- TLS lives only in the `ulei-web` helper (ADR-0015, ADR-0021): `ureq` is used only in
  `src/bin/ulei-web.rs`, and the helper's logic in `src/web*` compiles only with the `web`
  feature. `ulei-engine` never gets the feature. The helper's tests in `tests/web.rs` and the
  end-to-end run of `serve` in `tests/e2e.rs` need `--all-features` and use the fake Exa
  `tests/fake_exa/` on loopback, never the network.
- Tests that drive `ulei-engine serve` over its C6 socket start it and call it through
  `tests/c6_wire/` (`mod fake_gateway; mod c6_wire;`).
- Settings: `src/config.rs` reads `ulei.toml` only through `ulei_contracts::config` (ADR-0040):
  the file is required and checked whole against C9, and each `ULEI_*` setting variable wins
  over its key. A test, fake or CI driver that starts `ulei-engine` writes a valid C9 document or
  points at an empty one.
- `night` (`src/night.rs`, `src/night/`) is the night gardener of ADR-0078, an owner-designed
  addition, not a port: `ulei-engine night` runs once and exits, under
  `deploy/systemd/ulei-night.service` and its timer. It reaches the gateway, the garden (C7) and
  the archive (C10) only over their sockets through `gateway.rs`, writes the garden only through
  `propose`, `seedbed_done`, `forget_done` and `reconcile_done`, writes the forest only through `forest_put`
  (ADR-0079; no tool writes it), and keeps its cursor and run reports in `night.state`. A record or session is never marked done without an answer of the model or the
  edge; a malformed answer is asked once more, then left for the next night; a left history
  session does not stop the walk and is retried from the cursor's `retry` on later runs, and the
  run then stops `done_with_failures`. A seedbed batch
  whose answer hits `max_tokens` is split in halves instead, down to one record. A model call
  starts only under `night.max_usd`. Stage d, the judge (ADR-0096, `night/judge.rs`, its prompt
  `night/judge.txt`, `judge::PROMPT_VERSION`), takes the facts the garden hinted with
  `reconcile_take`, sends whole entities per call up to `night.judge_batch` new facts, reads
  the answer per new fact (a fact left out or named by a defective item waits, none of its
  verdicts applied, counted in `judge_dropped`; an answer deciding no fact is malformed) and
  hands the verdicts with its own counts to `reconcile_done`; it spends
  from `night.judge_max_usd`, a budget of its own, and runs after stage a or c met their limit;
  facts it leaves wait in the garden. Its report lines hold counts and refs, never fact text.
  The judge's accuracy is measured outside CI by `ci/judge_eval.py`. `--dry-run` calls no model and writes nothing. Its tests are the
  unit tests in `src/night/`, `tests/night.rs` on the fake gateway as gateway, garden and archive,
  and `ci/night_run.py` on the released binaries.
- The debug trace (`src/trace.rs`, ADR-0092) is an owner-designed addition, not a port: with
  `engine.trace_file` set, one JSON line per turn, turn context, model request, tool call and
  compaction, never a message or tool text; unset, nothing is written. A trace failure is one
  stderr line per process and never fails a turn; the model never sees the file.
- Talk to other modules only through `ulei-contracts` and the wire.
- Check: `python3 -m ci.broad_check` from the repository root.
