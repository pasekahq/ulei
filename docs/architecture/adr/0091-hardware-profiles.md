# ADR-0091: Hardware profiles: four tiers, the profile manifest, the hardware probe, reference hosts and per-profile budgets

Status: proposed (draft, not for merge)
Date: 2026-11-24
Affects: crates/core

## Context

The paseka ships to owners on very different machines, and the owner direction of 2026-11-24
sets the product shape: the owner installs through Homebrew, apt or a script, is asked what
they want, and picks a level. On an Apple Silicon desktop everything but the main LLMs runs
locally (embedders, reranker, speech to text, later a small LLM as a fallback when the
subscription runs out or to save money). An owner without hardware is told what each budget
buys: a small VPS works but searches worse and takes a Groq key in place of local models; a
larger one runs everything locally. Each level is squeezed for its hardware, cutting what it
cannot afford.

What stands today:

- ADR-0001 fixes one release target (`x86_64-unknown-linux-musl`, static) and one budget row
  per binary. The budgets bind the daemons (2.5 to 4 MiB static, 4 MiB peak RSS); nothing binds
  a model, a host total or a machine class.
- The garden's embedder is bge-m3 int8, a 569.7 MB file in the helper `ulei-garden-embed`,
  the one dynamic glibc binary (ADR-0047 section 1, A3): 25.3 s for a fresh process to load
  and warm it, warm p50 95 to 159 ms per query on CPU. Its RSS is the helper's, several
  hundred MB, and no budget names it (ADR-0047 section 7).
- Voice notes go to Groq `whisper-large-v3-turbo` (ADR-0035), the one model call outside qb.
- No reranker (`NoReranker`) and no local LLM exist.
- A reference of what a dedicated CPU engine costs, measured outside this repository on
  Apple M (darwin/arm64) and not on any host of ours: a pure-Go port of GigaAM v3 CTC with
  int8 weights by mmap, 222 MB file, RTF 0.30 on one thread, 256 MB peak RSS of which 22 MB
  dirty, WER equal to the original within 0.1 % absolute. It shows the order of what a
  profile can hold; its numbers are not budgets until measured on the reference hosts below.

The CPUs the profiles meet:

| Class | Where | SIMD the kernels can count on | Optional |
|---|---|---|---|
| x86_64 Intel VPS | Skylake to Cascade Lake and later Xeons, the common KVM VPS | AVX2, FMA | AVX-512F, AVX-512 VNNI (Cascade Lake+), often masked by the provider |
| x86_64 AMD VPS | EPYC Zen 2/3 | AVX2, FMA | none (no AVX-512 before Zen 4) |
| arm64 VPS | Ampere Altra (Neoverse N1), Graviton 2/3 | NEON, dot product (SDOT/UDOT) | i8mm (Graviton 3 and later; to verify per offer) |
| Apple Silicon | M1 to M4, Mac mini and laptops | NEON, dot product | i8mm (to verify per generation), SME (M4); Accelerate/AMX, Metal, ANE through system frameworks |

The pilot host is a 3 vCPU KVM VPS (Intel Skylake: AVX2, AVX-512F, no VNNI; 7.8 GiB RAM;
CPU steal 0 to 1 % when measured on 2026-11-24).

## Decision

### 1. Four profiles

| Profile | Minimum machine | Arch | Search | Speech to text | Reranker | Local LLM | Cloud the owner configures |
|---|---|---|---|---|---|---|---|
| `lite` | 2 vCPU, 4 GiB, 40 GB free | x86_64, arm64 | lexical only (FTS5), no embedder (section 1.1) | cloud (ADR-0035) | no | no | subscription through qb, a Groq key |
| `standard` | 4 vCPU, 8 GiB, 80 GB free | x86_64, arm64 | lexical + dense: multilingual-e5-small int8 by default; bge-m3 int8 when the owner accepts its RAM and first-index time (section 9.2) | local GigaAM v3 for Russian at about 3 points WER above the cloud on the owner's notes, or the cloud path by the owner's choice (section 9.3); English open (open question 6) | no | no (optional small model for background classes, section 3) | subscription through qb; a Groq key while English speech is cloud |
| `max` | 8 vCPU, 16 GiB, 150 GB free | x86_64, arm64 | lexical + dense bge-m3 int8 + reranker | as `standard` | yes | small model on CPU for the cheap task classes | subscription through qb |
| `mac` | Apple Silicon, 16 GB unified memory | aarch64-apple-darwin | as `max` | local whisper large-v3-turbo through Metal for both languages, with the vocabulary prompt, a filter of subtitle phrases and a GigaAM v3 cross-check of short notes (section 9.3) | yes | 7B to 14B class through Metal | subscription through qb for the main LLMs |

