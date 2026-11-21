# ADR-0050: The garden's real gold: the run outside the repository and its targets

Status: accepted
Date: 2026-11-21
Affects: crates/garden

## Context

`garden-eval` (ADR-0045 section 5) measures retrieval on a Markdown corpus against a gold set.
In the repository and in CI it runs only on the synthetic set (ADR-0045 section 6) and the
ONNX path only on a tiny generated model (ADR-0047 card 20). The owner's real corpus and gold
set (gold v2, 115 questions) are personal: they never enter the repository, CI, an issue or a
pull request, and the heads that build the garden never open them. The figures CI cannot give
are left to a run on the owner's host: recall and latency with the real bge-m3 int8 and ONNX
Runtime, the helper's RSS with the model (ADR-0047 section 7), and how long a query waits
behind the one passage in flight (ADR-0047 section 2). The owner's earlier retrieval prototype,
the reference of ADR-0045, reached recall@3 0.46 on gold v1.

## Decision

1. Who and where. The owner's secretary agent runs `docs/runbooks/garden-gold.md` on the
   owner's host after the change under test is merged, from a checkout of that commit. The gold
   set and the corpus stay at the host paths given to `garden-eval --gold` and `--corpus`; the
   report and its figures go to the owner. Nothing of the gold, the corpus or a report enters
   the repository or CI.
2. Order. First the baseline: the prototype's own harness on gold v2 and the same corpus gives
   recall@3, MRR and warm p50. Then `garden-eval`, built with feature `onnx` for
   `x86_64-unknown-linux-gnu`, with `--embedder onnx` on bge-m3 int8 (`onnx/model_quantized.onnx`
   and `tokenizer.json` of the ONNX conversion of ADR-0047 section 10) and the ONNX Runtime
   1.28.0 library of the NuGet package pinned by SHA-256 in the CI job `garden-onnx`: the
   passage size, then the pointer thresholds, then one final run at the chosen values. Last the
   helper `ulei-garden-embed` alone for its RSS, and the daemon with `garden.embedder.enabled`
   for `dense: ready` and a dense hit.
3. Choices on the real corpus. The passage size is the largest `--passage-chars` up to 1200
   among those measured whose hybrid recall@3 and MRR equal the best, the rule ADR-0045 section
   5 applies to the synthetic set; a size other than the default 1200 is a later change of
   ADR-0045. `pointer_min_cosine` and `pointer_min_lexical` follow the rule of ADR-0045 section
   3, each measured with the other source switched off: the middle of the widest range of
   values where no `none` row gets a pointer and the most scored rows get a pointer to a gold
   file. They go into the deployment's `ulei.toml` as `garden.api.pointer_min_cosine` and
   `garden.api.pointer_min_lexical`.
4. Targets of the final run, hybrid mode:
   - recall@3 not below the prototype's recall@3 on the same gold set, measured in step 2 (on
     gold v1 it is 0.46);
   - warm p50 at most 100 ms on CPU, the query embedding included, with the helper's default 2
     intra-op threads.
   Reported with them, without a limit yet: MRR, p95, the pointer counts, the helper's peak RSS
   with the model and the max of `build.passage_ms`, the wait of a query behind one passage,
   against the 150 ms deadline of ADR-0047 section 3b. The exact thresholds of recall, p50,
   RSS and wait are fixed after the first run by a separate change of this ADR. A run that
   misses a target is reported to the owner as a miss; no threshold or setting is moved to hide
   it.
5. The report to the owner holds figures only: the baseline's recall@3, MRR and p50; of the
   final run per mode recall@3, MRR, p50, p95, per category and the pointers (`right`, `wrong`,
   `silent`, `unrelated_pointed` of `unrelated`); `build.facts`, `files`, `rejected`,
   `passage_chars`, `model_load_ms`, `embed_ms` and `passage_ms` (p50, p95, max); the chosen
   passage size and pointer thresholds with the tables they were chosen from; the helper's peak
   RSS; the daemon's `health` and whether the dense hit came; the commit, the ONNX Runtime
   version, the SHA-256 of both model files, the CPU and the thread count. It quotes no
   question, passage or corpus path.

## Consequences

- The targets of the DoD are checked where the data lives, by the one agent allowed to read it;
  CI keeps proving the same code paths on synthetic data and the tiny model.
- Until the first run is recorded here, the pointer defaults stay those of the synthetic set
  and the fake embedder (ADR-0047 section 4), which do not hold for bge-m3.
