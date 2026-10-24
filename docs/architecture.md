# Architecture

TRUST is a Typst-to-Rust compiler. It consumes a Typst package — `.typ`
sources plus `typst.toml` — and emits a Rust crate that is behaviorally
compatible with the original package. This document records the decisions
that are stable across development: the value-model contract, the
compilation layers, the host-integration stance, and the verification
methodology.

## Value-model fidelity

Generated code introduces no private value model. Every value observable by
Typst code — function arguments, arrays, dictionaries, content, closures —
is a `typst-library` value (`Value`, `Func`, `Content`, `Scope`), and every
call goes through the typst call protocol (`Args` + `Func::call`). The
compilation target is the interpreter's per-call constant — scope-stack
maintenance, dispatch, span bookkeeping — not Typst's value semantics. This
mirrors Nuitka: Nuitka compiles Python to C, but the result still drives the
CPython object model instead of replacing it.

Consequences:

- Dynamic typing, copy-on-write arrays/dictionaries, and join semantics are
  inherited from `typst-library`, not reimplemented.
- Interop holds by construction: a translated module mixes with interpreted
  code, interpreted closures, and other packages in the same compilation.
- Spans are carried into the generated code, and runtime error messages are
  reproduced verbatim, because downstream packages assert on error text.

## Compilation layers

- **L0 — direct translation.** Each top-level `#let` becomes a Rust function
  operating on `Value` end to end. Control flow, operators, and binding
  rules compile to Rust; calls are constructed as `Args` and dispatched
  through `Func::call`. This layer alone removes the interpreter's per-call
  constant and is the carrier of correctness.
- **L1 — typed internals.** Values that are provably machine-internal (for
  example scalar geometry) are promoted to native types (`f64`, plain
  structs). The rule: user-visible values stay `Value`, machine geometry
  moves into structs. Translation cost is paid only where a closure actually
  reads a field, not on every operation.
- **L2 — cross-procedure optimization.** Inlining, unboxing, style
  generation caching, inlining of wasm leaf math. Each optimization ships
  only under oracle-comparison protection (see below).

Optimizations never change the semantic contract: when a pass rate
regresses, the offending layer is disabled and the corpus is re-compared
against L0.

## Host integration: unmodified typst

Upstream typst will not link native implementations of individual packages,
so a translated package ships as a crate that the host links into its own
build — the host build is the distribution vehicle. What TRUST removes is
the fork: where the hand-translated prior art required patching typst
itself, TRUST reaches the same integration on an unmodified typst 0.15.1
through public APIs only:

- global module injection into `library.global` (with a rebuilt `std`
  binding), and
- package virtualization: the World serves a virtual manifest and a shim
  entry for the target spec, so the native module is imported as if it were
  the original package version.

Each generated crate exposes `module()` (export-surface assembly),
`module_init` (per-build engine-value initialization), and
`install(world, library)` (the one-line host entry point). The contract for
a Rust host is: link the crate, call `install`, swap in the returned
library. Details in [host-integration.md](host-integration.md).

## Translation pipeline

1. Load the package: `typst.toml`, entrypoint, import graph.
2. Build the module graph (parsed with `typst-syntax`, pinned to the same
   version as the semantics reference, `typst-eval`) and derive the export
   surface: the name-by-name list of bindings the module must provide.
3. Generate one Rust function per top-level `#let`: signature analysis
   (positional/named/default/sink parameters) mapped to the typst calling
   convention, body translated expression by expression.
4. Pre-construct module-level constants as `LazyHash` values.
5. Emit `module()` / `module_init()` / `install()` plus crate scaffolding.

The export surface is machine-checked: every top-level `#let` in the source
package must have a same-named binding in the output; a gap is a compile
error. Anything the translator cannot express is a compile error carrying
the source location and the missing construct — no silent fallback, no
approximate codegen.

## Verification: differential, two tiers

Correctness is established by comparing translated programs against the
Typst interpreter, never against hand-written expectations.

- **Process verification (in `cargo test`).** Small self-built fixtures, one
  per language construct, run twice — interpreted by `typst-eval` and
  compiled by `trust` — and compare value by value. Target packages are
  deliberately absent from this loop: a daily loop coupled to the heaviest,
  most version-sensitive corpus invites overfitting. Runtime shims
  (parameter destructuring, join, call protocol) carry their own equivalence
  tests against `typst-eval` behavior.
- **Acceptance verification (external harness).** The official typst
  `tests/suite` corpus and real packages compile two ways through one
  binary — native registration versus the original interpreted sources —
  and compare: rendered SVG/PDF page by page, byte for byte, and
  diagnostics as sets of messages with positions stripped.

## Workspace layout

| Crate | Role |
|---|---|
| `crates/trust` | Translator: `typst-syntax` frontend, package/module graph, Rust codegen |
| `crates/trust-runtime` | Runtime support linked into every generated crate: calling convention, parameter destructuring, closures, join semantics, span handling |
| `crates/trust-world` | Injection plumbing: module injection, package virtualization, the differential-test World |
| `crates/trust-diff` | Differential harness: fixtures translated at build time and compared against `typst-eval` |
