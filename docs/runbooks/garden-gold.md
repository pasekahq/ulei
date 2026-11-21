# Runbook: the garden on the real gold set

A manual procedure, never run in CI: the repository and CI hold only the synthetic corpus and
gold set (`crates/garden/tests/fixtures/`) and the tiny model `ci/garden_onnx.py` writes at run
time. The owner's secretary agent runs it on the owner's host after the commit under test is
merged; the procedure, its targets and the report are fixed by ADR-0050. The gold set, the
corpus and every report stay on the host; the figures go to the owner. A step that fails stops
the run: record the command, its exit code and its last stderr lines, and report them.

Placeholders. Fill them on the host only; never write a value into this repository.

| Placeholder | Meaning |
|---|---|
| `<COMMIT>` | The ulei commit under test. |
| `<ULEI_CHECKOUT>` | A checkout of `<COMMIT>`. |
| `<GOLD>` | The gold set v2, JSON lines with `id`, `query`, `category`, `expected_source`, `gold_paths`. |
| `<CORPUS>` | The Markdown corpus the gold paths are relative to. |
| `<FILES>` | Optional: corpus-relative Markdown paths, one per line, when not every `*.md` of `<CORPUS>` belongs to the run. |
| `<WORK>` | A fresh directory of the running account, mode 0700, outside the checkout, for the ONNX Runtime, the reports and the daemon's scratch files. |
| `<MODEL_DIR>` | Holds `onnx/model_quantized.onnx` and `tokenizer.json` of the bge-m3 ONNX conversion of ADR-0047 section 10. |
| `<PROTOTYPE_EVAL>` | The command of the prototype's own retrieval harness on `<GOLD>` and `<CORPUS>`. |
| `<BASELINE_RECALL>` | The prototype's recall@3 on `<GOLD>` from step 1. |

The ONNX Runtime library is `<WORK>/ort/libonnxruntime.so` (step 3). Commands run from
`<ULEI_CHECKOUT>`; `cargo` may be `$HOME/.cargo/bin/cargo`. A release build uses fat LTO: run
one build at a time, under `nice`. Every `garden-eval` call below takes the same base flags:

```sh
EVAL=target/x86_64-unknown-linux-gnu/release/garden-eval
ONNX="--embedder onnx --model-dir <MODEL_DIR> --ort-lib <WORK>/ort/libonnxruntime.so --threads 2"
BASE="--gold <GOLD> --corpus <CORPUS> --k 3"   # add --files <FILES> when it is used
fig() { python3 -c 'import json,sys
r = json.load(open(sys.argv[1]))
for m in ("hybrid", "lexical"):
    x = r[m]
    if x:
        print(m, "recall@3 %.3f MRR %.3f p50 %.1f ms p95 %.1f ms" % (x["recall@3"], x["mrr"],
              x["latency_ms"]["p50"], x["latency_ms"]["p95"]), "pointers", x["pointers"])
print("build", r["build"])' "$1"; }
```

A report names the gold categories and the corpus sizes: it stays under `<WORK>`.

## 1. The prototype's baseline

Run `<PROTOTYPE_EVAL>` on `<GOLD>` and `<CORPUS>` and write down its recall@3, MRR and warm
p50. Its recall@3 is `<BASELINE_RECALL>`, the recall target of step 6 (ADR-0050 section 4).

## 2. Builds

```sh
nice cargo build --release --locked -p ulei-garden --features onnx --bin garden-eval \
  --target x86_64-unknown-linux-gnu
nice cargo build --release --locked -p ulei-garden --features onnx --bin ulei-garden-embed \
  --target x86_64-unknown-linux-gnu
nice cargo build --release --locked -p ulei-garden --bin ulei-garden \
  --target x86_64-unknown-linux-musl
```

The musl build needs `x86_64-linux-musl-gcc` (package `musl-tools`).

## 3. Model and runtime

The ONNX Runtime is the package the CI job `garden-onnx` pins: take `ORT_VERSION` (1.28.0) and
`ORT_NUPKG_SHA256` from the job in `.github/workflows/ci.yml` at `<COMMIT>`.

```sh
ORT_VERSION=1.28.0
ORT_NUPKG_SHA256=<ORT_NUPKG_SHA256 of the garden-onnx job>
curl -sSfL -o <WORK>/ort.nupkg \
  "https://api.nuget.org/v3-flatcontainer/microsoft.ml.onnxruntime/${ORT_VERSION}/microsoft.ml.onnxruntime.${ORT_VERSION}.nupkg"
echo "${ORT_NUPKG_SHA256}  <WORK>/ort.nupkg" | sha256sum -c -   # stop on a mismatch
unzip -j <WORK>/ort.nupkg runtimes/linux-x64/native/libonnxruntime.so -d <WORK>/ort
sha256sum <MODEL_DIR>/onnx/model_quantized.onnx <MODEL_DIR>/tokenizer.json
```

Write down both SHA-256 of the model files for the report.

## 4. Passage size

Measure every size, then take the largest up to 1200 whose hybrid recall@3 and MRR equal the
best (ADR-0050 section 3):

```sh
for n in 600 800 1000 1200; do
  $EVAL $BASE $ONNX --passage-chars $n --out <WORK>/passage-$n.json && fig <WORK>/passage-$n.json
done
```

The chosen size is `<PASSAGE_CHARS>` from here on.

## 5. Pointer thresholds

`pointers` makes a hit a pointer when its cosine reaches `--pointer-min-cosine` or its lexical
score (-bm25) reaches `--pointer-min-lexical`. Measure each with the other source switched off:
a cosine never exceeds 1, so `--pointer-min-cosine 2` lets no cosine through, and
`--pointer-min-lexical 1e9` lets no lexical score through. The lexical sweep needs no model.

