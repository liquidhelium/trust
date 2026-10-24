//! Direct expression translation: typst expressions → Rust expressions
//! returning `Value` (may contain `?`).
//!
//! The entry point `Ctx::expr` dispatches on expression kind; the other
//! methods (ident resolution, binary operations, calls, call-site
//! arguments, closures) are its internal steps.

use std::collections::HashSet;
use std::fmt::Write as _;

use typst_syntax::SyntaxKind;
use typst_syntax::ast::{self, AstNode};

use super::captures::collect_idents;
use super::ctx::{Ctx, Env};
use super::naming::{escape, sanitize_key};

/// Explicit mapping `ast::Unit` → Rust variant name (does not depend on
/// Debug output shape, so a typst rename becomes a compile error instead of
/// a silently wrong value).
fn unit_variant(unit: ast::Unit) -> &'static str {
    match unit {
        ast::Unit::Pt => "Pt",
        ast::Unit::Mm => "Mm",
        ast::Unit::Cm => "Cm",
        ast::Unit::In => "In",
        ast::Unit::Rad => "Rad",
        ast::Unit::Deg => "Deg",
        ast::Unit::Em => "Em",
        ast::Unit::Fr => "Fr",
        ast::Unit::Percent => "Percent",
    }
}

/// A content item + the binding set visible when it was generated
/// (top-level content has per-item visibility in source order, mirroring
/// typst's sequential markup evaluation; uniform inside a content block).
pub(super) struct TopItem<'a> {
    pub item: ContentItem<'a>,
    pub env: Env,
}

