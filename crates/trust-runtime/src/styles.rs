//! The `styles` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, destruct::*, eval::*, host::*, lookup::*,
    macros::*, slots::*, stub::*,
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

/// `#set elem(..)`: builds the style sheet (mirrors `Eval for
/// ast::SetRule`'s `target.set(engine, args)`).
pub fn set_rule(
    target: Value,
    args: Args,
    span: Span,
    engine: &mut Engine,
) -> SourceResult<typst::foundations::Styles> {
    let func = target.cast::<Func>().at(span)?;
    let element = func
        .to_element()
        .ok_or_else(|| eco_format!("only element functions can be used in set rules"))
        .at(span)?;
    Ok(element.set(engine, args)?.spanned(span).liftable())
}

/// An empty style sheet (when a set rule's condition is false; mirrors
/// `Ok(Styles::new())`).
pub fn styles_empty() -> typst::foundations::Styles {
    typst::foundations::Styles::new()
}

/// `#show selector?: transform`: builds the recipe (mirrors `Eval for
/// ast::ShowRule`: the selector is cast to `ShowableSelector` (a
/// non-showable selector errors with the original cast message at the
/// selector's span); also carries the original `show page` no-effect
/// warning and the `show par: set block(spacing: ..)` migration warning).
pub fn show_rule(
    selector: Option<Value>,
    sel_span: Span,
    transform: Value,
    span: Span,
    engine: &mut Engine,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| {
            value
                .cast::<typst::foundations::ShowableSelector>()
                .at(sel_span)
        })
        .transpose()?
        .map(|selector| selector.0);
    let transform = transform.cast::<Transformation>().at(span)?;
    let recipe = typst::foundations::Recipe::new(selector, transform, span);
    check_show_rules(engine, &recipe);
    Ok(recipe)
}

/// `#show selector?: set ..`: the transform is styles (mirrors
/// `Transformation::Style(set.eval(..)?)`).
pub fn show_rule_style(
    selector: Option<Value>,
    sel_span: Span,
    styles: typst::foundations::Styles,
    span: Span,
    engine: &mut Engine,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| {
            value
                .cast::<typst::foundations::ShowableSelector>()
                .at(sel_span)
        })
        .transpose()?
        .map(|selector| selector.0);
    let recipe = typst::foundations::Recipe::new(selector, Transformation::Style(styles), span);
    check_show_rules(engine, &recipe);
    Ok(recipe)
}

/// The original's two show-rule warnings (mirrors `check_show_page_rule`
/// and `check_show_par_set_block`).
fn check_show_rules(engine: &mut Engine, recipe: &typst::foundations::Recipe) {
    if let Some(Selector::Elem(elem, _)) = recipe.selector()
        && *elem == typst::foundations::Element::of::<typst::layout::PageElem>()
    {
        engine.sink.warn(
            SourceDiagnostic::warning(
                recipe.span(),
                typst::foundations::Str::from("`show page` is not supported and has no effect"),
            )
            .with_hint("customize pages with `set page(..)` instead"),
        );
    }
    if let Some(Selector::Elem(elem, _)) = recipe.selector()
        && *elem == typst::foundations::Element::of::<typst::model::ParElem>()
        && let Transformation::Style(styles) = recipe.transform()
    {
        let chain = typst::foundations::StyleChain::new(styles);
        if chain.has(typst::layout::BlockElem::above) || chain.has(typst::layout::BlockElem::below)
        {
            engine.sink.warn(
                SourceDiagnostic::warning(
                    recipe.span(),
                    typst::foundations::Str::from(
                        "`show par: set block(spacing: ..)` has no effect anymore",
                    ),
                )
                .with_hint("write `set par(spacing: ..)` instead")
                .with_hint(
                    "this is specific to paragraphs as they are not considered blocks anymore",
                ),
            );
        }
    }
}

/// Attaches styles to the tail content after a rule (mirrors eval_markup's
/// `tail.styled_with_map(styles)`).
pub fn styled_with_map(
    tail: typst::foundations::Content,
    styles: typst::foundations::Styles,
) -> typst::foundations::Content {
    tail.styled_with_map(styles)
}

/// Attaches a show rule to the tail content after a rule (mirrors
/// `tail.styled_with_recipe`; a rule without a selector applies
/// immediately).
pub fn styled_with_recipe(
    engine: &mut Engine,
    context: Tracked<Context>,
    tail: typst::foundations::Content,
    recipe: typst::foundations::Recipe,
) -> SourceResult<typst::foundations::Content> {
    tail.styled_with_recipe(engine, context, recipe)
}
