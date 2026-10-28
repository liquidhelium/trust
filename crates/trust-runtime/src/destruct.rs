//! The `destruct` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, eval::*, host::*, lookup::*, macros::*, slots::*,
    stub::*, styles::*,
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

/// Array construction convenience (the generated code's equivalent of the
/// `array!` macro).
pub fn array(values: Vec<Value>) -> Array {
    Array::from_iter(values)
}

/// Dictionary construction helper (generated code inserts key by key,
/// preserving source order).
pub fn dict_put(dict: &mut typst::foundations::Dict, key: &str, value: Value) {
    dict.insert(key.into(), value);
}

/// Dictionary item with an expression key (`("a" + x: v)`; mirrors
/// `Eval for ast::DictItem::Keyed`: the key is evaluated then cast to a
/// string, with errors reported at the key expression's span).
pub fn dict_put_key(
    dict: &mut typst::foundations::Dict,
    key: Value,
    value: Value,
    span: Span,
) -> SourceResult<()> {
    let key: Str = key.cast().at(span)?;
    dict.insert(key, value);
    Ok(())
}

/// Insert with an already-cast key (the dict literal's aggregating path:
/// invalid keys were collected by the caller, the default key is
/// inserted so later duplicates still see the same last-wins shape).
pub fn dict_insert(dict: &mut typst::foundations::Dict, key: Str, value: Value) {
    dict.insert(key, value);
}

/// Destructuring patterns over strings/bytes error up front at the
/// pattern span (mirrors typst-eval flow.rs:169-171: `cannot destructure
/// values of {ty}`; only Normal/Placeholder patterns may iterate
/// graphemes/integers).
pub fn for_destructure_check(iterable: &Value, pattern_at: Span) -> SourceResult<()> {
    match iterable {
        Value::Str(_) | Value::Bytes(_) => {
            bail!(pattern_at, "cannot destructure values of {}", iterable.ty())
        }
        _ => Ok(()),
    }
}

/// Dictionary spread `(..other)` (mirrors `Eval for ast::DictItem::Spread`).
pub fn dict_spread(
    dict: &mut typst::foundations::Dict,
    value: Value,
    span: Span,
) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Dict(other) => {
            for (key, value) in other {
                dict.insert(key, value);
            }
            Ok(())
        }
        other => bail!(span, "cannot spread {} into dictionary", other.ty()),
    }
}

/// Array spread `(..other)` (the generated code's array-literal spread).
pub fn array_spread(values: &mut Vec<Value>, value: Value, span: Span) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Array(array) => {
            values.extend(array);
            Ok(())
        }
        Value::Args(args) => {
            values.extend(args.items.into_iter().map(|arg| arg.value.v));
            Ok(())
        }
        other => bail!(span, "cannot spread {} into array", other.ty()),
    }
}

/// In-place invocation of mutable methods (push/pop/insert/remove); mirrors
/// `typst-eval/src/methods.rs::call_method_mut` (errors carry the method
/// call's tracepoint, same as native).
pub fn method_mut(
    engine: &mut Engine,
    value: &mut Value,
    method: &str,
    mut args: Args,
    span: Span,
) -> SourceResult<Value> {
    let ty = value.ty();
    let mut output = Value::None;
    match value {
        Value::Array(array) if matches!(method, "push" | "pop" | "insert" | "remove") => {
            match method {
                "push" => array.push(args.expect("value")?),
                "pop" => output = array.pop().at(span)?,
                "insert" => array
                    .insert(args.expect("index")?, args.expect("value")?)
                    .at(span)?,
                _ => {
                    output = array
                        .remove(args.expect("index")?, args.named("default")?)
                        .at(span)?
                }
            }
        }
        Value::Dict(dict) if matches!(method, "insert" | "remove") => match method {
            "insert" => dict.insert(args.expect("key")?, args.expect("value")?),
            _ => {
                output = dict
                    .remove(args.expect("key")?, args.named("default")?)
                    .at(span)?
            }
        },
        _ => bail!(span, "type {ty} has no method `{method}`"),
    }
    args.finish()?;
    let name = EcoString::from(method);
    Ok(output).trace(
        engine.world,
        || typst::diag::Tracepoint::Call(Some(name.clone())),
        span,
    )
}

/// **Runtime dispatch** for mutable-method calls (mirrors the full shape
/// of `maybe_resolve_mutating`): the four array methods and dictionary
/// insert/remove mutate in place; dictionary push/pop errors as a field
/// call; **every other type falls back to a plain method/field call**
/// (symbol modifiers etc., mirroring eval_field_callee — `s.push(none)`
/// returns a new symbol rather than erroring). The value's type is unknown
/// at translation time, so the dispatch must happen at runtime, per value.
pub fn method_mut_dispatch(
    engine: &mut Engine,
    context: Tracked<Context>,
    value: &mut Value,
    method: &str,
    mut args: Args,
    span: Span,
    access_text: &str,
) -> SourceResult<Value> {
    let is_array = matches!(value, Value::Array(_));
    let is_dict = matches!(value, Value::Dict(_));
    if (is_array && matches!(method, "push" | "pop" | "insert" | "remove"))
        || (is_dict && matches!(method, "insert" | "remove"))
    {
        return method_mut(engine, value, method, args, span);
    }
    // Fall back to a plain field call: a method call passes the target as
    // the first positional argument.
    let target = value.clone();
    let (func, bound) = method_callee(target.clone(), method, access_text, engine, span)?;
    if bound.is_some() {
        args.insert(0, span, target);
    }
    let name = EcoString::from(method);
    call_with(func, None, span, args, engine, context).trace(
        engine.world,
        move || typst::diag::Tracepoint::Call(Some(name.clone())),
        span,
    )
}

