//! The `content` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, context::*, destruct::*, eval::*, host::*, lookup::*, macros::*, slots::*,
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

/// Markup smart quotes (`'` / `"`): builds a smartquote element (the
/// open/close and language-dependent resolution semantics are left to
/// layout; mirrors eval_markup's SmartQuoteElem).
pub fn smartquote(double: bool) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    typst::text::SmartQuoteElem::new()
        .with_double(double)
        .pack()
}

/// Markup sequence folding (mirrors eval_markup's per-item dispatch): **a
/// label item attaches to the last attachable element in the sequence**
/// (`labelled`; a standalone label content item would be invisible to
/// queries); other values enter the sequence via display, and the result is
/// folded with Content::sequence at the end.
pub fn attach_labels(engine: &mut Engine, items: Vec<Value>) -> typst::foundations::Content {
    let mut seq: Vec<typst::foundations::Content> = Vec::with_capacity(items.len());
    for item in items {
        if let Value::Label(label) = item {
            // Mirrors eval_markup's two label-attachment warnings: already
            // labelled → "content labelled multiple times"; no attachable
            // element in the sequence → "label `x` is not attached to
            // anything" (warning spans may be the element's or detached;
            // only the message text is compared).
            if let Some(elem) = seq
                .iter_mut()
                .rev()
                .find(|n| !n.can::<dyn typst::foundations::Unlabellable>())
            {
                if elem.label().is_some() {
                    engine.sink.warn(typst::diag::warning!(
                        elem.span(),
                        "content labelled multiple times";
                        hint: "only the last label is used, the rest are ignored";
                    ));
                }
                *elem = std::mem::take(elem).labelled(label);
            } else {
                engine.sink.warn(typst::diag::warning!(
                    Span::detached(),
                    "label `<{}>` is not attached to anything",
                    label.resolve(),
                ));
            }
        } else {
            seq.push(item.display());
        }
    }
    typst::foundations::Content::sequence(seq)
}

/// `= body` (a level-depth heading): mirrors eval_markup's
/// `HeadingElem::new(body).with_depth(depth).pack()` spanned with the
/// heading's source span (the interpreter's `Eval for ast::Expr` wraps
/// every value in `.spanned(span)`; `hash128(&elem)` includes the span, so
/// a detached span breaks measure-introspection key matching between the
/// measured copy and the document copy — the convergence loop then
/// stabilizes where the interpreter's does not).
pub fn heading(
    body: typst::foundations::Content,
    depth: usize,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    // ast::Heading::depth() is the equals-sign count and is a NonZeroUsize
    // ≥ 1 by grammar; the generated literal passes it through verbatim.
    let depth = std::num::NonZeroUsize::new(depth).expect("heading depth >= 1 (grammar)");
    typst::model::HeadingElem::new(body)
        .with_depth(depth)
        .pack()
        .spanned(at)
}

/// `- body` list item: mirrors `ListItem::new(body).pack()` (consecutive
/// items are grouped into a List at realize time; no aggregation needed on
/// the translation side). Spanned like the interpreter's per-expression
/// `.spanned(span)` wrapper.
pub fn list_item(
    body: typst::foundations::Content,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    typst::model::ListItem::new(body).pack().spanned(at)
}

/// `+ body` enum item (`number` is the explicit index in the `+3.` form).
/// Spanned like the interpreter's per-expression `.spanned(span)` wrapper.
pub fn enum_item(
    body: typst::foundations::Content,
    number: Option<u64>,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::{NativeElement, Smart};
    let mut elem = typst::model::EnumItem::new(body);
    if let Some(n) = number {
        elem.number.set(Smart::Custom(n));
    }
    elem.pack().spanned(at)
}

/// `/ term: desc` term item. Spanned like the interpreter's
/// per-expression `.spanned(span)` wrapper.
pub fn term_item(
    term: typst::foundations::Content,
    description: typst::foundations::Content,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    typst::model::TermItem::new(term, description).pack().spanned(at)
}

/// Bare link in markup (`https://…`): mirrors `Url::new(...)` +
/// `LinkElem::from_url(url).pack()`.
pub fn link(url: &str, at: Span) -> SourceResult<typst::foundations::Content> {
    use typst::foundations::NativeElement;
    let url = typst::model::Url::new(url.to_string()).at(at)?;
    Ok(typst::model::LinkElem::from_url(url).pack().spanned(at))
}