The minimums are proposals, confirmed or changed by the measurements of section 5 before this
ADR is accepted. The embedder of each dense profile follows the first measurements of section
9 and is re-checked on the reference hosts (card 5). Every node of one installation uses the
same embedder, so changing the profile can mean re-embedding (section 6).

#### 1.1 `lite` searches lexically

`lite` installs no embedder and no model process: the garden runs with `garden.embedder`
off, its retrieval tiers on FTS5 alone (`health` answers `dense: off`, ADR-0047 section 2).
The embedder's RSS of several hundred MB (ADR-0047 section 7) does not fit beside the OS, the
daemons and the owner's own load in 4 GiB with room to spare, and its 25.3 s warm-up would be
paid on every restart of a memory-pressed host.

The limit is stated to the owner at install and in `ulei status`, in these terms:

- Search finds records that share words or word prefixes with the query. It does not find a
  record that says the same thing in other words (a synonym, a paraphrase, another language).
- The miss grows with the volume: with a few thousand records (notes, the agent's own memory,
  months of chats) the right record is usually among the first results; with years of mail and
  messengers imported (hundreds of thousands of records) the common words of a query match too
  much and the ranking degrades.
- The way out is `standard`: the records stay, the garden embeds them once (section 6).

First measurement (section 9.1, MIRACL dev, 300 questions per language): lexical recall@3
falls from 0.81 (ru) and 0.86 (en) at 10^3 records to 0.57 and 0.74 at 10^5, MRR from 0.94 and
0.98 to 0.73 and 0.88; questions sharing few words with their answer fall first (ru 0.71 at
10^3, 0.41 at 10^5). Across languages lexical search finds next to nothing (recall@3 0.02 to
0.12). The install text says so in numbers: up to about 10^4 records the right record is
usually first; at 10^5 a quarter to two fifths of answers drop out of the first three, more in
Russian; a question in the other language than the record is not found.

`lite` also gets the lexical fixes of section 9.1 (English stop words and stems), which made
English search 10 to 17 times faster at the same or better recall.

### 2. A profile is data, not a build

| Option | What differs per profile | Verdict |
|---|---|---|
| A. A source fork or a build per profile | code, features and kernels | Refused: four trees to keep equal, a fix lands four times, the test matrix multiplies |
| B. Binaries built per machine (`target-cpu=native`) | instruction set | Refused: a release must run on any machine of its class; providers mask CPU flags |
| C. One codebase and one release per target; modules a profile does not run are left out by cargo features or not installed; kernels picked at run time by CPU feature detection; the profile itself a manifest | models, installed modules, limits | Chosen |

- The binaries keep the target's baseline ISA (`x86-64` for x86_64, `armv8-a` for aarch64).
  Only kernel functions carry `#[target_feature]` and are dispatched once at start, each with a
  portable fallback and a fuzz test proving the SIMD and portable paths bit-equal.
- Assembly or intrinsics are written only where a profile measured the hot spot, one kernel at a
  time; the rest stays plain Rust.
- The manifest `deploy/profiles/<id>.toml` names: the modules installed and their units, the
  model set (name, file, size, SHA-256, licence), thread counts, the units' `MemoryMax` and
  `CPUQuota`, and the profile's budgets (section 5). The installer writes the chosen profile id
  into `ulei.toml`; the shape of that key goes into C9 by its own ADR.

### 3. What degrades, in a fixed order

A smaller profile loses capability in this order and says so in `ulei status`:

1. Reranker (`max`, `mac` only).
2. Dense search: large embedder, then lexical only (`lite`, section 1.1).
3. Local speech to text, replaced by the cloud path of ADR-0035.
4. Local LLM, replaced by the cloud: the cheap task classes go to the subscription.
5. No LLM reachable at all (subscription or tokens exhausted, qb down, no local LLM): the
   command layer of section 3.1 answers the commands it knows; everything else is queued for
   when a route returns, and the owner is told so.

