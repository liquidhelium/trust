//! `trust_runtime` 的 calls 分区(自 lib.rs 机械拆分)。

#![allow(unused_imports)]
use crate::{assign::*, content::*, context::*, destruct::*, eval::*, host::*, lookup::*, macros::*, slots::*, stub::*, styles::*};

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


/// Call-site guardrail: function call depth (mirrors
/// `vm.engine.route.check_call_depth`; mutual recursion produces an error
/// instead of a segfault).
pub fn check_call_depth(engine: &Engine, span: Span) -> SourceResult<()> {
    engine.route.check_call_depth().at(span)
}


/// Resolves the callee (mirrors native `callee.eval()?.cast::<Func>()`:
/// function values, type values (their constructors), and symbols (their
/// associated functions) are all callable; everything else errors with the
/// cast message — arguments are not evaluated before this point, matching
/// the native evaluation order). Takes the value by value (the call site
/// holds a temporary; the common path clones nothing).
pub fn callee(value: Value, span: Span) -> SourceResult<Func> {
    if let Value::Func(func) = value {
        return Ok(func);
    }
    value.cast::<Func>().at(span)
}


/// Unified call path (mirrors `call_func`): the func is tagged with the
/// call-site span and errors carry a tracepoint (`error occurred in this
/// call of function ...`); method calls pass the target as the first
/// positional argument via `insert` (mirrors
/// `args.insert(0, target_expr.span(), target)`).
pub fn call_with(
    func: Func,
    insert: Option<(Value, Span)>,
    span: Span,
    mut args: Args,
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Value> {
    let func = func.spanned(span);
    if let Some((target, target_span)) = insert {
        args.insert(0, target_span, target);
    }
    // The tracepoint name is materialized only on the error path (the trace
    // closure is called lazily). No stacker::maybe_grow: call depth is cut
    // off by the check_call_depth guardrail (80 levels) before stack
    // overflow can happen; a per-call stack probe would be pure overhead
    // (the native interpreter path needs it, the compiled path does not).
    func.call(engine, context, args).trace(
        engine.world,
        || typst::diag::Tracepoint::Call(func.name().map(EcoString::from)),
        span,
    )
}


/// Callee resolution for method calls (mirrors `eval_field_callee`: methods
/// on the type's scope first, then the content element's scope;
/// Symbol/Func/Type/Module allow field-call syntax; errors use the original
/// two-stage form + hint set).
///
/// Returns `(function value, target value when it is a method call)`; for a
/// method call the caller must pass the target as the first positional
/// argument (the target is returned by value, saving a clone).
pub fn method_callee(
    target: Value,
    name: &str,
    access_text: &str,
    engine: &mut Engine,
    span: Span,
) -> SourceResult<(Func, Option<Value>)> {
    let sink = (&mut *engine, span);
    let mut is_method_call = false;
    let callee_value = if let Some(method) = target.ty().scope().get(name) {
        is_method_call = true;
        method.read_checked(sink).clone()
    } else if let Value::Content(content) = &target
        && let Some(method) = content.elem().scope().get(name)
    {
        is_method_call = true;
        method.read_checked(sink).clone()
    } else if matches!(
        &target,
        Value::Symbol(_) | Value::Func(_) | Value::Type(_) | Value::Module(_)
    ) {
        target.field(name, sink).at(span)?
    } else {
        // Otherwise we cannot call this field and produce an error.
        match target.field(name, sink) {
            Ok(callee_value) => {
                let is_dict = matches!(target, Value::Dict(_));
                let is_named = matches!(target, Value::Args(_));
                let (kind, ty_name) = element_or_type_with_name(&target);
                let message = if is_dict {
                    EcoString::from("cannot directly call dictionary keys as functions")
                } else if is_named {
                    EcoString::from("cannot directly call named argument fields as functions")
                } else {
                    eco_format!("`{name}` is not a valid method for {kind} `{ty_name}`")
                };
                let mut err =
                    SourceDiagnostic::error(span, typst::foundations::Str::from(message));
                if callee_value.clone().cast::<Func>().is_ok() {
                    err = err.with_hint(eco_format!(
                        "to call the stored function, wrap the field access in parentheses: `({access_text})(..)`"
                    ));
                } else {
                    err = err.with_hint(eco_format!(
                        "to access the `{name}` {}, remove the function arguments: `{access_text}`",
                        if is_dict { "key" } else if is_named { "argument" } else { "field" },
                    ));
                }
                if is_dict {
                    err = err.with_hint(
                        "dictionary keys cannot be used with method syntax as keys \
                         could conflict with built-in method names",
                    );
                } else if is_named {
                    err = err.with_hint(
                        "named arguments cannot be used with method syntax as argument \
                         names could conflict with built-in method names",
                    );
                }
                return Err(eco_vec![err]);
            }
            Err(_) => {
                let (kind, ty_name) = element_or_type_with_name(&target);
                bail!(span, "{kind} {ty_name} has no method `{name}`")
            }
        }
    };
    match callee_value {
        Value::Func(func) if is_method_call => Ok((func, Some(target))),
        Value::Func(func) => Ok((func, None)),
        callee_value => callee_value
            .cast::<Func>()
            .map(|func| (func, None))
            .at(span),
    }
}


/// If the value is content, the string "element" and the name of its element
/// function, or the string "type" and the long name of the value's type.
fn element_or_type_with_name(value: &Value) -> (&'static str, EcoString) {
    if let Value::Content(content) = value {
        ("element", EcoString::from(content.elem().name()))
    } else {
        ("type", EcoString::from(value.ty().long_name()))
    }
}


/// Join semantics of code blocks (mirrors `ops::join`).
pub fn join(lhs: Value, rhs: Value, span: Span) -> SourceResult<Value> {
    ops::join(lhs, rhs).at(span)
}


/// Condition evaluation for if/while: non-boolean values error via the
/// original cast.
pub fn boolean(value: Value, span: Span) -> SourceResult<bool> {
    value.cast::<bool>().at(span)
}


/// Iteration guardrail limit for while loops (mirrors
/// `typst-eval/src/flow.rs`: `MAX_ITERATIONS = 10_000`, beyond which
/// "loop seems to be infinite" is reported).
pub const MAX_ITERATIONS: usize = 10_000;


/// Positional parameter binding (declaration order; mirrors eval_closure's
/// `args.expect`).
pub fn param_pos(args: &mut Args, name: &str, _span: Span) -> SourceResult<Value> {
    args.expect::<Value>(name)
}


/// Named-argument binding for stub closures: the stub binding stage already
/// filled in defaults, so this always succeeds (mirrors `args.named`; a
/// missing argument reports missing argument — only a bare call can trigger
/// it).
pub fn param_named(args: &mut Args, name: &str, span: Span) -> SourceResult<Value> {
    match args.named::<Value>(name)? {
        Some(value) => Ok(value),
        None => bail!(span, "missing argument: {name}"),
    }
}


/// Checks for leftover arguments after parameter binding (mirrors
/// `Args::finish`).
pub fn finish(args: &mut Args, _span: Span) -> SourceResult<()> {
    if let Some(arg) = args.items.first() {
        match &arg.name {
            Some(name) => bail!(arg.span, "unexpected argument: {name}"),
            None => bail!(arg.span, "unexpected argument"),
        }
    }
    Ok(())
}


/// Builds a named argument (used by generated code at call sites; `span` is
/// the argument item's span, `value_span` the value expression's span;
/// mirrors native `Eval for ast::Args`).
pub fn named_arg(span: Span, value_span: Span, name: &str, value: Value) -> Arg {
    Arg {
        span,
        name: Some(name.into()),
        value: Spanned::new(value, value_span),
    }
}


/// Spreads arguments (`f(..args)`; mirrors `Eval for ast::Arg::Spread`:
/// arrays by position, dictionaries by name, None skipped).
pub fn spread_args(args: &mut Args, span: Span, value: Value) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Array(array) => {
            for item in array {
                args.push(span, item);
            }
            Ok(())
        }
        Value::Dict(dict) => {
            for (key, value) in dict {
                args.items.push(named_arg(span, span, &key, value));
            }
            Ok(())
        }
        Value::Args(inner) => {
            for item in inner.items.iter() {
                args.items.push(item.clone());
            }
            Ok(())
        }
        other => bail!(span, "cannot spread {}", other.ty()),
    }
}

