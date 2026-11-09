//! Single-file driver: one typst module source file → one Rust module
//! source file.
//!
//! Output shape (per-build slot board, see `_things/DESIGN-per-build.md`):
//! - `__State`: one write-once slot (`OnceLock<Value>`) per top-level
//!   binding, one instance per build, filled in source order during
//!   `module_init`;
//! - `module_init(engine, context) -> Module`: the mirror of module
//!   evaluation; the host calls it once per build; top-level content
//!   (including set/show rules) folds into the module body in source order
//!   (`Module::with_content`);
//! - functions: references to module slots inside a body compile to
//!   `rt::read(&__st.slot)?`, and the body gets the `__st` prologue
//!   (`rt::state_require(engine)`).

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize_key, uppercase};

/// Top-level binding: a list of (name, slot) pairs + its kind (function
/// closure node or value init; a destructuring binding contributes several
/// names).
struct TopBinding<'a> {
    names: Vec<(String, String)>,
    kind: TopKind<'a>,
}

enum TopKind<'a> {
    Value {
        init: ast::Expr<'a>,
        pattern: Option<ast::Pattern<'a>>,
    },
    Fn {
        closure: ast::Closure<'a>,
    },
}

/// Translate one typst module source file into Rust source text.
pub fn translate(src: &str, module_name: &str) -> Result<String, Vec<Diag>> {
    // Parse through Source: spans carry a FileId (same origin as the
    // output's file_span), so error sites can point directly at the source
    // expression.
    let id = typst_syntax::RootedPath::new(
        typst_syntax::VirtualRoot::Project,
        typst_syntax::VirtualPath::new("main.typ").expect("path"),
    )
    .intern();
    let source = typst_syntax::Source::new(id, src.to_string());
    let root = source.root().clone();
    let Some(markup) = root.cast::<ast::Markup>() else {
        return Err(vec![Diag {
            span: typst_syntax::Span::detached(),
            message: "source file failed to parse as a markup module".into(),
            file: None,
        }]);
    };

    let mut cx = Ctx::new(false);
    // Pass one: collect top-level bindings and content items (interleaved
    // to preserve order: evaluation follows source order).
    #[derive(Clone)]
    enum Seq<'a> {
        Binding(usize),
        Content(super::expr::ContentItem<'a>),
    }
    let mut tops: Vec<TopBinding> = Vec::new();
    let mut seq: Vec<Seq> = Vec::new();
    for expr in markup.exprs() {
        if let ast::Expr::LetBinding(_) = expr {
            let before = tops.len();
            collect_top(expr, &mut tops, &mut cx);
            if tops.len() > before {
                seq.push(Seq::Binding(tops.len() - 1));
                // Mirrors eval_markup: an evaluated let likewise joins the
                // output as a display slot (None.display() is empty
                // content). The binding itself is emitted from `tops`
                // (slots + init_code) — BoundElsewhere keeps the display
                // slot without re-evaluating the init in the content
                // frame.
                seq.push(Seq::Content(super::expr::ContentItem::BoundElsewhere));
            } else {
                // No bindings (a `_`-only pattern): nothing for the
                // top-level machinery to fill — the content frame's
                // let_expr evaluates the init for its side effects and
                // discards the value.
                seq.push(Seq::Content(super::expr::ContentItem::Part(expr)));
            }
            continue;
        }
        // Every other top-level expression (including Space/Parbreak) is
        // module body content.
        match expr {
            ast::Expr::SetRule(set) => seq.push(Seq::Content(super::expr::ContentItem::Set(set))),
            ast::Expr::ShowRule(show) => {
                seq.push(Seq::Content(super::expr::ContentItem::Show(show)))
            }
            other => seq.push(Seq::Content(super::expr::ContentItem::Part(other))),
        }
    }
    fn collect_top<'a>(
        expr: ast::Expr<'a>,
        tops: &mut Vec<TopBinding<'a>>,
        cx: &mut super::ctx::Ctx,
    ) {
        let ast::Expr::LetBinding(lets) = expr else {
            return;
        };
        match lets.kind() {
            ast::LetBindingKind::Closure(name) => {
                let Some(ast::Expr::Closure(closure)) = lets.init() else {
                    cx.diag(
                        lets.span(),
                        "closure binding is missing its closure initializer".into(),
                    );
                    return;
                };
                let id = cx.fresh_id();
                let slot = format!("V{id}_{}", sanitize_key(name.as_str()));
                tops.push(TopBinding {
                    names: vec![(name.as_str().to_string(), slot)],
                    kind: TopKind::Fn { closure },
                });
            }
            ast::LetBindingKind::Normal(pattern) => {
                let Some(init) = lets.init() else {
                    if let Some(name) = pattern.bindings().first() {
                        cx.diag(
                            lets.span(),
                            format!(
                                "top-level `#let {}` is missing an initializer",
                                name.as_str()
                            ),
                        );
                    }
                    return;
                };
                // One slot per pattern name (destructuring binds several).
                let names: Vec<(String, String)> = pattern
                    .bindings()
                    .iter()
                    .map(|ident| {
                        let id = cx.fresh_id();
                        (
                            ident.as_str().to_string(),
                            format!("V{id}_{}", sanitize_key(ident.as_str())),
                        )
                    })
                    .collect();
                if names.is_empty() {
                    cx.diag(lets.span(), "top-level binding has no name".into());
                    return;
                }
                tops.push(TopBinding {
                    names,
                    kind: TopKind::Value {
                        init,
                        pattern: Some(pattern),
                    },
                });
            }
        }
    }

    // Pass two: emit evaluation statements and the module body in source
    // order.
    let mut init_code = String::new();
    let mut fields = String::new();
    let mut items = String::new();
    let mut content_items: Vec<super::expr::TopItem> = Vec::new();
    // The set of bindings visible to content items grows with source order
    // (mirrors typst's sequential evaluation; #17).
    let mut visible: Vec<(String, Ref)> = Vec::new();
    for step in &seq {
        let Seq::Binding(i) = step else {
            if let Seq::Content(item) = step {
                content_items.push(super::expr::TopItem {
                    item: item.clone(),
                    env: Env {
                        locals: Vec::new(),
                        module: visible.clone(),
                        wildcards: Vec::new(),
                        floats: std::collections::BTreeSet::new(),
                    },
                });
            }
            continue;
        };
        visible.extend(
            tops[*i]
                .names
                .iter()
                .map(|(name, slot)| (name.clone(), Ref::Slot(slot.clone()))),
        );
        let i = *i;
        let top = &tops[i];
        // The function body sees itself (recursion) and all previous
        // bindings; a value init does not include its own binding.
        let slots_of = |range: std::ops::RangeInclusive<usize>| {
            range
                .flat_map(|j| {
                    tops[j]
                        .names
                        .iter()
                        .map(|(name, slot)| (name.clone(), Ref::Slot(slot.clone())))
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let env = Env {
            locals: Vec::new(),
            module: slots_of(0..=i),
            wildcards: Vec::new(),
            floats: std::collections::BTreeSet::new(),
        };
        let init_env = if i > 0 {
            Env {
                locals: Vec::new(),
                module: slots_of(0..=i - 1),
                wildcards: Vec::new(),
                floats: std::collections::BTreeSet::new(),
            }
        } else {
            Env {
                locals: Vec::new(),
                module: Vec::new(),
                wildcards: Vec::new(),
                floats: std::collections::BTreeSet::new(),
            }
        };
        for (_, slot) in &top.names {
            let _ = writeln!(fields, "    {slot}: std::sync::OnceLock<Value>,");
        }
        match &top.kind {
            TopKind::Value {
                init,
                pattern: None,
            } => {
                let (name, slot) = &top.names[0];
                let mut ie = init_env.clone();
                let expr = cx.expr(*init, &mut ie);
                let _ = writeln!(
                    init_code,
                    "    {{ let v = {expr}; rt::fill(&__st.{slot}, v.clone())?; scope.define(\"{}\", v); }}",
                    escape(name)
                );
            }
            TopKind::Value {
                init,
                pattern: Some(pattern),
            } => {
                // Top-level destructuring: slot names are pre-bound as
                // temporary locals; destructure runs with assign=true
                // (writing to existing targets) and writes back to these
                // names, then each slot is filled and registered — the
                // block-let shadowing semantics do not apply here (the
                // names are slot aliases).
                let mut ie = init_env.clone();
                let expr = cx.expr(*init, &mut ie);
                let mut tmp_env = init_env.clone();
                let mut decls = String::new();
                for (name, slot) in &top.names {
                    tmp_env.locals.push((name.clone(), slot.clone()));
                    let _ = writeln!(decls, "        let mut {slot} = Value::None;");
                }
                let stmts = cx.destructure(*pattern, "v.clone()".to_string(), &mut tmp_env, true);
                let mut fills = String::new();
                for (name, slot) in &top.names {
                    let _ = writeln!(
                        fills,
                        "        rt::fill(&__st.{slot}, {slot}.clone())?; scope.define(\"{}\", {slot}.clone());",
                        escape(name)
                    );
                }
                let _ = writeln!(
                    init_code,
                    "    {{ let v = {expr};
{decls}{stmts}{fills}    }}"
                );
            }
            TopKind::Fn { closure } => {
                let (name, slot) = &top.names[0];
                let fun = format!("f_{}", lowercase(slot));
                let data = format!("D_{}", uppercase(slot));
                // Global names materialize at creation time (mirrors typst
                // closure capture: a name visible at creation time is
                // captured; invisible ones are left to call-time
                // resolution). The routing rules live with the capture
                // analysis (see global_capture_slots).
                let mut g_env = env.clone();
                let mut g_code = String::new();
                let body = closure.body().to_untyped();
                let captures = super::captures::global_capture_slots(
                    super::captures::top_closure_free_names(*closure),
                    &env.module,
                    super::captures::has_wildcard_import(body),
                    slot,
                );
                for g in &captures {
                    let _ = writeln!(fields, "    {}: std::sync::OnceLock<Value>,", g.slot);
                    let gname_escaped = super::naming::escape(&g.name);
                    let resolver = if g.in_math {
                        "capture_math_opt"
                    } else {
                        "capture_code_opt"
                    };
                    let _ = writeln!(
                        g_code,
                        "    if let Some(v) = rt::{resolver}(engine, \"{gname_escaped}\")? {{ rt::fill(&__st.{}, v)?; }}",
                        g.slot
                    );
                    g_env.module.push((
                        g.name.clone(),
                        Ref::GlobalFallback {
                            slot: g.slot.clone(),
                            name: gname_escaped,
                        },
                    ));
                }
                init_code.push_str(&g_code);
                items.push_str(&cx.gen_function(&fun, slot, *closure, &mut g_env));
                items.push_str(&gen_func_data(&data, &fun, name, *closure));
                // Defaults materialize at creation time (mirrors typst:
                // a closure evaluates its parameters one by one when
                // created). The same expression batch feeds the stub's
                // defaults — the evaluation point is the creation of the
                // closure value (inside module_init), and the value also
                // fills the DEF slot for the function body's
                // missing-argument path: **evaluated once** (previously
                // def_code and defaults were emitted twice and default
                // expressions with side effects ran twice).
                let mut defaults = Vec::new();
                for param in closure.params().children() {
                    if let ast::Param::Named(named) = param {
                        let pname = named.name().as_str();
                        let def_slot = format!("DEF_{slot}_{}", sanitize_key(pname));
                        let _ = writeln!(fields, "    {def_slot}: std::sync::OnceLock<Value>,");
                        let mut ie2 = init_env.clone();
                        let value = cx.expr(named.expr(), &mut ie2);
                        defaults.push(format!(
                            "{{ let v = {value}; rt::fill(&__st.{def_slot}, v.clone())?; v }}"
                        ));
                    }
                }
                // Native closure (uniform Closure-form stub): whether it
                // carries state is an internal detail, not leaked into its
                // external identity (same shape as package mode).
                let params_src = closure.params().to_untyped().full_text();
                let defaults = if defaults.is_empty() {
                    "Vec::new()".to_string()
                } else {
                    format!("vec![{}]", defaults.join(", "))
                };
                let state_arg = if cx.used_state {
                    "Some(&rt::StateSlot::new(__st.clone()))"
                } else {
                    "None"
                };
                let func_value =
                    format!("rt::native_closure(&{data}, {params_src:?}, {defaults}, {state_arg})");
                let _ = writeln!(
                    init_code,
                    "    {{ let v = Value::Func({func_value}); rt::fill(&__st.{slot}, v.clone())?; scope.define(\"{}\", v); }}",
                    escape(name)
                );
            }
        }
    }

    // Module body: top-level content folds in source order (rules wrap
    // subsequent content; per-item visibility).
    let body = cx.content_seq_top(&content_items);
    // Native bodies and per-call-site node statics of C2 value-position
    // closures (emitted at the root).
    let closure_items = format!(
        "{}{}",
        std::mem::take(&mut cx.closure_items),
        std::mem::take(&mut cx.closure_nodes)
    );

    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    let span_consts = cx.span_consts();
    Ok(format!(
        "// Generated by trust from `{module_name}`; do not edit.\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult, bail}};\n\
         use ecow::eco_vec;\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Content, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
         use typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         /// Fallback error span for sites not covered by a source span.\n\
         const SPAN: Span = Span::detached();\n{span_consts}\
         \
         /// Anchor span of this file (carries a FileId; span-located global functions such as plugins rely on it).\n\
         #[allow(non_snake_case, dead_code)]\n\
         pub(crate) fn file_span() -> Span {{\n    static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n    *CELL.get_or_init(|| {{\n        let id = typst::syntax::RootedPath::new(\n            typst::syntax::VirtualRoot::Project,\n            typst::syntax::VirtualPath::new(\"main.typ\").expect(\"path\"),\n        )\n        .intern();\n        typst::syntax::Source::new(id, String::new()).root().span()\n    }})\n}}\n\n\
         /// Evaluation state of this module: one write-once slot per top-level binding (one instance per build).\n\
         #[derive(Default)]\n\
         struct __State {{\n{fields}}}\n\n\
         /// Mirror of module evaluation: the host calls this once per build (World); evaluates all top-level bindings in source order.\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let mut scope = Scope::new();\n    let __st = std::sync::Arc::new(__State::default());\n    let _ = context;\n{init_code}    Ok(Module::new(\"{module_name}\", scope).with_content({body}).with_file_id(file_span().id().expect(\"file span id\")))\n}}\n\n\
         /// One-line integration: drives module_init and returns the module (to inject into a host library use\n\
         /// `rt::bind_module`; the test world's InjectWorld::init_native_module already wires it).\n\
         pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {{\n    rt::init_module(world, library, module_init)\n}}\n{closure_items}{items}"
    ))
}
