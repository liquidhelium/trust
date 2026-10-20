//! 翻译器本体:typst 源码 → Rust 源码(nuitka 路线的 L0 直译层)。
//!
//! 每个顶层 `#let` 绑定编译为:
//! - 函数 → `NativeFuncData` 静态(手写字段,不经宏):体为参数解构 +
//!   表达式直译(全程 `Value`,运算走 `typst::foundations::ops`,报错
//!   文案与 span 行为对照 typst-eval);
//! - 值 → `LazyLock<Value>` 槽,模块构建时按源序注册。
//!
//! 语义边界(v1,详见 docs/research-route.md):
//! - 顶层值表达式必须免引擎(字面量/集合/闭包/槽引用);需要引擎的
//!   构造(函数调用、标准库引用)请包进函数;
//! - 命名参数默认值在每次调用时求值(原版在定义期求值一次;纯代码
//!   等价,求值次数有差);
//! - 闭包体由 typst-eval 解释执行(保真路径);捕获在创建期物化;
//! - 错误的 span 使用 detached(差分对拍比较值,不比较定位)。

use std::collections::HashSet;
use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

/// 翻译诊断:语法 span + 消息。
#[derive(Debug, Clone)]
pub struct Diag {
    pub span: typst_syntax::Span,
    pub message: String,
}

/// 把一个 typst 模块源文件翻译为 Rust 源码文本。
pub fn translate(src: &str, module_name: &str) -> Result<String, Vec<Diag>> {
    let root = typst_syntax::parse(src);
    let Some(markup) = root.cast::<ast::Markup>() else {
        return Err(vec![Diag { span: typst_syntax::Span::detached(), message: "源文件未能解析为 markup 模块".into() }]);
    };

    let mut cx = Ctx { diags: Vec::new(), next_id: 0 };
    // 第一遍:收集顶层绑定(名字 → 槽),函数绑定取其闭包节点。
    let mut tops: Vec<TopBinding> = Vec::new();
    for expr in markup.exprs() {
        match expr {
            ast::Expr::Space(_) | ast::Expr::Parbreak(_) => {}
            ast::Expr::LetBinding(lets) => match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    let Some(ast::Expr::Closure(closure)) = lets.init() else {
                        cx.diag(lets.span(), "闭包绑定缺少闭包初始化".into());
                        continue;
                    };
                    let id = cx.fresh_id();
                    let rust = format!("V{id}_{}", sanitize(name.as_str()));
                    tops.push(TopBinding {
                        name: name.as_str().to_string(),
                        rust,
                        kind: TopKind::Fn { closure },
                    });
                }
                ast::LetBindingKind::Normal(pattern) => {
                    let names = pattern.bindings();
                    let [name] = names.as_slice() else {
                        cx.diag(lets.span(), "顶层解构绑定暂不支持".into());
                        continue;
                    };
                    let Some(init) = lets.init() else {
                        cx.diag(lets.span(), format!("顶层 `#let {}` 缺少初始化", name.as_str()));
                        continue;
                    };
                    let id = cx.fresh_id();
                    let rust = format!("V{id}_{}", sanitize(name.as_str()));
                    tops.push(TopBinding {
                        name: name.as_str().to_string(),
                        rust,
                        kind: TopKind::Value { init },
                    });
                }
            },
            other => {
                let kind = other.to_untyped().kind();
                cx.diag(
                    other.span(),
                    format!("模块顶层暂只支持 `#let` 绑定,遇到 {kind:?}(标记内容/导入等属高级语义)"),
                );
            }
        }
    }

    // 第二遍:逐绑定生成(函数体可见此前所有绑定 → 静态作用域)。
    let mut items = String::new();
    let mut defines = String::new();
    for (i, top) in tops.iter().enumerate() {
        let env = Env {
            locals: Vec::new(),
            module: tops[..=i].iter().map(|t| (t.name.clone(), t.rust.clone())).collect(),
        };
        match &top.kind {
            TopKind::Value { init } => {
                let expr = cx.expr(*init, &env, Mode::ModuleInit);
                let id = &top.rust;
                let _ = writeln!(
                    items,
                    "\n#[allow(non_snake_case, dead_code)]\nfn {id}_init() -> Value {{\n    {expr}\n}}\n\
                     #[allow(non_snake_case, dead_code)]\nstatic {id}: rt::Slot = LazyLock::new(&{id}_init);"
                );
                let _ =
                    writeln!(defines, "    scope.define(\"{}\", rt::slot(&{id}));", escape(&top.name));
            }
            TopKind::Fn { closure } => {
                let fun = format!("f_{}", lowercase(&top.rust));
                let data = format!("D_{}", uppercase(&top.rust));
                items.push_str(&cx.gen_function(&fun, *closure, &env));
                items.push_str(&gen_func_data(&data, &fun, &top.name, *closure));
                let id = &top.rust;
                let _ = writeln!(
                    items,
                    "\n#[allow(non_snake_case, dead_code)]\nfn {id}_init() -> Value {{\n    Value::Func(Func::from(&{data}))\n}}\n\
                     #[allow(non_snake_case, dead_code)]\nstatic {id}: rt::Slot = LazyLock::new(&{id}_init);"
                );
                let _ = writeln!(defines, "    scope.define(\"{}\", rt::slot(&{id}));", escape(&top.name));
            }
        }
    }

    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    Ok(format!(
        "// 由 trust 从 `{module_name}` 生成,勿手改。\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult}};\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
         use trust_runtime as rt;\n\n\
         /// 生成的错误 span:detached(差分对拍只比较值)。\n\
         const SPAN: Span = Span::detached();\n\n\
         /// 构建该模块(名字与导出面照抄源文件)。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{defines}    Module::new(\"{module_name}\", scope)\n}}\n{items}"
    ))
}