/// `*body*`: mirrors eval_markup's `StrongElem::new(body).pack()` spanned
/// with the expression's source span.
pub fn strong(
    body: typst::foundations::Content,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    typst::model::StrongElem::new(body).pack().spanned(at)
}

/// `_body_`: mirrors eval_markup's `EmphElem::new(body).pack()` spanned
/// with the expression's source span.
pub fn emph(
    body: typst::foundations::Content,
    at: Span,
) -> typst::foundations::Content {
    use typst::foundations::NativeElement;
    typst::model::EmphElem::new(body).pack().spanned(at)
}

/// Markup escapes/shorthands: mirrors the original
/// `Value::Symbol(runtime_char)` (note: **not** a text element — the two
/// have the same repr shape but different types, so equality tells them
/// apart; translating `[\[]` to text would make `== [\[]` always false).
/// The caller's display materializes it into a symbol element when it
/// enters a content sequence.
pub fn symbol_value(c: char) -> Value {
    Value::Symbol(typst::foundations::Symbol::runtime_char(c))
}

/// Code-position Raw: builds a RawElem from the line list + language +
/// block flag (mirrors typst-eval's `Eval for ast::Raw`; line spans are
/// detached — diagnostic-only, does not affect the value).
pub fn raw(lines: EcoVec<EcoString>, lang: Option<EcoString>, block: bool) -> Value {
    use typst::foundations::NativeElement;
    let lines: EcoVec<(EcoString, Span)> =
        lines.into_iter().map(|l| (l, Span::detached())).collect();
    let mut elem =
        typst::text::RawElem::new(typst::text::RawContent::Lines(lines)).with_block(block);
    if let Some(lang) = lang {
        elem.lang.set(Some(lang));
    }
    Value::Content(elem.pack())
}

/// A text run. Spanned with the text's source span (the interpreter's
/// `Value::Content(..).spanned(span)` in `Eval for ast::Expr`).
pub fn text(s: &str, at: Span) -> typst::foundations::Content {
    typst::text::TextElem::packed(typst::foundations::Str::from(s)).spanned(at)
}

/// A space.
pub fn space() -> typst::foundations::Content {
    typst::text::SpaceElem::shared().clone()
}

/// A soft linebreak `\`.
pub fn linebreak() -> typst::foundations::Content {
    typst::text::LinebreakElem::shared().clone()
}

/// A paragraph break.
pub fn parbreak() -> typst::foundations::Content {
    typst::model::ParbreakElem::shared().clone()
}

/// Markup sequence composition (mirrors `Content::sequence`).
pub fn content_sequence(parts: Vec<typst::foundations::Content>) -> typst::foundations::Content {
    typst::foundations::Content::sequence(parts)
}

/// Converts an embedded expression's value to content (mirrors the display
/// semantics of `#expr` in markup).
pub fn embed(value: Value) -> typst::foundations::Content {
    value.display()
}

/// `@target` reference shorthand (mirrors typst-eval markup.rs's `Eval for
/// ast::Ref` → `RefElem::new(label)` + optional supplement; without it,
/// references and bibliographies render as nothing).
pub fn ref_elem(target: &str, supplement: Option<Value>) -> typst::foundations::Content {
    use typst::foundations::IntoValue;
    let label = typst::foundations::Label::construct(target.into())
        .unwrap_or_else(|_| panic!("reference target must not be empty: {target}"));
    let mut elem = typst::model::RefElem::new(label);
    if let Some(v) = supplement {
        elem.supplement.set(typst::foundations::Smart::Custom(Some(
            typst::model::Supplement::Content(v.display()),
        )));
    }
    match elem.into_value() {
        typst::foundations::Value::Content(c) => c,
        _ => unreachable!("RefElem packs to content"),
    }
}

/// Label literal `<name>` (mirrors `Eval for ast::Label`).
pub fn label(name: &str) -> Value {
    let label = typst::foundations::Label::construct(name.into())
        .unwrap_or_else(|_| panic!("label name must not be empty: {name}"));
    Value::Label(label)
}
