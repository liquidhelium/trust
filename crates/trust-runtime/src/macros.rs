//! `trust_runtime` 的 macros 分区(自 lib.rs 机械拆分)。

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, destruct::*, eval::*, host::*, lookup::*,
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

/// **Self-contained materialization** of generated spans: replaces the
/// translator process's FileId number with the running process's own,
/// keeping the in-file position number (low 48 bits, deterministic for the
/// same text) as is.
///
/// Background: the generated `S{n}` constants bake in the absolute
/// numbering of the translator process's interner — a span's high 16 bits
/// are the FileId number and its low 48 bits the position (the
/// FileId/position split is `Span`'s public documented convention), and a
/// FileId number is only meaningful within the process that created it.
/// This function interns the path in the current process on **first use**
/// (`intern` is idempotent for the same path) and swaps in the current
/// process's number, so the host no longer needs a "pre-intern with the
/// same ordering" convention. Layout drift is self-checked via `id()`
/// equality.
///
/// Per-output span tables: materialize every error-site span of a
/// generated module in one shot (one `OnceLock` instead of one
/// `LazyLock` per span; sites read `sp(n)`). Entries are `(path, raw)`
/// pairs.
#[macro_export]
macro_rules! span_table {
    ($entries:expr) => {
        pub(crate) fn sp(n: usize) -> typst::syntax::Span {
            static TABLE: std::sync::OnceLock<Vec<typst::syntax::Span>> =
                std::sync::OnceLock::new();
            TABLE.get_or_init(|| {
                $entries
                    .iter()
                    .map(|(p, r)| $crate::source_span(p, *r))
                    .collect()
            })[n]
        }
    };
}

/// Argument-list segments, shared by [`call!`], [`mcall!`], and
/// [`args!`]. Segments expand **inline** (token pasting, no closure), so
/// flow expressions (`break` / `continue` / `return`) inside argument
/// values keep their enclosing loop/function targets — exactly like the
/// interpreter's inline argument evaluation. Forms:
/// - `p(span, { value })` — positional
/// - `n(span, value_span, "name", { value })` — named
/// - `s(span, { value })` — spread
/// Values are brace-wrapped blocks (both for readability and because
/// `expr` fragments may not end a parenthesized matcher).
#[doc(hidden)]
#[macro_export]
macro_rules! __args_build {
    ($args:expr;) => {};
    ($args:expr; p($span:expr, {$($v:tt)*}) $($rest:tt)*) => {
        $args.push($span, {$($v)*});
        $crate::__args_build!($args; $($rest)*);
    };
    ($args:expr; n($span:expr, $vspan:expr, $name:literal, {$($v:tt)*}) $($rest:tt)*) => {
        $args.items.push($crate::named_arg($span, $vspan, $name, {$($v)*}));
        $crate::__args_build!($args; $($rest)*);
    };
    ($args:expr; s($span:expr, {$($v:tt)*}) $($rest:tt)*) => {
        $crate::spread_args(&mut $args, $span, {$($v)*})?;
        $crate::__args_build!($args; $($rest)*);
    };
}

/// Call a resolved callee: depth check → callee value → arguments →
/// call, expanded inline. `$callee` is the callee expression (sites
/// pass `rt::callee(expr, span)?`, keeping its own resolution span).
/// Argument evaluation order matches the interpreter: depth check,
/// callee, then arguments left to right.
#[macro_export]
macro_rules! call {
    ($engine:expr, $context:expr, $at:expr, $callee:expr; $($segs:tt)*) => {{
        $crate::check_call_depth($engine, $at)?;
        let __trust_callee = $callee;
        let mut __trust_args =
            typst::foundations::Args::new($at, std::iter::empty::<typst::foundations::Value>());
        $crate::__args_build!(__trust_args; $($segs)*);
        $crate::call_with(__trust_callee, ::core::option::Option::None, $at, __trust_args, $engine, $context)?
    }};
}

/// Method call: resolve the method callee on a target, then call with
/// the target bound as the implicit first argument. Segments as in
/// [`call!`].
#[macro_export]
macro_rules! mcall {
    ($engine:expr, $context:expr, $at:expr, $target:expr, $field:literal, $access:literal, $field_at:expr, $tgt_at:expr; $($segs:tt)*) => {{
        $crate::check_call_depth($engine, $at)?;
        let __trust_target = $target;
        let __trust_mc = $crate::method_callee(__trust_target, $field, $access, $engine, $field_at)?;
        let mut __trust_args =
            typst::foundations::Args::new($at, std::iter::empty::<typst::foundations::Value>());
        $crate::__args_build!(__trust_args; $($segs)*);
        $crate::call_with(
            __trust_mc.0,
            __trust_mc.1.map(|t| (t, $tgt_at)),
            $at,
            __trust_args,
            $engine,
            $context,
        )?
    }};
}

/// Build an `Args` value from segments (for sites that pass the
/// argument list as a value rather than calling immediately, e.g.
/// mutating-method dispatch). Inline block — flow expressions in
/// argument values escape into the enclosing loop/function body.
#[macro_export]
macro_rules! args {
    ($at:expr; $($segs:tt)*) => {{
        let mut __trust_args =
            typst::foundations::Args::new($at, std::iter::empty::<typst::foundations::Value>());
        $crate::__args_build!(__trust_args; $($segs)*);
        __trust_args
    }};
}

/// The `Value` of an expression statement (`#let`/`#import` evaluate to
/// None; `eval_markup` displays it to empty content, so the sequence item
/// is empty content — not a `Value::None`).
pub fn mi_none() -> Value {
    embed(Value::None).into_value()
}

/// A whitespace sequence item (`SpaceElem`, the interpreter's
/// inter-statement whitespace node).
pub fn mi_space() -> Value {
    space().into_value()
}

