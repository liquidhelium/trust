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
//!
//! The original manages scopes with `internal.enter()/exit()`; we implement
//! the equivalent with push/pop on the bound stack plus truncation at block
//! boundaries; membership semantics match (original
//! `internal.get().is_ok()`, ours `bound.contains`).

use typst_syntax::ast::{self, AstNode};

/// The name a bare `import <source>` binds, as the upstream accessor
/// computes it (`ast::ModuleImport::bare_name`: ident/field access, spec
/// name, or file stem validated as an identifier). Emission sites report
/// the error forms with upstream's wording.
pub(super) fn bare_import_name(
    import: ast::ModuleImport,
) -> Result<String, ast::BareImportError> {
    import.bare_name().map(|name| name.to_string())
}

/// Analysis fallback for capture bookkeeping when the upstream accessor
/// errors (the import itself will error at runtime; the name never
/// binds, so the guess is inert): a Str source that parses as a package
/// spec binds the package name; any other Str source binds the file
/// stem; a non-Str source (dotted path / call result) binds its last
/// dotted segment.
pub(super) fn bare_import_bound_name(source: ast::Expr) -> String {
    if let ast::Expr::Str(s) = source {
        let raw = s.get();
        if let Some(rest) = raw.strip_prefix('@') {
            // `@namespace/name:version` binds the package name.
            if let Some(name) = rest
                .split('/')
                .next_back()
                .and_then(|n| n.split(':').next())
            {
                return name.to_string();
            }
        }
        let file = raw.rsplit('/').next().unwrap_or(&raw);
        return file.split('.').next().unwrap_or(file).to_string();
    }
    source
        .to_untyped()
        .full_text()
        .rsplit('.')
        .next()
        .unwrap_or("m")
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .next()
        .unwrap_or("m")
        .to_string()
}

/// Whether a subtree contains a wildcard module import (`import x: *`).
/// Names such an import may bind are unknowable at translation time; capture
/// routing must leave those names to the body's own runtime layers (mirrors
/// typst: an unresolvable name is simply not captured, and the body's
/// in-scope import binds it at call time).
pub(super) fn has_wildcard_import(node: &typst_syntax::SyntaxNode) -> bool {
    if let Some(import) = node.cast::<ast::ModuleImport>()
        && matches!(import.imports(), Some(ast::Imports::Wildcard))
    {
        return true;
    }
    node.children().any(has_wildcard_import)
}

/// Free names of a **top-level let-closure's** body, with the closure's own
/// bindings (name/params/sink) pre-seeded — parameters are not free names
/// (otherwise `__merge(base, …)`'s base was collected as a global slot and
/// reported unknown variable at call time; mirrors CapturesVisitor's Closure
/// branch binding before walking the body).
pub(super) fn top_closure_free_names(closure: ast::Closure) -> Vec<(String, bool)> {
    let mut bound: Vec<String> = Vec::new();
    if let Some(name) = closure.name() {
        bound.push(name.as_str().to_string());
    }
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(pattern) => {
                bound.extend(
                    pattern
                        .bindings()
                        .into_iter()
                        .map(|i| i.as_str().to_string()),
                );
            }
            ast::Param::Named(named) => bound.push(named.name().as_str().to_string()),
            ast::Param::Spread(spread) => {
                if let Some(ident) = spread.sink_ident() {
                    bound.push(ident.as_str().to_string());
                }
            }
        }
    }
    let mut names = Vec::new();
    collect_idents(closure.body().to_untyped(), &mut bound, &mut names);
    names
}

/// One creation-time global capture of a top-level let-closure.
pub(super) struct GlobalCapture {
    /// The free name (typst-side).
    pub name: String,
    /// Minted slot: `G_{owner}_{name}`.
    pub slot: String,
    /// Math name (`capture_math_opt`) vs code name (`capture_code_opt`).
    pub in_math: bool,
}

