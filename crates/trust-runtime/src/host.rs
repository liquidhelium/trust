//! `trust_runtime` 的 host 分区(自 lib.rs 机械拆分)。

#![allow(unused_imports)]
use crate::{assign::*, calls::*, content::*, context::*, destruct::*, eval::*, lookup::*, macros::*, slots::*, stub::*, styles::*};

use comemo::{Track, Tracked, TrackedMut};
use typst::Library;
use typst::World;
use typst::diag::{At, SourceResult, bail};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{
    Arg, Args, Array, Binding, Capturer, Closure, ClosureNode, Content, Context, Func,
    IntoValue, Module, Scope, Scopes, Selector, Str, Transformation, Value, ops,
};
use typst::introspection::{EmptyIntrospector, Introspector};
use typst::utils::{LazyHash, Protected};
use typst::syntax::ast::{self, AstNode};
use typst::syntax::{Span, Spanned, SyntaxKind};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use typst::diag::{Hint, SourceDiagnostic, Trace};


pub fn source_span(path: &str, orig_raw: u64) -> Span {
    /// Mask for the low 48 bits (position number); the file number sits in
    /// the high bits (public documented convention:
    /// | 16 bits file id | 48 bits number |).
    const NUMBER_MASK: u64 = (1 << 48) - 1;
    if path.is_empty() {
        return Span::detached();
    }
    // Recursive-dependency form: an `@ns/name:ver/in-package-path` prefix
    // resolves to the Package root (same origin as the FileId the host
    // would create when loading the real package; after intern dedup the
    // span points straight at the host's package file, and plugin
    // relative-path resolution also anchors at the package root).
    let id = if path.starts_with('@') {
        let colon = path.find(':').expect("spec contains a version separator");
        let slash = path[colon..].find('/').expect("spec is followed by a path") + colon;
        let spec: typst::syntax::package::PackageSpec = path[..slash]
            .parse()
            .unwrap_or_else(|e| panic!("invalid spec {}: {e}", &path[..slash]));
        typst::syntax::RootedPath::new(
            typst::syntax::VirtualRoot::Package(spec),
            typst::syntax::VirtualPath::new(&path[slash + 1..])
                .unwrap_or_else(|_| panic!("invalid path {path}")),
        )
        .intern()
    } else {
        typst::syntax::RootedPath::new(
            typst::syntax::VirtualRoot::Project,
            typst::syntax::VirtualPath::new(path)
                .unwrap_or_else(|_| panic!("invalid path {path}")),
        )
        .intern()
    };
    let file_num = id.into_raw().get() as u64;
    let number = orig_raw & NUMBER_MASK;
    let raw = std::num::NonZeroU64::new((file_num << 48) | number)
        .expect("file id combined with position number is nonzero");
    let span = Span::from_raw(raw);
    assert_eq!(
        span.id(),
        Some(id),
        "span layout does not match typst's public documentation (version drift?); refusing to silently misalign"
    );
    span
}

// --- Host integration protocol (self-contained; the host does not need trust-world) ---


/// Function shape of the generated `module_init` (the module-assembly entry
/// evaluated once per build).
pub type ModuleInit = fn(&mut Engine, Tracked<Context>) -> SourceResult<Module>;


/// Driver shell for `module_init` (mirrors `typst_eval::eval`'s module
/// evaluation cache): builds an Engine (EmptyIntrospector + fresh Sink +
/// Route) plus `Context::none`; comemo memoizes on (world read constraints,
/// library contents, init fn), invalidates automatically when dependencies
/// change, and re-evaluates after eviction (`rt::fill` is idempotent for the
/// same value, so re-entry is safe). Only requires the host World to serve
/// external package files and Project-root data files (wasm plugins).
#[comemo::memoize]
pub fn init_module(
    world: Tracked<'_, dyn World + '_>,
    library: &LazyHash<Library>,
    init: ModuleInit,
) -> SourceResult<Module> {
    let mut sink = Sink::new();
    let traced = Traced::default();
    let mut engine = Engine {
        library,
        world,
        introspector: Protected::new(EmptyIntrospector.track()),
        traced: traced.track(),
        sink: sink.track_mut(),
        route: Route::default(),
    };
    let context = Context::none();
    init(&mut engine, context.track())
}


/// Injects a module into library.global (bind, not define, so same-named
/// definitions may be replaced) + **rebuilds the std binding** (std holds
/// another Arc copy of global; scope copy-on-write would fork the two) +
/// rewraps the `LazyHash` (visible to comemo's cache hashing). Returns the
/// new library for the host to swap into its world.
pub fn bind_module(
    library: &LazyHash<Library>,
    name: &'static str,
    module: Module,
) -> LazyHash<Library> {
    let mut library: Library = (**library).clone();
    library
        .global
        .scope_mut()
        .bind(name.into(), Binding::detached(module));
    library.std = Binding::detached(library.global.clone().into_value());
    install_stub_fast_path(&mut library);
    typst::utils::LazyHash::new(library)
}


/// Imports an external package at runtime (untranslated dependencies run
/// through the interpreter — mixed execution; mirrors typst-eval's
/// `import_package` + `import_file`: sink/route are passed through (warnings
/// and call depth share the same chain), cycle detection, and the module is
/// bound under the package's short name).
pub fn import_external(engine: &mut Engine, spec: &str, span: Span) -> SourceResult<Module> {
    // Direct reuse of the pub `typst_eval::import` (a hand-rolled version
    // drifted in three places: the spec-parse error message, the manifest
    // validation chain, and the package-name mapping — the original gets
    // all of them right).
    typst_eval::import(engine, spec, span)
}


/// Explicit degradation marker: emits a warning and returns none (the
/// no-silent-degradation principle — the generated artifact lacks
/// functionality here, and the user must see it).
pub fn degraded(engine: &mut Engine, message: &str) -> SourceResult<Value> {
    let diagnostic = typst::diag::SourceDiagnostic::warning(
        Span::detached(),
        typst::foundations::Str::from(format!("trust degraded: {message}")),
    );
    engine.sink.warn(diagnostic);
    Ok(Value::None)
}


/// The **error** form of a translation-time degradation point (a semantic
/// error rather than a missing feature: break/continue outside a loop,
/// etc.; complements [`degraded`]'s warning form; the message follows
/// typst's original wording, and the error tests match on the message).
pub fn bail_value(_engine: &mut Engine, message: &str) -> SourceResult<Value> {
    Err(eco_vec![SourceDiagnostic::error(
        Span::detached(),
        eco_format!("{message}"),
    )])
}