/// One item of a content stream: a plain expression / set rule / show
/// rule / include. Top-level content and content blocks share the same
/// folding pipeline.
#[derive(Clone)]
pub(super) enum ContentItem<'a> {
    Part(ast::Expr<'a>),
    Set(ast::SetRule<'a>),
    Show(ast::ShowRule<'a>),
    /// Package mode: the target file's body slot name.
    Include(String),
    /// Dynamic-source include (zone-1 glue): carries the emitted
    /// evaluation code (`rt::eval_include` drives the original Eval;
    /// file-not-found and friends are reported by the original).
    DynamicInclude(String),
}

impl Ctx {
    /// Direct expression translation: returns a Rust expression of type
    /// `Value` (may contain `?`).
    pub(super) fn expr(&mut self, expr: ast::Expr, env: &mut Env) -> String {
        match expr {
            ast::Expr::Ident(ident) => self.ident(ident, env),
            ast::Expr::None(_) => "Value::None".into(),
            ast::Expr::Auto(_) => "Value::Auto".into(),
            ast::Expr::Bool(b) => format!("Value::Bool({})", b.get()),
            ast::Expr::Int(i) => format!("Value::Int({})", i.get()),
            // inf/nan Debug shapes are not valid Rust literals; go through
            // constants.
            ast::Expr::Float(f) if f.get().is_infinite() => format!(
                "Value::Float(f64::{}INFINITY)",
                if f.get() > 0.0 { "" } else { "-" }
            ),
            ast::Expr::Float(f) if f.get().is_nan() => "Value::Float(f64::NAN)".to_string(),
            ast::Expr::Float(f) => format!("Value::Float({:?})", f.get()),
            ast::Expr::Numeric(n) => {
                let (value, unit) = n.get();
                format!("Value::numeric(({value:?}, Unit::{}))", unit_variant(unit))
            }
            ast::Expr::Str(s) => format!("Value::Str({:?}.into())", s.get().as_str()),
            ast::Expr::Raw(raw) => {
                // Code-position Raw (mirrors typst-eval markup.rs's `Eval
                // for ast::Raw`): build RawElem from the line table +
                // language + block flag; field accesses like `.text` go
                // through element fields (same semantics as native).
                let lines = raw
                    .lines()
                    .map(|l| format!("{:?}.into()", l.get().as_str()))
                    .collect::<Vec<_>>()
                    .join(", ");
                let lang = match raw.lang() {
                    Some(l) => format!("Some({:?}.into())", l.get().as_str()),
                    None => "None".to_string(),
                };
                format!(
                    "rt::raw(eco_vec![{lines}], {lang}, {})",
                    raw.block()
                )
            }
            ast::Expr::Parenthesized(p) => self.expr(p.expr(), env),
            ast::Expr::CodeBlock(block) => self.block(block.body().exprs(), env),
            ast::Expr::DestructAssignment(assign) => {
                // Destructuring assignment (expression position; top-level
                // marked statements take this same arm; mirrors binding.rs
                // `Eval for ast::DestructAssignment`: value evaluated
                // first, leaves written back via access). Assign leaves
                // bind no new names; the env clone only satisfies the &mut
                // signature.
                let value = self.expr(assign.value(), env);
                let mut env2 = env.clone();
                let stmts = self.destructure(assign.pattern(), value, &mut env2, true);
                format!("{{ {} Value::None }}", stmts)
            }
            ast::Expr::LetBinding(lets) => {
                // Expression-position `let` (mirrors `Eval for
                // ast::LetBinding`: init evaluated first, then bound, value
                // is none): the binding name is pushed into env **at its
                // position** (references before the position still resolve
                // to the outer scope), and the value goes into a hoisted
                // slot registered by the enclosing scan (see the scans in
                // block/content_part; unscanned contexts degrade).
                self.let_expr(lets, env)
            }
            ast::Expr::ContentBlock(block) => {
                // Direct translation of a content block: the subset
                // Text/Space/Linebreak/Parbreak/Escape/embedded expressions
                // + set/show rules; construction mirrors
                // typst-eval/src/markup.rs (rules wrap all subsequent
                // content).
                let items: Vec<ContentItem> = block
                    .body()
                    .exprs()
                    .map(|expr| match expr {
                        ast::Expr::SetRule(set) => ContentItem::Set(set),
                        ast::Expr::ShowRule(show) => ContentItem::Show(show),
                        other => ContentItem::Part(other),
                    })
                    .collect();
                format!("Value::Content({})", self.content_seq(&items, env))
            }
            ast::Expr::ModuleInclude(include) => {
                // `#include "x.typ"` (package mode): the target module's
                // body content (mirrors `Eval for ast::ModuleInclude` ->
                // Content).
                match include.source() {
                    ast::Expr::Str(path) => {
                        let raw = path.get().to_string();
                        let (Some(file), Some(files), Some(mods)) = (
                            self.current_file.clone(),
                            self.package_files.clone(),
                            self.package_mods.clone(),
                        ) else {
                            self.diag(path.span(), "include is only supported in package mode".into());
                            return "Value::None".into();
                        };
                        let target = super::paths::normalize_import(&file, &raw);
                        let mod_name = mods.get(&target).cloned();
                        match (files.contains_key(&target), mod_name) {
                            (true, Some(mod_name)) => {
                                self.used_state = true;
                                format!("Value::Content(rt::read_content(&__st.BODY_{mod_name})?)")
                            }
                            // Not in the file set / arbitrary expression
                            // source: zone-1 glue — rt::eval_include drives
                            // the original Eval; the world may still
                            // resolve it, and file-not-found and friends
                            // are reported by the original.
                            _ => self.include_glue_expr(expr, env),
                        }
                    }
                    _ => self.include_glue_expr(expr, env),
                }
            }
            ast::Expr::Contextual(ctx) => {
                // `#context expr`: build a deferred-evaluation closure
                // (mirrors `Eval for ast::Contextual`), evaluated during
                // realize/layout with location/styles.
                let body = ctx.body().to_untyped().full_text().to_string();
                let mut names: Vec<(String, bool)> = Vec::new();
                collect_idents(ctx.body().to_untyped(), &mut Vec::new(), &mut names);
                let (captured, pending) = self.capture_entries(&names, env);
                // pending (names outside locals/module slots) materialize
                // at runtime through the global + math scopes (mirrors the
                // original CapturesVisitor's get/get_in_math).
                let pending_lit = pending_list(&pending);
                if captured.is_empty() {
                    format!(
                        "Value::Content(rt::context_body(rt::context_closure({body:?}, &[], {pending_lit}, engine)))"
                    )
                } else {
                    // Constructed and consumed inside the block (&__caps
                    // never escapes it).
                    let init = format!("vec![{}]", captured.join(", "));
                    format!(
                        "{{ let __caps: Vec<(&'static str, Value)> = {init}; Value::Content(rt::context_body(rt::context_closure({body:?}, &__caps, {pending_lit}, engine))) }}"
                    )
                }
            }
            ast::Expr::Array(array) => {
                let mut out = String::from("{ let mut __a: Vec<Value> = Vec::new();");
                for item in array.items() {
                    match item {
                        ast::ArrayItem::Pos(expr) => {
                            let value = self.expr(expr, env);
                            let _ = write!(out, " __a.push({value});");
                        }
                        ast::ArrayItem::Spread(spread) => {
                            let value = self.expr(spread.expr(), env);
                            let at = self.span_of(spread.span());
                            let _ = write!(out, " rt::array_spread(&mut __a, {value}, {at})?;");
                        }
                    }
                }
                out.push_str(" rt::array(__a).into_value() }");
                out
            }
            ast::Expr::Dict(dict) => {
                let mut out = String::from("{ let mut __d = Dict::new();");
                for item in dict.items() {
                    match item {
                        ast::DictItem::Named(named) => {
                            let key = named.name().as_str();
                            let value = self.expr(named.expr(), env);
                            let _ = write!(
                                out,
                                " rt::dict_put(&mut __d, \"{}\", {value});",
                                escape(key)
                            );
                        }
                        ast::DictItem::Keyed(keyed) => {
                            // The key is an expression: evaluate then cast
                            // to string (mirrors `Eval for
                            // ast::Dict::Keyed`).
                            let key = self.expr(keyed.key(), env);
                            let key_at = self.span_of(keyed.key().span());
                            let value = self.expr(keyed.expr(), env);
                            let _ = write!(
                                out,
                                " rt::dict_put_key(&mut __d, {key}, {value}, {key_at})?;"
                            );
                        }
                        ast::DictItem::Spread(spread) => {
                            let value = self.expr(spread.expr(), env);
                            let at = self.span_of(spread.span());
                            let _ = write!(out, " rt::dict_spread(&mut __d, {value}, {at})?;");
                        }
                    }
                }
                out.push_str(" Value::Dict(__d) }");
                out
            }
            ast::Expr::Unary(unary) => {
                let inner = self.expr(unary.expr(), env);
                let op = match unary.op() {
                    ast::UnOp::Pos => "pos",
                    ast::UnOp::Neg => "neg",
                    ast::UnOp::Not => "not",
                };
                {
                    let at = self.span_of(unary.span());
                    format!("ops::{op}({inner}).at({at})?")
                }
            }
            ast::Expr::Binary(binary) => self.binary(binary, env),
            ast::Expr::FieldAccess(access) => {
                let target = self.expr(access.target(), env);
                let field = access.field().as_str();
                {
                    // Mirrors access_field: the evaluation/error site is
                    // the field-name span.
                    let at = self.span_of(access.field().span());
                    format!(
                        "rt::field(&({target}), \"{}\", engine, context, {at})?",
                        escape(field)
                    )
                }
            }
            ast::Expr::FuncCall(call) => self.func_call(call, env),
            ast::Expr::Closure(closure) => self.closure_expr(closure, env),
            ast::Expr::Conditional(cond) => {
                let condition = self.expr(cond.condition(), env);
                let then = self.expr(cond.if_body(), env);
                let otherwise = match cond.else_body() {
                    Some(e) => self.expr(e, env),
                    None => "Value::None".into(),
                };
                let at = self.span_of(cond.span());
                format!("if rt::boolean({condition}, {at})? {{ {then} }} else {{ {otherwise} }}")
            }
            ast::Expr::WhileLoop(while_loop) => {
                let condition = self.expr(while_loop.condition(), env);
                self.in_loop += 1;
                let body = self.expr(while_loop.body(), env);
                self.in_loop -= 1;
                let cond_at = self.span_of(while_loop.condition().span());
                let join_at = self.span_of(while_loop.body().span());
                let loop_at = self.span_of(while_loop.span());
                // Static "always true" detection (mirrors the original's
                // i==0 branch with is_invariant/can_diverge): when the
                // condition is invariant and the body contains no
                // break/return, a truthy first evaluation is an error;
                // **a false condition is a legal exit** — `while false {}`
                // returns none without an error (the condition's side
                // effects are still evaluated first).
                if is_invariant(while_loop.condition().to_untyped())
                    && !can_diverge(while_loop.body().to_untyped())
                {
                    return format!(
                        "{{ let __c = rt::boolean({condition}, {cond_at})?; if __c {{ bail!({cond_at}, \"condition is always true\") }} else {{ Value::None }} }}"
                    );
                }
                // Iteration guard (mirrors the original MAX_ITERATIONS:
                // exceeding it reports "loop seems to be infinite" instead
                // of hanging).
                format!(
                    "{{ let mut __out = Value::None; let mut __guard = 0usize;\n  \
                     while rt::boolean({condition}, {cond_at})? {{\n  \
                     \x20   if __guard >= rt::MAX_ITERATIONS {{ bail!({loop_at}, \"loop seems to be infinite\"); }}\n  \
                     \x20   __guard += 1usize;\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, {join_at})?;\n  \
                     }}\n  \
                     __out }}"
                )
            }
            ast::Expr::FuncReturn(func_return) => {
                // `return` compiles to a Rust return (from the generated
                // f_ function); outside a function body (module
                // evaluation, default values, ...) the enclosing Rust
                // function has a different return type, and a direct
                // return would not compile — degrade with a diagnostic.
                if self.in_fn == 0 {
                    let message = "return is only supported inside a function body (v1 language subset)".to_string();
                    self.diag(func_return.span(), message.clone());
                    return format!(
                        "{{ /* trust-degraded: {} */ rt::degraded(engine, {:?})? }}",
                        message, message
                    );
                }
                let value = match func_return.body() {
                    Some(expr) => self.expr(expr, env),
                    None => "Value::None".into(),
                };
                format!("return Ok({value}) /* never */")
            }
            ast::Expr::LoopBreak(lb) => {
                // break outside a loop (mirrors typst-eval's semantic
                // error; driven by the error tests): degrades to a
                // runtime error with the same message.
                if self.in_loop == 0 {
                    let message = "cannot use `break` outside of a loop".to_string();
                    self.diag(lb.span(), message.clone());
                    return format!("{{ rt::bail_value(engine, {:?})? }}", message);
                }
                "{ break; Value::None }".into()
            }
            ast::Expr::LoopContinue(lc) => {
                if self.in_loop == 0 {
                    let message = "cannot use `continue` outside of a loop".to_string();
                    self.diag(lc.span(), message.clone());
                    return format!("{{ rt::bail_value(engine, {:?})? }}", message);
                }
                "{ continue; Value::None }".into()
            }
            ast::Expr::ForLoop(for_loop) => {
                let iterable = self.expr(for_loop.iterable(), env);
                let id = self.fresh_id();
                let binder = format!("__it{id}");
                // Pattern destructuring expands at the head of the loop
                // body; a simple identifier pattern binds the iterated
                // value directly (zero clones, mirroring the original
                // destructure's Ident branch = define; per-read cloning on
                // the use side matches the original).
                let mut body_env = env.clone();
                let pattern_code = match for_loop.pattern() {
                    ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
                        let rust = super::ctx::bind_local(&mut body_env, ident.as_str());
                        format!("let mut {rust} = {binder};\n")
                    }
                    // Placeholder pattern: binds nothing (the original's
                    // iter! macro likewise just iterates without naming for
                    // Placeholder), zero clones.
                    ast::Pattern::Placeholder(_) => String::new(),
                    pattern => {
                        let inner = format!("{{ {binder}.clone() }}");
                        self.destructure(pattern, inner, &mut body_env, false)
                    }
                };
                self.in_loop += 1;
                let mut body_env = body_env;
                let body = self.expr(for_loop.body(), &mut body_env);
                self.in_loop -= 1;
                let iter_at = self.span_of(for_loop.iterable().span());
                let join_at = self.span_of(for_loop.body().span());
                format!(
                    "{{ let mut __out = Value::None;\n  \
                     for {binder} in rt::iter_values(&({iterable}), {iter_at})? {{\n  \
                     \x20   {pattern_code}\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, {join_at})?;\n  \
                     }}\n  \
                     __out }}"
                )
            }
            // math/Equation: large and essentially never a performance
            // problem — the source snippet is evaluated by the typst-eval
            // interpreter (the same fidelity path as interpreted closures,
            // not a gap; other out-of-subset expressions still degrade via
            // diagnostics). Free identifiers (including math names)
            // materialize through the same capture analysis as closures.
            ast::Expr::Label(l) => format!("rt::label({:?})", l.get()),
            ast::Expr::Heading(h) => {
                // `= body` (depth = number of equals signs): mirrors `Eval
                // for ast::Heading`.
                let items: Vec<ContentItem> = h
                    .body()
                    .exprs()
                    .map(ContentItem::Part)
                    .collect();
                let body = self.content_seq(&items, env);
                format!("Value::Content(rt::heading({body}, {}))", h.depth())
            }
            ast::Expr::ListItem(li) => {
                let items: Vec<ContentItem> = li
                    .body()
                    .exprs()
                    .map(ContentItem::Part)
                    .collect();
                let body = self.content_seq(&items, env);
                format!("Value::Content(rt::list_item({body}))")
            }
            ast::Expr::EnumItem(ei) => {
                let items: Vec<ContentItem> = ei
                    .body()
                    .exprs()
                    .map(ContentItem::Part)
                    .collect();
                let body = self.content_seq(&items, env);
                let num = match ei.number() {
                    Some(n) => format!("Some({n}u64)"),
                    None => "None".to_string(),
                };
                format!("Value::Content(rt::enum_item({body}, {num}))")
            }
            ast::Expr::TermItem(ti) => {
                let term: Vec<ContentItem> =
                    ti.term().exprs().map(ContentItem::Part).collect();
                let desc: Vec<ContentItem> =
                    ti.description().exprs().map(ContentItem::Part).collect();
                let term = self.content_seq(&term, env);
                let desc = self.content_seq(&desc, env);
                format!("Value::Content(rt::term_item({term}, {desc}))")
            }
            ast::Expr::Link(l) => {
                // Markup bare link: mirrors `Eval for ast::Link`
                // (Url::new can fail).
                let at = self.span_of(l.span());
                format!(
                    "Value::Content(rt::link({:?}, {at})?)",
                    l.get().to_string()
                )
            }
            ast::Expr::Ref(r) => {
                // `@target` reference shorthand (mirrors `Eval for
                // ast::Ref` → RefElem; supplement is the `[...]` extra
                // content, its value contentified via display).
                let target = r.target().to_string();
                let supp = match r.supplement() {
                    Some(block) => format!("Some({})", self.expr(ast::Expr::ContentBlock(block), env)),
                    None => "None".to_string(),
                };
                format!("Value::Content(rt::ref_elem({:?}, {supp}))", target)
            }
            ast::Expr::Equation(_) => {
                let src = expr.to_untyped().full_text().trim().to_string();
                let mut names: Vec<(String, bool)> = Vec::new();
                collect_idents(expr.to_untyped(), &mut Vec::new(), &mut names);
                let (captured, pending) = self.capture_entries(&names, env);
                let pending_lit = pending_list(&pending);
                // Embedded `#let` (code embedded in math) escapes to the
                // surrounding code scope as in the original: evaluated
                // through the export-wrapped interpreter, with exports
                // written back to block-level hoisted slots (the names must
                // already be on the hoisting surface — the block scan
                // descends into Equation).
                let mut lets: Vec<String> = Vec::new();
                scan_let_exprs(expr.to_untyped(), &mut lets);
                let frame = self.let_hoists.last().cloned();
                let export_slots: Vec<(String, String)> = match frame {
                    Some(ref map)
                        if !lets.is_empty()
                            && lets
                                .iter()
                                .all(|n| map.contains_key(n)) =>
                    {
                        lets.iter()
                            .map(|n| (n.clone(), map[n].clone()))
                            .collect()
                    }
                    _ => Vec::new(),
                };
                if !export_slots.is_empty() {
                    for (name, rust) in &export_slots {
                        env.locals.push((name.clone(), rust.clone()));
                    }
                    let names_lit = format!(
                        "&[{}]",
                        export_slots
                            .iter()
                            .map(|(n, _)| format!("{n:?}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                    let writes = export_slots
                        .iter()
                        .enumerate()
                        .map(|(i, (_, rust))| format!("{rust} = __mx.1[{i}].clone();"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if captured.is_empty() {
                        return format!(
                            "{{ let __mx = rt::eval_vm_exports({src:?}, {names_lit}, &[], {pending_lit}, engine, context)?; {writes} __mx.0 }}"
                        );
                    }
                    let init = format!("vec![{}]", captured.join(", "));
                    return format!(
                        "{{ let __caps: Vec<(&'static str, Value)> = {init}; let __mx = rt::eval_vm_exports({src:?}, {names_lit}, &__caps, {pending_lit}, engine, context)?; {writes} __mx.0 }}"
                    );
                }
                if captured.is_empty() {
                    format!("rt::eval_expr({src:?}, &[], {pending_lit}, engine, context)?")
                } else {
                    let init = format!("vec![{}]", captured.join(", "));
                    format!(
                        "{{ let __caps: Vec<(&'static str, Value)> = {init}; rt::eval_expr({src:?}, &__caps, {pending_lit}, engine, context)? }}"
                    )
                }
            }
            other => {
                let kind = other.to_untyped().kind();
                let message = format!("expression {kind:?} is not yet supported (v1 language subset)");
                self.diag(other.span(), message.clone());
                format!(
                    "{{ /* trust-degraded: {} */ rt::degraded(engine, {:?})? }}",
                    message.replace('\n', " "),
                    message
                )
            }
        }
    }

    /// Emission of expression-position `let` (mirrors `Eval for
    /// ast::LetBinding`: init evaluated first, then bound, value is none):
    /// the binding name is pushed into env **at its position** (references
    /// before the position still resolve to the outer scope), and the value
    /// goes into a hoisted slot registered by the enclosing scope.
    fn let_expr(&mut self, lets: ast::LetBinding, env: &mut Env) -> String {
        let names: Vec<String> = match lets.kind() {
            ast::LetBindingKind::Closure(name) => vec![name.as_str().to_string()],
            ast::LetBindingKind::Normal(pattern) => pattern
                .bindings()
                .iter()
                .map(|i| i.as_str().to_string())
                .collect(),
        };
        // The hoisted slots must be registered by an enclosing scannable
        // scope (block / bare-expression function body / content item);
        // unregistered contexts (content sequences across items, etc.)
        // degrade.
        let Some(map) = self.let_hoists.last().cloned() else {
            self.diag(lets.span(), "let binding at this position is not supported (scope not scanned)".into());
            return "Value::None".into();
        };
        let hoisted: Vec<(String, String)> = match names
            .iter()
            .map(|n| map.get(n).map(|r| (n.clone(), r.clone())))
            .collect::<Option<Vec<_>>>()
        {
            Some(v) => v,
            None => {
                self.diag(lets.span(), "let binding at this position is not supported (name not registered by the scan)".into());
                return "Value::None".into();
            }
        };
        // init is evaluated first (before the binding takes effect;
        // same-name references resolve to the outer scope).
        let init = match lets.init() {
            Some(e) => self.expr(e, env),
            None => "Value::None".into(),
        };
        match lets.kind() {
            ast::LetBindingKind::Closure(name) => {
                let (_, rust) = hoisted[0].clone();
                env.locals.push((name.as_str().to_string(), rust.clone()));
                format!("{{ {rust} = {init}; Value::None }}")
            }
            ast::LetBindingKind::Normal(pattern) => {
                // Pattern names first all point at the hoisted slots (the
                // assign leaves of destructure find them via classify),
                // then write per pattern.
                for (name, rust) in &hoisted {
                    env.locals.push((name.clone(), rust.clone()));
                }
                let stmts = self.destructure(pattern, init, env, true);
                format!("{{ {} Value::None }}", stmts)
            }
        }
    }

    /// Open a hoisted-slot frame (scanning the given statement/expression
    /// root set) and return the declaration code; the frame is closed by
    /// [`Self::close_let_hoists`].
    pub(super) fn open_let_hoists<'n>(
        &mut self,
        roots: impl Iterator<Item = &'n typst_syntax::SyntaxNode>,
    ) -> String {
        let mut names: Vec<String> = Vec::new();
        for root in roots {
            scan_let_exprs(root, &mut names);
        }
        let mut map = std::collections::HashMap::new();
        let mut decls = String::new();
        for name in names {
            if map.contains_key(&name) {
                continue;
            }
            let rust = format!("__lh{}_{}", self.fresh_id(), sanitize_key(&name));
            decls.push_str(&format!("let mut {rust} = Value::None;\n"));
            map.insert(name, rust);
        }
        self.let_hoists.push(map);
        decls
    }

    /// Close the topmost hoisted-slot frame.
    pub(super) fn close_let_hoists(&mut self) {
        self.let_hoists.pop();
    }

    fn ident(&mut self, ident: ast::Ident, env: &mut Env) -> String {
        let name = ident.as_str();
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
            return format!("{rust}.clone()");
        }
        // Runtime wildcard layer (after `import <runtime value>: *`): names
        // in the layer shadow outer module names and globals; a miss falls
        // back along the chain (mirrors the typst scope chain, inner layer
        // first across multiple layers).
        if !env.wildcards.is_empty() {
            let at = self.span_of(ident.span());
            let name_escaped = escape(name);
            let layers = wild_layers(env);
            if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
                let static_v = self.ref_value(module_ref);
                return format!(
                    "rt::wild_or({layers}, \"{name_escaped}\", {static_v}, {at})?"
                );
            }
            return format!(
                "rt::wild_or_global({layers}, \"{name_escaped}\", engine, {at})?"
            );
        }
        if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
            return self.ref_value(module_ref);
        }
        format!("rt::global(engine, \"{}\", file_span())?", escape(name))
    }


    /// Zone-1 glue expression for includes: `rt::eval_include(statement
    /// text, span, visible names, engine, context)` drives the original
    /// `Eval for ast::ModuleInclude`.
    fn include_glue_expr(&mut self, expr: ast::Expr, env: &mut Env) -> String {
        let stmt_text = expr.to_untyped().full_text().trim().to_string();
        let at = self.span_of(expr.span());
        let locals: Vec<(String, String)> = env.locals.clone();
        let modules: Vec<(String, super::ctx::Ref)> = env.module.clone();
        let mut caps: Vec<String> = locals
            .iter()
            .map(|(n, rust)| {
                format!("(\"{}\", {rust}.clone())", super::naming::escape(n))
            })
            .collect();
        caps.extend(modules.iter().map(|(n, r)| {
            format!("\"{}\", {}", super::naming::escape(n), self.ref_value(r))
        }));
        let caps_lit = if caps.is_empty() {
            "Vec::new()".to_string()
        } else {
            format!("vec![{}]", caps.join(", "))
        };
        format!(
            "Value::Content(rt::eval_include({stmt_text:?}, {at}, &{caps_lit}, engine, context)?)"
        )
    }

    fn binary(&mut self, binary: ast::Binary, env: &mut Env) -> String {
        use ast::BinOp::*;
        let op = binary.op();
        // The assignment family (expression and statement positions share
        // one semantics; mirrors `apply_assignment` being a branch of
        // Binary evaluation): uniformly emitted through classification.
        if matches!(op, Assign | AddAssign | SubAssign | MulAssign | DivAssign) {
            return self.assign_code(binary, env);
        }
        // and/or short-circuit (mirrors typst-eval/src/ops.rs).
        if matches!(op, And | Or) {
            let lhs = self.expr(binary.lhs(), env);
            let rhs = self.expr(binary.rhs(), env);
            let (test, fuse, rest) = match op {
                And => ("__l == Value::Bool(false)", "Value::Bool(false)", "and"),
                Or => ("__l == Value::Bool(true)", "Value::Bool(true)", "or"),
                _ => unreachable!(),
            };
            let at = self.span_of(binary.span());
            return format!(
                "{{ let __l = {lhs}; if {test} {{ {fuse} }} else {{ ops::{rest}(__l, {rhs}).at({at})? }} }}"
            );
        }
        let lhs = self.expr(binary.lhs(), env);
        let rhs = self.expr(binary.rhs(), env);
        match op {
            Add => self.span_fmt(binary.span(), format!("ops::add({lhs}, {rhs}).at({{at}})?")),
            Sub => self.span_fmt(binary.span(), format!("ops::sub({lhs}, {rhs}).at({{at}})?")),
            Mul => self.span_fmt(binary.span(), format!("ops::mul({lhs}, {rhs}).at({{at}})?")),
            Div => self.span_fmt(binary.span(), format!("ops::div({lhs}, {rhs}).at({{at}})?")),
            Eq => self.span_fmt(binary.span(), format!("ops::eq({lhs}, {rhs}).at({{at}})?")),
            Neq => self.span_fmt(binary.span(), format!("ops::neq({lhs}, {rhs}).at({{at}})?")),
            Lt | Leq | Gt | Geq => {
                let ord = self.span_fmt(
                    binary.span(),
                    format!("ops::compare(&({lhs}), &({rhs})).at({{at}})?"),
                );
                let test = match op {
                    Lt => "std::cmp::Ordering::Less",
                    Leq => "std::cmp::Ordering::Less | std::cmp::Ordering::Equal",
                    Gt => "std::cmp::Ordering::Greater",
                    Geq => "std::cmp::Ordering::Greater | std::cmp::Ordering::Equal",
                    _ => unreachable!(),
                };
                format!("Value::Bool(matches!({ord}, {test}))")
            }
            In => self.span_fmt(binary.span(), format!("ops::in_({lhs}, {rhs}).at({{at}})?")),
            NotIn => self.span_fmt(
                binary.span(),
                format!("ops::not_in({lhs}, {rhs}).at({{at}})?"),
            ),
            other => {
                let message = format!("binary operation {other:?} is not yet supported");
                self.diag(binary.span(), message.clone());
                format!(
                    "{{ /* trust-degraded: {} */ rt::degraded(engine, {:?})? }}",
                    message, message
                )
            }
        }
    }

    fn func_call(&mut self, call: ast::FuncCall, env: &mut Env) -> String {
        let at = self.span_of(call.span());
        // Arguments evaluate in source order (mirrors `Eval for
        // ast::Args`); Args.span is the whole call's span (mirrors
        // `.spanned(self.span())`).
        if let ast::Expr::FieldAccess(access) = call.callee() {
            let field = access.field().as_str();
            if matches!(field, "push" | "pop" | "insert" | "remove") {
                // Mutating method (mirrors `maybe_resolve_mutating`:
                // **arguments evaluate before the target**; the target
                // resolves mutably via `Access`).
                let args = self.gen_args(call.args(), env, &at);
                let access_text = access.to_untyped().full_text().to_string();
                match self.classify_place(access.target(), env) {
                    super::block::Place::Local { root, path } => {
                        let path_lit = super::block::place_path_lit(&path);
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; let __place = rt::place_mut(&mut {root}, {path_lit}, {at})?; rt::method_mut_dispatch(engine, context, __place, \"{}\", __mut_args, {at}, {:?})? }}",
                            escape(field), access_text
                        );
                    }
                    super::block::Place::Slot { slot, path } => {
                        // Mutable module slot: read-modify-write (in-place
                        // mutation semantics); the body's reference to
                        // __st must set used_state (the function prologue
                        // carries state based on it).
                        self.used_state = true;
                        let path_lit = super::block::place_path_lit(&path);
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; let mut __msv = rt::read_ms(&__st.{slot})?; let __place = rt::place_mut(&mut __msv, {path_lit}, {at})?; let __r = rt::method_mut_dispatch(engine, context, __place, \"{}\", __mut_args, {at}, {:?})?; rt::write_ms(&__st.{slot}, __msv)?; __r }}",
                            escape(field), access_text
                        );
                    }
                    // Write to a capture (including the module-slot-inside-
                    // closure form): arguments already evaluated; mirrors
                    // `Access` rejecting Captured bindings — a push
                    // modifier on a symbol reports the read-only error here
                    // as well.
                    super::block::Place::Captured => {
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; rt::bail_captured(typst::foundations::Capturer::Function)? }}"
                        );
                    }
                    // Lexical miss: arguments already evaluated; look up
                    // the global at runtime (constant / unknown-variable
                    // error, mirroring `Scopes::get_mut`).
                    super::block::Place::Unresolved { name } => {
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; rt::check_mut_unknown(engine, \"{}\", {at})?; Value::None }}",
                            escape(&name)
                        );
                    }
                    // Not an lvalue: arguments already evaluated; the
                    // target evaluates for side effects via `Access`
                    // recursion (FieldAccess evaluates only the target
                    // chain), then reports temporary.
                    super::block::Place::Temporary => {
                        let target = self.eval_for_access(access.target(), env);
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; let _ = {target}; rt::bail_temporary({at})? }}"
                        );
                    }
                    super::block::Place::Deferred => {
                        self.diag(
                            call.span(),
                            "target of a mutating method is a runtime module reference (not yet supported)".into(),
                        );
                        return "Value::None".into();
                    }
                }
            }
            // Method/field call: resolve the callee value first (a
            // non-callable reports before argument evaluation, mirroring
            // `eval_field_callee` preceding args.eval; a method call passes
            // the target as the first positional argument, inserted at the
            // target expression's span).
            let target = self.expr(access.target(), env);
            let tgt_at = self.span_of(access.target().span());
            let field_at = self.span_of(access.field().span());
            let access_text = access.to_untyped().full_text().to_string();
            let args = self.gen_args(call.args(), env, &at);
            return format!(
                "{{ rt::check_call_depth(engine, {at})?; let __target = {target}; let __mc = rt::method_callee(__target, \"{}\", {access_text:?}, engine, {field_at})?; let __call_args = {args}; rt::call_with(__mc.0, __mc.1.map(|t| (t, {tgt_at})), {at}, __call_args, engine, context)? }}",
                escape(field)
            );
        }
        // Plain call: the callee evaluates before the arguments (mirrors
        // `Eval for ast::FuncCall`); when not callable, the arguments are
        // not evaluated.
        let callee_at = self.span_of(call.callee().span());
        let callee = self.expr(call.callee(), env);
        let args = self.gen_args(call.args(), env, &at);
        format!(
            "{{ rt::check_call_depth(engine, {at})?; let __callee = rt::callee({callee}, {callee_at})?; let __call_args = {args}; rt::call_with(__callee, None, {at}, __call_args, engine, context)? }}"
        )
    }

    /// Call-site arguments: build `Args` in source order; each item uses
    /// its **own** span (item span and value span are separate, mirroring
    /// the original `Eval for ast::Args`); the overall span is the call
    /// site.
    fn gen_args(&mut self, args: ast::Args, env: &mut Env, call_at: &str) -> String {
        let mut out = format!(
            "{{ let mut __call_args_v = Args::new({call_at}, std::iter::empty::<Value>());"
        );
        for arg in args.items() {
            let item_at = self.span_of(arg.span());
            match arg {
                ast::Arg::Pos(expr) => {
                    let value = self.expr(expr, env);
                    let _ = write!(out, " __call_args_v.push({item_at}, {value});");
                }
                ast::Arg::Named(named) => {
                    let name = named.name().as_str();
                    let value_at = self.span_of(named.expr().span());
                    let value = self.expr(named.expr(), env);
                    let _ = write!(
                        out,
                        " __call_args_v.items.push(rt::named_arg({item_at}, {value_at}, \"{}\", {value}));",
                        escape(name)
                    );
                }
                ast::Arg::Spread(spread) => {
                    let value = self.expr(spread.expr(), env);
                    let _ = write!(
                        out,
                        " rt::spread_args(&mut __call_args_v, {item_at}, {value})?;"
                    );
                }
            }
        }
        out.push_str(" __call_args_v }");
        out
    }

    /// Closures: the source snippet is kept and interpreted by typst-eval
    /// (the fidelity path); defaults evaluate at creation time; captures =
    /// subtree identifiers ∩ outer resolvable names (conservative
    /// over-capture; the interpreter overrides via parameter/local
    /// shadowing, so semantics are unchanged).
    fn closure_expr(&mut self, closure: ast::Closure, env: &mut Env) -> String {
        // Micro closures (predicates/projections like `i => i * 2`) stay
        // interpreted: interpreting the body costs less than one stub hop
        // (eval_closure bookkeeping + forwarded body interpretation + the
        // native call protocol overhead), so C2 is a net loss for them
        // (measured ~1.0× → 0.4× on map-heavy code). The threshold is the
        // body's AST node count: ≤ 8 counts as micro; ctx thunks are
        // hundred-node scale and stay unaffected.
        let body_nodes = closure.body().to_untyped().len();
        if body_nodes <= 32 {
            return self.closure_expr_interp(closure, env);
        }
        // A named closure whose body references itself (local recursion):
        // take the C2 self-reference protocol (SelfFuncCell: rebuild the
        // same-identity stub once per instance, prologue binds its own
        // name).
        let self_name = closure
            .name()
            .filter(|name| closure.body().to_untyped().full_text().contains(name.as_str()))
            .map(|name| name.as_str().to_string());
        self.closure_expr_c2(closure, env, self_name)
    }

    /// Interpreted path for micro closures (the original v1 shape: the
    /// snippet source is interpreted by typst-eval).
    fn closure_expr_interp(&mut self, closure: ast::Closure, env: &mut Env) -> String {
        // A named closure (`#let f(x) = ..` or a local `let f(x) = ..`) has
        // node text shaped `f(x) = ..`; prepending `let` makes it
        // independently parseable source.
        let raw = closure.to_untyped().full_text().to_string();
        let src_text = if closure.name().is_some() {
            format!("let {raw}")
        } else {
            raw
        };
        let mut defaults = Vec::new();
        for param in closure.params().children() {
            if let ast::Param::Named(named) = param {
                defaults.push(self.expr(named.expr(), env));
            }
        }
        let mut names: Vec<(String, bool)> = Vec::new();
        collect_idents(closure.to_untyped(), &mut Vec::new(), &mut names);
        let (captured, captured_globals) = self.capture_entries(&names, env);
        let defaults = if defaults.is_empty() {
            "Vec::new()".into()
        } else {
            format!("vec![{}]", defaults.join(", "))
        };
        let pending_lit = pending_list(&captured_globals);
        let captured = if captured.is_empty() {
            "&[]".into()
        } else {
            format!("&[{}]", captured.join(", "))
        };
        // Per-call-site static: parsed once; creation takes zero locks and
        // zero lookups.
        let node = self.closure_node_static(&src_text);
        format!("Value::Func(rt::closure_node_at(&{node}, {defaults}, {captured}, {pending_lit}, engine))")
    }

    /// C2: the native path for value-position closures.
    fn closure_expr_c2(
        &mut self,
        closure: ast::Closure,
        env: &mut Env,
        self_name: Option<String>,
    ) -> String {
        // C2: value-position closures go native too — the closure
        // expression compiles to a top-level `f_c{id}` native body exposed
        // through a signature-faithful stub; captured values travel with
        // the closure value as a name-sorted tuple (StateSlot packs the
        // tuple, same protocol as the top-level C packed state). Identical
        // definitions (same source text + same captured-name set) dedup;
        // multiple call sites share one native body.
        let raw = closure.to_untyped().full_text().to_string();
        // The stub signature uses the parameter source text (including
        // default-value shapes; at stub binding time the evaluator fills
        // missing named arguments from defaults, so parameters are always
        // complete when forwarded to the native body).
        let mut params_src = closure.params().to_untyped().full_text().to_string();
        // An anonymous single parameter can be written `x => ..` (no
        // parentheses); the stub parse needs the parameter-list form.
        if !params_src.trim_start().starts_with('(') {
            params_src = format!("({params_src})");
        }
        // Defaults evaluate at creation time (on every creation; typst
        // closure semantics).
        let mut defaults = Vec::new();
        for param in closure.params().children() {
            if let ast::Param::Named(named) = param {
                defaults.push(self.expr(named.expr(), env));
            }
        }
        let defaults = if defaults.is_empty() {
            "Vec::new()".to_string()
        } else {
            format!("vec![{}]", defaults.join(", "))
        };
        // Captures: locals/module slots materialize immediately (Some
        // wrapping the value); names invisible at creation time materialize
        // at runtime through the global + math scopes (Option); None at
        // call time reports unknown variable (mirrors CapturesVisitor's
        // `Err(_) => return`).
        let mut names: Vec<(String, bool)> = Vec::new();
        collect_idents(closure.to_untyped(), &mut Vec::new(), &mut names);
        let (mut pairs, pending) = self.capture_pairs(&names, env);
        for (name, in_math) in &pending {
            let resolver = if *in_math { "capture_math_opt" } else { "capture_code_opt" };
            pairs.push((
                name.clone(),
                format!("rt::{resolver}(engine, \"{}\")?", escape(name)),
            ));
        }
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        let cap_names: Vec<String> = pairs.iter().map(|(n, _)| n.clone()).collect();
        let key = format!("{raw}\u{0}{}", cap_names.join(","));

        let dbg = std::env::var_os("TRUST_DEBUG_DEDUP").is_some();
        if dbg {
            eprintln!("[dedup?] key={:?}", &key[..key.len().min(70)]);
        }
        let (id, has_st) = match self.closure_dedup.get(&key).cloned() {
            Some(hit) => {
                if dbg {
                    eprintln!("[dedup=hit] {hit:?}");
                }
                hit
            }
            None => {
                let id = format!("c{}", self.next_closure_id);
                self.next_closure_id += 1;
                if dbg {
                    eprintln!("[dedup=alloc] {id}");
                }
                // The body is generated under an independent counter
                // (gen_closure_fn internally resets used_state=false and
                // then tracks the body's own references); afterwards has_st
                // is read out and the outer counter merged back (when the
                // capture point / default expressions reference __st, the
                // outer scope needs the __st prologue).
                let saved = self.used_state;
                let item = self.gen_closure_fn(
                    &format!("f_{id}"),
                    &format!("D_{id}"),
                    closure,
                    &cap_names,
                    self_name.as_deref(),
                    &params_src,
                );
                // At the end of gen_closure_fn, used_state = whether the
                // body references __st (module slots / in-body string
                // imports); read it out and merge the outer counter back.
                let has_st = std::mem::replace(&mut self.used_state, saved);
                // Closure bodies hoisted to the output root: file_span
                // references are module-qualified by their source file
                // (invisible from the root scope; in both the tree and
                // flat forms a file module is a sibling of the root, so
                // `{mod}::file_span()` is reachable isomorphically).
                let item = match (&self.current_file, &self.package_mods) {
                    (Some(path), Some(mods)) => match mods.get(path.as_str()) {
                        Some(mod_name) => {
                            item.replace("file_span()", &format!("{mod_name}::file_span()"))
                        }
                        None => item,
                    },
                    _ => item,
                };
                self.closure_items.push_str(&item);
                self.closure_items.push_str(&super::func::gen_func_data(
                    &format!("D_{id}"),
                    &format!("f_{id}"),
                    "closure",
                    closure,
                ));
                self.closure_dedup.insert(key, (id.clone(), has_st));
                if dbg {
                    eprintln!("[dedup=miss] id={id} has_st={has_st}");
                }
                (id, has_st)
            }
        };
        if has_st {
            // The creation point references __st.clone(): the enclosing
            // function must carry the state prologue.
            self.used_state = true;
        }
        // Creation point: capture tuple + StateSlot + stub.
        let mut tuple: Vec<String> = Vec::new();
        if has_st {
            tuple.push("__st.clone()".to_string());
        }
        for (name, expr) in &pairs {
            if pending.iter().any(|(p, _)| p == name) {
                tuple.push(expr.clone());
            } else {
                tuple.push(format!("Some({expr})"));
            }
        }
        if self_name.is_some() {
            // Self-recursion: defaults travel with the instance (used when
            // rebuilding the stub).
            tuple.push(format!(
                "std::sync::Arc::new(rt::self_func_cell({defaults}))"
            ));
        }
        // The stub source is composed at translation time (a pure
        // function); its node goes into a per-call-site static — creation
        // takes zero lookups and zero locks (native_closure_node).
        let stub_name = closure
            .name()
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "closure".to_string());
        let stub_src = trust_runtime::compose_stub(&stub_name, &params_src);
        let node_name = self.closure_node_static(&stub_src);
        format!(
            "{{ let __slot_{id} = std::sync::Arc::new(({})); Value::Func(rt::native_closure_node(&D_{id}, &{node_name}, {defaults}, Some(&rt::StateSlot::new(__slot_{id})))) }}",
            tuple.join(", ")
        )
    }



    /// Content sequence: split at set/show rules; a rule wraps "everything
    /// after it" (mirrors `eval_markup`'s tail recursion:
    /// `tail.styled_with_map/_recipe`).
    pub(super) fn content_seq(&mut self, items: &[ContentItem], env: &mut Env) -> String {
        let tops: Vec<TopItem> = items
            .iter()
            .map(|item| TopItem {
                item: item.clone(),
                env: env.clone(),
            })
            .collect();
        self.content_seq_top(&tops)
    }

    /// Top-level content folding: each item is generated with its own
    /// visible binding set (rules wrap subsequent items).
    pub(super) fn content_seq_top(&mut self, items: &[TopItem]) -> String {
        for (i, top) in items.iter().enumerate() {
            let mut env = top.env.clone();
            match &top.item {
                ContentItem::Set(set) => {
                    let mut parts: Vec<String> =
                        items[..i].iter().map(|e| self.content_part(e)).collect();
                    let tail = self.content_seq_top(&items[i + 1..]);
                    // Styles evaluate before the tail content (mirrors
                    // eval_code/eval_markup: set.eval precedes the tail;
                    // Rust evaluates arguments left to right, so the styles
                    // are materialized in a local first).
                    let rule_at = self.span_of(set.span());
                    let styles = self.set_styles_code(*set, &mut env, &rule_at);
                    parts.push(format!("{{ let __s = {styles}; rt::styled_with_map({tail}, __s) }}"));
                    return self.content_join(parts);
                }
                ContentItem::Show(show) => {
                    let mut parts: Vec<String> =
                        items[..i].iter().map(|e| self.content_part(e)).collect();
                    let tail = self.content_seq_top(&items[i + 1..]);
                    // Evaluation order mirrors `Eval for ast::ShowRule`:
                    // selector → transform (set or expression) → tail
                    // content.
                    let show_at = self.span_of(show.span());
                    let (sel_arg, sel_at) = match show.selector() {
                        Some(sel) => {
                            let sel_at = self.span_of(sel.span());
                            (format!("Some({})", self.expr(sel, &mut env)), sel_at)
                        }
                        None => ("None".to_string(), show_at.clone()),
                    };
                    let recipe = match show.transform() {
                        ast::Expr::SetRule(set) => {
                            // show-set: the transform is styles; the set's
                            // condition is handled inside set.eval (a false
                            // condition → empty styles).
                            let set_at = self.span_of(set.span());
                            let styles = self.set_styles_code(set, &mut env, &set_at);
                            format!(
                                "{{ let __sel = {sel_arg}; let __styles = {styles}; rt::show_rule_style(__sel, {sel_at}, __styles, {show_at}, engine)? }}"
                            )
                        }
                        transform => {
                            format!(
                                "{{ let __sel = {sel_arg}; let __tf = {{ {} }}; rt::show_rule(__sel, {sel_at}, __tf, {show_at}, engine)? }}",
                                self.expr(transform, &mut env)
                            )
                        }
                    };
                    parts.push(format!(
                        "{{ let __recipe = {recipe}; let __tail = {tail}; rt::styled_with_recipe(engine, context, __tail, __recipe)? }}"
                    ));
                    return self.content_join(parts);
                }
                _ => {}
            }
        }
        let parts: Vec<String> = items.iter().map(|e| self.content_part(e)).collect();
        self.content_join(parts)
    }

    /// Style evaluation code for a set rule (false condition → empty
    /// styles; the condition evaluates before target and arguments,
    /// mirroring `Eval for ast::SetRule`). `call_at` is the call-site span
    /// (Args.span and the rule span share their origin). Shared by the
    /// set/show-set arms.
    pub(super) fn set_styles_code(&mut self, set: ast::SetRule, env: &mut Env, call_at: &str) -> String {
        let args = self.gen_args(set.args(), env, call_at);
        let target = self.expr(set.target(), env);
        match set.condition() {
            Some(cond) => {
                let cond_at = self.span_of(cond.span());
                let cond = self.expr(cond, env);
                format!(
                    "if rt::boolean({cond}, {cond_at})? {{ rt::set_rule({target}, {args}, {call_at}, engine)? }} else {{ rt::styles_empty() }}"
                )
            }
            None => format!("rt::set_rule({target}, {args}, {call_at}, engine)?"),
        }
    }

    /// A single part of content (an expression produces Content
    /// construction code; env is that item's own visible binding set).
    fn content_part(&mut self, top: &TopItem) -> String {
        let mut env = top.env.clone();
        // Hoisting shell for let expressions inside the item: the item is a
        // single Rust expression, so hoisted slots are declared at the
        // item's block top and discarded with it (approximating the content
        // block's own scope; mirrors typst's content-block scoping).
        // Statement-shaped Parts (let/import/include) are handled by the
        // outer statement machinery and stay off the hoisting surface.
        let scan_root: Option<&typst_syntax::SyntaxNode> = match &top.item {
            ContentItem::Part(ast::Expr::LetBinding(_))
            | ContentItem::Part(ast::Expr::ModuleImport(_))
            | ContentItem::Part(ast::Expr::ModuleInclude(_)) => None,
            ContentItem::Part(expr) => Some(expr.to_untyped()),
            _ => None,
        };
        let hoist = match scan_root {
            Some(root) => {
                let d = self.open_let_hoists(std::iter::once(root));
                d
            }
            None => String::new(),
        };
        let out = self.content_part_inner(top, &mut env);
        if scan_root.is_some() {
            self.close_let_hoists();
        }
        if hoist.is_empty() {
            out
        } else {
            format!("{{ {} {out} }}", hoist)
        }
    }

    fn content_part_inner(&mut self, top: &TopItem, env: &mut Env) -> String {
        match &top.item {
            ContentItem::Include(body_slot) => {
                self.used_state = true;
                format!("rt::read_content(&__st.{body_slot})?")
            }
            ContentItem::DynamicInclude(code) => format!("Value::Content({code})"),
            ContentItem::Part(ast::Expr::Text(t)) => {
                format!("rt::text({:?})", t.get().as_str())
            }
            ContentItem::Part(ast::Expr::Space(_)) => "rt::space()".into(),
            ContentItem::Part(ast::Expr::Shorthand(s)) => {
                // Markup shorthands (`~` nbsp, `--` en, etc.): mirrors the
                // original `Value::Symbol(runtime_char)`, entering the
                // sequence via display (a Symbol element and a text element
                // have different types — equality tells them apart).
                format!("rt::symbol_value({:?})", s.get())
            }
            ContentItem::Part(ast::Expr::SmartQuote(q)) => {
                // Smart quotes: build the smartquote element; resolution is
                // left to layout time.
                format!("rt::smartquote({})", q.double())
            }
            ContentItem::Part(ast::Expr::Label(l)) => {
                // Labels are not embedded: they enter the sequence as
                // Value::Label and attach_labels sticks them onto the last
                // attachable element (mirrors eval_markup's Label branch).
                format!("rt::label({:?})", l.get())
            }
            ContentItem::Part(ast::Expr::Strong(strong)) => {
                // `*body*` (mirrors eval_markup: StrongElem::new(body));
                // the body translates as a nested markup sequence.
                let items: Vec<ContentItem> = strong
                    .body()
                    .exprs()
                    .map(|e| match e {
                        ast::Expr::SetRule(set) => ContentItem::Set(set),
                        ast::Expr::ShowRule(show) => ContentItem::Show(show),
                        other => ContentItem::Part(other),
                    })
                    .collect();
                format!("rt::strong({})", self.content_seq(&items, env))
            }
            ContentItem::Part(ast::Expr::Emph(emph)) => {
                // `_body_` (mirrors eval_markup: EmphElem::new(body)).
                let items: Vec<ContentItem> = emph
                    .body()
                    .exprs()
                    .map(|e| match e {
                        ast::Expr::SetRule(set) => ContentItem::Set(set),
                        ast::Expr::ShowRule(show) => ContentItem::Show(show),
                        other => ContentItem::Part(other),
                    })
                    .collect();
                format!("rt::emph({})", self.content_seq(&items, env))
            }
            ContentItem::Part(ast::Expr::Linebreak(_)) => "rt::linebreak()".into(),
            ContentItem::Part(ast::Expr::Parbreak(_)) => "rt::parbreak()".into(),
            ContentItem::Part(ast::Expr::Escape(e)) => {
                // The escape `\[`: mirrors the original
                // `Value::Symbol(runtime_char)` (not text — `== [\[]`
                // relies on the type distinction).
                format!("rt::symbol_value({:?})", e.get())
            }
            ContentItem::Part(ast::Expr::LetBinding(_))
            | ContentItem::Part(ast::Expr::ModuleImport(_)) => "rt::embed(Value::None)".into(),
            ContentItem::Part(other) => {
                // Values enter the sequence **without** embed: the display
                // in attach_labels has the same shape as eval_markup's
                // value.display(); in particular `#label(expression)` (a
                // function call with a computed name) yields Value::Label,
                // attached by attach_labels rather than standing as its own
                // content item (an embedded computed label was unfindable
                // by the entry's label query).
                self.expr(*other, env)
            }
            ContentItem::Set(_) | ContentItem::Show(_) => {
                unreachable!("rules handled by content_seq_top")
            }
        }
    }

    fn content_join(&mut self, parts: Vec<String>) -> String {
        if parts.is_empty() {
            "Content::sequence(Vec::new())".into()
        } else {
            // Mirrors eval_markup: labels attach to the last attachable
            // element (attach_labels); other values display into the
            // sequence (the lookup semantics exercised by glossary-style
            // code).
            let items = parts
                .iter()
                .map(|p| {
                    if p.contains("/* never */") {
                        // Never-typed items (`#return`, the divergence
                        // check of always-true while): a trailing
                        // never-typed expression makes the bare block
                        // never-typed, covariant into Vec<Value>;
                        // `.into_value()` cannot apply to never. The
                        // emitter marks them with ` /* never */`
                        // (scripting-return/while driven).
                        format!("{{ {p} }}")
                    } else {
                        format!("({p}).into_value()")
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ let __items = vec![{items}]; rt::attach_labels(engine, __items) }}")
        }
    }

    /// Capture-list construction (shared by closures and context):
    /// locals/module slots materialize directly; a global name materializes
    /// only if it exists (names invisible at creation time are left to
    /// in-body import).
    fn capture_entries(
        &mut self,
        names: &[(String, bool)],
        env: &mut Env,
    ) -> (Vec<String>, Vec<(String, bool)>) {
        let (pairs, globals) = self.capture_pairs(names, env);
        (
            pairs
                .into_iter()
                .map(|(name, expr)| format!("(\"{}\", {expr})", escape(&name)))
                .collect(),
            globals,
        )
    }

    /// Structured capture table: a list of (name, evaluation expression)
    /// plus global names left for runtime materialization (C2 closures
    /// assemble their capture tuple sorted by name, so names and
    /// expressions must be separate).
    fn capture_pairs(
        &mut self,
        names: &[(String, bool)],
        env: &mut Env,
    ) -> (Vec<(String, String)>, Vec<(String, bool)>) {
        let mut captured: Vec<(String, String)> = Vec::new();
        let mut globals: Vec<(String, bool)> = Vec::new();
        let mut seen = HashSet::new();
        for (name, in_math) in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            let _ = in_math;
            if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
                captured.push((name.clone(), format!("{rust}.clone()")));
            } else if !env.wildcards.is_empty() {
                // A name inside the wildcard layer (or one that may be
                // shadowed by it): snapshot at creation time (mirrors
                // CapturesVisitor — visible at creation means captured; a
                // layer miss falls back along the chain, same order as
                // ident).
                let name_escaped = escape(name);
                let layers = wild_layers(env);
                if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
                    let static_v = self.ref_value(module_ref);
                    captured.push((
                        name.clone(),
                        format!(
                            "rt::wild_or({layers}, \"{name_escaped}\", {static_v}, file_span())?"
                        ),
                    ));
                } else {
                    captured.push((
                        name.clone(),
                        format!(
                            "rt::wild_or_global({layers}, \"{name_escaped}\", engine, file_span())?"
                        ),
                    ));
                }
            } else if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name)
                // Materialized global names (G_ slots / GlobalFallback)
                // may be invisible at creation time — they take the pending
                // protocol (resolve_pending's soft capture_opt
                // materialization; empty means None, reported only at call
                // time; same capture timing as typst. A sibling closure
                // parameter is a genuinely free name, always empty at
                // creation time, and an eager read_or once errored
                // immediately).
                && !matches!(module_ref, super::ctx::Ref::GlobalFallback { .. })
                && !matches!(module_ref, super::ctx::Ref::Slot(s) if s.starts_with("G_"))
            {
                let value = self.ref_value(module_ref);
                captured.push((name.clone(), value));
            } else {
                globals.push((name.clone(), *in_math));
            }
        }
        (captured, globals)
    }
}

