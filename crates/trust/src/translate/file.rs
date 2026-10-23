//! 单文件驱动:一个 typst 模块源文件 → 一个 Rust 模块源文件。
//!
//! 生成形态(per-build 槽板,见 `_things/DESIGN-per-build.md`):
//! - `__State`:每个顶层绑定一个写一次槽(`OnceLock<Value>`),
//!   每构建一份实例,按源序在 `module_init` 中求值填充;
//! - `module_init(engine, context) -> Module`:模块求值的镜像,
//!   宿主每次构建调用一次;顶层内容(含 set/show 规则)按源序
//!   折叠进模块 body(`Module::with_content`);
//! - 函数:体内引用模块槽时编译为 `rt::read(&__st.槽名)?`,
//!   函数体带 `__st` 序言(`rt::state_require(engine)`)。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, sanitize_key, uppercase};

/// 顶层绑定:(名字, 槽) 对列表 + 形态(函数闭包节点或值 init;
/// 解构绑定贡献多个名字)。
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

/// 把一个 typst 模块源文件翻译为 Rust 源码文本。
pub fn translate(src: &str, module_name: &str) -> Result<String, Vec<Diag>> {
    // 经 Source 解析:span 携带 FileId(与生成物 file_span 同源),
    // 错误站点的定位可直接指向源表达式。
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
            message: "源文件未能解析为 markup 模块".into(),
            file: None,
        }]);
    };

    let mut cx = Ctx::new(false);
    // 第一遍:收集顶层绑定与内容项(交错保序:求值按源序)。
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
                // 对照 eval_markup:let 求值后同样并入一个 display 槽
                // (None.display() 为空内容)。
                seq.push(Seq::Content(super::expr::ContentItem::Part(expr)));
            }
            continue;
        }
        // 其余顶层表达式(含 Space/Parbreak)都是模块 body 内容。
        match expr {
            ast::Expr::SetRule(set) => seq.push(Seq::Content(super::expr::ContentItem::Set(set))),
            ast::Expr::ShowRule(show) => {
                seq.push(Seq::Content(super::expr::ContentItem::Show(show)))
            }
            other => seq.push(Seq::Content(super::expr::ContentItem::Part(other))),
        }
    }
    let _ = &mut seq;
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
                    cx.diag(lets.span(), "闭包绑定缺少闭包初始化".into());
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
                            format!("顶层 `#let {}` 缺少初始化", name.as_str()),
                        );
                    }
                    return;
                };
                // 每个模式名字开一个槽(解构绑定多名)。
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
                    cx.diag(lets.span(), "顶层绑定没有名字".into());
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

    // 第二遍:按源序生成求值语句与模块 body。
    let mut init_code = String::new();
    let mut fields = String::new();
    let mut items = String::new();
    let mut content_items: Vec<super::expr::TopItem> = Vec::new();
    // 内容项可见的绑定集随源序增长(对照 typst 顺序求值:#17)。
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
        // 函数体可见自身(递归)与此前全部绑定;值 init 不含本绑定。
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
        };
        let init_env = if i > 0 {
            Env {
                locals: Vec::new(),
                module: slots_of(0..=i - 1),
                wildcards: Vec::new(),
            }
        } else {
            Env {
                locals: Vec::new(),
                module: Vec::new(),
                wildcards: Vec::new(),
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
                // 顶层解构:槽名预绑为临时局部,destructure 经
                // assign=true(写既有目标)写回这些名字,再逐槽填充
                // 注册——块的 let 遮蔽语义不适用于此(名字即槽别名)。
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
                // 全局名创建期物化(对照 typst 闭包捕获:创建期可见即
                // 捕获,不可见的名字留调用期解析)。
                let mut g_env = env.clone();
                let mut g_code = String::new();
                let mut g_seen = std::collections::HashSet::new();
                let mut names = Vec::new();
                // 先注入闭包自身绑定(名字/参数/sink)再收集——参数不是
                // 自由名(codly 驱动:`__merge(base, …)` 的 base 曾被
                // 收成全局槽,调用期报 unknown variable;对照
                // CapturesVisitor 的 Closure 分支先 bind 后访体)。
                let mut g_bound: Vec<String> = Vec::new();
                if let Some(name) = closure.name() {
                    g_bound.push(name.as_str().to_string());
                }
                for param in closure.params().children() {
                    match param {
                        ast::Param::Pos(pattern) => g_bound
                            .extend(pattern.bindings().into_iter().map(|i| i.as_str().to_string())),
                        ast::Param::Named(named) => {
                            g_bound.push(named.name().as_str().to_string())
                        }
                        ast::Param::Spread(spread) => {
                            if let Some(ident) = spread.sink_ident() {
                                g_bound.push(ident.as_str().to_string());
                            }
                        }
                    }
                }
                super::captures::collect_idents(
                    closure.body().to_untyped(),
                    &mut g_bound,
                    &mut names,
                );
                for (gname, g_in_math) in names {
                    if !g_seen.insert(gname.clone()) {
                        continue;
                    }
                    if env.module.iter().any(|(n, _)| *n == gname) {
                        continue;
                    }
                    let gslot = format!("G_{slot}_{}", sanitize_key(&gname));
                    let _ = writeln!(fields, "    {gslot}: std::sync::OnceLock<Value>,");
                    let gname_escaped = escape(&gname);
                    let resolver = if g_in_math { "capture_math_opt" } else { "capture_code_opt" };
                    let _ = writeln!(
                        g_code,
                        "    if let Some(v) = rt::{resolver}(engine, \"{gname_escaped}\")? {{ rt::fill(&__st.{gslot}, v)?; }}"
                    );
                    g_env.module.push((
                        gname,
                        Ref::GlobalFallback {
                            slot: gslot.clone(),
                            name: gname_escaped,
                        },
                    ));
                }
                init_code.push_str(&g_code);
                items.push_str(&cx.gen_function(&fun, slot, *closure, &mut g_env));
                items.push_str(&gen_func_data(&data, &fun, name, *closure));
                // 默认值创建期物化(对照 typst:闭包创建时逐参数求值)。
                // 同批表达式进 stub 的 defaults——求值点即闭包值创建
                // (module_init 内),值同时填 DEF 槽供函数体缺参取用,
                // **单次求值**(此前 def_code 与 defaults 双发射,默认
                // 值表达式副作用会执行两次)。
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
                // 原生闭包(C 方案):引用模块槽的函数经签名保真 stub
                // 携带状态;纯函数保持裸 native(与包模式同款判定)。
                let func_value = if cx.used_state {
                    let params_src = closure.params().to_untyped().full_text();
                    let defaults = if defaults.is_empty() {
                        "Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", defaults.join(", "))
                    };
                    format!(
                        "rt::native_closure(&{data}, {params_src:?}, {defaults}, &rt::StateSlot::new(__st.clone()))"
                    )
                } else {
                    format!("Func::from(&{data})")
                };
                let _ = writeln!(
                    init_code,
                    "    {{ let v = Value::Func({func_value}); rt::fill(&__st.{slot}, v.clone())?; scope.define(\"{}\", v); }}",
                    escape(name)
                );
            }
        }
    }

    // 模块 body:顶层内容按源序折叠(规则包装后续内容,逐项可见性)。
    let body = cx.content_seq_top(&content_items);
    // C2 值位置闭包的原生体与每调用点节点静态(根级发射)。
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
        "// 由 trust 从 `{module_name}` 生成,勿手改。\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult, bail}};\n\
         use ecow::eco_vec;\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Content, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
         use typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         /// 兜底错误 span(未被源站点覆盖处;差分对拍只比较值)。\n\
         const SPAN: Span = Span::detached();\n{span_consts}\