// --- Content translation helpers (mirroring the construction in
// typst-eval/src/markup.rs) ---

/// Iterator-shaped loop sources (mirrors `Eval for ast::ForLoop`: arrays
/// consumed by value, dictionaries as key-value pairs, strings by grapheme,
/// byte strings by byte; all streaming, zero materialization —
/// dictionaries/strings/byte strings yield lazily) for generated code's
/// inline `for` (break/continue work natively). Combinations of
/// destructuring patterns with Str/Bytes are guaranteed absent by the
/// caller at translation time (native errors on them too). The iterator
/// borrows `value` (in generated code the loop-source temporary lives for
/// the whole for statement).
pub fn iter_values<'a>(
    value: &'a Value,
    span: Span,
) -> SourceResult<Box<dyn Iterator<Item = Value> + 'a>> {
    use unicode_segmentation::UnicodeSegmentation;
    Ok(match value {
        Value::Array(array) => Box::new(array.clone().into_iter()),
        Value::Dict(dict) => Box::new(dict.iter().map(|(key, item): (&Str, &Value)| {
            Array::from_iter([key.clone().into_value(), item.clone()]).into_value()
        })),
        Value::Str(string) => Box::new(
            string
                .as_str()
                .graphemes(true)
                .map(|g| Value::Str(g.into())),
        ),
        Value::Bytes(bytes) => Box::new(bytes.as_slice().iter().map(|b| Value::Int(*b as i64))),
        other => bail!(span, "cannot loop over {}", other.ty()),
    })
}

// --- Destructuring helpers (mirroring the semantics and error messages of
// typst-eval/src/binding.rs) ---

/// Array destructuring: the i-th element (errors when short). `expected` is
/// the pattern's total positional-item count and `at_least` whether it has
/// a sink, exactly as `wrong_number_of_elements` computes them
/// (binding.rs: the hint carries the pattern's count, not the index the
/// destructure reached).
pub fn d_at(
    array: &typst::foundations::Array,
    i: usize,
    expected: usize,
    at_least: bool,
    span: Span,
) -> SourceResult<Value> {
    match array.at(i as i64, None) {
        Ok(value) => Ok(value),
        Err(_) => Err(elements_mismatch_diagnostic(
            array.len(),
            expected,
            at_least,
            span,
        )),
    }
}

/// Array destructuring sink collection (`..rest`); `total` is the number of
/// positions the pattern consumes.
/// Array destructuring sink size (mirrors binding.rs's
/// `(1 + len).checked_sub(items)`): a shortfall is an element-count error.
pub fn d_sink_size(
    array: &typst::foundations::Array,
    total: usize,
    span: Span,
) -> SourceResult<usize> {
    match (1 + array.len()).checked_sub(total) {
        Some(size) => Ok(size),
        None => Err(elements_mismatch_diagnostic(
            array.len(),
            total - 1,
            true,
            span,
        )),
    }
}

/// Array destructuring sink slice (`size` items from `i`; out of bounds is
/// an element-count error; `count` is the number of non-sink items in the
/// pattern, used only in the error message).
pub fn d_rest(
    array: &typst::foundations::Array,
    i: usize,
    size: usize,
    count: usize,
    span: Span,
) -> SourceResult<typst::foundations::Array> {
    match array.as_slice().get(i..i + size) {
        Some(slice) => Ok(slice.iter().cloned().collect()),
        None => Err(elements_mismatch_diagnostic(array.len(), count, true, span)),
    }
}

/// Post-destructuring validation for arrays (leftover elements error;
/// `consumed` is the runtime index).
pub fn d_array_finish(
    array: &typst::foundations::Array,
    consumed: &usize,
    span: Span,
) -> SourceResult<()> {
    if *consumed < array.len() {
        return Err(elements_mismatch_diagnostic(
            array.len(),
            *consumed,
            false,
            span,
        ));
    }
    Ok(())
}

/// Dictionary destructuring: fetch by key (via `Dict::get`, keeping the
/// original hint).
pub fn d_get(dict: &typst::foundations::Dict, key: &str, span: Span) -> SourceResult<Value> {
    dict.get(key).cloned().at(span)
}

/// Dictionary destructuring sink collection (`..rest`, excluding used
/// keys).
pub fn d_dict_rest(dict: &typst::foundations::Dict, used: &[&str]) -> typst::foundations::Dict {
    let mut rest = typst::foundations::Dict::new();
    for (key, value) in dict.iter() {
        if !used.iter().any(|u| *u == key.as_str()) {
            rest.insert(key.clone(), value.clone());
        }
    }
    rest
}

/// Mirrors the original `wrong_number_of_elements`: main message + hint
/// (count semantics live in the hint, verbatim as in typst-eval).
fn elements_mismatch_diagnostic(
    len: usize,
    expected_count: usize,
    spread: bool,
    span: Span,
) -> EcoVec<SourceDiagnostic> {
    let quantifier = if len > expected_count {
        "too many"
    } else {
        "not enough"
    };
    let expected = match (spread, expected_count) {
        (true, 1) => "at least 1 element".to_string(),
        (true, c) => format!("at least {c} elements"),
        (false, 0) => "an empty array".to_string(),
        (false, 1) => "a single element".to_string(),
        (false, c) => format!("{c} elements"),
    };
    eco_vec![
        SourceDiagnostic::error(
            span,
            typst::foundations::Str::from(format!("{quantifier} elements to destructure"))
        )
        .with_hint(format!(
            "the provided array has a length of {len}, but the pattern expects {expected}"
        ))
    ]
}