The local LLM is a qb route, never a module of this repository: qb already routes, caps spend
and fails over. The task classes it may take (triage, short summaries, classification) and the
route itself are a later ADR in qb; this ADR only reserves the place. ulei writes no LLM
engine: llama.cpp and MLX hold years of hand-tuned Metal and NEON kernels, and the dedicated
engines of section 2 pay off for small fixed-architecture encoders (speech to text, embedder,
reranker), not for LLMs.

#### 3.1 The command layer: a floor without any LLM

Every profile, `lite` included, carries a command layer that needs no LLM, no network and no
model process: a fixed set of the secretary's frequent commands (reminders, calendar entries,
"what is on today", inbox and status queries, stop and cancel) recognised and executed locally.
It is the floor of the ladder above: when tokens run out, the owner keeps the basic commands
instead of a silent bot.

How it works, after the reference design of asvedr's `servitor` (dialog layer) and `myneuro`
(NLU), both pure Go, read as a reference and ported to Rust on our own commands and tests, not
vendored; their licence is being requested from the author, and the credit goes into
`THIRD_PARTY_NOTICES.md`:

- Intent: logistic regression over a static word-embedding table (navec-class or a model2vec
  distillation trimmed to the command vocabulary, about 6 to 25 MB), a hierarchical router with
  a confidence threshold; typo repair by Damerau-Levenshtein over a trigram index is optional
  (about +15 MB on a 50k-word table).
- Slots: typed extractors (int, enum by stem prefix, remainder) and a deterministic ru/en
  date-time resolver ("tomorrow at 6 pm", a bare hour by policy), with follow-up questions for
  a missing slot.
- A yes/no model for confirmation before any action that changes state.
- The commands, their examples in both launch languages, slots, questions and confirmations
  are data (one spec), compiled offline into the model; CI runs its cross-validation and typo
  robustness and fails below a floor set from the first measurement.
- Cost in the reference: tens of microseconds per classification and single to double-digit MB
  of RAM; inside the `lite` total of section 5. To be measured on the Rust port.

Below the threshold the layer does not guess: with an LLM route it hands the message on
untouched; without one it says which commands it knows. Its date-time resolver and yes/no
confirmation may also serve the LLM path as deterministic tools (open question 5).

The layer is a deviation from the Hermes reference (root AGENTS.md: port, do not invent): its
crate, place in the channel or the engine, and command list are decided by their own ADR.

### 4. The hardware probe

`ulei doctor --hw` (in `crates/core`, the `ulei` binary) prints the machine and the highest
profile it meets, with the reason for each profile it does not:

- arch; CPU features: `avx2`, `fma`, `avx512f`, `avx512_vnni` on x86_64; `asimddp`
  (dot product), `i8mm` on aarch64; logical and physical cores, and on Apple the performance
  and efficiency core counts;
- total and available memory, free disk on the data directory;
- virtualisation and, on Linux, CPU steal over one second.

Sources: Linux `/proc/cpuinfo`, `/proc/meminfo`, `/proc/stat`, `statvfs`; macOS `sysctl`
(`hw.memsize`, `hw.optional.arm.FEAT_DotProd`, `hw.optional.arm.FEAT_I8MM`,
`hw.perflevel0.physicalcpu`, `hw.perflevel1.physicalcpu`). No network, no new dependency on
Linux (the reads are files); macOS `sysctl` goes through `rustix` or `libc` and is decided with
the macOS target (section 7). It stays inside the `ulei` row of ADR-0001 (1 MiB peak RSS).

### 5. Reference hosts and per-profile budgets

Each profile has a reference host, and its budgets are measured there and enforced as ADR-0001
enforces the daemon rows. Every run records `lscpu` (or `sysctl -a hw machdep`), memory and
steal beside its numbers, so a number is never detached from its machine.

| Profile | Reference host | How it is reached |
|---|---|---|
| `lite` | 2 vCPU / 4 GiB x86_64 KVM VPS; one AMD EPYC host without AVX-512 | rented by the hour, deleted after the run |
| `standard` | the pilot host's class (3 to 4 vCPU Intel, 8 GiB) and one 4 vCPU / 8 GiB arm64 VPS | an hourly x86_64 host of the same class, not the production host; an hourly arm64 host |
| `max` | 8 vCPU / 16 GiB x86_64 | by the hour |
| `mac` | Apple M3 Pro, 18 GB (functional stand-in); a Mac mini when one exists | owner's machine, by hand |

