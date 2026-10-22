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
use super::naming::escape;

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
}

impl Ctx {
    /// 表达式直译:返回类型为 `Value` 的 Rust 表达式(可含 `?`)。
    pub(super) fn expr(&mut self, expr: ast::Expr, env: &Env) -> String {
        match expr {
            ast::Expr::Ident(ident) => self.ident(ident, env),
            ast::Expr::None(_) => "Value::None".into(),
            ast::Expr::Auto(_) => "Value::Auto".into(),
            ast::Expr::Bool(b) => format!("Value::Bool({})", b.get()),
            ast::Expr::Int(i) => format!("Value::Int({})", i.get()),
            ast::Expr::Float(f) => format!("Value::Float({:?})", f.get()),
            ast::Expr::Numeric(n) => {
                let (value, unit) = n.get();
                format!("Value::numeric(({value:?}, Unit::{}))", unit_variant(unit))
            }
            ast::Expr::Str(s) => format!("Value::Str({:?}.into())", s.get().as_str()),
            ast::Expr::Parenthesized(p) => self.expr(p.expr(), env),
            ast::Expr::CodeBlock(block) => self.block(block.body().exprs(), env),
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
                            _ => {
                                self.diag(path.span(), format!("include \"{raw}\" 不在包文件集"));
                                "Value::None".into()
                            }
                        }
                    }
                    other => {
                        self.diag(other.span(), "include 源暂只支持字符串路径".into());
                        "Value::None".into()
                    }
                }
            }
            ast::Expr::Contextual(ctx) => {
                // `#context expr`:构造推迟求值的闭包(对照 `Eval for
                // ast::Contextual`),realize/layout 期带 location/styles 求值。
                let body = ctx.body().to_untyped().full_text().to_string();
                let mut names: Vec<String> = Vec::new();
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
                let body = self.expr(while_loop.body(), env);
                let cond_at = self.span_of(while_loop.condition().span());
                let join_at = self.span_of(while_loop.body().span());
                let loop_at = self.span_of(while_loop.span());
                // 静态"常真"检测(对照原生 i==0 分支的 is_invariant/
                // can_diverge:条件不变式且体不含 break/return 即必为
                // 无限循环;条件本身仍先求值,保持副作用顺序)。
                if is_invariant(while_loop.condition().to_untyped())
                    && !can_diverge(while_loop.body().to_untyped())
                {
                    return format!(
                        "{{ rt::boolean({condition}, {cond_at})?; bail!({cond_at}, \"condition is always true\") }}"
                    );
                }
                // 迭代护栏(对照原生 MAX_ITERATIONS:超出报 "loop
                // seems to be infinite",而非挂死)。
                format!(
                    "{{ let mut __out = Value::None; let mut __guard = 0usize;\n  \
                     while rt::boolean({condition}, {cond_at})? {{\n  \
                     \x20   if __guard >= rt::MAX_ITERATIONS {{ bail!({loop_at}, \"loop seems to be infinite\"); }}\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, {join_at})?;\n  \
                     \x20   __guard += 1usize;\n  \
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
                format!("return Ok({value})")
            }
            ast::Expr::LoopBreak(_) => "{ break; Value::None }".into(),
            ast::Expr::LoopContinue(_) => "{ continue; Value::None }".into(),
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
                let body = self.expr(for_loop.body(), &body_env);
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
            ast::Expr::Equation(_) => {
                let src = expr.to_untyped().full_text().trim().to_string();
                let mut names: Vec<String> = Vec::new();
                collect_idents(expr.to_untyped(), &mut Vec::new(), &mut names);
                let (captured, pending) = self.capture_entries(&names, env);
                let pending_lit = pending_list(&pending);
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

    fn ident(&mut self, ident: ast::Ident, env: &Env) -> String {
        let name = ident.as_str();
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
            return format!("{rust}.clone()");
        }
        if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
            return self.ref_value(module_ref);
        }
        format!("rt::global(engine, \"{}\", file_span())?", escape(name))
    }

    fn binary(&mut self, binary: ast::Binary, env: &Env) -> String {
        use ast::BinOp::*;
        let op = binary.op();
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

    fn func_call(&mut self, call: ast::FuncCall, env: &Env) -> String {
        let at = self.span_of(call.span());
        // 参数按源序求值(对照 `Eval for ast::Args`);Args.span 为整个
        // 调用的 span(对照 `.spanned(self.span())`)。
        if let ast::Expr::FieldAccess(access) = call.callee() {
            let field = access.field().as_str();
            if matches!(field, "push" | "pop" | "insert" | "remove") {
                // 可变方法:目标 = 局部变量或字段/accessor 链
                // (typst 语义:就地修改,对照 `call_method_mut`;
                // 实参先于目标求值,对照 `maybe_resolve_mutating`)。
                if let Some((root, path)) = self.parse_place(access.target(), env) {
                    let path_lit = super::block::place_path_lit(&path);
                    let args = self.gen_args(call.args(), env, &at);
                    return format!(
                        "{{ rt::check_call_depth(engine, {at})?; let __mut_args = {args}; rt::method_mut(engine, rt::place_mut(&mut {root}, {path_lit}, {at})?, \"{}\", __mut_args, {at})? }}",
                        escape(field)
                    );
                }
                self.diag(
                    call.span(),
                    "可变方法的目标须为局部变量或字段/accessor 路径".into(),
                );
                return "Value::None".into();
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
    fn gen_args(&mut self, args: ast::Args, env: &Env, call_at: &str) -> String {
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
    fn closure_expr(&mut self, closure: ast::Closure, env: &Env) -> String {
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
    fn closure_expr_interp(&mut self, closure: ast::Closure, env: &Env) -> String {
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
        let mut names: Vec<String> = Vec::new();
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
        env: &Env,
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
        let mut names: Vec<String> = Vec::new();
        collect_idents(closure.to_untyped(), &mut Vec::new(), &mut names);
        let (mut pairs, pending) = self.capture_pairs(&names, env);
        for name in &pending {
            pairs.push((
                name.clone(),
                format!("rt::capture_opt(engine, \"{}\")?", escape(name)),
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
            if pending.iter().any(|p| p == name) {
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
    pub(super) fn content_seq(&mut self, items: &[ContentItem], env: &Env) -> String {
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
            let env = &top.env;
            match &top.item {
                ContentItem::Set(set) => {
                    let mut parts: Vec<String> =
                        items[..i].iter().map(|e| self.content_part(e)).collect();
                    let tail = self.content_seq_top(&items[i + 1..]);
                    // 样式先于尾部内容求值(对照 eval_code/eval_markup:
                    // set.eval 之后才是 tail;Rust 实参从左到右,故用
                    // 局部先物化样式)。
                    let rule_at = self.span_of(set.span());
                    let styles = self.set_styles_code(*set, env, &rule_at);
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
                            (format!("Some({})", self.expr(sel, env)), sel_at)
                        }
                        None => ("None".to_string(), show_at.clone()),
                    };
                    let recipe = match show.transform() {
                        ast::Expr::SetRule(set) => {
                            // show-set:transform 是样式;set 的条件在
                            // set.eval 内处理(条件假 → 空样式)。
                            let set_at = self.span_of(set.span());
                            let styles = self.set_styles_code(set, env, &set_at);
                            format!(
                                "rt::show_rule_style({sel_arg}, {sel_at}, {styles}, {show_at}, engine)?"
                            )
                        }
                        transform => {
                            format!(
                                "rt::show_rule({sel_arg}, {sel_at}, {}, {show_at}, engine)?",
                                self.expr(transform, env)
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
    pub(super) fn set_styles_code(&mut self, set: ast::SetRule, env: &Env, call_at: &str) -> String {
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
        let env = &top.env;
        match &top.item {
            ContentItem::Include(body_slot) => {
                self.used_state = true;
                format!("rt::read_content(&__st.{body_slot})?")
            }
            ContentItem::Part(ast::Expr::Text(t)) => {
                format!("rt::text({:?})", t.get().as_str())
            }
            ContentItem::Part(ast::Expr::Space(_)) => "rt::space()".into(),
            ContentItem::Part(ast::Expr::Linebreak(_)) => "rt::linebreak()".into(),
            ContentItem::Part(ast::Expr::Parbreak(_)) => "rt::parbreak()".into(),
            ContentItem::Part(ast::Expr::Escape(e)) => {
                format!("rt::text({:?})", e.get().to_string())
            }
            ContentItem::Part(ast::Expr::LetBinding(_))
            | ContentItem::Part(ast::Expr::ModuleImport(_)) => "rt::embed(Value::None)".into(),
            ContentItem::Part(other) => {
                let value = self.expr(*other, env);
                format!("rt::embed({value})")
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
            format!("rt::content_sequence(vec![{}])", parts.join(", "))
        }
    }

    /// 捕获列表构造(闭包与 context 共用):局部/模块槽直接物化,
    /// 全局名存在才物化(创建期不可见的交给体内 import)。
    fn capture_entries(&mut self, names: &[String], env: &Env) -> (Vec<String>, Vec<String>) {
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
    fn capture_pairs(&mut self, names: &[String], env: &Env) -> (Vec<(String, String)>, Vec<String>) {
        let mut captured: Vec<(String, String)> = Vec::new();
        let mut globals = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
                captured.push((name.clone(), format!("{rust}.clone()")));
            } else if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
                let value = self.ref_value(module_ref);
                captured.push((name.clone(), value));
            } else {
                globals.push(name.clone());
            }
        }
        (captured, globals)
    }
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
fn pending_list(names: &[String]) -> String {
    if names.is_empty() {
        "&[]".to_string()
    } else {
        format!(
            "&[{}]",
            names
                .iter()
                .map(|n| format!("{:?}", n))
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
