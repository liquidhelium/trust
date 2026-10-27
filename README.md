# trust - translator, from <ins>T</ins>ypst to <ins>RUST</ins>

`trust` is a translator from Typst to Rust. It can turn a single `.typ`
file into a standalone program, or a Typst package into a Rust crate.

The program translator (`trust translate`) emits a self-contained cargo
project for the given file — optionally with its `@spec` dependencies
merged into the translation (`--with-deps`) — and, with `--compile`,
builds it with cargo in one step. The resulting program renders the
document with no typst toolchain at run time.

The package translator (`trust translate-pkg`) consumes a package
directory (`typst.toml`, entrypoint, import graph) and emits one Rust
crate per package. The generated crate exposes `module()` (the export
surface of the original entry file, bound name by name), `module_init`
(per-build initialization of top-level engine values), and
`install(world, library)` (injection into a host).

The goal of trust is to improve runtime performance while maintaining
100% compatibility with Typst scripts. See [How it works](#how-it-works).

## How it works

Inspired by Nuitka — which compiles Python by translating it into C++
that calls the same C API the interpreter itself uses — trust applies the
same idea even more directly, because the Typst compiler is itself a Rust
program: generated code drives the very libraries the interpreter runs
on, `typst-library`'s `Value`, `Func`, and `Content`, and the same
layout and export pipelines. The output is not a reimplementation of
Typst — it is the same program in compiled form, which is what this
route's 100% compatibility rests on.

## Workspace layout

| Path | Contents |
|---|---|
| `crates/trust` | The translator: `typst-syntax` AST frontend → Rust code generation |
| `crates/trust-runtime` | Runtime support for generated code: parameter destructuring, call protocol, closure construction, content/markup construction |
| `crates/trust-world` | Injected World: global module injection, package virtualization, the `module_init` engine-value protocol |
| `crates/trust-diff` | Differential testing: fixtures translated at build time, compared case by case against the `typst-eval` interpreter |
| `crates/trust-suite` | Official typst `tests/suite` comparison harness: vendored corpus, per-case differential against the interpreter, known-divergence gating |
| `docs/` | Architecture and host-integration documents |

## Build

```sh
cargo build
```

Tests run offline, with no external dependencies:

```sh
cargo test
```

Performance comparison between the interpreter and compiled fixtures:

```sh
cargo test -p trust-diff --release --test bench -- --ignored --nocapture
```

## CLI

Translate a single file into a standalone program — a self-contained cargo
bin project that renders the document to PDF/SVG/PNG with no typst
toolchain, package cache, or font discovery at run time (evaluation is the
translated module; layout and export are borrowed from the typst crates):

```sh
trust translate <input.typ> [<out-dir>] [flags]
# default out dir: ./<stem>-program/
```

- `--compile` — also run `cargo build --release` on the generated project
  and report the binary path.
- `--input` is not a translate-time flag: the generated program takes
  `--input key=value` itself (typst CLI semantics — inputs are part of the
  invocation; the library is rebuilt from them on every run).
- `--embed <path>` — embed an extra file or directory (data files read by
  the document); the program errors with the closest embedded paths when
  something is missing.
- `--format pdf,svg,png` — default output formats baked into the program
  (the program's own `--format` flag overrides per invocation).
- `--system-fonts` — prefer system font discovery over the embedded
  typst-assets set (output is machine-independent with the default).
- `--with-deps`, `--lenient`, `--no-fmt` — as below.

Translate a package — or a single `.typ` file — into a library module (the
host-integration artifact; a single file is translated as a one-file
package with that file as the entry):

```sh
trust translate-pkg <package-root-or-file.typ> -o <output.rs | <output-dir>/ [flags]
```

- `--lenient` — downgrade unsupported constructs to runtime gaps instead of
  aborting; every downgrade is reported with its source location.
- `--with-deps` — resolve `@namespace/name:version` dependencies from the
  local package cache (`TYPST_PACKAGE_CACHE_PATH`, default
  `~/.cache/typst/packages`; missing `preview` packages are downloaded from
  packages.typst.org unless `TRUST_NO_FETCH` is set) and translate them into
  the same output; dependencies that cannot be resolved fall back to
  interpretation at runtime, with a warning.
- `-o <dir>/` (trailing slash) — emit a multi-file tree mirroring the source
  package instead of one generated source file.
- `--no-fmt` — emit the translator's raw output. By default the output is
  pretty-printed (prettyplease for plain Rust, the translator's own layout
  for the runtime DSL macros; token streams are preserved exactly, verified
  per chunk with any mismatch falling back to the original text).

Additional subcommands: `trust coverage <dir-or-file>` translates every
`.typ` file and clusters the gaps by diagnostic kind; `trust -V` prints the
version.

Generated code depends on `typst` and `trust-runtime`. For the host-side
contract see [docs/host-integration.md](docs/host-integration.md); for the
design basis see [docs/architecture.md](docs/architecture.md).

## Verification

- **Differential testing** (`cargo test`): each fixture runs twice —
  interpreted by `typst-eval` and compiled by `trust` — and the results are
  compared value by value. Expectations are produced by typst itself, never
  hand-written.
- **Official suite comparison** (`crates/trust-suite`): the official typst
  `tests/suite` corpus is vendored and driven case by case — each segment
  compiles two ways through one world, the translated assembly versus the
  original interpreted sources — comparing rendered output page by page,
  byte for byte, plus diagnostics and warnings as message sets. Remaining
  divergences are tracked in a known-divergence list gated in both
  directions (a new failure fails the suite; a tracked case silently
  passing fails it too).
- **Real-package rendering comparison**: real packages injected into
  unmodified typst render byte-identically to their interpreted originals
  (SVG compared as strings, PNG compared after rasterization).

## Status

Translation covers the Typst 0.15.1 scripting language — bindings, functions
with default/named/sink parameters, closures, destructuring, control flow,
content construction, context expressions, multi-file packages with all
import forms — and whole real packages compile and link without errors;
constructs outside the supported subset surface as explicit translation
errors, never as silent behavior changes. On the official typst
`tests/suite` corpus, all but 2 of the 3018 comparison cases currently
produce results identical to the interpreter; the two remainders are
tracked in the suite's known-divergence list.
