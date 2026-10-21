//! 表达式直译:typst 表达式 → 返回 `Value` 的 Rust 表达式(可含 `?`)。
//!
//! 入口 `Ctx::expr` 按表达式种类分派;其余方法(ident 解析、二元运算、
//! 调用、调用点参数、闭包)均为其内部步骤。

use std::collections::HashSet;
use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::captures::collect_idents;
use super::ctx::{Ctx, Env};
use super::naming::escape;

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
                format!("Value::numeric(({value:?}, Unit::{unit:?}))")
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
                let (captured, globals) = self.capture_entries(&names, env);
                if captured.is_empty() && globals.is_empty() {
                    format!("Value::Content(rt::context_body(rt::context_closure({body:?}, &[])))")
                } else {
                    // 块内构造、块内消费(&__caps 不逃出块)。
                    let init = if captured.is_empty() {
                        "Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", captured.join(", "))
                    };
                    let mut pushes = String::new();
                    for name in &globals {
                        pushes.push_str(&format!(
                            " if let Some(v) = rt::global_opt(engine, \"{}\")? {{ __caps.push((\"{}\", v)); }}",
                            escape(name),
                            escape(name)
                        ));
                    }
                    format!(
                        "{{ let mut __caps: Vec<(&'static str, Value)> = {init};{pushes} Value::Content(rt::context_body(rt::context_closure({body:?}, &__caps))) }}"
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
                            let ast::Expr::Str(key) = keyed.key() else {
                                unreachable!("keyed pair key is a string")
                            };
                            let key = key.get();
                            let value = self.expr(keyed.expr(), env);
                            let _ = write!(out, " rt::dict_put(&mut __d, {key:?}, {value});");
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
                    let at = self.span_of(access.span());
                    format!("rt::field(&({target}), \"{}\", {at})?", escape(field))
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
                let join_at = self.span_of(while_loop.span());
                format!(
                    "{{ let mut __out = Value::None;\n  \
                     while rt::boolean({condition}, {cond_at})? {{\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, {join_at})?;\n  \
                     }}\n  \
                     __out }}"
                )
            }
            ast::Expr::FuncReturn(func_return) => {
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
                // 模式解构在循环体首展开(单一标识符直接绑定)。
                let mut body_env = env.clone();
                let pattern_code = {
                    let inner = format!("{{ {binder}.clone() }}");
                    self.destructure(for_loop.pattern(), inner, &mut body_env, false)
                };
                let body = self.expr(for_loop.body(), &body_env);
                let iter_at = self.span_of(for_loop.iterable().span());
                let join_at = self.span_of(for_loop.span());
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
        // 参数按源序求值(对照 `Eval for ast::Args`)。
        let args = self.gen_args(call.args(), env);
        if let ast::Expr::FieldAccess(access) = call.callee() {
            let field = access.field().as_str();
            if matches!(field, "push" | "pop" | "insert" | "remove") {
                // 可变方法:目标 = 局部变量或字段/accessor 链
                // (typst 语义:就地修改,对照 `call_method_mut`)。
                if let Some((root, path)) = self.parse_place(access.target(), env) {
                    let path_lit = super::block::place_path_lit(&path);
                    let at = self.span_of(call.span());
                    return format!(
                        "{{ let __mut_args = {args}; rt::method_mut(rt::place_mut(&mut {root}, {path_lit}, {at})?, \"{}\", __mut_args, {at})? }}",
                        escape(field)
                    );
                }
                self.diag(
                    call.span(),
                    "可变方法的目标须为局部变量或字段/accessor 路径".into(),
                );
                return "Value::None".into();
            }
            let target = self.expr(access.target(), env);
            let at = self.span_of(call.span());
            return format!(
                "{{ let __target = {target}; let __call_args = {args}; rt::method(&__target, \"{}\", engine, context, {at}, __call_args)? }}",
                escape(field)
            );
        }
        let callee = self.expr(call.callee(), env);
        let at = self.span_of(call.span());
        format!(
            "{{ let __callee = {callee}; let __call_args = {args}; rt::call(&__callee, engine, context, {at}, __call_args)? }}"
        )
    }

    /// 调用点参数:按源序构造 `Args`。
    fn gen_args(&mut self, args: ast::Args, env: &Env) -> String {
        let mut out = String::from(
            "{ let mut __fs = file_span(); let mut __call_args_v = Args::new(__fs, std::iter::empty::<Value>());",
        );
        for arg in args.items() {
            match arg {
                ast::Arg::Pos(expr) => {
                    let value = self.expr(expr, env);
                    let _ = write!(out, " __call_args_v.push(__fs, {value});");
                }
                ast::Arg::Named(named) => {
                    let name = named.name().as_str();
                    let value = self.expr(named.expr(), env);
                    let _ = write!(
                        out,
                        " __call_args_v.items.push(rt::named_arg(__fs, \"{}\", {value}));",
                        escape(name)
                    );
                }
                ast::Arg::Spread(spread) => {
                    let value = self.expr(spread.expr(), env);
                    let _ = write!(out, " rt::spread_args(&mut __call_args_v, __fs, {value})?;");
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
        // 命名闭包(`#let f(x) = ..` 或局部 `let f(x) = ..`)的节点文本
        // 形如 `f(x) = ..`,补 `let` 前缀才是可独立解析的源码;名字保留
        // 以支持递归自引用。
        let raw = closure.to_untyped().full_text().to_string();
        let src_text = if closure.name().is_some() {
            format!("let {raw}")
        } else {
            raw
        };
        let mut defaults = Vec::new();
        for param in closure.params().children() {
            if let ast::Param::Named(named) = param {
                let value = self.expr(named.expr(), env);
                defaults.push(value);
            }
        }
        let mut names: Vec<String> = Vec::new();
        collect_idents(closure.to_untyped(), &mut Vec::new(), &mut names);
        // 全局名创建期物化;创建期不可见的(如通配 import 引入的名字)
        // 不捕获,由闭包体内自己的 import 在调用期绑定(对照 typst
        // CapturesVisitor 的 `Err(_) => return`)。
        let (captured, captured_globals) = self.capture_entries(&names, env);
        let defaults = if defaults.is_empty() {
            "Vec::new()".into()
        } else {
            format!("vec![{}]", defaults.join(", "))
        };
        if captured_globals.is_empty() {
            let captured = if captured.is_empty() {
                "&[]".into()
            } else {
                format!("&[{}]", captured.join(", "))
            };
            format!("Value::Func(rt::closure({src_text:?}, {defaults}, {captured}))")
        } else {
            let init = if captured.is_empty() {
                "Vec::new()".to_string()
            } else {
                format!("vec![{}]", captured.join(", "))
            };
            let mut pushes = String::new();
            for name in &captured_globals {
                let _ = write!(
                    pushes,
                    " if let Some(v) = rt::global_opt(engine, \"{}\")? {{ __caps.push((\"{}\", v)); }}",
                    escape(name),
                    escape(name)
                );
            }
            format!(
                "{{ let mut __caps: Vec<(&'static str, Value)> = {init};{pushes} Value::Func(rt::closure({src_text:?}, {defaults}, &__caps)) }}"
            )
        }
    }

    /// content 序列:遇 set/show 规则处切分,规则包装"后续全部"
    /// (对照 `eval_markup` 的尾部递归:`tail.styled_with_map/_recipe`)。
    pub(super) fn content_seq(&mut self, items: &[ContentItem], env: &Env) -> String {
        for (i, item) in items.iter().enumerate() {
            match item {
                ContentItem::Set(set) => {
                    let mut parts: Vec<String> = items[..i]
                        .iter()
                        .map(|e| self.content_part(e, env))
                        .collect();
                    let tail = self.content_seq(&items[i + 1..], env);
                    let args = self.gen_args(set.args(), env);
                    let target = self.expr(set.target(), env);
                    // 条件为假时规则为空样式(对照 `Ok(Styles::new())`)。
                    let styles = match set.condition() {
                        Some(cond) => format!(
                            "if rt::boolean({}, SPAN)? {{ rt::set_rule(engine, {target}, {args}, SPAN)? }} else {{ rt::styles_empty() }}",
                            self.expr(cond, env)
                        ),
                        None => format!("rt::set_rule(engine, {target}, {args}, SPAN)?"),
                    };
                    parts.push(format!("rt::styled_with_map({tail}, {styles})"));
                    return self.content_join(parts);
                }
                ContentItem::Show(show) => {
                    let mut parts: Vec<String> = items[..i]
                        .iter()
                        .map(|e| self.content_part(e, env))
                        .collect();
                    let tail = self.content_seq(&items[i + 1..], env);
                    let sel_arg = match show.selector() {
                        Some(sel) => format!("Some({})", self.expr(sel, env)),
                        None => "None".to_string(),
                    };
                    let recipe = match show.transform() {
                        ast::Expr::SetRule(set) => {
                            let args = self.gen_args(set.args(), env);
                            let target = self.expr(set.target(), env);
                            format!(
                                "rt::show_rule_style({sel_arg}, rt::set_rule(engine, {target}, {args}, SPAN)?, SPAN)?"
                            )
                        }
                        transform => {
                            format!(
                                "rt::show_rule({sel_arg}, {}, SPAN)?",
                                self.expr(transform, env)
                            )
                        }
                    };
                    parts.push(format!(
                        "{{ let __tail = {tail}; let __recipe = {recipe}; rt::styled_with_recipe(engine, context, __tail, __recipe)? }}"
                    ));
                    return self.content_join(parts);
                }
                _ => {}
            }
        }
        let parts: Vec<String> = items.iter().map(|e| self.content_part(e, env)).collect();
        self.content_join(parts)
    }

    /// content 的单个部分(表达式产出 Content 构造代码)。
    fn content_part(&mut self, item: &ContentItem, env: &Env) -> String {
        match item {
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
            // 语句类表达式(let/import)求值为 none,并入空 display 槽
            // (对照 eval_markup 的 `seq.push(value.display())`)。
            ContentItem::Part(ast::Expr::LetBinding(_))
            | ContentItem::Part(ast::Expr::ModuleImport(_)) => "rt::embed(Value::None)".into(),
            ContentItem::Part(other) => {
                let value = self.expr(*other, env);
                format!("rt::embed({value})")
            }
            ContentItem::Set(_) | ContentItem::Show(_) => {
                unreachable!("rules handled by content_seq")
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
        let mut captured = Vec::new();
        let mut globals = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
                captured.push(format!("(\"{}\", {rust}.clone())", escape(name)));
            } else if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
                let value = self.ref_value(module_ref);
                captured.push(format!("(\"{}\", {value})", escape(name)));
            } else {
                globals.push(name.clone());
            }
        }
        (captured, globals)
    }
}