Paid runs are the owner's: an agent never starts a rented host, as in the codegen
orchestrator's rule for its paid stand.

Budget rows per profile (targets to be replaced by measurements before acceptance):

| Row | `lite` | `standard` | `max` | `mac` |
|---|---|---|---|---|
| Host total steady RSS of all ulei units and models | ≤ 256 MiB | ≤ 3 GiB | ≤ 8 GiB | ≤ 6 GiB without the LLM |
| Model files on disk | 0 (no models) | ≤ 1.5 GB | ≤ 3 GB | ≤ 3 GB without the LLM |
| Garden search p50, warm | measure | measure | measure | measure |
| Speech to text RTF, one thread | n/a (cloud) | ≤ 0.5 | ≤ 0.5 | ≤ 0.3 |
| Cold start of the model process to ready | measure | measure | measure | measure |

The daemon rows of ADR-0001 stay as they are for every profile.

### 6. Changing the profile

Text is canonical; vectors are a derived cache tied to the embedder's identity (ADR-0047
section 2's SHA-256). A profile change that changes the embedder re-embeds every node; the
probe prints the estimate (records × measured embed rate of the target profile) before the
owner confirms. The command and its resume after a crash are a later ADR with the garden.

### 7. Targets

| Target | Status |
|---|---|
| `x86_64-unknown-linux-musl`, static | today (ADR-0001) |
| `aarch64-unknown-linux-musl`, static | added by its own ADR: CI runner `ubuntu-24.04-arm`, release artifact, budgets |
| `aarch64-apple-darwin` | added by its own ADR: no static libc; launchd in place of systemd units; `getpeereid` in place of `SO_PEERCRED`; Landlock and systemd hardening replaced by separate users, file modes and the sandbox the platform has; signing and notarisation |

### 8. Not decided here

- Memory nodes per project, the secretary's read-only search across them, and the move of
  ummanu's memory into them (own ADR).
- The model service shared by the nodes and its contracts for embedding and speech to text
  (own ADR; reserves the next free contract ids).
- Packaging: Homebrew tap, signed apt repository, install script, build attestations (own ADR).
- The qb route for a local LLM and its task classes (qb).
- The command layer's crate, placement and command list (section 3.1).

### 9. First measurements (2026-11-24)

Run on the pilot host's class (3 vCPU Skylake, AVX2 and AVX-512F, no VNNI, 7.8 GiB) with the
ummanu stack stopped, and on an Apple M3 Pro laptop for speech. No owner data: public labelled
sets and a synthetic personal corpus. `garden-eval` built from this repository's `main` of the
day; the variants of 9.1 are local patches, not merged. Search latencies are from a host that
was not otherwise idle for the first runs and are an upper bound.

Data:
- MIRACL dev, ru and en (Wikipedia passages, human relevance labels): 300 sampled questions
  per language, their 857 (ru) and 841 (en) gold passages plus random passages of the same
  corpus up to 10^3, 10^4 and 10^5 records; a mixed corpus of both; 40 + 40 questions
  translated by hand for the cross-language case. Questions are bucketed by the share of
  their words (5-letter prefixes) present in the gold passage.
- Synthetic personal corpus: one fictional life (a studio founder, family, team, clients,
  2016 to 2026, 67 events) rendered as 144 chats, mails, notes, calendar entries and documents
  in both languages, 188 questions in both languages (`exact`, `morph`, `paraphrase`,
  `temporal`, `none`), categories checked by word overlap; template filler up to 10^5 records.
  The filler is too unlike the needles to show the volume effect; the set serves model and
  category comparison.

#### 9.1 Lexical search (no model)

| Corpus | Records | recall@3 / MRR | p50 / p95 |
|---|---|---|---|
| MIRACL ru | 10^3 / 10^4 / 10^5 | 0.81 / 0.94; 0.75 / 0.90; 0.57 / 0.73 | 4 / 13; 9 / 43; 56 / 359 ms |
| MIRACL en | 10^3 / 10^4 / 10^5 | 0.86 / 0.98; 0.83 / 0.95; 0.74 / 0.88 | 7 / 19; 33 / 90; 283 / 760 ms |
| mixed ru+en | 2 × 10^5 | 0.65 / 0.80 | 173 / 1016 ms |

English was slow because the query keeps English function words ("the", "a", "of") as prefix
terms of an OR query (the stop list is Russian only), each matching a large share of the
corpus. Variants on the same data:

| Variant | en 10^5: recall@3 / p95 | ru 10^5: recall@3 / p95 | synthetic en 10^3 recall@3 |
|---|---|---|---|
| as is | 0.735 / 760 ms | 0.570 / 359 ms | 0.54 |
| English stop words; tokens under 4 letters without prefix | 0.732 / 54 ms | 0.559 / 195 ms | — |
| + Snowball English stems (as the Russian ones) | 0.749 / 69 ms | — | 0.63 (`morph` 0.57 → 0.72) |
| Russian: no prefix where a stem exists | — | 0.517 / 129 ms | — |

Decided by these: English stop words and English Snowball stems go into the garden (card 7);
the Russian prefix term stays (dropping it lost 4 points); Russian latency is cut another way
(translit clause only for mixed scripts, very frequent terms dropped by `fts5vocab`; card 7).

#### 9.2 Embedders (hybrid = lexical + dense by RRF, ONNX Runtime 1.28.0, 2 threads)

MIRACL mixed, 2 × 10^3 records, 160 questions (80 same-language, 80 cross-language):

| Model | File | recall@3 | same ru / en | en→ru / ru→en | embed p50 per passage | peak RSS |
|---|---|---|---|---|---|---|
| lexical only | — | 0.49 | 0.82 / 0.91 | 0.12 / 0.10 | — | — |
| multilingual-e5-small int8 | 118 MB | 0.65 | 0.88 / 0.91 | 0.37 / 0.43 | 73 ms | 295 MB |
| multilingual-e5-base int8 | 279 MB | 0.64 | 0.87 / 0.91 | 0.44 / 0.32 | 183 ms | 531 MB |
| bge-m3 int8 | 570 MB | 0.77 | 0.87 / 0.91 | 0.59 / 0.70 | 569 ms | 891 MB |

On all 600 MIRACL questions (2 × 10^3) bge-m3 lifts the low-overlap questions from 0.62 to 0.96
(ru) and 0.80 to 0.98 (en); MRR 0.95 → 0.99.

Synthetic personal corpus, mixed, 10^3 records, 376 question rows:

| Model | recall@3 | same language | cross-language | paraphrase | temporal | embed p50 |
|---|---|---|---|---|---|---|
| lexical only | 0.40 | 0.64 | 0.25 | 0.17 | 0.40 | — |
| e5-small | 0.51 | 0.71 | 0.39 | 0.30 | 0.53 | 31 ms |
| e5-base | 0.50 | 0.72 | 0.38 | 0.32 | 0.52 | 85 ms |
| bge-m3 | 0.68 | 0.81 | 0.60 | 0.49 | 0.73 | 278 ms |

Read: on Wikipedia-style questions in one language the small model equals bge-m3; on personal,
paraphrased and time questions bge-m3 leads by about 17 points, and across languages by 20 to
30. e5-base buys nothing over e5-small. Indexing cost on this host: 0.57 s per passage for
bge-m3, so 10^5 records take about 16 hours on 2 threads; e5-small about 2 hours. Hence:
`standard` defaults to e5-small and offers bge-m3; `max` and `mac` take bge-m3; first indexing of
a large import runs at night and incrementally, and dense vectors may cover only the head of
long documents (`dense-head`, card 9). The synthetic set is small and its questions were
written by an LLM; the size of the gap is confirmed on a larger set before acceptance (card 5).

#### 9.3 Speech to text (Apple M3 Pro and the pilot host, 200 clips per set)

| Engine | FLEURS ru / en | Common Voice ru | SOVA ru | YouTube ru | calls ru | LibriSpeech other en | RTF 1 thread | peak RSS |
|---|---|---|---|---|---|---|---|---|
| GigaAM v3 ru int8 (pure Go, mmap) | 7.9 / — | 3.9 | 10.6 | 9.2 | 18.1 | — | 0.34 | 243 to 305 MB (dirty 20 to 80) |
| GigaAM multilingual int8 | 8.6 / 14.5 | 4.8 | 12.8 | 11.6 | 23.2 | 14.1 | 0.34 | as above |
| whisper large-v3-turbo f16, Metal | 4.3 / 4.9 | 6.8 | 14.2 | 15.9 | 26.7 | 6.0 | 0.07 (batch, GPU) | 1.9 GB |
| whisper large-v3-turbo q5_0, Metal | 4.7 / 5.0 | 6.6 | 14.8 | 15.8 | 26.8 | 6.2 | 0.07 | 0.84 GB |

