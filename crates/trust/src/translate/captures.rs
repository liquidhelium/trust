//! Closure capture analysis: determine which outer names a closure subtree
//! references.
//!
//! **Mirrors the original**: `CapturesVisitor` at
//! `typst-eval/src/call.rs:708` (pub). The walk structure is branch-for-
//! branch isomorphic (each branch comment carries the line anchor); the
//! types differ, so it is not reused: the original holds a **runtime**
//! `Scopes`, resolves each ident on the spot and snapshots the binding with
//! `Binding::capture` (a miss means no capture, left to call time). This
//! crate has no runtime scopes at translation time (it depends only on
//! typst-syntax), so it produces a **name-level** free set
//! `(name, is-math-name)`; `capture_pairs` routes each name through our env
//! model into four buckets (local clone / module slot / wildcard layer /
//! pending), and runtime materialization goes through
//! `rt::capture_code_opt` / `capture_math_opt` (calling the pub
//! `Scopes::get` / `get_in_math` directly, same tiers as the original).
//!
//! Correspondence table (ours → branches of the original's visit()):
//! - Closure (defaults first, bindings shroud the body)
//!   → call.rs:764-792
//! - LetBinding (init first, binding deferred to the block boundary)
//!   → call.rs:795-806 (no enter; the binding joins the current level and
//!   only expires when the block exits — our push-without-pop plus
//!   truncation at block boundaries is equivalent)
//! - ForLoop (iterable outside, pattern shrouds the body)
//!   → call.rs:809-820
//! - ModuleImport (walk the source, bind list items)
//!   → call.rs:823-830
//! - FieldAccess / MathFieldAccess (walk target only) → call.rs:755-762
//! - CodeBlock / ContentBlock (scope boundary) → call.rs:746-753
//! - Named (the name part is not a reference, walk expr only)
//!   → call.rs:836-841 (inside the original's catch-all arm)
//! - Ident / MathIdent (capture tiers) → call.rs:739-743
//! - Keyed (walk both key and value) — the original falls into the generic
//!   children walk of its catch-all, same behavior; made explicit here
//!   with the reason noted (a computed key is a real evaluation)
//! - catch-all children walk → call.rs:843-847
//! The original manages scopes with `internal.enter()/exit()`; we implement
//! the equivalent with push/pop on the bound stack plus truncation at block
//! boundaries; membership semantics match (original
//! `internal.get().is_ok()`, ours `bound.contains`).

use typst_syntax::ast::{self, AstNode};