struct Ctx {
    diags: Vec<Diag>,
    next_id: usize,
}

struct TopBinding<'a> {
    name: String,
    rust: String,
    kind: TopKind<'a>,
}

enum TopKind<'a> {
    Value { init: ast::Expr<'a> },
    Fn { closure: ast::Closure<'a> },
}

/// 名字解析环境:局部栈 + 可见模块槽(静态作用域,按声明序累积)。
#[derive(Clone)]
struct Env {
    locals: Vec<(String, String)>,
    module: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    /// 函数体:引擎在手,全部构造可用。
    FnBody,
    /// 顶层值初始化:免引擎,涉及调用/标准库引用即报错。
    ModuleInit,
}

impl Ctx {
    fn fresh_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn diag(&mut self, span: typst_syntax::Span, message: String) {
        self.diags.push(Diag { span, message });
    }

    /// 生成一个函数:参数解构 + 体直译。
    fn gen_function(&mut self, fun: &str, closure: ast::Closure, env: &Env) -> String {
        let mut body = String::new();
        let mut fn_env = env.clone();

        for param in closure.params().children() {
            match param {
                ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                    let name = ident.as_str().to_string();
                    let rust = bind_local(&mut fn_env, &name);
                    let _ =
                        writeln!(body, "    let mut {rust} = rt::param(__args, \"{}\", span)?;", escape(&name));
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {}
                ast::Param::Pos(_) => {
                    self.diag(param.span(), "函数参数解构模式暂不支持(仅支持单一标识符)".into());
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str().to_string();
                    let rust = bind_local(&mut fn_env, &name);
                    let default = self.expr(named.expr(), &fn_env, Mode::FnBody);
                    // 惰性默认值:未提供时在调用点求值(语义边界见文件头)。
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = match __args.named_or_find::<Value>(\"{}\")? {{\n\
                         \x20       Some(value) => value,\n\
                         \x20       None => {default},\n\
                         \x20   }};",
                        escape(&name)
                    );
                }
                ast::Param::Spread(spread) => match spread.sink_ident() {
                    Some(ident) => {
                        let name = ident.as_str().to_string();
                        let rust = bind_local(&mut fn_env, &name);
                        let _ = writeln!(body, "    let mut {rust} = Value::Args(__args.take());");
                    }
                    None => {
                        let _ = writeln!(body, "    let mut _sink = Value::Args(__args.take());");
                    }
                },
            }
        }
        let _ = writeln!(body, "    rt::finish(__args, span)?;");

        let value = self.expr(closure.body(), &fn_env, Mode::FnBody);
        let _ = writeln!(body, "    Ok({value})");

        format!(
            "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{body}}}\n"
        )
    }

    /// 表达式直译:返回类型为 `Value` 的 Rust 表达式(可含 `?`)。
    fn expr(&mut self, expr: ast::Expr, env: &Env, mode: Mode) -> String {
        match expr {
            ast::Expr::Ident(ident) => self.ident(ident, env, mode),
            ast::Expr::None(_) => "Value::None".into(),
            ast::Expr::Auto(_) => "Value::Auto".into(),
            ast::Expr::Bool(b) => format!("Value::Bool({})", b.get()),
            ast::Expr::Int(i) => format!("Value::Int({})", i.get()),
            ast::Expr::Float(f) => format!("Value::Float({:?})", f.get()),
            ast::Expr::Str(s) => format!("Value::Str({:?}.into())", s.get().as_str()),
            ast::Expr::Parenthesized(p) => self.expr(p.expr(), env, mode),
            ast::Expr::CodeBlock(block) => self.block(block.body().exprs(), env, mode),
            ast::Expr::Array(array) => {
                let mut out =
                    String::from("{ let mut __a: Vec<Value> = Vec::new();");
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
                            let _ = write!(out, " rt::dict_put(&mut __d, \"{}\", {value});", escape(key));
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
            ast::Expr::ForLoop(for_loop) => {
                let iterable = self.expr(for_loop.iterable(), env, mode);
                let mut body_env = env.clone();
                let binder = match for_loop.pattern() {
                    ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
                        bind_local(&mut body_env, ident.as_str())
                    }
                    ast::Pattern::Placeholder(_) => {
                        bind_local(&mut body_env, "item")
                    }
                    _ => {
                        self.diag(for_loop.pattern().span(), "for 模式暂只支持单一标识符".into());
                        bind_local(&mut body_env, "item")
                    }
                };
                let body = self.expr(for_loop.body(), &body_env, mode);
                format!(
                    "{{ let mut __out = Value::None;\n  \
                     for {binder} in rt::iterate(&({iterable}), SPAN)? {{\n  \
                     \x20   let __v = {body};\n  \
                     \x20   __out = rt::join(__out, __v, SPAN)?;\n  \
                     }}\n  \
                     __out }}"
                )
            }
            other => {
                let kind = other.to_untyped().kind();
                self.diag(other.span(), format!("表达式 {kind:?} 暂不支持(v1 语言子集)"));
                "Value::None".into()
            }
        }
    }

    fn ident(&mut self, ident: ast::Ident, env: &Env, mode: Mode) -> String {
        let name = ident.as_str();
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| n == name) {
            return format!("{rust}.clone()");
        }
        if let Some((_, rust)) = env.module.iter().rev().find(|(n, _)| n == name) {
            return format!("rt::slot(&{rust})");
        }
        if mode == Mode::ModuleInit {
            self.diag(
                ident.span(),
                format!("顶层值引用标准库 `{name}`,需要引擎(暂不支持,请包进函数)"),
            );
            "Value::None".into()
        } else {
            format!("rt::global(engine, \"{}\", SPAN)?", escape(name))
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
                self.diag(binary.span(), format!("二元运算 {other:?} 暂不支持"));
                "Value::None".into()
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
                        "rt::method_mut(&mut {rust}, \"{}\", {args}, SPAN)?",
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
            self.diag(call.span(), "顶层值中的函数调用需要引擎(暂不支持,请包进函数)".into());
            return "Value::None".into();
        }
        let callee = self.expr(call.callee(), env, mode);
        format!("{{ let __callee = {callee}; let __call_args = {args}; rt::call(&__callee, engine, context, SPAN, __call_args)? }}")
    }

    /// 调用点参数:按源序构造 `Args`。
    fn gen_args(&mut self, args: ast::Args, env: &Env, mode: Mode) -> String {
        let mut out = String::from("{ let mut args = Args::new(SPAN, std::iter::empty::<Value>());");
        for arg in args.items() {
            match arg {
                ast::Arg::Pos(expr) => {
                    let value = self.expr(expr, env, mode);
                    let _ = write!(out, " args.push(SPAN, {value});");
                }
                ast::Arg::Named(named) => {
                    let name = named.name().as_str();
                    let value = self.expr(named.expr(), env, mode);
                    let _ = write!(
                        out,
                        " args.items.push(rt::named_arg(SPAN, \"{}\", {value}));",
                        escape(name)
                    );
                }
                ast::Arg::Spread(spread) => {
                    let value = self.expr(spread.expr(), env, mode);
                    let _ = write!(out, " rt::spread_args(&mut args, SPAN, {value})?;");
                }
            }
        }
        out.push_str(" args }");
        out
    }

    /// 闭包:保留源码片段交由 typst-eval 解释(保真路径);
    /// 默认值创建期求值;捕获 = 子树标识符 ∩ 外层可解析名字
    /// (保守过捕,解释器按参数/局部遮蔽覆盖,语义不变)。
    fn closure_expr(&mut self, closure: ast::Closure, env: &Env, mode: Mode) -> String {
        let src_text = closure.to_untyped().full_text().to_string();
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
            } else if let Some((_, rust)) = env.module.iter().rev().find(|(n, _)| *n == name) {
                captured.push(format!("(\"{}\", rt::slot(&{rust}))", escape(&name)));
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
        let defaults = if defaults.is_empty() { "Vec::new()".into() } else { format!("vec![{}]", defaults.join(", ")) };
        let captured = if captured.is_empty() { "&[]".into() } else { format!("&[{}]", captured.join(", ")) };
        format!("Value::Func(rt::closure({src_text:?}, {defaults}, {captured}))")
    }

    /// 代码块:非末位语句值按 join 语义累积,末位表达式为块值
    /// (对照 typst code block 求值)。
    fn block<'a>(
        &mut self,
        stmts: impl Iterator<Item = ast::Expr<'a>>,
        env: &Env,
        mode: Mode,
    ) -> String {
        let mut out = String::from("{ let mut __out = Value::None;\n");
        let mut env = env.clone();
        let mut last: Option<String> = None;
        for stmt in stmts {
            if let Some(prev) = last.take() {
                let _ = writeln!(out, "    __out = rt::join(__out, {prev}, SPAN)?;");
            }
            if let ast::Expr::LetBinding(lets) = stmt {
                let init = match lets.init() {
                    Some(init) => self.expr(init, &env, mode),
                    None => "Value::None".into(),
                };
                let name = match lets.kind() {
                    ast::LetBindingKind::Closure(name) => Some(name.as_str().to_string()),
                    ast::LetBindingKind::Normal(pattern) => match pattern.bindings().as_slice() {
                        [name] => Some(name.as_str().to_string()),
                        _ => {
                            self.diag(lets.span(), "局部解构绑定暂不支持".into());
                            None
                        }
                    },
                };
                if let Some(name) = name {
                    let rust = bind_local(&mut env, &name);
                    let _ = writeln!(out, "    let mut {rust} = {init};");
                }
                continue;
            }
            if let ast::Expr::Binary(binary) = stmt
                && binary.op() == ast::BinOp::Assign
            {
                // 局部重赋值 `x = e`(typst 局部可变;遮蔽链上最新槽)。
                if let ast::Expr::Ident(ident) = binary.lhs() {
                    let name = ident.as_str();
                    match env.locals.iter().rev().find(|(n, _)| n == name) {
                        Some((_, rust)) => {
                            let value = self.expr(binary.rhs(), &env, mode);
                            let _ = writeln!(out, "    {rust} = {value};");
                        }
                        None => {
                            self.diag(
                                binary.span(),
                                format!("赋值目标 `{name}` 不是局部变量(模块槽/字段赋值暂不支持)"),
                            );
                        }
                    }
                } else {
                    self.diag(binary.span(), "字段/解构赋值暂不支持".into());
                }
                continue;
            }
            let value = self.expr(stmt, &env, mode);
            last = Some(value);
        }
        match last {
            Some(tail) => {
                let _ = writeln!(out, "    let _ = __out;");
                let _ = writeln!(out, "    {tail}");
                out.push('}');
                out
            }
            None => {
                let _ = writeln!(out, "    let _ = __out;");
                let _ = writeln!(out, "    Value::None");
                out.push('}');
                out
            }
        }
    }
}

