# trust

`trust` compiles Typst packages to Rust. The output is not a standalone
program: it is a Rust crate that drives the same value model as the Typst
interpreter — `typst-library`'s `Value`, `Func`, and `Content` — so a
compiled package behaves like, and interoperates with, the original `.typ`
package inside an unmodified typst. The relationship is the one Nuitka has
to CPython: compile the program, keep the host runtime and its object model.

The translator consumes a package directory (`typst.toml`, entrypoint,
import graph) and emits one Rust crate per package. The generated crate
exposes `module()` (the export surface of the original entry file, bound
name by name), `module_init` (per-build initialization of top-level engine
values), and `install(world, library)` (injection into a host).

## Workspace layout

| Path | Contents |
|---|---|
| `crates/trust` | The translator: `typst-syntax` AST frontend → Rust code generation |
| `crates/trust-runtime` | Runtime support for generated code: parameter destructuring, call protocol, closure construction, content/markup construction |
| `crates/trust-world` | Injected World: global module injection, package virtualization, the `module_init` engine-value protocol |
| `crates/trust-diff` | Differential testing: fixtures translated at build time, compared case by case against the `typst-eval` interpreter |
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

Translate a single file:

```sh
trust translate <input.typ> [--name <module>] [-o <output.rs>]
```

Translate a package (entrypoint discovered from `typst.toml`, module name
taken from `package.name`):

```sh
trust translate-pkg <package-root> -o <output.rs | <output-dir>/ [flags]
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
- **Official suite comparison** (external harness): the official typst
  `tests/suite` corpus compiles two ways through one binary — native
  registration versus the original interpreted sources — and compares
  rendered output page by page, byte for byte, plus diagnostics as message
  sets with positions stripped.
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
`tests/suite` segments the comparison harness drives, 97.6% currently
produce results identical to the interpreter.
