//! Flow events (break/continue/return): the generated code mirrors
//! typst-eval's `vm.flow` — a per-frame `Option<FlowEvent>` flag.
//!
//! Semantics (typst-eval code.rs/flow.rs): a flow statement only **sets
//! the flag** (its own expression value is `none`); the flag is checked at
//! **statement boundaries** (after each statement's join in a code block)
//! and at loop iteration boundaries (Break/Continue consumed, Return
//! propagates). Argument evaluation performs no checks — a call whose
//! argument contains `break` still evaluates all its arguments and runs.
//! Every block/loop **saves and clears** the flag on entry and restores it
//! on exit, so a nested expression (e.g. a later argument's inner loop)
//! runs to completion under a set flag and the flag survives for the
//! enclosing boundary. A `let` whose init set the flag skips its binding;
//! a set/show rule whose args set the flag skips its tail.

use typst::foundations::Value;
use typst::syntax::Span;

#[derive(Clone)]
pub enum FlowEvent {
    Break,
    Continue,
    /// `return expr` carries the value (a bare `return` yields the
    /// function-body partial join at its boundary), the return's span
    /// (for the discarded-content warning), and the conditional marker
    /// (a return flowing out of an `if`/loop arm is conditional and does
    /// not warn).
    Return(Option<Value>, Span, bool),
}

impl FlowEvent {
    /// Mark a propagated return as conditional (mirrors the marking in
    /// typst-eval's Conditional/While/For evals).
    pub fn mark_conditional(&mut self) {
        if let FlowEvent::Return(_, _, cond) = self {
            *cond = true;
        }
    }
}

/// The boundary warning for an unconditional `return expr` that discards
/// joined content (mirrors typst-eval code.rs's `warn_for_discarded_content`
/// verbatim, including the state/counter hint).
#[allow(clippy::match_like_matches_macro)]
pub fn warn_return_discard(
    engine: &mut typst::engine::Engine,
    flow: &Option<FlowEvent>,
    joined: Value,
) {
    use typst::foundations::Selector;
    use typst::introspection::{Counter, State};
    let Some(FlowEvent::Return(_, span, false)) = flow else {
        return;
    };
    let Value::Content(tree) = &joined else {
        return;
    };

    let selector = Selector::Or(ecow::eco_vec![State::select_any(), Counter::select_any()]);

    let mut warning = typst::diag::warning!(
        *span,
        "this return unconditionally discards the content before it";
        hint: "try omitting the `return` to automatically join all values";
    );

    if tree.query_first_naive(&selector).is_some() {
        warning.hint(
            "state/counter updates are content that must end up in the document to have an effect",
        );
    }

    engine.sink.warn(warning);
}
