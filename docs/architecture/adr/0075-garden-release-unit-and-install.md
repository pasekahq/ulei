# ADR-0075: `ulei-garden` and `ulei-garden-embed` in the release, the garden's user unit

Status: accepted
Date: 2026-11-26
Affects: crates/garden, deploy/systemd, deploy/install.sh, ci/release.sh, ci/install_run.py, ci/units_run.py, .github/workflows/ci.yml
Amended by: ADR-0104 (section 1: the released helper is the static musl build of the package `ulei-garden-embed` with `native`; the glibc build with `onnx` is not released).

## Context

The garden daemon `ulei-garden serve` (C7, ADR-0047) and its embedder helper `ulei-garden-embed`
(ADR-0047 section 1) are built and held to their budgets in CI, but they are not released, the
install script does not know them, and there is no unit. A deployment runs the daemon as a
hand-made transient unit, which a reboot loses. Before the engine can see the garden
(issue:76b6f0e180339e492edd), the garden has to be a shipped service like the archive
(ADR-0070).

## Decision

### 1. Release and install

`ci/release.sh` adds two binaries, each from the build line of the budgets job:

- `ulei-garden`, static `x86_64-unknown-linux-musl` without `onnx` (`-p ulei-garden --bin
  ulei-garden`), checked static like the others.
- `ulei-garden-embed`, static `x86_64-unknown-linux-musl` with the encoder engine
  (`-p ulei-garden-embed --features native`, ADR-0104), checked static like the others.

Both get their `SHA256SUMS` lines and go through the rebuild comparison of ADR-0042 section 10;
the artifact keeps its name. `deploy/install.sh` puts both into `$HOME/.local/bin` and
`ulei-garden.service` beside the other units, and prints `systemctl --user enable --now
ulei-garden.service` as a step of its own, to run once `ulei.toml` has `[garden]`.
`ci/install_run.py` expects them.

The helper needs on the host what the release does not ship: the model directory holding the
encoder engine's weight file `model.ulei-infer`, which `ulei-garden-embed convert` writes from an
fp32 checkpoint (ADR-0087 section 2). The model is not downloaded by the daemon or the install
script; `docs/runbooks/garden-service.md` names where it goes and the keys that point at it.

### 2. One unit for the daemon and its helper

The daemon starts the helper as its child over a pipe (`embed::supervisor`), so one unit
`deploy/systemd/ulei-garden.service` covers both: the helper runs in the unit's cgroup, sandbox
and limits, and the unit's stop ends both.

| Item | Value | Why |
|---|---|---|
| `ExecStart` | `%h/.local/bin/ulei-garden serve`, `Type=exec` | The ready line `ulei-garden ready <socket>` is on stdout; the binary speaks no readiness protocol. |
| Order | `After=ulei-inbox.service`, no `Wants=` or `Requires=` | The inbox creates `producer.key` at its first start; a miss waits in the outbox while the inbox is down, so the garden runs without it. |
| Writable | `ReadWritePaths=%S/ulei/garden %t/ulei`, `StateDirectory=ulei/garden`, the shared `RuntimeDirectory=ulei` with `RuntimeDirectoryPreserve=yes` | `garden.store` and `garden.core_file` lie under `%S/ulei/garden`; `garden.socket` under `%t/ulei`, beside `inbox.socket` and `engine.c6_socket`, the one directory of `/run/user/<uid>` the engine's system unit shows (ADR-0069 section 2), so the engine reaches the garden there. |
| Read | `ProtectSystem=strict`, `ProtectHome=read-only` | The producer key in the inbox root, the helper, the model directory and the ONNX Runtime library are read where `ulei.toml` names them; no drop-in is needed for them. A `core_file` outside the state directory needs its directory in a drop-in. |
| Network | `RestrictAddressFamilies=AF_UNIX` | Its own socket and the inbox's; the helper speaks over a pipe. |
| Limits | `MemoryMax=3G`, `TasksMax=32` | Section 3. |
| Restart | as ADR-0042 section 6, `RestartPreventExitStatus=2` | Section 4. |

The hardening is ADR-0042 section 8 with `MemoryDenyWriteExecute=yes`: the CPU kernels of ONNX
Runtime are compiled ahead of time and its `dlopen` maps no writable code. Offline exposure 5.5
on systemd 255, the threshold of the `systemd-units` job, as `ulei-memory.service`.

