# Host Integration

How a host program — any Rust program that already embeds typst, such as
typst-cli or an application with its own compile loop — integrates a
translated package crate. The contract below is stated as facts about the
generated code; `InjectWorld` (the test World in `trust-world`) is one
implementation of the host-side plumbing.

## Integration protocol

The generated crate is the only artifact. Its public surface:

- `pub fn module() -> Module` — the package's export surface, bound name by
  name exactly as the original entry file defines it.
- `pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module>` — per-build assembly: top-level values that need an
  engine are parked in lazy slots during translation and registered here.
- `pub fn install(world: Tracked<dyn World>, library: &LazyHash<Library>) -> SourceResult<Module>` — the one-line host entry point; equivalent to
  `trust_runtime::init_module(world, library, module_init)`.

Two primitives in `trust-runtime` (linked by every generated crate)
implement the protocol:

```rust
/// Drives a ModuleInit: builds an Engine (EmptyIntrospector, fresh Sink,
/// fresh Route) and Context::none. comemo memoizes on (world read
/// constraints, library content, init fn), so the module re-evaluates
/// automatically when its inputs change; re-filling a slot with the same
/// value is idempotent.
pub fn init_module(
    world: Tracked<'_, dyn World>,
    library: &LazyHash<Library>,
    init: ModuleInit,
) -> SourceResult<Module>

/// Binds the module into library.global (bind, not define, so an existing
/// definition can be replaced), rebuilds the std binding (std holds another
/// Arc of global; scope copy-on-write would fork the two apart), and
/// rewraps LazyHash so comemo caching observes the change. The host swaps
/// the returned library into its World.
pub fn bind_module(
    library: &LazyHash<Library>,
    name: &'static str,
    module: Module,
) -> LazyHash<Library>
```

The minimal contract for a Rust host is therefore two steps: link the crate,
and inject the module into a library the host constructs. Every typst
embedding constructs a `Library` somewhere; that is the natural mount point.
`InjectWorld::init_native_module` is a thin wrapper over these two
primitives, kept for test convenience.

## Modules are self-contained after injection

State travels with the function values (per-build state slots), not in a
global registry. Once `install` returns, subsequent calls run under the
host's own real engine during realization and layout. The host keeps its own
`World` — disk access, package resolution, font discovery, and incremental
compilation are untouched by the injected module.

## Spans are self-contained

Span numbers interned in the translation process are meaningless in the
host process, so generated code never relies on absolute span ids. Span
constants are lazy: on first use the runtime interns the original file path
in the current process (`trust_runtime::source_span(path, position)`),
keeping the position number, which is deterministic for identical text.
The host has no interner obligations of any kind.

## What the host World must serve

Any production World already provides all of this; the list exists because
the test World implements each item explicitly:

- Files below the project root that the package reads at runtime — wasm
  plugins, data files. (`InjectWorld` falls back to the directory given by
  `TRUST_WORLD_ROOT`.)
- External packages that are not part of the translated crate:
  `InjectWorld` resolves them from the standard package cache layout
  (`~/.cache/typst/packages`).
- Additional source files the host program itself imports
  (`InjectWorld::add_source`).
- Fonts: `InjectWorld` starts with zero fonts; inject explicitly, e.g.
  `set_fonts(typst_assets::fonts())`.
- `sys.inputs`: baked into the library at construction
  (`InjectWorld::with_inputs`); per-build slots keep two builds with
  different inputs isolated.

## Package virtualization

For hosts that resolve packages through their World rather than through
library injection, `InjectWorld` demonstrates package virtualization: when
the evaluator asks for `@ns/name:ver`, the World serves a virtual manifest
and a shim entry (`#import name: *`), so the native module is imported
exactly as if it were the original package version. Specs that are not
virtualized resolve through the normal file and package sources.

## Limitations

- The generated artifact is a Rust crate. There is no dylib or wasm
  loadable form; non-Rust hosts — a prebuilt typst-cli binary, for example —
  cannot load it.
- Crossing a dynamic-loading boundary with `Value`/`Engine` semantics would
  require a separately designed safety boundary; no such form is provided.