/// Which free names of a top-level let-closure take the creation-time
/// global-slot protocol (a `G_` fill at module init; body refs read the
/// slot with a global fallback): deduped, module-scope names excluded
/// (they resolve statically), and — when the body contains a wildcard
/// import — everything left uncaptured (those names resolve through the
/// body's own runtime layers at call time, same as typst).
pub(super) fn global_capture_slots(
    names: Vec<(String, bool)>,
    module_env: &[(String, super::ctx::Ref)],
    body_has_wildcard: bool,
    owner_slot: &str,
) -> Vec<GlobalCapture> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (name, in_math) in names {
        if !seen.insert(name.clone()) {
            continue;
        }
        if module_env.iter().any(|(n, _)| *n == name) {
            continue;
        }
        if body_has_wildcard {
            continue;
        }
        out.push(GlobalCapture {
            slot: format!("G_{owner_slot}_{}", super::naming::sanitize_key(&name)),
            name,
            in_math,
        });
    }
    out
}

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
            // treatment, same as the original — the original cannot know a
            // bare import's runtime name and so leaves its body references
            // as free names, silently dropped when the creation-time lookup
            // misses. We derive the bare name statically (Str sources carry
            // the path), which matches the runtime derivation and keeps the
            // body references non-free).
            let Some(import) = node.cast::<ast::ModuleImport>() else {
                return;
            };
            collect_idents(import.source().to_untyped(), bound, out);
            match import.imports() {
                Some(ast::Imports::Items(items)) => {
                    for item in items.iter() {
                        bound.push(item.bound_name().as_str().to_string());
                    }
                }
                None => {
                    if let Some(new_name) = import.new_name() {
                        bound.push(new_name.as_str().to_string());
                    } else {
                        bound.push(bare_import_bound_name(import.source()));
                    }
                }
                Some(ast::Imports::Wildcard) => {}
            }
        }
        K::DestructAssignment => {
            // The pattern of `(a, b) = value` holds no references: dict keys
            // are keys, not idents, and the sub-pattern identifiers are
            // bindings. The original walks the pattern through its catch-all
            // (keys become capture *attempts* that miss silently; a hit only
            // creates an unused capture). We bind the pattern's names and
            // skip the walk: observable behavior matches (an unused capture
            // is invisible), and the keys never pollute the free set.
            let Some(assignment) = node.cast::<ast::DestructAssignment>() else {
                return;
            };
            collect_idents(assignment.value().to_untyped(), bound, out);
            for ident in assignment.pattern().bindings() {
                bound.push(ident.as_str().to_string());
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
            !out.iter().any(|(n, _)| n == "from-check")
                && !out.iter().any(|(n, _)| n == "from-cast"),
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
        assert!(
            out.iter().any(|(n, _)| n == "arrow"),
            "target names must be collected: {out:?}"
        );
    }

    #[test]
    fn destruct_assignment_keys_not_collected() {
        // cetz `draw.scope` shape: the dict key `bounds` in a destructuring
        // assignment is a key, not a reference — collecting it made the
        // closure carry a bogus pending capture that errored "unknown
        // variable: bounds" at call time.
        let src = "#let scope(body) = (ctx => { (ctx: group-ctx, drawables, bounds: _) = process.many(group-ctx, body) })";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "bounds"),
            "destructuring-assignment dict keys must not be collected: {out:?}"
        );
        assert!(
            out.iter().any(|(n, _)| n == "process") && out.iter().any(|(n, _)| n == "group-ctx"),
            "value-side references must still be collected: {out:?}"
        );
    }

    #[test]
    fn bare_string_import_binds_file_stem() {
        // cetz-plot `setup-axes` shape: an in-body `import "/src/axes.typ"`
        // binds `axes` (file stem); previously the derivation took the
        // extension and bound `typ`, leaving body references free.
        let src = "#let f() = { import \"/src/axes.typ\"; axes.prepare-axis() }";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.iter().any(|(n, _)| n == "axes"),
            "in-body bare import must bind its file stem: {out:?}"
        );
        let bare = typst_syntax::parse_code("\"/src/axes.typ\"")
            .children()
            .find_map(|c| c.cast::<typst_syntax::ast::Expr>())
            .map(bare_import_bound_name)
            .expect("expr");
        assert_eq!(bare, "axes");
        let pkg = typst_syntax::parse_code("\"@preview/cetz:0.5.2\"")
            .children()
            .find_map(|c| c.cast::<typst_syntax::ast::Expr>())
            .map(bare_import_bound_name)
            .expect("expr");
        assert_eq!(pkg, "cetz");
    }
}
