//! 表达式直译:typst 表达式 → 返回 `Value` 的 Rust 表达式(可含 `?`)。
//!
//! 入口 `Ctx::expr` 按表达式种类分派;其余方法(ident 解析、二元运算、
//! 调用、调用点参数、闭包)均为其内部步骤。

use std::collections::HashSet;
use std::fmt::Write as _;

use typst_syntax::SyntaxKind;
use typst_syntax::ast::{self, AstNode};

use super::captures::collect_idents;
use super::ctx::{Ctx, Env};
use super::naming::{escape, sanitize_key};

/// `ast::Unit` → Rust 变体名的显式映射(不依赖 Debug 输出形态,
/// typst 改名即编译错,而非静默错值)。
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

/// 内容项 + 生成该项时可见的绑定集(顶层内容按源序逐项可见性,
/// 对照 typst 的 markup 顺序求值;content 块内均匀)。
pub(super) struct TopItem<'a> {
    pub item: ContentItem<'a>,
    pub env: Env,
}

/// 内容流的一项:普通表达式 / set 规则 / show 规则 / include。
/// 顶层内容与 content 块共用同一条折叠管线。
#[derive(Clone)]
pub(super) enum ContentItem<'a> {
    Part(ast::Expr<'a>),
    Set(ast::SetRule<'a>),
    Show(ast::ShowRule<'a>),
    /// 包模式:目标文件 body 槽名。
    Include(String),
    /// 动态源 include(区 1 胶水):携带发射好的求值代码
    /// (`rt::eval_include` 驱动原版 Eval,file-not-found 等原版报)。
    DynamicInclude(String),
}

impl Ctx {
    /// 表达式直译:返回类型为 `Value` 的 Rust 表达式(可含 `?`)。
    pub(super) fn expr(&mut self, expr: ast::Expr, env: &mut Env) -> String {
        match expr {
            ast::Expr::Ident(ident) => self.ident(ident, env),
            ast::Expr::None(_) => "Value::None".into(),
            ast::Expr::Auto(_) => "Value::Auto".into(),
            ast::Expr::Bool(b) => format!("Value::Bool({})", b.get()),
            ast::Expr::Int(i) => format!("Value::Int({})", i.get()),
            // inf/nan 的 Debug 形态不是合法 Rust 字面量,走常量。
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
                // 代码位 Raw(对照 typst-eval markup.rs 的 `Eval for
                // ast::Raw`):行表 + 语言 + 块标记构造 RawElem;
                // `.text` 等字段访问走元素字段(与原生同语义)。
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
                // 解构赋值(表达式位置,顶层标记语句同走此臂;对照
                // binding.rs `Eval for ast::DestructAssignment`:值先求,
                // 叶子走 access 写回)。assign 叶子不绑新名,env 克隆
                // 仅为满足 &mut 签名。
                let value = self.expr(assign.value(), env);
                let mut env2 = env.clone();
                let stmts = self.destructure(assign.pattern(), value, &mut env2, true);
                format!("{{ {} Value::None }}", stmts)
            }
            ast::Expr::LetBinding(lets) => {
                // 表达式位置 `let`(对照 `Eval for ast::LetBinding`:init
                // 先求,再绑定,值为 none):绑定名在**位点**压入 env(位点
                // 前的引用仍解析外层),值写块顶提升槽(扫描期登记,
                // 见 block/content_part 的扫描;未扫描上下文降级)。
                self.let_expr(lets, env)
            }
            ast::Expr::ContentBlock(block) => {
                // content 块直译:子集 Text/Space/Linebreak/Parbreak/Escape/
                // 嵌入表达式 + set/show 规则,构造方式对照
                // typst-eval/src/markup.rs(规则包装后续全部内容)。
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
                // `#include "x.typ"`(包模式):目标模块的 body 内容
                // (对照 `Eval for ast::ModuleInclude` -> Content)。
                match include.source() {
                    ast::Expr::Str(path) => {
                        let raw = path.get().to_string();
                        let (Some(file), Some(files), Some(mods)) = (
                            self.current_file.clone(),
                            self.package_files.clone(),
                            self.package_mods.clone(),
                        ) else {
                            self.diag(path.span(), "include 仅在包模式支持".into());
                            return "Value::None".into();
                        };
                        let target = super::paths::normalize_import(&file, &raw);
                        let mod_name = mods.get(&target).cloned();
                        match (files.contains_key(&target), mod_name) {
                            (true, Some(mod_name)) => {
                                self.used_state = true;
                                format!("Value::Content(rt::read_content(&__st.BODY_{mod_name})?)")
                            }
                            // 不在文件集/任意表达式源:区 1 胶水——
                            // rt::eval_include 驱动原版 Eval,世界可能
                            // 仍能解析,file-not-found 等由原版报。
                            _ => self.include_glue_expr(expr, env),
                        }
                    }
                    _ => self.include_glue_expr(expr, env),
                }
            }
            ast::Expr::Contextual(ctx) => {
                // `#context expr`:构造推迟求值的闭包(对照 `Eval for
                // ast::Contextual`),realize/layout 期带 location/styles 求值。
                let body = ctx.body().to_untyped().full_text().to_string();
                let mut names: Vec<(String, bool)> = Vec::new();
                collect_idents(ctx.body().to_untyped(), &mut Vec::new(), &mut names);
                let (captured, pending) = self.capture_entries(&names, env);
                // pending(局部/模块槽之外的名字)运行期经全局+数学
                // 作用域物化(对照原生 CapturesVisitor 的 get/get_in_math)。
                let pending_lit = pending_list(&pending);
                if captured.is_empty() {
                    format!(
                        "Value::Content(rt::context_body(rt::context_closure({body:?}, &[], {pending_lit}, engine)))"
                    )
                } else {
                    // 块内构造、块内消费(&__caps 不逃出块)。
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
                            // 键是表达式:求值后 cast 到字符串(对照
                            // `Eval for ast::Dict::Keyed`)。
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
                    // 对照 access_field:求值/报错站点为字段名 span。
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
                // 静态"常真"检测(对照原生 i==0 分支的 is_invariant/
                // can_diverge:条件不变式且体不含 break/return 时,首次
                // 求值为真即报错;**条件为假则合法退出**——`while false
                // {}` 返回 none,不报错(条件副作用仍先求值)。
                if is_invariant(while_loop.condition().to_untyped())
                    && !can_diverge(while_loop.body().to_untyped())
                {
                    return format!(
                        "{{ let __c = rt::boolean({condition}, {cond_at})?; if __c {{ bail!({cond_at}, \"condition is always true\") }} else {{ Value::None }} }}"
                    );
                }
                // 迭代护栏(对照原生 MAX_ITERATIONS:超出报 "loop
                // seems to be infinite",而非挂死)。
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
                // return 编译为 Rust return(从生成的 f_ 函数返回);
                // 函数体之外(模块求值/默认值等上下文)外层 Rust 函数
                // 返回类型不同,直接 return 会让生成物无法编译——按
                // 缺口降级诊断。
                if self.in_fn == 0 {
                    let message = "return 仅在函数体内支持(v1 语言子集)".to_string();
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
                // 循环外 break(对照 typst-eval 的语义错误;错误测试
                // 驱动):降级为运行期同信息错误。
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
                // 模式解构在循环体首展开;简单标识符模式直接绑定迭代
                // 值(零克隆,对照原生 destructure 的 Ident 分支=define;
                // 读取侧的按次克隆与原生一致)。
                let mut body_env = env.clone();
                let pattern_code = match for_loop.pattern() {
                    ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
                        let rust = super::ctx::bind_local(&mut body_env, ident.as_str());
                        format!("let mut {rust} = {binder};\n")
                    }
                    // 占位模式:不绑定(对照原生 iter! 宏对 Placeholder
                    // 同样只迭代不取名),零克隆。
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
            // math/Equation:体量大、几乎不构成性能问题——源码片段
            // 经 typst-eval 解释求值(与解释闭包同一保真路径,不是
            // 缺口;其余子集外表达式仍走诊断降级)。自由标识符(含
            // 数学名)按闭包同款捕获分析物化。
            ast::Expr::Label(l) => format!("rt::label({:?})", l.get()),
            ast::Expr::Heading(h) => {
                // `= body`(depth = 等号数):对照 Eval for ast::Heading。
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
                // 标记裸链接:对照 Eval for ast::Link(Url::new 可错)。
                let at = self.span_of(l.span());
                format!(
                    "Value::Content(rt::link({:?}, {at})?)",
                    l.get().to_string()
                )
            }
            ast::Expr::Ref(r) => {
                // `@target` 引用缩写(对照 Eval for ast::Ref → RefElem;
                // supplement 为 `[...]` 补充内容,值经 display 内容化)。
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
                // 嵌入 `#let`(math 内代码嵌入)按原版逃逸到外围代码
                // 作用域:经导出包裹解释求值,导出写回块级提升槽
                //(名字须已在提升面——块扫描对 Equation 下行)。
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
                let message = format!("表达式 {kind:?} 暂不支持(v1 语言子集)");
                self.diag(other.span(), message.clone());
                format!(
                    "{{ /* trust-degraded: {} */ rt::degraded(engine, {:?})? }}",
                    message.replace('\n', " "),
                    message
                )
            }
        }
    }

    /// 表达式位置 `let` 的发射(对照 `Eval for ast::LetBinding`:init
    /// 先求、再绑定、值为 none):绑定名在**位点**压入 env(位点前的
    /// 引用仍解析外层),值写外层作用域登记的提升槽。
    fn let_expr(&mut self, lets: ast::LetBinding, env: &mut Env) -> String {
        let names: Vec<String> = match lets.kind() {
            ast::LetBindingKind::Closure(name) => vec![name.as_str().to_string()],
            ast::LetBindingKind::Normal(pattern) => pattern
                .bindings()
                .iter()
                .map(|i| i.as_str().to_string())
                .collect(),
        };
        // 提升槽须由外层可扫描作用域(块/裸表达式函数体/内容项)登记;
        // 未登记(content 序列跨项等)降级。
        let Some(map) = self.let_hoists.last().cloned() else {
            self.diag(lets.span(), "此位置的 let 绑定暂不支持(作用域未扫描)".into());
            return "Value::None".into();
        };
        let hoisted: Vec<(String, String)> = match names
            .iter()
            .map(|n| map.get(n).map(|r| (n.clone(), r.clone())))
            .collect::<Option<Vec<_>>>()
        {
            Some(v) => v,
            None => {
                self.diag(lets.span(), "此位置的 let 绑定暂不支持(未入扫描面)".into());
                return "Value::None".into();
            }
        };
        // init 先求(绑定生效前,同名引用解析外层)。
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
                // 模式名先全部指向提升槽(destructure 的 assign 叶子经
                // classify 找到它们),再按模式写入。
                for (name, rust) in &hoisted {
                    env.locals.push((name.clone(), rust.clone()));
                }
                let stmts = self.destructure(pattern, init, env, true);
                format!("{{ {} Value::None }}", stmts)
            }
        }
    }

    /// 开一层提升槽帧(扫描给定语句/表达式根集),返回声明代码;
    /// 帧由 [`Self::close_let_hoists`] 关闭。
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

    /// 关闭最上层提升槽帧。
    pub(super) fn close_let_hoists(&mut self) {
        self.let_hoists.pop();
    }

    fn ident(&mut self, ident: ast::Ident, env: &mut Env) -> String {
        let name = ident.as_str();
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
            return format!("{rust}.clone()");
        }
        // 运行期通配层(`import <运行期值>: *` 之后):层内名字遮蔽
        // 外层模块名与全局;未命中沿链回落(对照 typst 作用域链,
        // 多层内层优先)。
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


    /// include 的区 1 胶水表达式:`rt::eval_include(语句原文, span,
    /// 可见名, engine, context)` 驱动原版 `Eval for ast::ModuleInclude`。
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
        // 赋值族(表达式位置与语句位置同一语义,对照 `apply_assignment`
        // 是 Binary 求值的一个分支):统一走分类发射。
        if matches!(op, Assign | AddAssign | SubAssign | MulAssign | DivAssign) {
            return self.assign_code(binary, env);
        }
        // and/or 短路(对照 typst-eval/src/ops.rs)。
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
                let message = format!("二元运算 {other:?} 暂不支持");
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
        // 参数按源序求值(对照 `Eval for ast::Args`);Args.span 为整个
        // 调用的 span(对照 `.spanned(self.span())`)。
        if let ast::Expr::FieldAccess(access) = call.callee() {
            let field = access.field().as_str();
            if matches!(field, "push" | "pop" | "insert" | "remove") {
                // 可变方法(对照 `maybe_resolve_mutating`:**实参先于
                // 目标求值**,目标经 `Access` 可变解析)。
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
                        // 可变模块槽:读改写回(就地修改语义);体引用
                        // __st 须置 used_state(函数序言按它带状态)。
                        self.used_state = true;
                        let path_lit = super::block::place_path_lit(&path);
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; let mut __msv = rt::read_ms(&__st.{slot})?; let __place = rt::place_mut(&mut __msv, {path_lit}, {at})?; let __r = rt::method_mut_dispatch(engine, context, __place, \"{}\", __mut_args, {at}, {:?})?; rt::write_ms(&__st.{slot}, __msv)?; __r }}",
                            escape(field), access_text
                        );
                    }
                    // 写捕获(含模块槽在闭包内的形态):实参已求,对照
                    // `Access` 对 Captured 绑定的拒绝——symbol 上的
                    // push 修饰符同样在此报只读错。
                    super::block::Place::Captured => {
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; rt::bail_captured(typst::foundations::Capturer::Function)? }}"
                        );
                    }
                    // 词法层未命中:实参已求,运行期查全局(常量/未知
                    // 变量错,对照 `Scopes::get_mut`)。
                    super::block::Place::Unresolved { name } => {
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; rt::check_mut_unknown(engine, \"{}\", {at})?; Value::None }}",
                            escape(&name)
                        );
                    }
                    // 非左值:实参已求,目标按 `Access` 递归求副作用
                    //(FieldAccess 只求目标链)后报 temporary。
                    super::block::Place::Temporary => {
                        let target = self.eval_for_access(access.target(), env);
                        return format!(
                            "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; let _ = {target}; rt::bail_temporary({at})? }}"
                        );
                    }
                    super::block::Place::Deferred => {
                        self.diag(
                            call.span(),
                            "可变方法的目标为运行期模块引用(本轮不承载)".into(),
                        );
                        return "Value::None".into();
                    }
                }
            }
            // 方法/字段调用:先解析被调值(不可调用则在实参求值前报错,
            // 对照 `eval_field_callee` 先于 args.eval;方法调用把目标
            // 作为首个位置参数,插入位点为目标表达式 span)。
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
        // 普通调用:callee 先于实参求值(对照 `Eval for ast::FuncCall`),
        // 不可调用时实参不被求值。
        let callee_at = self.span_of(call.callee().span());
        let callee = self.expr(call.callee(), env);
        let args = self.gen_args(call.args(), env, &at);
        format!(
            "{{ rt::check_call_depth(engine, {at})?; let __callee = rt::callee({callee}, {callee_at})?; let __call_args = {args}; rt::call_with(__callee, None, {at}, __call_args, engine, context)? }}"
        )
    }

    /// 调用点参数:按源序构造 `Args`;每项用**自身** span(项 span 与
    /// 值 span 分开,对照原生 `Eval for ast::Args`),整体 span 为调用点。
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

    /// 闭包:保留源码片段交由 typst-eval 解释(保真路径);
    /// 默认值创建期求值;捕获 = 子树标识符 ∩ 外层可解析名字
    /// (保守过捕,解释器按参数/局部遮蔽覆盖,语义不变)。
    fn closure_expr(&mut self, closure: ast::Closure, env: &mut Env) -> String {
        // 微型闭包(谓词/投影,如 `i => i * 2`)保持解释:体的解释成本
        // 低于一次 stub 跳数(eval_closure 记账 + 转发体解释 + 原生调用
        // 协议开销),C2 反而净亏(map-heavy 实测 1.0× → 0.4×)。阈值取
        // 体的 AST 节点数:≤ 8 视为微型;cetz/fletcher 的 ctx thunk 均
        // 为百节点级,不受影响。
        let body_nodes = closure.body().to_untyped().len();
        if body_nodes <= 32 {
            return self.closure_expr_interp(closure, env);
        }
        // 命名闭包若体引用自身(局部递归,如 cetz 的 split-rec):
        // 走 C2 的自引用协议(SelfFuncCell:每实例一次重建同身份
        // stub,序言绑定自身名)。
        let self_name = closure
            .name()
            .filter(|name| closure.body().to_untyped().full_text().contains(name.as_str()))
            .map(|name| name.as_str().to_string());
        self.closure_expr_c2(closure, env, self_name)
    }

    /// 微型闭包的解释路径(v1 原形态:片段源经 typst-eval 解释)。
    fn closure_expr_interp(&mut self, closure: ast::Closure, env: &mut Env) -> String {
        // 命名闭包(`#let f(x) = ..` 或局部 `let f(x) = ..`)的节点文本
        // 形如 `f(x) = ..`,补 `let` 前缀才是可独立解析的源码。
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
        // 每调用点静态:parse 一次,创建零锁零查找。
        let node = self.closure_node_static(&src_text);
        format!("Value::Func(rt::closure_node_at(&{node}, {defaults}, {captured}, {pending_lit}, engine))")
    }

    /// C2:值位置闭包的原生路径。
    fn closure_expr_c2(
        &mut self,
        closure: ast::Closure,
        env: &mut Env,
        self_name: Option<String>,
    ) -> String {
        // C2:值位置闭包也走原生——闭包表达式编译为顶层 `f_c{id}`
        // 原生体,经签名保真 stub 暴露;捕获值按名排序打成元组随闭包
        // 值走(StateSlot 装元组,与顶层 C 装包状态同协议)。同定义
        // (同原文 + 同捕获名集)去重,多调用点共用一份原生体。
        let raw = closure.to_untyped().full_text().to_string();
        // stub 签名用参数原文(含默认值形态;stub 绑定期由求值器用
        // defaults 填齐缺失命名参数,转发到原生体时参数必然齐全)。
        let mut params_src = closure.params().to_untyped().full_text().to_string();
        // 匿名单参可写 `x => ..`(无括号),stub 解析需参数表形态。
        if !params_src.trim_start().starts_with('(') {
            params_src = format!("({params_src})");
        }
        // 默认值在创建期求值(每次创建;typst 闭包语义)。
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
        // 捕获:局部/模块槽即时物化(Some 包值);创建期不可见的名字
        // 运行期经全局+数学作用域物化(Option),调用期 None 报
        // unknown variable(对照 CapturesVisitor 的 `Err(_) => return`)。
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
                // 体生成在独立计数下(gen_closure_fn 内部会置
                // used_state=false 再追踪体自身引用),生成后取出
                // has_st 并把外层计数并回(捕获点/默认值表达式引用
                // __st 时外层需要 __st 前言)。
                let saved = self.used_state;
                let item = self.gen_closure_fn(
                    &format!("f_{id}"),
                    &format!("D_{id}"),
                    closure,
                    &cap_names,
                    self_name.as_deref(),
                    &params_src,
                );
                // gen_closure_fn 末尾 used_state = 体是否引用 __st
                //(模块槽/体内字符串 import),取出后并回外层计数。
                let has_st = std::mem::replace(&mut self.used_state, saved);
                // 提升到产物根的闭包体:file_span 引用按来源文件模块限定
                //(根作用域不可见;树/扁平两形态的文件模块都是根的兄弟,
                // `{mod}::file_span()` 同构可达)。
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
            // 创建点引用 __st.clone():外层函数必须携带状态前言。
            self.used_state = true;
        }
        // 创建点:捕获元组 + StateSlot + stub。
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
            // 自递归:defaults 随实例携带(重建 stub 用)。
            tuple.push(format!(
                "std::sync::Arc::new(rt::self_func_cell({defaults}))"
            ));
        }
        // stub 源翻译期组合(纯函数),节点进每调用点静态——创建
        // 零查找零锁(native_closure_node)。
        let stub_name = closure
            .name()
            .map(|n| n.as_str().to_string())
            .unwrap_or_else(|| "closure".to_string());
        let stub_src = trust_runtime::compose_stub(&stub_name, &params_src);
        let node_name = self.closure_node_static(&stub_src);
        format!(
            "{{ let __slot_{id} = std::sync::Arc::new(({})); Value::Func(rt::native_closure_node(&D_{id}, &{node_name}, {defaults}, &rt::StateSlot::new(__slot_{id}))) }}",
            tuple.join(", ")
        )
    }



    /// content 序列:遇 set/show 规则处切分,规则包装"后续全部"
    /// (对照 `eval_markup` 的尾部递归:`tail.styled_with_map/_recipe`)。
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

    /// 顶层内容折叠:各项按自己的可见绑定集生成(规则包装后续项)。
    pub(super) fn content_seq_top(&mut self, items: &[TopItem]) -> String {
        for (i, top) in items.iter().enumerate() {
            let mut env = top.env.clone();
            match &top.item {
                ContentItem::Set(set) => {
                    let mut parts: Vec<String> =
                        items[..i].iter().map(|e| self.content_part(e)).collect();
                    let tail = self.content_seq_top(&items[i + 1..]);
                    // 样式先于尾部内容求值(对照 eval_code/eval_markup:
                    // set.eval 之后才是 tail;Rust 实参从左到右,故用
                    // 局部先物化样式)。
                    let rule_at = self.span_of(set.span());
                    let styles = self.set_styles_code(*set, &mut env, &rule_at);
                    parts.push(format!("{{ let __s = {styles}; rt::styled_with_map({tail}, __s) }}"));
                    return self.content_join(parts);
                }
                ContentItem::Show(show) => {
                    let mut parts: Vec<String> =
                        items[..i].iter().map(|e| self.content_part(e)).collect();
                    let tail = self.content_seq_top(&items[i + 1..]);
                    // 求值序对照 `Eval for ast::ShowRule`:selector →
                    // transform(set 或表达式)→ 尾部内容。
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
                            // show-set:transform 是样式;set 的条件在
                            // set.eval 内处理(条件假 → 空样式)。
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

    /// set 规则的样式求值代码(条件假 → 空样式;条件先于目标与实参
    /// 求值,对照 `Eval for ast::SetRule`)。`call_at` 为调用点 span
    /// (Args.span 与规则 span 同源)。set/show-set 两臂共用。
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

    /// content 的单个部分(表达式产出 Content 构造代码;env 取该项
    /// 自身的可见绑定集)。
    fn content_part(&mut self, top: &TopItem) -> String {
        let mut env = top.env.clone();
        // 项内 let 表达式提升壳:项是单条 Rust 表达式,提升槽在项的
        // 块顶声明,绑定随项弃(content 块作用域近似;对照 typst 内容
        // 块的自作用域)。语句形态的 Part(let/import/include)由
        // 外层语句机制处理,不入提升面。
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
                // markup 简写(`~` nbsp、`--` en 等):对照原版
                // `Value::Symbol(runtime_char)`,经 display 进序列
                // (Symbol 元素与文本元素不同型——等值判断可区分)。
                format!("rt::symbol_value({:?})", s.get())
            }
            ContentItem::Part(ast::Expr::SmartQuote(q)) => {
                // 智能引号:构造 smartquote 元素,解析留给 layout 期。
                format!("rt::smartquote({})", q.double())
            }
            ContentItem::Part(ast::Expr::Label(l)) => {
                // label 不 embed:以 Value::Label 入序,由 attach_labels
                // 贴到末个可贴元素(对照 eval_markup 的 Label 分支)。
                format!("rt::label({:?})", l.get())
            }
            ContentItem::Part(ast::Expr::Strong(strong)) => {
                // `*body*`(对照 eval_markup:StrongElem::new(body)),
                // 体按嵌套 markup 序列翻译。
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
                // `_body_`(对照 eval_markup:EmphElem::new(body))。
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
                // 转义 `\[`:对照原版 `Value::Symbol(runtime_char)`
                // (不是 text——physica 的 `== [\[]` 靠类型区分)。
                format!("rt::symbol_value({:?})", e.get())
            }
            ContentItem::Part(ast::Expr::LetBinding(_))
            | ContentItem::Part(ast::Expr::ModuleImport(_)) => "rt::embed(Value::None)".into(),
            ContentItem::Part(other) => {
                // 值**不经 embed** 直接入序:attach_labels 里的
                // display 与 eval_markup 的 value.display() 同形;
                // 尤其 `#label(表达式)`(函数调用,计算名)产出
                // Value::Label,由 attach_labels 贴附而非独立内容项
                // (glossarium 驱动:`#label(prefix + key)` 曾被 embed,
                // 词条标签查询不到)。
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
            // 对照 eval_markup:label 贴到末个可贴元素(attach_labels),
            // 其余值 display 入序(glossarium 驱动的查询语义)。
            let items = parts
                .iter()
                .map(|p| {
                    if p.contains("/* never */") {
                        // never 型项(`#return`、常真 while 的发散检查):
                        // 尾式 never 使裸块为 never 型,协变入 Vec<Value>;
                        // `.into_value()` 不可作用于 never。发射侧以
                        // ` /* never */` 标记(scripting-return/while 驱动)。
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

    /// 捕获列表构造(闭包与 context 共用):局部/模块槽直接物化,
    /// 全局名存在才物化(创建期不可见的交给体内 import)。
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

    /// 结构化捕获表:(名字, 求值表达式) 列表 + 待运行期物化的全局名
    /// (C2 闭包按名排序组捕获元组,需要名字与表达式分离)。
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
                // 通配层内名(或可能经层遮蔽):创建期快照(对照
                // CapturesVisitor——创建期可见即捕获;层内未命中沿
                // 链回落,与 ident 同序)。
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
                // 全局物化名(G_ 槽/GlobalFallback):创建期可能不可见
                // ——走 pending 协议(resolve_pending 的 capture_opt 软物
                // 化,空则 None,调用期才报;与 typst 捕获时机一致。
                // elembic 驱动:`from-error => value => … from-check(…)`
                // 的 from-check 是兄弟闭包参数,真自由名,创建期必空,
                // 曾经 read_or 急切报错)。
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

/// 运行期通配层切片表达式(`&[&__w1, &__w2]`,内层在尾,查找自尾
/// 向首=内层遮蔽外层)。
fn wild_layers(env: &mut Env) -> String {
    let refs: Vec<String> = env.wildcards.iter().map(|w| format!("&{w}")).collect();
    format!("&[{}]", refs.join(", "))
}

/// 表达式是否恒值(对照 typst-eval flow.rs 的 `is_invariant`:
/// 标识符与数学标识符视作可变,字段访问随目标,调用随被调+实参,
/// 其余节点看全部子节点)。
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

/// pending 名单的字面量形态(`&["a", "b"]`;创建期经全局+数学作用域
/// 物化,见 rt::resolve_pending)。
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

/// 表达式是否含 break/return(对照 typst-eval flow.rs 的 `can_diverge`)。
fn can_diverge(expr: &typst_syntax::SyntaxNode) -> bool {
    matches!(expr.kind(), SyntaxKind::Break | SyntaxKind::Return)
        || expr.children().any(can_diverge)
}

/// 扫描表达式位置 `let` 绑定的名字(为提升槽登记)。作用域边界
/// 对照 typst:不穿越闭包/代码块/内容块(各有自身作用域——闭包经
/// CapturesVisitor 自管,代码块由 block() 递归自扫,内容块由
/// content_part 项内自包);LetBinding 的 init 内再嵌 let 仍绑定本
/// 作用域,继续下行。
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
