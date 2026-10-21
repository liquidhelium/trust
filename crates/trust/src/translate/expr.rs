//! 表达式直译:typst 表达式 → 返回 `Value` 的 Rust 表达式(可含 `?`)。
//!
//! 入口 `Ctx::expr` 按表达式种类分派;其余方法(ident 解析、二元运算、
//! 调用、调用点参数、闭包)均为其内部步骤。

use std::collections::HashSet;
use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::captures::collect_idents;
use super::ctx::{Ctx, Env, Mode, Ref};
use super::naming::escape;

impl Ctx {
    /// 表达式直译:返回类型为 `Value` 的 Rust 表达式(可含 `?`)。
    pub(super) fn expr(&mut self, expr: ast::Expr, env: &Env, mode: Mode) -> String {
        // 顶层值上下文(engine-free init 返回 Value)不能使用 `?`:
        // 可失败运算(算术/比较/迭代/join)一律判为需要引擎。
        if mode == Mode::ModuleInit
            && matches!(
                expr,
                ast::Expr::Unary(_)
                    | ast::Expr::Binary(_)
                    | ast::Expr::ForLoop(_)
                    | ast::Expr::WhileLoop(_)
            )
        {
            self.diag(
                expr.span(),
                "顶层值中的可失败运算需要引擎(请包进函数)".into(),
            );
            return "Value::None".into();
        }
        match expr {
            ast::Expr::Ident(ident) => self.ident(ident, env, mode),
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
            ast::Expr::Parenthesized(p) => self.expr(p.expr(), env, mode),
            ast::Expr::CodeBlock(block) => self.block(block.body().exprs(), env, mode),
            ast::Expr::ContentBlock(block) => {
                // content 块直译:子集 Text/Space/Linebreak/Parbreak/Escape/
                // 嵌入表达式,构造方式对照 typst-eval/src/markup.rs。
                let mut parts = Vec::new();
                for expr in block.body().exprs() {
                    match expr {
                        ast::Expr::Text(t) => {
                            parts.push(format!("rt::text({:?})", t.get().as_str()));
                        }
                        ast::Expr::Space(_) => parts.push("rt::space()".into()),
                        ast::Expr::Linebreak(_) => parts.push("rt::linebreak()".into()),
                        ast::Expr::Parbreak(_) => parts.push("rt::parbreak()".into()),
                        ast::Expr::Escape(e) => {
                            parts.push(format!("rt::text({:?})", e.get().to_string()));
                        }
                        other => {
                            let value = self.expr(other, env, mode);
                            parts.push(format!("rt::embed({value})"));
                        }
                    }
                }
                if parts.is_empty() {
                    "Value::Content(Content::sequence(Vec::new()))".into()
                } else {
                    format!(
                        "Value::Content(rt::content_sequence(vec![{}]))",
                        parts.join(", ")
                    )
                }
            }
            ast::Expr::Array(array) => {
                let mut out = String::from("{ let mut __a: Vec<Value> = Vec::new();");
                for item in array.items() {
                    match item {
                        ast::ArrayItem::Pos(expr) => {
                            let value = self.expr(expr, env, mode);
                            let _ = write!(out, " __a.push({value});");
                        }
                        ast::ArrayItem::Spread(spread) => {
                            let value = self.expr(spread.expr(), env, mode);
                            let _ = write!(out, " rt::array_spread(&mut __a, {value}, SPAN)?;");
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
                            let value = self.expr(named.expr(), env, mode);
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
                            let value = self.expr(keyed.expr(), env, mode);
                            let _ = write!(out, " rt::dict_put(&mut __d, {key:?}, {value});");
                        }
                        ast::DictItem::Spread(spread) => {
                            let value = self.expr(spread.expr(), env, mode);
                            let _ = write!(out, " rt::dict_spread(&mut __d, {value}, SPAN)?;");
                        }
                    }
                }
                out.push_str(" Value::Dict(__d) }");
                out
            }
            ast::Expr::Unary(unary) => {
                let inner = self.expr(unary.expr(), env, mode);
                let op = match unary.op() {
                    ast::UnOp::Pos => "pos",
                    ast::UnOp::Neg => "neg",
                    ast::UnOp::Not => "not",
                };
                format!("ops::{op}({inner}).at(SPAN)?")
            }
            ast::Expr::Binary(binary) => self.binary(binary, env, mode),
            ast::Expr::FieldAccess(access) => {
                let target = self.expr(access.target(), env, mode);
                let field = access.field().as_str();
                format!("rt::field(&({target}), \"{}\", SPAN)?", escape(field))
            }
            ast::Expr::FuncCall(call) => self.func_call(call, env, mode),
            ast::Expr::Closure(closure) => self.closure_expr(closure, env, mode),
            ast::Expr::Conditional(cond) => {
                let condition = self.expr(cond.condition(), env, mode);
                let then = self.expr(cond.if_body(), env, mode);
                let otherwise = match cond.else_body() {
                    Some(e) => self.expr(e, env, mode),
                    None => "Value::None".into(),
                };
                format!("if rt::boolean({condition}, SPAN)? {{ {then} }} else {{ {otherwise} }}")
            }
            ast::Expr::WhileLoop(while_loop) => {
                let condition = self.expr(while_loop.condition(), env, mode);
                let body = self.expr(while_loop.body(), env, mode);
                format!(
                    "{{ let mut __out = Value::None;\n  \
                     while rt::boolean({condition}, SPAN)? {{\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, SPAN)?;\n  \
                     }}\n  \
                     __out }}"
                )
            }
            ast::Expr::FuncReturn(func_return) => {
                let value = match func_return.body() {
                    Some(expr) => self.expr(expr, env, mode),
                    None => "Value::None".into(),
                };
                format!("return Ok({value})")
            }
            ast::Expr::LoopBreak(_) => "{ break; Value::None }".into(),
            ast::Expr::LoopContinue(_) => "{ continue; Value::None }".into(),
            ast::Expr::ForLoop(for_loop) => {
                let iterable = self.expr(for_loop.iterable(), env, mode);
                let id = self.fresh_id();
                let binder = format!("__it{id}");
                // 模式解构在循环体首展开(单一标识符直接绑定)。
                let mut body_env = env.clone();
                let pattern_code = {
                    let inner = format!("{{ {binder}.clone() }}");
                    self.destructure(for_loop.pattern(), inner, &mut body_env, mode, false)
                };
                let body = self.expr(for_loop.body(), &body_env, mode);
                format!(
                    "{{ let mut __out = Value::None;\n  \
                     for {binder} in rt::iter_values(&({iterable}), SPAN)? {{\n  \
                     \x20   {pattern_code}\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, SPAN)?;\n  \
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

    fn ident(&mut self, ident: ast::Ident, env: &Env, mode: Mode) -> String {
        let name = ident.as_str();
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
            return format!("{rust}.clone()");
        }
        if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
            return match module_ref {
                Ref::Slot(id) => format!("rt::slot(&{id})"),
                Ref::Engine(id) => format!("{id}_get()?"),
                Ref::ModuleField { expr, name } => {
                    format!("rt::field(&({expr}), \"{name}\", SPAN)?")
                }
                Ref::Value(expr) => expr.clone(),
            };
        }
        if mode == Mode::ModuleInit {
            self.diag(
                ident.span(),
                format!("顶层值引用标准库 `{name}`,需要引擎(暂不支持,请包进函数)"),
            );
            "Value::None".into()
        } else {
            format!("rt::global(engine, \"{}\", file_span())?", escape(name))
        }
    }

    fn binary(&mut self, binary: ast::Binary, env: &Env, mode: Mode) -> String {
        use ast::BinOp::*;
        let op = binary.op();
        // and/or 短路(对照 typst-eval/src/ops.rs)。
        if matches!(op, And | Or) {
            let lhs = self.expr(binary.lhs(), env, mode);
            let rhs = self.expr(binary.rhs(), env, mode);
            let (test, fuse, rest) = match op {
                And => ("__l == Value::Bool(false)", "Value::Bool(false)", "and"),
                Or => ("__l == Value::Bool(true)", "Value::Bool(true)", "or"),
                _ => unreachable!(),
            };
            return format!(
                "{{ let __l = {lhs}; if {test} {{ {fuse} }} else {{ ops::{rest}(__l, {rhs}).at(SPAN)? }} }}"
            );
        }
        let lhs = self.expr(binary.lhs(), env, mode);
        let rhs = self.expr(binary.rhs(), env, mode);
        match op {
            Add => format!("ops::add({lhs}, {rhs}).at(SPAN)?"),
            Sub => format!("ops::sub({lhs}, {rhs}).at(SPAN)?"),
            Mul => format!("ops::mul({lhs}, {rhs}).at(SPAN)?"),
            Div => format!("ops::div({lhs}, {rhs}).at(SPAN)?"),
            Eq => format!("ops::eq({lhs}, {rhs}).at(SPAN)?"),
            Neq => format!("ops::neq({lhs}, {rhs}).at(SPAN)?"),
            Lt | Leq | Gt | Geq => {
                let ord = format!("ops::compare(&({lhs}), &({rhs})).at(SPAN)?");
                let test = match op {
                    Lt => "std::cmp::Ordering::Less",
                    Leq => "std::cmp::Ordering::Less | std::cmp::Ordering::Equal",
                    Gt => "std::cmp::Ordering::Greater",
                    Geq => "std::cmp::Ordering::Greater | std::cmp::Ordering::Equal",
                    _ => unreachable!(),
                };
                format!("Value::Bool(matches!({ord}, {test}))")
            }
            In => format!("ops::in_({lhs}, {rhs}).at(SPAN)?"),
            NotIn => format!("ops::not_in({lhs}, {rhs}).at(SPAN)?"),
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

    fn func_call(&mut self, call: ast::FuncCall, env: &Env, mode: Mode) -> String {
        // 参数按源序求值(对照 `Eval for ast::Args`)。
        let args = self.gen_args(call.args(), env, mode);
        if let ast::Expr::FieldAccess(access) = call.callee() {
            let field = access.field().as_str();
            if matches!(field, "push" | "pop" | "insert" | "remove") {
                // 可变方法:目标须为局部变量(typst 语义:就地修改)。
                if let ast::Expr::Ident(ident) = access.target()
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    return format!(
                        "{{ let __mut_args = {args}; rt::method_mut(&mut {rust}, \"{}\", __mut_args, SPAN)? }}",
                        escape(field)
                    );
                }
                self.diag(
                    call.span(),
                    "可变方法的目标不是局部变量(暂只支持直接局部变量)".into(),
                );
                return "Value::None".into();
            }
            let target = self.expr(access.target(), env, mode);
            return format!(
                "{{ let __target = {target}; let __call_args = {args}; rt::method(&__target, \"{}\", engine, context, SPAN, __call_args)? }}",
                escape(field)
            );
        }
        if mode == Mode::ModuleInit {
            self.diag(
                call.span(),
                "顶层值中的函数调用需要引擎(暂不支持,请包进函数)".into(),
            );
            return "Value::None".into();
        }
        let callee = self.expr(call.callee(), env, mode);
        format!(
            "{{ let __callee = {callee}; let __call_args = {args}; rt::call(&__callee, engine, context, SPAN, __call_args)? }}"
        )
    }

    /// 调用点参数:按源序构造 `Args`。
    fn gen_args(&mut self, args: ast::Args, env: &Env, mode: Mode) -> String {
        let mut out = String::from(
            "{ let mut __fs = file_span(); let mut __call_args_v = Args::new(__fs, std::iter::empty::<Value>());",
        );
        for arg in args.items() {
            match arg {
                ast::Arg::Pos(expr) => {
                    let value = self.expr(expr, env, mode);
                    let _ = write!(out, " __call_args_v.push(__fs, {value});");
                }
                ast::Arg::Named(named) => {
                    let name = named.name().as_str();
                    let value = self.expr(named.expr(), env, mode);
                    let _ = write!(
                        out,
                        " __call_args_v.items.push(rt::named_arg(__fs, \"{}\", {value}));",
                        escape(name)
                    );
                }
                ast::Arg::Spread(spread) => {
                    let value = self.expr(spread.expr(), env, mode);
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
    fn closure_expr(&mut self, closure: ast::Closure, env: &Env, mode: Mode) -> String {
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
                let value = self.expr(named.expr(), env, mode);
                defaults.push(value);
            }
        }
        let mut names: Vec<String> = Vec::new();
        collect_idents(closure.to_untyped(), &mut Vec::new(), &mut names);
        let mut captured = Vec::new();
        let mut seen = HashSet::new();
        for name in names {
            if !seen.insert(name.clone()) {
                continue;
            }
            if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| *n == name) {
                captured.push(format!("(\"{}\", {rust}.clone())", escape(&name)));
            } else if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| *n == name)
            {
                match module_ref {
                    Ref::Slot(id) => {
                        captured.push(format!("(\"{}\", rt::slot(&{id}))", escape(&name)))
                    }
                    Ref::Engine(id) => {
                        captured.push(format!("(\"{}\", {id}_get()?)", escape(&name)))
                    }
                    Ref::ModuleField { expr, name: field } => captured.push(format!(
                        "(\"{}\", rt::field(&({expr}), \"{field}\", SPAN)?)",
                        escape(&name)
                    )),
                    Ref::Value(expr) => captured.push(format!("(\"{}\", {expr})", escape(&name))),
                }
            } else if mode == Mode::FnBody {
                // 全局名:typst 闭包求值不带库基座(Scopes::new(None)),
                // 全局引用须在创建期物化进 captured(对照 CapturesVisitor)。
                captured.push(format!(
                    "(\"{}\", rt::global(engine, \"{}\", SPAN)?)",
                    escape(&name),
                    escape(&name)
                ));
            }
        }
        let defaults = if defaults.is_empty() {
            "Vec::new()".into()
        } else {
            format!("vec![{}]", defaults.join(", "))
        };
        let captured = if captured.is_empty() {
            "&[]".into()
        } else {
            format!("&[{}]", captured.join(", "))
        };
        format!("Value::Func(rt::closure({src_text:?}, {defaults}, {captured}))")
    }
}