fn bind_local(env: &mut Env, name: &str) -> String {
    let rust = fresh_rust_ident(name, &env.locals);
    env.locals.push((name.to_string(), rust.clone()));
    rust
}

/// 收集闭包子树中的自由变量名(对照 typst-eval 的 `CapturesVisitor`,
/// 但目标是从"外层视角"确定捕获集):
/// - 闭包名/参数/内部 let/for 变量/内层闭包的绑定是内部名,跳过;
/// - 命名参数默认值在外层作用域求值,按外层绑定集分析;
/// - 字段访问的字段名、命名参数与字典的键名不是变量引用,跳过。
fn collect_idents(node: &typst_syntax::SyntaxNode, bound: &mut Vec<String>, out: &mut Vec<String>) {
    use typst_syntax::SyntaxKind as K;
    match node.kind() {
        K::Closure => {
            let Some(cl) = node.cast::<ast::Closure>() else { return };
            // 默认值在外层作用域求值。
            for param in cl.params().children() {
                if let ast::Param::Named(named) = param {
                    collect_idents(named.expr().to_untyped(), bound, out);
                }
            }
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
            let Some(lets) = node.cast::<ast::LetBinding>() else { return };
            if let Some(init) = lets.init() {
                collect_idents(init.to_untyped(), bound, out);
            }
            let mut pushed = 0;
            for ident in lets.kind().bindings() {
                bound.push(ident.as_str().to_string());
                pushed += 1;
            }
            for _ in 0..pushed {
                bound.pop();
            }
        }
        K::ForLoop => {
            let Some(for_loop) = node.cast::<ast::ForLoop>() else { return };
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
            let Some(access) = node.cast::<ast::FieldAccess>() else { return };
            collect_idents(access.target().to_untyped(), bound, out);
        }
        K::Named => {
            let Some(named) = node.cast::<ast::Named>() else { return };
            collect_idents(named.expr().to_untyped(), bound, out);
        }
        K::Keyed => {
            let Some(keyed) = node.cast::<ast::Keyed>() else { return };
            collect_idents(keyed.expr().to_untyped(), bound, out);
        }
        K::Ident => {
            let name = node.full_text().to_string();
            if !bound.contains(&name) {
                out.push(name);
            }
        }
        _ => {
            for child in node.children() {
                collect_idents(child, bound, out);
            }
        }
    }
}

fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.is_empty() {
        out.push('v');
    }
    if !(out.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')) {
        out.insert(0, '_');
    }
    out
}

fn fresh_rust_ident(name: &str, locals: &[(String, String)]) -> String {
    let base = sanitize(name);
    let mut candidate = base.clone();
    let mut n = 1;
    while locals.iter().any(|(_, rust)| *rust == candidate) {
        n += 1;
        candidate = format!("{base}{n}");
    }
    candidate
}

fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn lowercase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

fn uppercase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

fn format_param(name: &str, named: bool, positional: bool, variadic: bool) -> String {
    format!(
        "NativeParamInfo {{ name: \"{}\", docs: \"\", def_site: None, input: CastInfo::Any, default: None, positional: {positional}, named: {named}, variadic: {variadic}, required: {}, settable: false }}",
        escape(name),
        positional && !variadic
    )
}

fn gen_func_data(data: &str, fun: &str, name: &str, closure: ast::Closure) -> String {
    let mut params = Vec::new();
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                params.push(format_param(ident.as_str(), false, true, false));
            }
            ast::Param::Pos(_) => params.push(format_param("_", false, true, false)),
            ast::Param::Named(named) => {
                params.push(format_param(named.name().as_str(), true, true, false))
            }
            ast::Param::Spread(_) => params.push(format_param("args", false, true, true)),
        }
    }
    let list = if params.is_empty() {
        "Vec::new()".into()
    } else {
        format!("vec![\n        {},\n    ]", params.join(",\n        "))
    };
    format!(
        "\n#[allow(non_snake_case, dead_code)]\nfn {fun}_params() -> Vec<NativeParamInfo> {{\n    {list}\n}}\n\
         #[allow(non_snake_case, dead_code)]\nstatic {data}: NativeFuncData = NativeFuncData {{\n    \
         function: NativeFuncPtr(&{fun}),\n    \
         name: \"{name}\",\n    \
         title: \"{name}\",\n    \
         docs: \"\",\n    \
         def_site: None,\n    \
         keywords: &[],\n    \
         contextual: false,\n    \
         scope: LazyLock::new(&rt::empty_scope),\n    \
         params: LazyLock::new(&{fun}_params),\n    \
         returns: LazyLock::new(&rt::any_cast_info),\n}};\n"
    )
}