`deploy/systemd/system/client-no-userns.conf` (ADR-0069 section 2) applies to
`ulei-garden.service` too on a host whose engine runs as the system unit: in a user namespace it
could not connect to `inbox.socket`, bound in another mount namespace, so it publishes no miss.

### 3. Memory and tasks

Without the embedder the daemon stays within its 4 MiB VmHWM budget (ADR-0047 section 7). With
it, the unit's cgroup holds the helper's model, which no CI run measures yet (the helper's peak
RSS is fixed after the first real run, ADR-0050), so the limit is reasoned:

| Part | Size |
|---|---|
| bge-m3 int8 weights, resident in the helper | about 570 MB, the file's size |
| A second copy while ONNX Runtime optimises the graph at load, freed after | up to 570 MB |
| Activations of one batch, 8 inputs of 512 tokens (`embed::onnx`), 24 layers of width 1024 | about 0.3 to 0.5 GB in the arena |
| The tokenizer of `tokenizer.json`, the helper's own heap | about 0.1 GB |
| The daemon's resident matrix, 4 KiB per vector | 0.2 GB at 50,000 vectors |

That is about 1.9 GB at its peak; `MemoryMax=3G` leaves the page cache of the model file and the
store room, which the cgroup charges and reclaims. A run of `docs/runbooks/garden-gold.md` step 7
measures the helper's peak; a change of this limit follows that measurement.

`TasksMax=32`: the daemon's main, worker and pipe threads (a second pipe thread while a restarted
helper starts), the helper's main thread, one inter-op thread and `garden.embedder.threads`
intra-op threads (default 2), with room for that setting up to about 24.

### 4. Exit 2 on a refused config

`ulei-garden serve` exits 2 when its config is refused, as the engine and the archive do
(ADR-0069 section 3, ADR-0070), so a typo is not restarted for ever. One refusal stays exit 1: a
`garden.producer_key_file` that does not exist, since the inbox creates it at its first start and
a first start of all units may start the garden first; the unit restarts it until the key is
there. A key file that exists and is refused (mode, owner, length, symlink) is exit 2. A failed
open of the store, bind, render of the core profile, helper start or `accept` stays exit 1 and
restarts.

### 5. Without the model

The daemon needs neither the helper nor ONNX Runtime nor the model while `garden.embedder.enabled`
is false, the default: `health` answers `dense: off` and `search` and `pointers` are lexical.
The unit starts so, and `docs/ulei.example.toml` keeps the embedder keys commented. With the
embedder enabled, a helper that does not get ready still stops the start (ADR-0047 section 2):
the daemon never starts lexical-only when the model is enabled, and the unit restarts it after
2 s, doubling up to 60 s, while the journal names the cause. The runbook enables the embedder
only once the library and the model are in place.

### 6. CI

The `systemd-units` job's exposure step gives `ulei-garden.service` the threshold 5.5, and
`systemd-analyze verify` covers it with the others. `ci/units_run.py` starts it with the other
units, without the embedder, and checks that it runs, answers C7 `health` with `dense: off` and
answers a `search`, that its ReadWritePaths hold exactly the store, core profile and socket the
config names, which the daemon wrote, its NoNewPrivs, seccomp filter and cgroup limits, and a
clean stop. The helper with the real ONNX Runtime stays in the `garden-onnx` job (ADR-0047
section 8).

Rejected:
- A second unit for the helper with a socket between them: the daemon starts, supervises and
  restarts the helper over its pipe (ADR-0047 section 2), and a second unit would duplicate that
  supervision.
- Starting lexical-only when an enabled helper does not get ready: ADR-0047 section 2 chose a
  visible failure over a silent loss of the dense side; this ADR does not change it.
- Shipping ONNX Runtime or the model in the release: the library is a third-party binary of
  8 MB and more, the model 570 MB; both stay the deployment's (ADR-0047 section 10).

## Consequences

- The release has eleven binaries, all static; `deploy/install.sh` installs seven units.
- A deployment replaces its hand-made unit with `ulei-garden.service` and keeps its store
  (`docs/runbooks/garden-service.md`).
- The engine step of issue:76b6f0e180339e492edd (the core profile in the prompt, the pointers and
  the tools) is the next change; this one does not touch the engine.
- No new dependency and no contract change.
