# ADR-0013: `regex-lite` for the engine's `grep` tool

Status: accepted
Date: 2026-11-17
Affects: crates/engine, dep:regex-lite

## Context

The built-in tools of `ulei-engine` include `grep`, a regular-expression search over files.
std has no regular expressions. ADR-0010 lets each dependency of the engine in only with its
own record and holds the binary to the daemon budgets of ADR-0001.

## Decision

### 1. Dependency

| Crate | Features | Why |
|---|---|---|
| `regex-lite` 0.1 | `std`, `string` | The `pattern` of `grep`, with case folding. No dependencies of its own; MIT OR Apache-2.0; built for small code size rather than search speed, which the scan limits of `grep` bound anyway. |

Glob matching (`*`, `?`, `[...]`, `**`) for the `glob` tool and the `glob` filter of `grep` is
written on std.

### 2. Measured size

`x86_64-unknown-linux-musl`, release profile of ADR-0001, rustc 1.100.0, measured as in
ADR-0010 section 3:

| Build | Static size |
|---|---|
| `ulei-engine` with `grep`, `glob` and `regex-lite` in its library (static-pie, stripped) | 381,624 B |
| Same binary before them | 381,624 B |

The binary does not call the tools yet, so the linker drops them. A throwaway `main` that
dispatches one call through `Registry::builtin()` measured 570,152 B with four tools and
676,648 B with six: `grep`, `glob`, their directory walk and `regex-lite` together add
106,496 B once the loop calls the registry.

## Consequences

- `grep` patterns use the `regex-lite` syntax: no Unicode classes such as `\p{L}`, and
  `\w`, `\d`, `\s` match ASCII only.
- About 2.1 MiB of the static budget of ADR-0010 is left before the tool loop links the
  registry, about 2.0 MiB after.