/// Slice expression for the runtime wildcard layers (`&[&__w1, &__w2]`,
/// innermost last; lookup runs from tail to head = inner shadows outer).
fn wild_layers(env: &mut Env) -> String {
    let refs: Vec<String> = env.wildcards.iter().map(|w| format!("&{w}")).collect();
    format!("&[{}]", refs.join(", "))
}

/// Whether an expression is invariant (mirrors typst-eval flow.rs's
/// `is_invariant`: identifiers and math identifiers count as mutable,
/// field access follows its target, calls follow callee + arguments, all
/// other nodes look at every child).
fn is_invariant(expr: &typst_syntax::SyntaxNode) -> bool {
    match expr.cast() {
        Some(ast::Expr::Ident(_)) => false,
        Some(ast::Expr::MathIdent(_)) => false,
        Some(ast::Expr::FieldAccess(access)) => is_invariant(access.target().to_untyped()),
        Some(ast::Expr::FuncCall(call)) => {
            is_invariant(call.callee().to_untyped()) && is_invariant(call.args().to_untyped())
        }
        _ => expr.children().all(is_invariant),
    }
}

/// Literal form of the pending-name list (`&["a", "b"]`; materialized at
/// creation time via the global + math scopes, see rt::resolve_pending).
fn pending_list(names: &[(String, bool)]) -> String {
    if names.is_empty() {
        "&[]".to_string()
    } else {
        format!(
            "&[{}]",
            names
                .iter()
                .map(|(n, m)| format!("({n:?}, {m})"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

/// Whether an expression contains break/return (mirrors typst-eval
/// flow.rs's `can_diverge`).
fn can_diverge(expr: &typst_syntax::SyntaxNode) -> bool {
    matches!(expr.kind(), SyntaxKind::Break | SyntaxKind::Return)
        || expr.children().any(can_diverge)
}

/// Scan the names bound by expression-position `let` (for hoisted-slot
/// registration). Scope boundaries mirror typst: do not cross
/// closures/code blocks/content blocks (each has its own scope — closures
/// self-manage via CapturesVisitor, code blocks re-scan recursively in
/// block(), content blocks are self-contained per content_part item); a
/// let nested in a LetBinding's init still binds this scope, so the scan
/// keeps descending.
pub(super) fn scan_let_exprs(node: &typst_syntax::SyntaxNode, out: &mut Vec<String>) {
    use typst_syntax::ast::AstNode as _;
    if let Some(expr) = node.cast::<ast::Expr>() {
        match expr {
            ast::Expr::Closure(_)
            | ast::Expr::CodeBlock(_)
            | ast::Expr::ContentBlock(_) => return,
            ast::Expr::LetBinding(lets) => match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    out.push(name.as_str().to_string());
                }
                ast::LetBindingKind::Normal(pattern) => {
                    out.extend(
                        pattern.bindings().iter().map(|i| i.as_str().to_string()),
                    );
                }
            },
            _ => {}
        }
    }
    for child in node.children() {
        scan_let_exprs(child, out);
    }
}
