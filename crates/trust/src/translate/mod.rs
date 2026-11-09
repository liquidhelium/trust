//! The translator proper: typst source → Rust source (the L0 direct
//! translation layer on the Nuitka route).
//!
//! Output shape (per-build slot board, see `_things/DESIGN-per-build.md`):
//! every top-level `#let` binding compiles to
//! - a function → a `NativeFuncData` static (fields written by hand, not
//!   via macros): the body is parameter destructuring + direct expression
//!   translation (all `Value`; arithmetic goes through
//!   `typst::foundations::ops`; error messages and span behavior mirror
//!   typst-eval);
//! - values and functions alike become write-once slots (`OnceLock<Value>`)
//!   on a per-build state struct, filled in source order during
//!   `module_init` (the mirror of module evaluation, invoked once per build
//!   by the host); references inside function bodies compile to
//!   `rt::read(&__st.slot_name)?` (static offset).
//!
//! Semantic boundaries (v1):
//! - named-parameter defaults are evaluated at every call (the original
//!   evaluates them once at definition time; equivalent for pure code, but
//!   the number of evaluations differs);
//! - closure bodies are interpreted by typst-eval (the fidelity path);
//!   captures are materialized at creation time (global names likewise,
//!   mirroring CapturesVisitor);
//! - global names inside top-level function bodies resolve at call time
//!   (`rt::global`), which differs in timing from the creation-time
//!   materialization used for closures (a timing difference only;
//!   equivalent for pure code);
//! - errors use a detached span (only values are compared, not positions).
//!
//! Module layout (read top down):
//! - `ctx`: translation context and name-resolution environment
//!   (`Ctx`/`Env`/`Ref`);
//! - `expr`: direct expression translation (literals/collections/calls/
//!   closures/binary operations);
//! - `block`: code blocks and statements (let/in-body import/assignment);
//! - `destructure`: destructuring (array/dictionary patterns);
//! - `func`: generation of function bodies and `NativeFuncData` statics;
//! - `captures`: closure capture analysis;
//! - `naming`/`paths`: helpers for identifiers and import paths;
//! - `file`: single-file driver (`translate`);
//! - `plan` + `package`: multi-file package driver (structure analysis +
//!   code generation).

mod block;
mod captures;
mod ctx;
mod destructure;
mod expr;
mod file;
mod func;
mod naming;
mod package;
mod paths;
mod plan;
mod typed;
pub use file::translate;
pub use package::{
    PackageInput, translate_package, translate_package_lenient, translate_package_tree,
    translate_package_tree_lenient,
};

/// A translation diagnostic: syntax span + message + owning file (in
/// package mode).
#[derive(Debug, Clone)]
pub struct Diag {
    pub span: typst_syntax::Span,
    pub message: String,
    pub file: Option<String>,
}