/// A paragraph-break sequence item (a blank line in markup).
pub fn mi_par() -> Value {
    parbreak().into_value()
}

/// A markup sequence with run-length shorthand — "a vec that supports
/// run-length encoding". Syntax: a capacity literal first (the emitter's
/// exact item count — the vec gets one precise allocation, matching the
/// item-by-item literal's shape), then comma-separated bracket groups:
/// - `[none]` / `[none n]` — expression-statement Nones, n-fold
/// - `[space]` / `[space n]` — whitespace items, n-fold
/// - `[par]` / `[par n]` — paragraph breaks, n-fold
/// - `[<expr>]` — any other item, already in `Value` form
///
/// Expands to push/extend on a pre-sized vec: the same items in the same
/// order as the item-by-item form, with runs staying as runtime loops
/// (fewer post-expansion tokens than unrolling). One expansion level, no
/// recursion — sequence lengths are unbounded.
#[macro_export]
macro_rules! markup {
    ($cap:literal; $([$($item:tt)*]),* $(,)?) => {{
        let mut __trust_seq: ::std::vec::Vec<typst::foundations::Value> =
            ::std::vec::Vec::with_capacity($cap);
        $($crate::__markup_item!(__trust_seq; $($item)*);)*
        __trust_seq
    }};
}

/// The Label-free fast path of [`markup`](macro@markup): identical
/// bracket-group syntax, but items are **Content** (the emitter proved no
/// item can be a `Value::Label`), so the sequence builds as
/// `Vec<Content>` directly — no `Vec<Value>` intermediate, no per-item
/// `into_value()` wrapping, no label scan, no engine parameter.
#[macro_export]
macro_rules! cseq {
    ($cap:literal; $([$($item:tt)*]),* $(,)?) => {{
        let mut __trust_cseq: ::std::vec::Vec<typst::foundations::Content> =
            ::std::vec::Vec::with_capacity($cap);
        $($crate::__cseq_item!(__trust_cseq; $($item)*);)*
        __trust_cseq
    }};
}

#[doc(hidden)]
#[macro_export]
macro_rules! __cseq_item {
    ($seq:ident; none $n:literal) => {
        $seq.extend(::std::iter::repeat_n(
            $crate::embed(typst::foundations::Value::None),
            $n,
        ));
    };
    ($seq:ident; none) => {
        $seq.push($crate::embed(typst::foundations::Value::None));
    };
    ($seq:ident; space $n:literal) => {
        $seq.extend(::std::iter::repeat_n($crate::space(), $n));
    };
    ($seq:ident; space) => {
        $seq.push($crate::space());
    };
    ($seq:ident; par $n:literal) => {
        $seq.extend(::std::iter::repeat_n($crate::parbreak(), $n));
    };
    ($seq:ident; par) => {
        $seq.push($crate::parbreak());
    };
    ($seq:ident; $e:expr) => {
        $seq.push($e);
    };
}

#[doc(hidden)]
#[macro_export]
macro_rules! __markup_item {
    ($seq:ident; none $n:literal) => {
        $seq.extend(::std::iter::repeat_n($crate::mi_none(), $n));
    };
    ($seq:ident; none) => {
        $seq.push($crate::mi_none());
    };
    ($seq:ident; space $n:literal) => {
        $seq.extend(::std::iter::repeat_n($crate::mi_space(), $n));
    };
    ($seq:ident; space) => {
        $seq.push($crate::mi_space());
    };
    ($seq:ident; par $n:literal) => {
        $seq.extend(::std::iter::repeat_n($crate::mi_par(), $n));
    };
    ($seq:ident; par) => {
        $seq.push($crate::mi_par());
    };
    ($seq:ident; $e:expr) => {
        $seq.push($e);
    };
}

/// Per-file scaffolding: the `file_span` anchor plus signature wrappers
/// for the `eval`/`module` pair every generated file module carries.
/// The bodies are pasted **from the call site with their own parameter
/// names** (ident passthrough) — macro hygiene keeps macro-internal
/// bindings invisible to pasted tokens, so the bodies must name their
/// own locals; the macro contributes only types and the boilerplate
/// anchor (file_span) that references nothing from the bodies.
#[macro_export]
macro_rules! file_module {
    (
        path: $path:literal,
        state_ty: $state_ty:ident,
        eval_body($st:ident, $eng:ident, $ctx:ident) { $($ev:tt)* }
        module_body($mst:ident, $meng:ident, $mctx:ident) { $($mo:tt)* }
        file_fns { $($fns:tt)* }
    ) => {
        #[allow(non_snake_case, dead_code)]
        pub(crate) fn file_span() -> typst::syntax::Span {
            static CELL: std::sync::OnceLock<typst::syntax::Span> =
                std::sync::OnceLock::new();
            *CELL.get_or_init(|| {
                let id = typst::syntax::RootedPath::new(
                    typst::syntax::VirtualRoot::Project,
                    typst::syntax::VirtualPath::new($path).expect("path"),
                )
                .intern();
                typst::syntax::Source::new(id, String::new()).root().span()
            })
        }
        $($fns)*
        #[allow(unused_variables)]
        pub(crate) fn eval(
            $st: &std::sync::Arc<$state_ty>,
            $eng: &mut typst::engine::Engine,
            $ctx: comemo::Tracked<typst::foundations::Context>,
        ) -> typst::diag::SourceResult<()> {
            $($ev)*
        }
        #[allow(unused_variables)]
        pub(crate) fn module(
            $mst: &std::sync::Arc<$state_ty>,
            $meng: &mut typst::engine::Engine,
            $mctx: comemo::Tracked<typst::foundations::Context>,
        ) -> typst::diag::SourceResult<typst::foundations::Module> {
            $($mo)*
        }
    };
}