pub(super) fn collect_idents(
    node: &typst_syntax::SyntaxNode,
    bound: &mut Vec<String>,
    out: &mut Vec<(String, bool)>,
) {
    use typst_syntax::SyntaxKind as K;
    match node.kind() {
        K::Closure => {
            // Mirrors typst-eval call.rs:764-792: named-parameter defaults
            // are visited **before** internal.enter() (evaluated at creation
            // time in the outer scope, and must not see the preceding
            // parameter bindings).
            let Some(cl) = node.cast::<ast::Closure>() else {
                return;
            };
            for param in cl.params().children() {
                if let ast::Param::Named(named) = param {
                    collect_idents(named.expr().to_untyped(), bound, out);
                }
            }
            // Mirrors call.rs:771-790: own name + the three parameter
            // binding kinds shroud the body.
            let mut pushed = 0;
            if let Some(name) = cl.name() {
                bound.push(name.as_str().to_string());
                pushed += 1;
            }
            for param in cl.params().children() {
                match param {
                    ast::Param::Pos(pattern) => {
                        for ident in pattern.bindings() {
                            bound.push(ident.as_str().to_string());
                            pushed += 1;
                        }
                    }
                    ast::Param::Named(named) => {
                        bound.push(named.name().as_str().to_string());
                        pushed += 1;
                    }
                    ast::Param::Spread(spread) => {
                        if let Some(ident) = spread.sink_ident() {
                            bound.push(ident.as_str().to_string());
                            pushed += 1;
                        }
                    }
                }
            }
            collect_idents(cl.body().to_untyped(), bound, out);
            for _ in 0..pushed {
                bound.pop();
            }
        }
        K::LetBinding => {
            // Mirrors call.rs:795-806: init is walked first (references to
            // the same name before the binding takes effect resolve to the
            // outer scope); the binding joins the current level — a
            // statement-level let lasts until the end of its block (the pop
            // is left to the block boundary, matching the original's
            // no-enter semantics).
            let Some(lets) = node.cast::<ast::LetBinding>() else {
                return;
            };
            if let Some(init) = lets.init() {
                collect_idents(init.to_untyped(), bound, out);
            }
            for ident in lets.kind().bindings() {
                bound.push(ident.as_str().to_string());
            }
        }
        K::ModuleImport => {
            // Mirrors call.rs:823-830: the source expression is traversed
            // as usual; names of list imports are internal bindings (bound
            // at call time by the in-body import itself; bare/rename get no
            // treatment, same as the original).
            let Some(import) = node.cast::<ast::ModuleImport>() else {
                return;
            };
            collect_idents(import.source().to_untyped(), bound, out);
            if let Some(ast::Imports::Items(items)) = import.imports() {
                for item in items.iter() {
                    bound.push(item.bound_name().as_str().to_string());
                }
            }
        }
        K::ForLoop => {
            // Mirrors call.rs:809-820: the iterable stays outside, the
            // pattern bindings shroud the body.
            let Some(for_loop) = node.cast::<ast::ForLoop>() else {
                return;
            };
            collect_idents(for_loop.iterable().to_untyped(), bound, out);
            let mut pushed = 0;
            for ident in for_loop.pattern().bindings() {
                bound.push(ident.as_str().to_string());
                pushed += 1;
            }
            collect_idents(for_loop.body().to_untyped(), bound, out);
            for _ in 0..pushed {
                bound.pop();
            }
        }
        K::FieldAccess => {
            // Mirrors call.rs:755-758: the field name is not a variable
            // reference; walk the target only.
            let Some(access) = node.cast::<ast::FieldAccess>() else {
                return;
            };
            collect_idents(access.target().to_untyped(), bound, out);
        }
        // Math field access (`$arrow.t$` / three-segment chains like
        // `$arrow.t.twohead$`), mirrors call.rs:759-762: visit the target
        // only; field names do not count as captures (otherwise the `t` in
        // `$arrow.t$` is treated as a variable and reports unknown variable
        // at call time).
        K::MathFieldAccess => {
            if let Some(access) = node.cast::<ast::MathFieldAccess>() {
                collect_idents(access.target().to_untyped(), bound, out);
            }
        }
        K::Named => {
            // Mirrors call.rs:836-841 (special-cased inside the original's
            // catch-all arm): the name part of a named argument is not a
            // variable reference; walk the value expression only.
            let Some(named) = node.cast::<ast::Named>() else {
                return;
            };
            collect_idents(named.expr().to_untyped(), bound, out);
        }
        K::Keyed => {
            // The original handles this via the generic children walk of
            // its catch-all (both key and value visited): in a computed key
            // `("pt-" + str(i)): v`, outer references like str are really
            // evaluated — skipping them would make the interpreted closure
            // hit unknown variable at runtime, so the same shape is kept
            // explicitly here.
            let Some(keyed) = node.cast::<ast::Keyed>() else {
                return;
            };
            collect_idents(keyed.key().to_untyped(), bound, out);
            collect_idents(keyed.expr().to_untyped(), bound, out);
        }
        K::CodeBlock | K::ContentBlock => {
            // Mirrors call.rs:746-753: block scope; the binding set is
            // restored on exit (original internal.enter/exit, ours
            // mark/truncate).
            let mark = bound.len();
            for child in node.children() {
                collect_idents(child, bound, out);
            }
            bound.truncate(mark);
        }
        K::Ident | K::MathIdent => {
            // Tiering mirrors call.rs:739-743: Ident → capture(get)
            // (locals → global → std), MathIdent → capture(get_in_math)
            // (locals → math scope → std). The original resolves and
            // snapshots on the spot; we record (name, tier), and the tier
            // carries through to runtime materialization
            // (capture_code_opt / capture_math_opt) — math names do not
            // fall back to the global scope (issue-2885: `$rgb$` should
            // report unknown variable), and code names do not fall back to
            // the math scope.
            let in_math = node.kind() == K::MathIdent;
            let name = node.full_text().to_string();
            if !bound.contains(&name) {
                out.push((name, in_math));
            }
        }
        _ => {
            // Mirrors call.rs:843-847: everything else is traversed left to
            // right.
            for child in node.children() {
                collect_idents(child, bound, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_closure_params_not_collected() {
        let src = "#let outer(a) = { call(c: from-check => if from-check == none { none } else { from-check(1) }, d: 2); a }";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "from-check"),
            "inline closure parameters must not be collected: {out:?}"
        );
    }

    #[test]
    fn multiline_inline_closure_not_collected() {
        let src = r#"#let declare(from, cast-check) = {
  let from-cast = from.cast

  types.wrap(
    from,
    check: from-check => if from-check == none {
      if cast-check == none { none } else { from.cast }
    } else {
      from-check(1)
    },
  )
  from
}"#;
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "from-check"),
            "multiline inline closure parameters must not be collected: {out:?}"
        );
    }

    #[test]
    fn elembic_real_closure_shape() {
        let src = "            false,
            message: \"elembic: types.declare: cast 'with' can only be omitted for 'from: dictionary'. It must receive the default constructor and return a function 'casted value => your type'.\"
          )
        }

        if type(with) != function {
          assert(
            false,
            message: \"elembic: types.declare: cast 'with' must receive the default constructor and return a function 'casted value => your type'. Received \" + base.typename(with)
          )
        }

        let from-cast = from.cast

        types.wrap(
          from,
          check: from-check => if from-check == none {
            if cast-check == none {
              none
            } else if from.cast == none {
              cast-check
            } else {
              value => cast-check(from-cast(value))
            }
          } else if cast-check == none {
            from-check
          } else if from.cast == none {
            value => from-check(value) and cast-check(value)
          } else {
            value => from-check(value) and cast-check(from-cast(value))
          },

          output: (typeid,),

          cast: from-cast => if from-cast == none {
            with
          } else {
";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "from-check") && !out.iter().any(|(n, _)| n == "from-cast"),
            "real elembic shape: inline closure parameters must not be collected: {out:?}"
        );
    }

    #[test]
    fn elembic_declare_slice() {
        let src = include_str!("../../tests/fixtures/elembic-declare.txt");
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "from-check"),
            "declare slice: from-check must not be collected: {out:?}"
        );
    }

    #[test]
    fn math_field_names_not_collected() {
        let src = "#let f(x) = { box($arrow.t.twohead$); $arrows.tt$; $arrow.t$; x }";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "twohead") && !out.iter().any(|(n, _)| n == "tt"),
            "math field names must not be collected: {out:?}"
        );
        assert!(out.iter().any(|(n, _)| n == "arrow"), "target names must be collected: {out:?}");
    }
}
