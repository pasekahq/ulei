# `ulei-engine` size

Where the bytes of the static musl `ulei-engine` go, how the binary grew, and which
reductions are known with their saving and risk. The static limit of the binary is
4,194,304 B (4 MiB) by the owner's decision (ADR-0053). None of the reductions below is
applied; each one that is adopted needs its own change and rewrites its row here.

All figures are for main `d5b7d89` unless a row names another commit.

## Measurement

The binary measured is the one CI's `budgets` job measures: the release profile of ADR-0001
with the package opt-level of ADR-0027, `x86_64-unknown-linux-musl`, rustc 1.100.0, GNU ld
2.42, the C compiler `x86_64-linux-musl-gcc` (a `gcc -specs` wrapper over the headers and
libraries of the distribution's `musl-dev` 1.2.4, as `musl-tools` installs it), `CARGO_INCREMENTAL=0`, `RUSTFLAGS="-D warnings"`:

```sh
cargo build --release --locked -p ulei-engine --bin ulei-engine --target x86_64-unknown-linux-musl
stat -c %s target/x86_64-unknown-linux-musl/release/ulei-engine
size -A target/x86_64-unknown-linux-musl/release/ulei-engine
```

At `d5b7d89` this gives 2,620,544 B, CI's figure. Two builds of the commit from a clean tree
are identical to the byte. File sizes move in steps of 4,096 B because the loadable segments
are page-aligned, so a change smaller than a page can measure as 0 B or as 4,096 B.

The attribution comes from the GNU ld map (`-Wl,-Map`) of the same commit. That link gives the
same file size, and its sections differ from the CI build by less than 100 B. Rust symbols are
demangled and assigned to the crate that owns the item (for a trait impl, the crate of the
implementing type when the trait is from `core`, `alloc` or `std`). Merged string and constant
sections (`.rodata.str*`, `.rodata.cst*`) carry no symbol, so their bytes are spread over
their input sections in proportion to the input sizes.

## Attribution

### Sections

| Part | Bytes |
|---|---|
| `.text` | 1,956,758 |
| `.rodata` | 254,796 |
| `.eh_frame` | 188,352 |
| `.rela.dyn` | 100,704 |
| `.data.rel.ro` | 63,024 |
| `.eh_frame_hdr` | 25,420 |
| `.data` | 17,584 |
| `.gcc_except_table` | 6,032 |
| `.got`, `.dynamic`, `.tdata`, init and fini arrays, `.plt`, notes, hash, `.comment`, `.shstrtab` | 2,222 |
| Section header table (26 entries) | 1,664 |
| ELF header and program headers | 624 |
| Alignment padding between segments | 3,364 |
| File | 2,620,544 |

`.rela.dyn` holds 4,196 relative relocations of 24 B each: the static-pie start code
relocates the binary to its load address. `.eh_frame` and `.eh_frame_hdr` are the unwind
tables of the Rust code (185,268 B of `.eh_frame`), libunwind (2,936 B) and the C runtime
objects (148 B); the C code is built without them (ADR-0001 section 8).

### By crate

Symbols of `.text`, `.rodata`, `.data.rel.ro`, `.data` and `.gcc_except_table`, 2,291,234 B;
another 6,964 B of those sections is alignment fill.

| Crate or object | Bytes |
|---|---|
| bundled SQLite (C) | 691,636 |
| `ulei_engine` | 593,700 |
| Rust anonymous constants and merged strings | 194,861 |
| `core` | 177,124 |
| `std` | 151,236 |
| musl libc (C) | 80,007 |
| libunwind (C/C++) | 60,829 |
| `serde_json` | 60,437 |
| `alloc` | 57,835 |
| `gimli` | 41,606 |
| `regex_lite` | 38,294 |
| `rustc_demangle` | 20,119 |
| `serde_core` | 18,121 |
| `hashbrown` | 16,867 |
| `rusqlite` | 16,111 |
| methods of primitive types (`core`, `alloc`) | 14,814 |
| `addr2line` | 12,051 |
| `sha2` | 12,032 |
| `ulei_contracts` | 10,723 |
| `miniz_oxide` | 9,937 |
| 11 others (`memchr`, `signal_hook`, `zmij`, `std_detect`, `rustix`, ...) | 12,894 |

`gimli`, `addr2line`, `rustc_demangle` and `miniz_oxide` (83,713 B) together with their
callers in `std` make up the backtrace symbolizer of the default panic hook, 166,269 B of
symbols (ADR-0027 section 4).

### `ulei_engine` by module

| Module | Bytes | Module | Bytes |
|---|---|---|---|
| crate root (`journaled`, `main`) | 65,045 | `tools::fuzzy` | 15,801 |
| `mcp` | 47,121 | `config` | 15,788 |
| `compress` | 38,953 | `skills` | 13,386 |
| `agent` | 38,452 | `journal` | 13,026 |
| `serve` | 38,107 | `gateway` | 12,653 |
| `tools::terminal` | 34,469 | `tools` | 10,800 |
| `tools::skills` | 29,066 | `policy::no_ask_guard` | 9,093 |
| `policy::hardline` | 23,466 | `policy::tool_guardrails` | 9,024 |
| `tools::search` | 21,246 | `tools::web` | 8,369 |
| `prompt` | 21,197 | `tools::difflib` | 8,262 |
| `tools::v4a` | 20,738 | `policy::state_db_guard` | 5,953 |
| `tools::memory` | 18,519 | `zone` | 5,473 |
| `tools::read` | 17,719 | `tools::json_object` | 4,555 |
| `tools::files` | 17,367 | 22 smaller modules, items outside a module | 14,214 |
| `tools::redact` | 15,838 | | |

An impl of a `core`, `alloc` or `std` trait for an engine type (`Debug`, `Drop`, `Iterator`,
`Deserialize`) counts under the type's module; a generic function counts under the crate that
defines it, whatever it is instantiated for.

### Largest symbols

| Bytes | Section | Symbol |
|---|---|---|
| 51,250 | `.text` | `ulei_engine::journaled` |
| 32,207 | `.rodata` | merged strings `.rodata.str1.1` |
| 31,264 | `.text` | `serve::Server::dispatch` |
| 26,456 | `.text` | SQLite `sqlite3VdbeExec` |
| 24,943 | `.text` | `compress::Compressor::compress` |
| 19,651 | `.text` | `agent::Agent::run` |
| 19,111 | `.text` | `std` backtrace `gimli::Cache::with_global` |
| 17,520 | `.text` | `prompt::build` |
| 17,499 | `.text` | libunwind `CFI_Parser::parseFDEInstructions` |
| 16,291 | `.text` | `tools::v4a::patch_v4a` |
| 14,379 | `.text` | `tools::search::SearchFiles::run` |
| 14,300 | `.text` | SQLite `sqlite3Select` |
| 14,274 | `.text` | `tools::skills::SkillView::view` |
| 13,561 | `.text` | SQLite `sqlite3Pragma` |
| 12,934 | `.text` | SQLite `sqlite3Parser` |
| 12,830 | `.text` | SQLite `sqlite3WhereBegin` |
| 12,813 | `.text` | `agent::read_unless_interrupted` |
| 12,456 | `.text` | `std` backtrace `gimli::Context::new` |
| 12,416 | `.text` | `ulei_engine::main` |
| 11,400 | `.text` | `tools::read::ReadFile::run` |

### Generic families

`.text` of Rust functions grouped by name with their generic arguments removed, three or
more instances:

| Bytes | Instances | Family |
|---|---|---|
| 80,418 | 14 | `<..>::run` (the tools) |
| 44,602 | 222 | `<..>::fmt` (`Debug` and `Display`) |
| 27,925 | 22 | `<..>::new` |
| 23,374 | 107 | `<..>::next` |
| 22,651 | 391 | `drop_glue` |
| 22,276 | 22 | `<..>::deserialize` |
| 20,979 | 12 | `<..>::definition` (tool schemas) |
| 14,256 | 11 | `__rust_begin_short_backtrace` (thread bodies) |
| 8,655 | 16 | `hashbrown` `reserve_rehash` |

`serde_json` deserialisation is instantiated over two readers: `StrRead` (43 symbols,
18,413 B) for `from_str` and `SliceRead` (71 symbols, 29,625 B) for `from_slice`.

## Growth by merge

The binary at each main merge from `633ba60` (2,144,384 B) to `d5b7d89` (2,620,544 B) that
changes the engine's inputs, built as above. Built merges with a 0 B delta are left out. Eleven
such merges were not built on their own; their bytes are in the next built merge's delta, which
is 0 B for all but `9ee9d5b`, marked with +.

| Merge | Size | Delta | Change |
|---|---|---|---|
| `09313e9` | 2,168,960 | +24,576 | no-ask-guard policy (ADR-0023) |
| `9ee9d5b`+, `3deece7` | 2,214,016 | +45,056 | tool guardrails (ADR-0024) |
| `d313241` | 2,218,112 | +4,096 | compressor, default path (ADR-0025) |
| `86b468e` | 2,337,408 | +119,296 | compaction triggers (ADR-0025) |
| `3c32fa8` | 2,509,568 | +172,160 | MCP client, session and tools (ADR-0026) |
| `db5d1c9` | 2,530,048 | +20,480 | MCP client, reconnect (ADR-0026) |
| `c977f8b` | 2,276,096 | -253,952 | opt-level "z" and the shared sort (ADR-0027) |
| `1d424af` | 2,312,960 | +36,864 | C6 server, `serve` and `submit_turn` (ADR-0028) |
| `e1be3eb` | 2,329,344 | +16,384 | C6 server, `interrupt` (ADR-0028) |
| `294785d` | 2,341,632 | +12,288 | C6 server, `inject` (ADR-0028) |
| `6c27be2` | 2,353,920 | +12,288 | C6 `turn_events` (ADR-0031) |
| `83096a3` | 2,366,208 | +12,288 | inbox turns and `report_delivery` (ADR-0036) |
| `5ff1d4b` | 2,374,400 | +8,192 | durable inject delivery record (ADR-0037) |
| `815ab82` | 2,378,496 | +4,096 | C4G vendored from qb (ADR-0038) |
| `fdf1e08` | 2,382,592 | +4,096 | the `ulei.toml` decoder in `ulei-contracts` (ADR-0040) |
| `37f691c` | 2,390,784 | +8,192 | the engine reads `ulei.toml` through it (ADR-0040) |
| `4efb322` | 2,526,336 | +135,552 | FTS5 compiled into the shared bundled SQLite (ADR-0044) |
| `3f1a07e` | 2,530,432 | +4,096 | a panic becomes a visible turn failure |
| `156cad0` | 2,534,528 | +4,096 | C7 0.3.0 and C9 0.2.0 (ADR-0047) |
| `fcc57c9` | 2,542,720 | +8,192 | journal version 6 (ADR-0051) |
| `c139bfc` | 2,546,816 | +4,096 | prompt cache mode in requests |
| `5b7573d` | 2,575,488 | +28,672 | `memory` tool, writes |
| `152881e` | 2,579,584 | +4,096 | `memory` tool, drift guard |
| `01b5fc8` | 2,600,064 | +20,480 | hardline floor, patterns and policy (ADR-0016) |
| `d5b7d89` | 2,620,544 | +20,480 | hardline floor, detection variants (ADR-0016) |

The same growth by contributing change:

| Change | Delta |
|---|---|
| MCP client | +192,640 |
| FTS5 in bundled SQLite | +135,552 |
| Compaction | +123,392 |
| C6 server and `turn_events` | +77,824 |
| Tool guardrails | +45,056 |
| Hardline floor | +40,960 |
| `memory` tool | +32,768 |
| No-ask-guard | +24,576 |
| Owner inbox delivery | +20,480 |
| `ulei.toml` decoder | +12,288 |
| C4G vendoring and prompt cache mode | +8,192 |
| Journal version 6 | +8,192 |
| C7 and C9 contracts | +4,096 |
| Panic as turn failure | +4,096 |
| Opt-level "z" and shared sort | -253,952 |
| Total | +476,160 |

Per added non-blank, non-comment line of `crates/engine/src` and `crates/contracts/src`, the
measured changes cost 25 B (hardline floor) to 73 B (tool guardrails), 42 B to 46 B for
compaction, the `memory` tool and no-ask-guard; in-file tests are among the counted lines.

## Applied

- Opt-level "z" for the package `ulei-engine` and the shared sort (ADR-0027). The package at
  "s" instead measures 2,841,728 B, so "z" accounts for 221,184 B.
- C code without unwind tables, `panic = "abort"`, fat LTO, one codegen unit, `strip`
  (ADR-0001 section 8).
- Bundled SQLite without the optional modules no ulei SQL uses (ADR-0007 section 2).

## Reductions found

Measured means a release build as above with the change applied to a scratch copy; the
saving is against the base the row names. "cand" is the build with R1 and R2 applied,
2,505,928 B.

### R1. Packed relative relocations

Link the musl `ulei-engine` with `-Wl,-z,pack-relative-relocs`, from a build script
`crates/engine/build.rs` that prints
`cargo::rustc-link-arg-bin=ulei-engine=-Wl,-z,pack-relative-relocs` when
`CARGO_CFG_TARGET_ENV` is `musl`. A build script is the way that survives CI's global
`RUSTFLAGS`, which override target rustflags.

- Saving, measured: -98,232 B against `d5b7d89` (2,522,312 B). `.rela.dyn` (100,608 B) becomes
  `.relr.dyn` (1,256 B).
- Touches: a new `crates/engine/build.rs`; `ulei-web` and the other binaries are not linked
  with it.
- Risk, low: the linker must know the option (GNU ld 2.38 or later) and the loader must apply
  `DT_RELR`. A static-pie musl binary has no `PT_INTERP`; it relocates itself in `_start_c` of
  `rcrt1.o` (musl's `dlstart`). Rust's self-contained musl for this target is 1.2.5, whose
  `dlstart` applies `DT_RELR` (supported since musl 1.2.4); its `_start_c` reads dynamic tags
  up to `DT_RELR` (36). A musl older than 1.2.4 would start with unrelocated pointers. The
  binary built this way gives the same output as `d5b7d89` for `--version`, a missing config
  file, a missing `qb.socket` and an unknown command; no test suite was run on it.

### R2. One `serde_json` reader

Parse with `serde_json::from_slice(text.as_bytes())` in place of `serde_json::from_str(text)` at
the 9 call sites that use `from_str` (`journal.rs` 4, `policy/tool_guardrails.rs` 2,
`policy/no_ask_guard.rs`, `mcp/client.rs`, `compress/text.rs`), so that the deserialiser is
instantiated over `SliceRead` only.

- Saving, measured: -16,384 B against `d5b7d89` (2,604,160 B; `.text` -12,432 B, `.eh_frame`
  -2,504 B). With R1 applied as well the saving is the same, -16,384 B (cand against R1), so
  R1 and R2 add up to -114,616 B.
- Touches: 5 files of `crates/engine/src`, 15 lines added, 9 removed.
- Risk, low: every input is a `&str`, so it is valid UTF-8 and parses to the same value or the
  same error. A new `from_str` call brings the `StrRead` instantiation back; nothing enforces
  the rule.

### R3. No position-independent executable

Build with `-C relocation-model=static`: a static, non-PIE binary.

- Saving, measured: -102,904 B against `d5b7d89` (2,517,640 B); -4,672 B against cand
  (2,501,256 B). R1 takes almost all of it.
- Touches: the rustflags of the release build of `ulei-engine` (CI, `ci/release.sh`).
- Risk: the engine loses ASLR. A rustflag is overridden by CI's global `RUSTFLAGS`, so it needs
  a change of every build that produces the release binary. An owner's reserve.

### R4. No unwind tables

Discard `.eh_frame` and `.eh_frame_hdr` of everything but the C runtime objects
(`crtbegin*.o`, `crtend*.o`) with a full linker script derived from `ld -pie --verbose`, and
link with `--no-eh-frame-hdr`.

- Saving, measured: -213,000 B against `d5b7d89` (2,407,544 B); -208,904 B against cand
  (2,297,024 B).
- Touches: a linker script and the linker arguments of `ulei-engine`.
- Risk: with `panic = "abort"` nothing unwinds, but the default panic hook can no longer walk
  the stack, so `RUST_BACKTRACE` shows no frames; the panic message and its location stay. The
  linker script is a copy of the default script of one binutils version and has to follow
  the linker. `-C force-unwind-tables=no` does not do it on stable (ADR-0027 section 4). An
  owner's reserve.

### R5. Helper binary for the MCP client

Move the MCP client (`mcp`, its HTTP and SSE code) into a helper process the engine reaches
over a local wire, as `ulei-web` is for the web tools (ADR-0015).

- Saving: measured -94,336 B for the engine without the client (cand with `mcp::connect`
  replaced by an empty session list, 2,411,592 B). Estimated net -75,000 to -85,000 B: the
  engine side of the wire adds back about as much as a C7 client does (below, +20,480 B).
- Touches: a new binary, a wire between it and the engine (ADR), its budgets, the deployment.
- Risk: a second process with its own RSS while MCP servers are connected; a new boundary and
  failure mode; a layout that differs from the reference's single process, which needs a
  recorded deviation. An owner's reserve.

The same cut for compaction saves 4,096 B (cand with the compressor left out of `run`,
2,501,832 B): the compressor stays reachable from its other callers.

### R6. More SQLite compile-time omissions

Add to `LIBSQLITE3_FLAGS` in `.cargo/config.toml`: `SQLITE_OMIT_DEPRECATED`,
`SQLITE_OMIT_SHARED_CACHE`, `SQLITE_OMIT_PROGRESS_CALLBACK`, `SQLITE_OMIT_DECLTYPE`,
`SQLITE_DEFAULT_MEMSTATUS=0`, `SQLITE_OMIT_COMPLETE`, `SQLITE_OMIT_GET_TABLE`,
`SQLITE_OMIT_TRACE`, `SQLITE_OMIT_TCL_VARIABLE`, `SQLITE_OMIT_UTF16`,
`SQLITE_OMIT_COMPILEOPTION_DIAGS`, `SQLITE_OMIT_AUTHORIZATION`.

- Saving, measured: -12,416 B against `d5b7d89` (2,608,128 B). `SQLITE_OMIT_INCRBLOB` does not
  link: `rusqlite` calls `sqlite3_blob_*`.
- Touches: the workspace-wide `LIBSQLITE3_FLAGS`, so `ulei-inbox` and `ulei-garden` change too
  (not measured).
- Risk: SQLite does not support most `OMIT` options in the amalgamation build; the build only
  links, and no test suite was run on it. A `rusqlite` API that calls an omitted function
  stops linking.

### R7. Opt-level "z" for the whole profile

- Saving, measured: -12,288 B against cand (2,493,640 B).
- Touches: `[profile.release] opt-level` in `Cargo.toml`.
- Risk: puts the cold start, syscall, publish p99 and ready budgets of `ulei` and `ulei-inbox`
  at stake for binaries that do not need the room (ADR-0027 section 4). An owner's reserve.

### R8. Tool schemas from static JSON text

Build the input schemas of the 12 `definition` functions from one `&'static str` of JSON each,
parsed once, instead of `json!` literals.

- Saving, estimated and not prototyped: at most 20,979 B, the `.text` of the `definition`
  family, less the parse calls and the schema text, which then sits in `.rodata`.
- Touches: the tool modules under `crates/engine/src/tools` and `mcp/tools.rs`.
- Risk: a malformed schema fails when the tool registers instead of at compile time; the
  schema text must stay identical to the reference's.

### R9. No backtrace symbolizer

- Saving, estimated: about 166,269 B, the symbols of the symbolizer (ADR-0027 section 4).
- Risk: needs nightly `panic = "immediate-abort"`; not available on the stable toolchain.

## Room for planned work

At `d5b7d89` the binary has 1,573,760 B of room under its limit. A C7 `search` client (the
generated types, one call over the gateway's HTTP client, the result as tool JSON) measures
+20,480 B (+17,488 B of sections) on cand. At the highest measured rate of 73 B per line, the
room holds about 21,500 more lines of engine code. No per-item projection of the remaining
cards of ADR-0051 section 8 is made here.