\
         /// 当前文件的锚点 span(带 FileId;plugin 等按 span 定位的全局函数依赖它)。\n\
         #[allow(non_snake_case, dead_code)]\n\
         pub(crate) fn file_span() -> Span {{\n    static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n    *CELL.get_or_init(|| {{\n        let id = typst::syntax::RootedPath::new(\n            typst::syntax::VirtualRoot::Project,\n            typst::syntax::VirtualPath::new(\"main.typ\").expect(\"path\"),\n        )\n        .intern();\n        typst::syntax::Source::new(id, String::new()).root().span()\n    }})\n}}\n\n\
         /// 本模块的求值状态:每个顶层绑定一个写一次槽(每构建一份)。\n\
         #[derive(Default)]\n\
         struct __State {{\n{fields}}}\n\n\
         /// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let mut scope = Scope::new();\n    let __st = std::sync::Arc::new(__State::default());\n    let _ = context;\n{init_code}    Ok(Module::new(\"{module_name}\", scope).with_content({body}).with_file_id(file_span().id().expect(\"file span id\")))\n}}\n\n\
         /// 一行接入:驱动 module_init 求值并返回模块(注入宿主 library 用\n\
         /// `rt::bind_module`;测试世界 InjectWorld::init_native_module 已内置)。\n\
         pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {{\n    rt::init_module(world, library, module_init)\n}}\n{closure_items}{items}"
    ))
}