On the pilot host (Skylake, AVX2 kernel; checked that the kernel dispatch sees AVX2), GigaAM v3
on FLEURS ru, one thread: WER 7.86 % (the same 298 errors as on the Mac: the kernels are
bit-equal), RTF 0.86 (1949 s for 37.7 min), peak RSS 292 MB. That is 2.5 times slower than
the Mac's SDOT path: a 10 s voice note takes about 9 s on one core. Opus, English and
multi-core runs on this host are still to do (card 11).

WER in %. On public conversational Russian GigaAM v3 leads by 3 to 9 points; whisper leads on
read sentences and by far in English. Opus at 24 kbit/s (Telegram voice) costs at most 0.35
points. GigaAM was probably trained on the training splits of these Russian sets (not
verified), so the deciding set is the owner's own speech:

Owner's voice notes, with consent, processed only locally on the owner's laptop: 25 notes
(8.5 min, 1045 words) of 66, reference transcribed by the owner from three anonymised
candidates. WER in %, filler words removed:

| Engine | all 25 | without the forwarded note (24) | without digits (18) |
|---|---|---|---|
| cloud: Groq whisper-large-v3-turbo with the vocabulary prompt (production today) | 6.8 | 6.1 | 5.0 |
| whisper.cpp turbo f16 with the same prompt, Metal | 7.6 | 6.7 | 8.0 |
| whisper.cpp turbo f16 without prompt | 8.4 | 7.3 | 6.3 |
| GigaAM v3 ru with VAD | 9.7 | 10.4 | 8.8 |

Bootstrap over notes (4000 resamples): GigaAM minus Groq +2.9 points, 95 % interval -1.2 to
+6.7; no difference is significant at 25 notes, GigaAM is probably worse. On the owner's speech
(technical vocabulary, mixed Latin terms, project names) the picture reverses the public sets;
77 of GigaAM's 101 word errors are ordinary Russian words. Whisper hallucinates subtitle phrases
("Спасибо за субтитры ...") about once per 1000 public clips and once in 66 owner notes, mostly
appended (a filter removes them) and rarely substituting a very short note; GigaAM, a CTC
model, cannot. Whisper.cpp's Silero VAD removes them at +2 points WER.

