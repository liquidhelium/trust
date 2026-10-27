//! The `eval` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, destruct::*, host::*, lookup::*, macros::*,
    slots::*, stub::*, styles::*,
};

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use typst::Library;
use typst::World;
use typst::diag::{At, SourceResult, bail};
use typst::diag::{Hint, SourceDiagnostic, Trace};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{
    Arg, Args, Array, Binding, Capturer, Closure, ClosureNode, Content, Context, Func, IntoValue,
    Module, Scope, Scopes, Selector, Str, Transformation, Value, ops,
};
use typst::introspection::{EmptyIntrospector, Introspector};
use typst::syntax::ast::{self, AstNode};
use typst::syntax::{Span, Spanned, SyntaxKind};
use typst::utils::{LazyHash, Protected};

/// **Math expressions** outside the v1 language subset (Equation): their
/// volume is large but they pose almost no performance problem — they are
/// **interpreted** by typst-eval from a source snippet (the same fidelity
/// path as interpreted closures: a ContextNode closure + captured scope,
/// with snippets parse-cached by text). Other gaps do not take this path
/// and still degrade with a translation-time diagnostic.
pub fn eval_expr(
    src: &str,
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Value> {
    let func = context_closure(src, Span::detached(), 0, captured, pending, engine);
    func.call::<[Value; 0]>(engine, context, [])
}

/// Interprets an **import statement** (glue; mirrors `Eval for
/// ast::ModuleImport` — import.rs:15-178, a private trait impl, driven
/// directly through the pub `Vm`/`Eval`): visible names are materialized
/// into the Vm's `scopes.top` (plain bindings, i.e. the enclosing block
/// scope shape); the verbatim statement takes its node from the parse cache
/// and is **synthesized onto the original statement's span** (twice over:
/// relative paths resolve against the original file — `import()`'s path
/// resolution anchors on `span.id()`; diagnostic sites point at the real
/// statement). The original code handles source dispatch/loading (including
/// pub `typst_eval::import`'s file-not-found/cycle detection/package
/// validation)/the three bare-name tiers/Items nesting and unresolved/all
/// warnings. The diff is read back: newly bound values are returned in
/// `expected` order (bare names/renames/item names are all statically
/// known).
pub fn eval_import(
    src: &str,
    span: Span,
    expected: &[&str],
    captured: &[(&'static str, Value)],
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Vec<Value>> {
    use typst_eval::{Eval as _, Vm};

    let mut node = cached_body_node(src);
    node.synthesize(span);
    if std::env::var_os("TRUST_TRACE_IMPORT").is_some() {
        eprintln!(
            "[eval_import] src={src:?} span_file={:?} span={:?}",
            span.id().map(|id| id.vpath().get_with_slash().to_string()),
            span
        );
    }
    let expr = node
        .cast::<ast::Expr>()
        .expect("import statement parses to an expression");
    // Visible names = the enclosing scope (plain bindings; import writes
    // new names and never touches read-only marks).
    let mut top = Scope::new();
    for (name, value) in captured {
        top.bind((*name).into(), Binding::detached(value.clone()));
    }
    let mut scopes = Scopes::new(Some(engine.library));
    scopes.top = top;
    let engine2 = Engine {
        world: engine.world,
        library: engine.library,
        introspector: engine.introspector,
        traced: engine.traced,
        sink: TrackedMut::reborrow_mut(&mut engine.sink),
        route: Route::extend(engine.route.track()),
    };
    let mut vm = Vm::new(engine2, context, scopes, span);
    expr.eval(&mut vm)?;
    let mut out = Vec::with_capacity(expected.len());
    for name in expected {
        // A captured (already visible) name is fine here: imports re-bind
        // names they mention (`import x as x: item` re-defines `x`,
        // `import mymod` re-binds the ident) — typst's `vm.define`/
        // `scopes.top.bind` write the binding regardless of prior
        // visibility, and the written value equals the read-back one.
        match vm.scopes.top.get(name) {
            Some(binding) => out.push(binding.read().clone()),
            None => {
                return Err(eco_vec![SourceDiagnostic::error(
                    span,
                    eco_format!("import diff is missing binding `{name}` (generator bug)"),
                )]);
            }
        }
    }
    Ok(out)
}

/// Interprets an **include statement** (same glue; mirrors `Eval for
/// ast::ModuleInclude`): returns its content, with no bindings read back.
pub fn eval_include(
    src: &str,
    span: Span,
    captured: &[(&'static str, Value)],
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Content> {
    use typst_eval::{Eval as _, Vm};

    let mut node = cached_body_node(src);
    node.synthesize(span);
    let expr = node
        .cast::<ast::Expr>()
        .expect("include statement parses to an expression");
    let mut top = Scope::new();
    for (name, value) in captured {
        top.bind((*name).into(), Binding::detached(value.clone()));
    }
    let mut scopes = Scopes::new(Some(engine.library));
    scopes.top = top;
    let engine2 = Engine {
        world: engine.world,
        library: engine.library,
        introspector: engine.introspector,
        traced: engine.traced,
        sink: TrackedMut::reborrow_mut(&mut engine.sink),
        route: Route::extend(engine.route.track()),
    };
    let mut vm = Vm::new(engine2, context, scopes, span);
    match expr.eval(&mut vm)? {
        Value::Content(content) => Ok(content),
        // Mirrors the original: include always evaluates to content; any
        // other shape is a generator-side bug.
        v => Err(eco_vec![SourceDiagnostic::error(
            span,
            eco_format!("include evaluated to non-content {}", v.ty()),
        )]),
    }
}

/// Interpreted evaluation with read-back (the escape hatch for `#let`
/// embedded in math): **drives the pub `Vm`/`Eval` directly** (mirroring
/// eval_closure's internal shape — build the Vm with the captured clone as
/// `scopes.top`; after evaluation new bindings land in top, and the
/// `Scopes` field is pub so it can be read back directly), with zero
/// synthesized typst source and zero export tuples — the snippet evaluates
/// verbatim, embedded lets bind into top per typst semantics, and we share
/// typst's own evaluation machinery. Returns (expression value, sequence of
/// bound values for the exported names).
pub fn eval_vm_exports(
    src: &str,
    exports: &[&str],
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<(Value, Vec<Value>)> {
    use typst_eval::{Eval as _, Vm};

    let node = cached_body_node(src);
    let expr = node
        .cast::<ast::Expr>()
        .expect("cached body node casts to an expression");
    let scope = captured_scope(captured, pending, engine);
    let mut scopes = Scopes::new(Some(engine.library));
    scopes.top = scope;
    // Same engine/route shape as eval_closure: all fields pub, constructed
    // by copy; the route shares its depth counter via track+extend (nested
    // guards stay continuous).
    let engine = Engine {
        world: engine.world,
        library: engine.library,
        introspector: engine.introspector,
        traced: engine.traced,
        sink: TrackedMut::reborrow_mut(&mut engine.sink),
        route: Route::extend(engine.route.track()),
    };
    let mut vm = Vm::new(engine, context, scopes, node.span());
    let value = expr.eval(&mut vm)?;
    let out = exports
        .iter()
        .map(|name| {
            vm.scopes
                .top
                .get(name)
                .map(|b| b.read().clone())
                .unwrap_or(Value::None)
        })
        .collect();
    Ok((value, out))
}

/// Propagate a collected diagnostic list (the dict literal's
/// invalid-key aggregation: all keys reported together, mirrors
/// `Eval for ast::Dict` returning `invalid_keys` at the end).
pub fn bail_diags(diags: EcoVec<SourceDiagnostic>) -> SourceResult<()> {
    Err(diags)
}