/// 待翻译的多文件包。
pub struct PackageInput {
    /// 相对路径(POSIX 风格,相对包根)→ 源码。
    pub files: std::collections::BTreeMap<String, String>,
    /// 入口文件相对路径。
    pub entry: String,
}

/// 把一个多文件包翻译为**单个** Rust 源文件:
/// 每个源文件对应一个 `mod m<i>_<stem>`(扁平唯一命名),文件内顶层
/// 绑定成为该 mod 的静态槽;包内 `#import` 在翻译期解析为跨 mod 的
/// 槽引用,导出面按 typst 语义抄入(bare → 模块值;`: *`/列表 → 绑定)。
pub fn translate_package(
    input: &PackageInput,
    module_name: &str,
) -> Result<String, Vec<Diag>> {
    let entry = input.entry.trim_start_matches('/');
    if !input.files.contains_key(entry) {
        return Err(vec![Diag {
            span: typst_syntax::Span::detached(),
            message: format!("入口文件 {entry} 不在文件集中"),
        }]);
    }

    // 第一遍:每个文件的顶层绑定与 import 边。
    struct FilePlan<'a> {
        path: String,
        mod_name: String,
        markup: ast::Markup<'a>,
        tops: Vec<(String, String)>,      // 绑定名 → 槽名(该文件命名空间)
        imports: Vec<ImportEdge>,
    }
    struct ImportEdge {
        span: typst_syntax::Span,
        target: String,                   // 目标文件路径
        items: ImportItems,
    }
    enum ImportItems {
        Bare,
        Rename(String),
        List(Vec<(String, Option<String>)>), // (名字, rename)
        Wildcard,
    }

    let mut cx = Ctx { diags: Vec::new(), next_id: 0 };
    let mut plans: Vec<FilePlan> = Vec::new();
    let mut roots: Vec<(String, typst_syntax::SyntaxNode)> = Vec::new();
    for (path, src) in input.files.iter() {
        roots.push((path.clone(), typst_syntax::parse(src)));
    }
    for (idx, (path, root)) in roots.iter().enumerate() {
        let Some(markup) = root.cast::<ast::Markup>() else {
            cx.diag(typst_syntax::Span::detached(), format!("{path} 未能解析为 markup 模块"));
            continue;
        };
        let stem = path.rsplit('/').next().unwrap_or("f").split('.').next().unwrap_or("f");
        let mod_name = format!("m{idx}_{}", sanitize(stem));
        let mut tops = Vec::new();
        let mut imports = Vec::new();
        for expr in markup.exprs() {
            match expr {
                ast::Expr::Space(_) | ast::Expr::Parbreak(_) => {}
                ast::Expr::ModuleImport(import) => {
                    let ast::Expr::Str(path_expr) = import.source() else {
                        cx.diag(expr.span(), "包内 import 暂只支持字符串路径".into());
                        continue;
                    };
                    let raw = path_expr.get().to_string();
                    let target = normalize_import(path, &raw);
                    if !input.files.contains_key(&target) {
                        cx.diag(
                            path_expr.span(),
                            format!("包内 import \"{raw}\" 解析到 {target},不在文件集中"),
                        );
                        continue;
                    }
                    let items = match import.imports() {
                        None => match import.new_name() {
                            Some(new_name) => ImportItems::Rename(new_name.as_str().to_string()),
                            None => ImportItems::Bare,
                        },
                        Some(ast::Imports::Wildcard) => ImportItems::Wildcard,
                        Some(ast::Imports::Items(items)) => ImportItems::List(
                            items
                                .iter()
                                .map(|item| {
                                    let original = item.original_name().as_str().to_string();
                                    let bound = match item {
                                        ast::ImportItem::Simple(_) => original.clone(),
                                        ast::ImportItem::Renamed(renamed) => {
                                            renamed.new_name().as_str().to_string()
                                        }
                                    };
                                    (original, Some(bound))
                                })
                                .collect(),
                        ),
                    };
                    imports.push(ImportEdge { span: expr.span(), target, items });
                }
                ast::Expr::LetBinding(lets) => {
                    let names = match lets.kind() {
                        ast::LetBindingKind::Closure(name) => vec![name.as_str().to_string()],
                        ast::LetBindingKind::Normal(pattern) => {
                            pattern.bindings().iter().map(|i| i.as_str().to_string()).collect()
                        }
                    };
                    for name in names {
                        let id = cx.fresh_id();
                        tops.push((name.clone(), format!("V{id}_{}", sanitize(&name))));
                    }
                }
                other => {
                    let kind = other.to_untyped().kind();
                    cx.diag(
                        other.span(),
                        format!("模块顶层暂只支持 `#let`/`#import`,遇到 {kind:?}"),
                    );
                }
            }
        }
        plans.push(FilePlan { path: path.clone(), mod_name, markup, tops, imports });
    }
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    // import 环检测(typst 模块循环求值同样报错)。
    {
        fn dfs(
            node: &str,
            edges: &std::collections::BTreeMap<String, Vec<String>>,
            seen: &mut Vec<String>,
        ) -> bool {
            if seen.iter().any(|p| p == node) {
                return false;
            }
            seen.push(node.to_string());
            if let Some(nexts) = edges.get(node) {
                for next in nexts {
                    if !dfs(next, edges, seen) {
                        return false;
                    }
                }
            }
            seen.pop();
            true
        }
        let mut edges: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();
        for plan in &plans {
            edges.entry(plan.path.clone()).or_default().extend(
                plan.imports.iter().map(|edge| edge.target.clone()),
            );
        }
        let mut stack = Vec::new();
        if !dfs(entry, &edges, &mut stack) {
            return Err(vec![Diag {
                span: typst_syntax::Span::detached(),
                message: "包内 import 成环(typst 运行时同样会报循环求值)".into(),
            }]);
        }
    }

    // 第二遍:逐文件生成(每个文件的函数体可见:自己的绑定 + import 抄入)。
    // 顶层绑定查找表:路径 → (名字 → qualified 槽)。
    let export_table: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
        plans
            .iter()
            .map(|plan| {
                (
                    plan.path.clone(),
                    plan.tops
                        .iter()
                        .map(|(name, slot)| (name.clone(), format!("{}::{slot}", plan.mod_name)))
                        .collect(),
                )
            })
            .collect();

    let mut items = String::new();
    let mut top_defines = String::new();
    for plan in &plans {
        let is_entry = plan.path == entry;
        let mut env = Env { locals: Vec::new(), module: Vec::new() };
        // 当前文件的 import 抄入(可见绑定 + module() 定义)。
        let mut file_defines = String::new();
        for edge in &plan.imports {
            let target_mod = &plans
                .iter()
                .find(|p| p.path == edge.target)
                .expect("target checked")
                .mod_name;
            let exports = &export_table[edge.target.as_str()];
            match &edge.items {
                ImportItems::Bare => {
                    let name = edge
                        .target
                        .rsplit('/')
                        .next()
                        .unwrap_or("m")
                        .split('.')
                        .next()
                        .unwrap_or("m")
                        .to_string();
                    let value = format!("Value::Module({target_mod}::module())");
                    let slot = format!(
                        "IMPORT_{}_{}",
                        sanitize(&plan.mod_name),
                        sanitize(&name)
                    );
                    let _ = writeln!(
                        items,
                        "\n#[allow(non_snake_case, dead_code)]\nstatic {slot}: rt::Slot = LazyLock::new(&|| {value});"
                    );
                    env.module.push((name.clone(), format!("{slot}")));
                    let _ = writeln!(file_defines, "    scope.define(\"{}\", rt::slot(&{slot}));", escape(&name));
                }
                ImportItems::Rename(name) => {
                    let value = format!("Value::Module({target_mod}::module())");
                    let slot = format!(
                        "IMPORT_{}_{}",
                        sanitize(&plan.mod_name),
                        sanitize(name)
                    );
                    let _ = writeln!(
                        items,
                        "\n#[allow(non_snake_case, dead_code)]\nstatic {slot}: rt::Slot = LazyLock::new(&|| {value});"
                    );
                    env.module.push((name.clone(), format!("{slot}")));
                    let _ = writeln!(file_defines, "    scope.define(\"{}\", rt::slot(&{slot}));", escape(name));
                }
                ImportItems::List(list) => {
                    for (name, rename) in list {
                        let Some(qualified) = exports.get(name) else {
                            cx.diag(
                                edge.span,
                                format!("`{}` 不包含导出 `{name}`", edge.target),
                            );
                            continue;
                        };
                        let as_name = rename.clone().unwrap_or_else(|| name.clone());
                        env.module.push((as_name.clone(), qualified.clone()));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::slot(&{qualified}));",
                            escape(&as_name)
                        );
                    }
                }
                ImportItems::Wildcard => {
                    for (name, qualified) in exports {
                        if env.module.iter().any(|(n, _)| n == name) {
                            continue;
                        }
                        env.module.push((name.clone(), qualified.clone()));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::slot(&{qualified}));",
                            escape(name)
                        );
                    }
                }
            }
        }

        // 本文件顶层绑定(追加在 import 之后:遮蔽语义)。
        let mut file_items = String::new();
        let mut own: Vec<(String, String)> = Vec::new();
        for expr in plan.markup.exprs() {
            let ast::Expr::LetBinding(lets) = expr else { continue };
            match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    let Some(ast::Expr::Closure(closure)) = lets.init() else {
                        cx.diag(lets.span(), "闭包绑定缺少闭包初始化".into());
                        continue;
                    };
                    let (_, slot) = plan
                        .tops
                        .iter()
                        .find(|(n, _)| *n == name.as_str())
                        .expect("slot collected");
                    let mut env2 = env.clone();
                    for (seen, seen_slot) in &own {
                        env2.module.push((seen.clone(), seen_slot.clone()));
                    }
                    env2.module.push((name.as_str().to_string(), format!("{slot}")));
                    let fun = format!("f_{}", lowercase(slot));
                    let data = format!("D_{}", uppercase(slot));
                    file_items.push_str(&cx.gen_function(&fun, closure, &env2));
                    file_items.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
                    let _ = writeln!(
                        file_items,
                        "\n#[allow(non_snake_case, dead_code)]\nfn {slot}_init() -> Value {{\n    Value::Func(Func::from(&{data}))\n}}\n\
                         #[allow(non_snake_case, dead_code)]\npub(super) static {slot}: rt::Slot = LazyLock::new(&{slot}_init);"
                    );
                    own.push((name.as_str().to_string(), slot.clone()));
                }
                ast::LetBindingKind::Normal(pattern) => {
                    let names = pattern.bindings();
                    let [name] = names.as_slice() else {
                        cx.diag(lets.span(), "顶层解构绑定暂不支持".into());
                        continue;
                    };
                    let Some(init_expr) = lets.init() else {
                        cx.diag(lets.span(), format!("顶层 `#let {}` 缺少初始化", name.as_str()));
                        continue;
                    };
                    let (_, slot) = plan
                        .tops
                        .iter()
                        .find(|(n, _)| *n == name.as_str())
                        .expect("slot collected");
                    let mut env2 = env.clone();
                    for (seen, seen_slot) in &own {
                        env2.module.push((seen.clone(), seen_slot.clone()));
                    }
                    env2.module.push((name.as_str().to_string(), slot.clone()));
                    let value = cx.expr(init_expr, &env2, Mode::ModuleInit);
                    file_items.push_str(&format!(
                        "\n#[allow(non_snake_case, dead_code)]\nfn {slot}_init() -> Value {{\n    {value}\n}}\n\
                         #[allow(non_snake_case, dead_code)]\npub(super) static {slot}: rt::Slot = LazyLock::new(&{slot}_init);\n"
                    ));
                    own.push((name.as_str().to_string(), slot.clone()));
                }
            }
        }
        for (name, slot) in &own {
            // 入口的导出面在顶层 module(),须限定文件 mod;文件内部 module() 裸引用即可。
            let qualified = format!("{}::{slot}", plan.mod_name);
            let _ = writeln!(
                file_defines,
                "    scope.define(\"{}\", rt::slot(&{qualified}));",
                escape(name)
            );
        }
        let visible = format!(
            "\n/// `{}` 的模块作用域(import 抄入 + 本文件顶层绑定)。\n\
             #[allow(non_snake_case, dead_code)]\npub(super) fn module() -> Module {{\n    let mut scope = Scope::new();\n{file_defines}    Module::new(\"{}\", scope)\n}}\n",
            plan.path,
            escape(&plan.path)
        );
        if is_entry {
            top_defines = file_defines;
        }
        let _ = writeln!(
            items,
            "\nmod {} {{\n    use super::*;\n{file_items}{visible}}}\n",
            plan.mod_name
        );
    }

    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }

    Ok(format!(
        "// 由 trust 从包 `{module_name}`(入口 {entry})生成,勿手改。\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult}};\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
         use trust_runtime as rt;\n\n\
         const SPAN: Span = Span::detached();\n\n\
         /// 入口模块的导出面。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{top_defines}    Module::new(\"{module_name}\", scope)\n}}\n{items}"
    ))
}

/// 相对当前文件目录解析 import 路径(POSIX 风格)。
fn normalize_import(from: &str, raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let dir = match from.rfind('/') {
        Some(idx) => from[..idx].split('/').collect::<Vec<_>>(),
        None => Vec::new(),
    };
    parts.extend(dir);
    for seg in raw.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