Hence: `mac` runs whisper turbo locally for both languages with the vocabulary prompt and a
subtitle-phrase filter, and cross-checks notes under about 15 s with GigaAM v3 (one core, about
300 MB; a filtered or strongly diverging whisper text takes GigaAM's text or marks the note).
VPS profiles run GigaAM v3 for Russian at about 3 points above the cloud, or keep the cloud
path by the owner's choice (privacy against quality, open question 7); English on VPS profiles
is open (open question 6).

#### 9.4 What a dedicated engine buys

The engines that would replace ONNX Runtime by the method of section 2 (own encoder, int8
weights by mmap, own int8 GEMM kernels) save RAM, start-up and deployment, not CPU time:
- the weights stay clean file pages, evicted when idle, shared between processes; the token
  table (about 250 MB of bge-m3's 570) is mostly never paged in;
- start-up is a mapping instead of a 25.3 s load;
- the helper becomes a static binary without the glibc exception of ADR-0047;
- indexing is not faster: ONNX Runtime's kernels are at least as fast as a hand-written engine
  (the Go STT engine is about 2.8 times slower than ONNX Runtime on one thread).

All three measured embedders are one architecture family (BERT / XLM-RoBERTa encoders with the
same 250k SentencePiece vocabulary, whose tokenizer the garden already has); they differ in
layer count and width, layer-norm epsilon, position offset and pooling (CLS or mean with
`query:`/`passage:` prefixes). One parameterised engine serves all of them; a port starts with
e5-small's golden tests and scales to bge-m3 (card 10). The RAM of the vector matrix is cut
independently by int8 vectors (bge-m3 at 10^5 records: about 400 MB float32, 100 MB int8;
card 8).

## Consequences

- One release per target serves every profile; profiles differ in manifests and model sets.
- Budgets gain a second axis: per binary (ADR-0001) and per profile per host.
- The probe gives the installer, `ulei status` and support one answer to "what can this
  machine run".
- Measurements on rented hosts cost money per run and wait for the owner.

## Open questions

1. Russian latency at 10^5 records (p95 about 200 ms after the stop words): are the translit
   restriction and frequent-term dropping of card 7 enough to reach the 150 ms deadline of
   ADR-0047 on a `lite` host?
2. `standard`: e5-small by default and bge-m3 on request (section 9.2), or bge-m3 by default
   once card 10 cuts its RAM? Decided after the larger synthetic set of card 5.
3. Is the local speech-to-text engine a ulei crate (Rust port of the GigaAM engine) or the
   existing Go engine as an external binary reached over the wire until the port exists?
4. Minimum macOS version.
5. Does the command layer (section 3.1) also run as a fast path in front of the LLM while the
   subscription is active (zero tokens for known commands), or only as the floor without an
   LLM? And does the LLM path call its date-time resolver and confirmation as tools?
6. English speech on VPS profiles: GigaAM multilingual is 14 to 15 % WER, whisper turbo on CPU
   costs about 2 GB and RTF 0.32 on 4 threads. Candidates to measure: smaller CTC/TDT English
   models (NVIDIA Parakeet class) and distilled whisper; until then English voice notes on VPS
   profiles stay on the cloud path.
7. Russian speech on VPS profiles: local GigaAM v3 (no data leaves the host, about 3 points WER
   worse on the owner's notes) or the cloud path (better, the audio goes to the provider) as the
   default? Re-measured on a larger set of owner notes if the owner wants.

## Cards

1. `ulei doctor --hw` on Linux: the probe of section 4, the profile mapping, tests on recorded
   `/proc` fixtures of the reference hosts.
2. `deploy/profiles/{lite,standard}.toml` and a check in CI that every model listed has a
   SHA-256, a size and a licence.
3. Bench harness: one script that runs the budget rows of section 5 on a given host and writes
   them with the host's `lscpu`, memory and steal; run on the pilot host's class by the owner.
4. `aarch64-unknown-linux-musl` release in CI (with its ADR).
5. `garden-eval` of candidate embedders (bge-m3 int8, multilingual-e5 base int8) on the
   `standard` reference hosts, and lexical against hybrid recall@k at 10^3, 10^4 and 10^5
   records for the `lite` limit text (section 1.1), each in Russian and English, the two
   launch languages (others later), with question categories `exact`, `morph`, `paraphrase`,
   `xlang` (ru to en and en to ru) and `none`. Sources: public labelled sets per language and a
   synthetic personal corpus generated from one fictional life rendered in both languages and
   mixed; no owner data. The tables go into this ADR before acceptance. First round done
   (section 9); still open: a larger synthetic set (about 500 needles, LLM-written filler that
   shows the volume effect), and the same runs on the `lite` and `standard` reference hosts.
6. Command layer: the list of 10 to 15 most frequent secretary commands with ru and en examples
   (wording only, no owner data), its cross-validation and typo robustness on a first port, and
   the ADR of section 3.1's last paragraph.
7. Garden lexical fixes (with their ADR): an English stop list beside the Russian one; tokens
   under 4 letters as exact terms; English Snowball stems in the `stems` column, a hand port as
   the Russian one (no new dependency); the translit clause only for queries with mixed scripts
   or names; very frequent terms dropped by their `fts5vocab` share. Re-run 9.1.
8. int8 vectors in the resident matrix and the store; recall loss and RSS measured on the 9.2
   sets.
9. Dense head and night indexing: `dense-head` sizes against recall on the 9.2 sets; the first
   import embedded at night and incrementally, with progress in `ulei status`.
10. Encoder engine port (section 9.4; detailed in ADR-0087): one parameterised BERT / XLM-R encoder, int8 weights by
   mmap, the int8 GEMM kernels of the reference Go engine ported; golden tests against ONNX
   Runtime layer by layer for e5-small first, then bge-m3; RSS dirty and clean, start-up and
   recall compared with 9.2.
11. Speech to text on the VPS class: GigaAM v3 FLEURS ru on one thread done (section 9.3); still
   to do on the pilot host: Opus, English (multilingual), all cores, one clip's start and RSS;
   the English candidates of open question 6; splitting one long note across cores, since the
   engine is single-threaded per file and a VPS core gives RTF 0.86.
12. Subtitle-phrase filter for whisper output (the ru/en phrase list of section 9.3, appended
   and whole-text cases) and the GigaAM cross-check of short notes on `mac`.