```sh
for c in 0.40 0.45 0.50 0.55 0.60 0.65 0.70; do
  $EVAL $BASE $ONNX --passage-chars <PASSAGE_CHARS> --pointer-min-cosine $c \
    --pointer-min-lexical 1e9 --out <WORK>/cosine-$c.json && fig <WORK>/cosine-$c.json
done
for l in 4 6 8 10 12 14 16 20; do
  $EVAL $BASE --embedder none --passage-chars <PASSAGE_CHARS> --pointer-min-cosine 2 \
    --pointer-min-lexical $l --out <WORK>/lexical-$l.json && fig <WORK>/lexical-$l.json
done
```

Read `pointers` of the hybrid mode for the cosine sweep and of the lexical mode for the lexical
sweep. For each threshold keep the values with `unrelated_pointed` 0, among them those with the
most `right`, and take the middle of the widest run of such values; widen the sweep when the run
touches its end. These are `<MIN_COSINE>` and `<MIN_LEXICAL>`.

## 6. The final run

```sh
$EVAL $BASE $ONNX --passage-chars <PASSAGE_CHARS> --pointer-min-cosine <MIN_COSINE> \
  --pointer-min-lexical <MIN_LEXICAL> --min-recall <BASELINE_RECALL> --max-p50-ms 100 \
  --out <WORK>/final.json
echo "exit $?"; fig <WORK>/final.json
```

`garden-eval: measured ...` on stderr names both modes; exit 1 with one line per miss when a
mode misses a target. The targets bind the hybrid mode (ADR-0050 section 4): a lexical line
alone is reported, not a miss. `build.passage_ms.max` is the longest one passage held the
embedder, the most a query waits behind the passage in flight; compare it with 150 ms.

## 7. The helper's RSS with the model

One passage of `<PASSAGE_CHARS>` characters and one query through the helper alone, which loads
the model as the daemon starts it and exits when its input ends:

```sh
python3 -c 'import json, sys
text = open(sys.argv[1], encoding="utf-8").read()[:int(sys.argv[2])]
print(json.dumps({"kind": "passage", "texts": [text]}))
print(json.dumps({"kind": "query", "texts": ["test query"]}))' <a long file of CORPUS> <PASSAGE_CHARS> \
  | /usr/bin/time -f 'helper maxrss %M kB' target/x86_64-unknown-linux-gnu/release/ulei-garden-embed \
      --model-dir <MODEL_DIR> --ort-lib <WORK>/ort/libonnxruntime.so --threads 2 >/dev/null
```

The `helper maxrss` line is the helper's peak RSS with the model (`/usr/bin/time` is GNU time,
package `time`).

## 8. The daemon with the model

A scratch store under `<WORK>/garden` with fictional facts only, never the corpus:

```sh
mkdir -m 700 <WORK>/garden <WORK>/garden/run
head -c 32 /dev/urandom > <WORK>/garden/producer.key && chmod 600 <WORK>/garden/producer.key
cat > <WORK>/garden/ulei.toml <<EOF
[garden]
store = "<WORK>/garden/garden.sqlite3"
socket = "<WORK>/garden/run/garden.sock"
producer_key_file = "<WORK>/garden/producer.key"

[garden.embedder]
enabled = true
helper = "<ULEI_CHECKOUT>/target/x86_64-unknown-linux-gnu/release/ulei-garden-embed"
model_dir = "<MODEL_DIR>"
ort_library = "<WORK>/ort/libonnxruntime.so"

[inbox]
socket = "<WORK>/garden/inbox.sock"
EOF
target/x86_64-unknown-linux-musl/release/ulei-garden serve --config <WORK>/garden/ulei.toml &
GARDEN=$!
g() { curl -sS --unix-socket <WORK>/garden/run/garden.sock -d "$2" "http://localhost/v1/$1"; echo; }
g health '{}'
g propose '{"fact": "keeps a brass lantern in the attic.", "entity": "Ilsa Varn", "kind": "person", "source_kind": "owner", "source_ref": "runbook"}'
g propose '{"fact": "buys fig jam on Mondays.", "entity": "Odo Quill", "kind": "person", "source_kind": "owner", "source_ref": "runbook"}'
sleep 5
g search '{"q": "lamp storage spot"}'
grep VmHWM /proc/$(pgrep -P $GARDEN)/status
kill $GARDEN
```

Pass: the ready line, `health` with `"dense": "ready"`, both proposes `accepted`, and the search
answer ranks the lantern fact first without `"lexical_only": true`: no query word starts a word
of the facts, so the hit is the dense side's. Nobody serves the inbox
socket here: misses stay in the outbox and `health` counts them. `VmHWM` is the helper's peak
RSS after it embedded the passages; report it beside step 7.

## 9. Report to the owner

The fields of ADR-0050 section 5: the baseline of step 1; `fig <WORK>/final.json` and the
targets met or missed; the per-category figures of `final.json`; the tables of steps 4 and 5
and the chosen `<PASSAGE_CHARS>`, `<MIN_COSINE>`, `<MIN_LEXICAL>`; `build.passage_ms` against
150 ms; the RSS of steps 7 and 8; the pass or fail of step 8; `<COMMIT>`, the ONNX Runtime
version, both model SHA-256, the CPU model and the thread count. Quote no question, passage or
corpus path. The chosen pointer thresholds go into the deployment's `ulei.toml`; the first
run's figures become the thresholds of ADR-0050 by a separate change.
