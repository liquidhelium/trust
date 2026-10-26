//! `trust_runtime` 的 context 分区(自 lib.rs 机械拆分)。

#![allow(unused_imports)]
use crate::{assign::*, calls::*, content::*, destruct::*, eval::*, host::*, lookup::*, macros::*, slots::*, stub::*, styles::*};

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


/// `#context expr`: builds the deferred-evaluation closure (mirrors `Eval
/// for ast::Contextual`; evaluated during realize/layout with
/// location/styles).
pub fn context_closure(
    body_src: &str,
    span: Span,
    base: usize,
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
) -> Func {
    let mut node = cached_body_node(body_src);
    // Synthesize real, per-node spans: the snippet parse leaves detached
    // spans, and FileId-carrying spans are load-bearing inside the
    // interpreted body — image()/bibliography() resolve their paths via
    // span.id() ("cannot access file system from here" otherwise), and
    // span identity feeds citation matching and the layout convergence
    // machinery. Mapping (RangeMapper, the primitive typst uses for
    // doc-comment code) keeps every node at its own offset in the
    // original file — collapsing the whole body onto ONE span once made
    // a measure-loop converge that must not converge.
    match span.id() {
        Some(id) => {
            let len = body_src.trim().len();
            match typst_syntax::RangeMapper::new(std::iter::once(base..base + len))
                .and_then(|mapper| node.synthesize_mapped(id, &mapper))
            {
                Ok(()) => {}
                Err(_) => node.synthesize(span),
            }
        }
        None => node.synthesize(span),
    }
    let scope = captured_scope(captured, pending, engine);
    Func::from(Closure {
        node: ClosureNode::Context(node.clone()),
        defaults: vec![],
        captured: scope,
        num_pos_params: 0,
    })
}


/// Snippet source → AST expression node (a lookup cache keyed by the
/// **original snippet text**; hits do zero formatting). A context body is
/// an **expression** (a code block or a single expression), so `parse_code`
/// takes the first expression under the Code root — the source must not be
/// wrapped in `#let` (under that root the first castable node is the
/// LetBinding itself: evaluation would run the body, but the returned value
/// is the let's none, which ContextElem displays as empty content). Both
/// the parsed and registered text are trimmed (leading/trailing trivia
/// would otherwise make the "first expression node" a Space).
pub(crate) fn cached_body_node(body_src: &str) -> typst_syntax::SyntaxNode {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, typst_syntax::SyntaxNode>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(hit) = CACHE.read().unwrap().get(body_src) {
        return hit.clone();
    }
    let text = body_src.trim().to_string();
    // Parse after wrapping in parentheses: the snippet has left its
    // original parse context — if it originally sat inside parentheses/a
    // block (a parameter default, an array element, a code block, ...),
    // newlines are mere trivia and the expression continues across lines;
    // top-level `parse_code` instead terminates the expression at a
    // newline (only else/. continue, mirroring parser.rs's
    // AtNewline::ContextualContinue), so a multi-line body would be
    // truncated to its first expression. Wrapping in a pair of parentheses
    // restores Continue semantics; the Parenthesized layer is then peeled
    // to take the inner expression (mirroring how typst-eval's `Eval for
    // ast::Contextual` directly holds the in-place body node).
    // CLOSURE_SOURCES registers the wrapped text, so spans correspond byte
    // for byte with the source.
    let wrapped = format!("({text})");
    let root = typst_syntax::parse_code(&wrapped);
    let first = root
        .children()
        .find(|child| child.cast::<ast::Expr>().is_some())
        .unwrap_or_else(|| panic!("context body parses to an expression: {text:?}"));
    let node = match first.cast::<ast::Expr>() {
        Some(ast::Expr::Parenthesized(paren)) => paren.expr().to_untyped().clone(),
        _ => first.clone(),
    };
    let n = CLOSURE_SOURCES.read().unwrap().len();
    let id = typst::syntax::RootedPath::new(
        typst::syntax::VirtualRoot::Project,
        typst::syntax::VirtualPath::new(format!("__trust_closure_{n}.typ"))
            .expect("valid closure path"),
    )
    .intern();
    CLOSURE_SOURCES.write().unwrap().push((id, wrapped.clone()));
    CACHE
        .write()
        .unwrap()
        .insert(body_src.to_string(), node.clone());
    node
}


/// Captured scope (pending materialization + the Captured read-only mark;
/// mirrors eval_closure's `scopes.top = closure.captured.clone()`).
pub(crate) fn captured_scope(
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
) -> Scope {
    let mut captured: Vec<(&'static str, Value)> = captured.to_vec();
    resolve_pending(engine, &mut captured, pending);
    let mut scope = Scope::new();
    for (name, value) in &captured {
        // Captured bindings get the read-only mark via `Binding::capture`
        // (mirroring typst closures' captured scope): write-captures like
        // `context (i = 1)` are rejected by typst's evaluator through
        // `Binding::write` with "variables from outside the context
        // expression are read-only...".
        scope.bind(
            (*name).into(),
            Binding::detached(value.clone()).capture(Capturer::Context),
        );
    }
    scope
}


/// Carrier element for context closures (mirrors
/// `ContextElem::new(func).pack()`; ContextElem implements IntoValue once
/// wrapped in `Packed`).
pub fn context_body(func: Func) -> typst::foundations::Content {
    use typst::foundations::IntoValue;
    match typst::foundations::ContextElem::new(func).into_value() {
        typst::foundations::Value::Content(content) => content,
        _ => unreachable!("ContextElem packs to content"),
    }
}

