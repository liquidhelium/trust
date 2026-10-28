//! Destructuring: array/dictionary patterns → Rust statements.
//!
//! Mirrors typst-eval/src/binding.rs: array/dictionary runtime dispatch,
//! with identical error messages and arity checks; `assign = true` means a
//! destructuring assignment (targets must be existing locals).

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::escape;

impl Ctx {
    pub(super) fn destructure(
        &mut self,
        pattern: ast::Pattern,
        value: String,
        env: &mut Env,
        assign: bool,
    ) -> String {
        match pattern {
            ast::Pattern::Normal(ast::Expr::Ident(ident)) if !assign => {
                // `let` is always **shadowing**: a let inside a typst block
                // does not affect an outer binding of the same name (it
                // expires when the block scope exits). Reusing the outer
                // variable would let the block-scoped let write through to
                // the outer binding — e.g. a `let result = ...` inside a
                // branch once overwrote an outer accumulated array with a
                // dictionary, and a later push then failed with
                // "type dictionary has no method `push`".
                let rust = bind_local(env, ident.as_str());
                format!("let mut {rust} = {value};\n")
            }
            ast::Pattern::Normal(expr) if assign => {
                // Destructuring-assignment leaf (Ident and at-position
                // leaves): the target resolves mutably via `Access` and
                // writes back through at_mut (no insert-on-create); see
                // `place_write_leaf`.
                self.place_write_leaf(expr, &value, env)
            }
            ast::Pattern::Normal(other) => {
                // Non-identifier leaf of a `let` pattern (mirrors the leaf
                // f of binding.rs `destructure`: anything but an Ident is
                // "cannot assign to this expression"; the value is already
                // evaluated, keep the side effects).
                let at = self.span_of(other.span());
                format!(
                    "{{ let _ = {value}; bail!({at}, \"cannot assign to this expression\"); }}\n"
                )
            }
            ast::Pattern::Placeholder(_) => format!("let _ = {value};\n"),
            ast::Pattern::Parenthesized(p) => self.destructure(p.pattern(), value, env, assign),
            ast::Pattern::Destructuring(d) => {
                let at = self.span_of(d.span());
                let id = self.fresh_id();
                let n_pattern = d
                    .items()
                    .filter(|it| matches!(it, ast::DestructuringItem::Pattern(_)))
                    .count();
                let has_spread = d
                    .items()
                    .any(|it| matches!(it, ast::DestructuringItem::Spread(_)));
                let tmp = format!("__d{id}");
                let n_items = d.items().count();
                let mut out = format!("let {tmp} = {value};\n");
                // Pre-allocate all binding names and hoist their
                // declarations outside the match: both the array and the
                // dictionary branch write the same variables by assignment.
                // **Let mode only** (leaves write pre-bound locals);
                // assignment mode never pre-binds — leaves write to
                // existing targets via classify (local/module slot), and
                // pre-binding a local would hijack a module-slot target
                // into a fresh local so the slot never updates (the root
                // cause of the destructuring-assign bug).
                for ident in pattern.bindings() {
                    if assign {
                        continue;
                    }
                    let rust = bind_local(env, ident.as_str());
                    out.push_str(&format!("let mut {rust} = Value::None;\n"));
                }

                // Array branch (the index is a runtime variable; with a
                // sink in the middle it advances by the sink size —
                // mirrors binding.rs `i += sink_size`).
                let mut arr = String::new();
                let arr_ref = format!("__a{id}");
                let idx = format!("__i{id}");
                arr.push_str(&format!("let mut {idx} = 0usize;\n"));
                for item in d.items() {
                    match item {
                        ast::DestructuringItem::Pattern(p) => {
                            let v = format!(
                                "rt::d_at(&{arr_ref}, {idx}, {n_pattern}, {has_spread}, {at})?"
                            );
                            arr.push_str(&self.destructure(p, v, env, true));
                            arr.push_str(&format!("{idx} += 1usize;\n"));
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            // sink size = 1 + len − total pattern items
                            // (mirrors binding.rs's `checked_sub` and its
                            // arity error).
                            let sink_size =
                                format!("rt::d_sink_size(&{arr_ref}, {n_items}, {at})?");
                            let n_plain = n_items - 1;
                            let v = format!(
                                "Value::Array(rt::d_rest(&{arr_ref}, {idx}, {sink_size}, {n_plain}, {at})?)"
                            );
                            match spread.sink_expr() {
                                Some(expr) => {
                                    arr.push_str(&self.destructure(
                                        ast::Pattern::Normal(expr),
                                        v,
                                        env,
                                        true,
                                    ));
                                }
                                None => arr.push_str(&format!("let _ = {v};\n")),
                            }
                            arr.push_str(&format!("{idx} += {sink_size};\n"));
                        }
                        ast::DestructuringItem::Named(_) => {
                            arr.push_str(&format!(
                                "bail!({at}, \"cannot destructure named pattern from an array\");\n"
                            ));
                        }
                    }
                }
                // Final check: without a sink, leftovers mean an arity
                // error (mirrors binding.rs `if i < len`; with a sink the
                // remainder is absorbed, always equal length).
                arr.push_str(&format!("rt::d_array_finish(&{arr_ref}, &{idx}, {at})?;\n"));

                // Dictionary branch.
                let mut dict = String::new();
                let dict_ref = format!("__k{id}");
                let mut used: Vec<String> = Vec::new();
                let mut sink: Option<(ast::Expr, String)> = None;
                for item in d.items() {
                    match item {
                        ast::DestructuringItem::Pattern(ast::Pattern::Normal(
                            ast::Expr::Ident(ident),
                        )) => {
                            let key = ident.as_str().to_string();
                            used.push(key.clone());
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", {at})?", escape(&key));
                            dict.push_str(&self.destructure(
                                ast::Pattern::Normal(ast::Expr::Ident(ident)),
                                v,
                                env,
                                true,
                            ));
                        }
                        ast::DestructuringItem::Named(named) => {
                            let key = named.name().as_str().to_string();
                            used.push(key.clone());
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", {at})?", escape(&key));
                            dict.push_str(&self.destructure(named.pattern(), v, env, true));
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            if let Some(expr) = spread.sink_expr() {
                                sink = Some((expr, format!("__r{id}")));
                            }
                        }
                        ast::DestructuringItem::Pattern(p) => {
                            let _ = p;
                            dict.push_str(&format!(
                                "bail!({at}, \"cannot destructure unnamed pattern from dictionary\");\n"
                            ));
                        }
                    }
                }
                if let Some((expr, sink_tmp)) = sink {
                    let used_list = used
                        .iter()
                        .map(|k| format!("\"{}\"", escape(k)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    dict.push_str(&format!(
                        "let {sink_tmp} = rt::d_dict_rest(&{dict_ref}, &[{used_list}]);\n"
                    ));
                    dict.push_str(&self.destructure(
                        ast::Pattern::Normal(expr),
                        format!("Value::Dict({sink_tmp})"),
                        env,
                        true,
                    ));
                }

                out.push_str(&format!(
                    "match &{tmp} {{\n    Value::Array({arr_ref}) => {{\n{arr}    }}\n    Value::Dict({dict_ref}) => {{\n{dict}    }}\n    other => bail!({at}, \"cannot destructure {{}}\", other.ty()),\n}}\n"
                ));
                out
            }
        }
    }
}
