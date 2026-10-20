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

/// 宽松翻译(探索用):缺口降级为占位,返回 (代码, 诊断清单)。
pub fn translate_package_lenient(
    input: &PackageInput,
    module_name: &str,
) -> (String, Vec<Diag>) {
    let mut cx = Ctx { diags: Vec::new(), lenient: true, next_id: 0, current_file: None, package_files: None, package_mods: None };
    let code = translate_package_inner(input, module_name, &mut cx);
    (code, cx.diags)
}

/// 把一个 typst 模块源文件翻译为 Rust 源码文本。
pub fn translate(src: &str, module_name: &str) -> Result<String, Vec<Diag>> {
    let root = typst_syntax::parse(src);
    let Some(markup) = root.cast::<ast::Markup>() else {
        return Err(vec![Diag { span: typst_syntax::Span::detached(), message: "源文件未能解析为 markup 模块".into() }]);
    };

    let mut cx = Ctx { diags: Vec::new(), lenient: false, next_id: 0, current_file: None, package_files: None, package_mods: None };
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

    // 预判每个顶层值是否需要引擎(决定槽形态;两次生成,翻译纯函数)。
    let engine_flags: Vec<bool> = tops
        .iter()
        .map(|top| match &top.kind {
            TopKind::Value { init } => {
                let env = Env { locals: Vec::new(), module: Vec::new() };
                let before = cx.diags.len();
                let _ = cx.expr(*init, &env, Mode::ModuleInit);
                let needs = cx.diags.len() > before;
                cx.diags.truncate(before);
                needs
            }
            _ => false,
        })
        .collect();

    let mut items = String::new();
    let mut defines = String::new();
    let mut engine_defines = String::new();
    for (i, top) in tops.iter().enumerate() {
        // 函数体可见自身(递归)与此前全部绑定;值 init 在绑定生效前求值,
        // 同名引用解析到外层,由下方值分支自行处理。
        let mut env = Env {
            locals: Vec::new(),
            module: tops[..=i]
                .iter()
                .enumerate()
                .map(|(j, t)| {
                    let r = if engine_flags[j] { Ref::Engine(t.rust.clone()) } else { Ref::Slot(t.rust.clone()) };
                    (t.name.clone(), r)
                })
                .collect(),
        };
        let init_env = Env {
            locals: Vec::new(),
            module: tops[..i]
                .iter()
                .enumerate()
                .map(|(j, t)| {
                    let r = if engine_flags[j] { Ref::Engine(t.rust.clone()) } else { Ref::Slot(t.rust.clone()) };
                    (t.name.clone(), r)
                })
                .collect(),
        };
        let _ = &mut env;
        match &top.kind {
            TopKind::Value { init } => {
                let id = &top.rust;
                if engine_flags[i] {
                    // 引擎值:惰性 OnceLock,首访从函数体/宿主 module_init 强制。
                    let expr = cx.expr(*init, &init_env, Mode::FnBody);
                    let _ = writeln!(
                        items,
                        "\n#[allow(non_snake_case, dead_code)]\nfn {id}_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    Ok({expr})\n}}\n\
                         #[allow(non_snake_case, dead_code)]\nstatic {id}: std::sync::OnceLock<Value> = std::sync::OnceLock::new();\n\
                         #[allow(non_snake_case, dead_code)]\nfn {id}_get(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    if let Some(v) = {id}.get() {{ return Ok(v.clone()); }}\n    let v = {id}_init(engine, context)?;\n    let _ = {id}.set(v.clone());\n    Ok(v)\n}}"
                    );
                    let _ = writeln!(
                        engine_defines,
                        "    scope.define(\"{}\", {id}_get(engine, context)?);",
                        escape(&top.name)
                    );
                } else {
                    let expr = cx.expr(*init, &init_env, Mode::ModuleInit);
                    let _ = writeln!(
                        items,
                        "\n#[allow(non_snake_case, dead_code)]\nfn {id}_init() -> Value {{\n    {expr}\n}}\n\
                         #[allow(non_snake_case, dead_code)]\nstatic {id}: rt::Slot = LazyLock::new(&{id}_init);"
                    );
                    let _ = writeln!(
                        defines,
                        "    scope.define(\"{}\", rt::slot(&{id}));",
                        escape(&top.name)
                    );
                }
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
         use typst::diag::{{At, SourceResult, bail}};\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\nuse typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         /// 生成的错误 span:detached(差分对拍只比较值)。\n\
         const SPAN: Span = Span::detached();\n\n\
         /// 构建该模块(名字与导出面照抄源文件)。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{defines}    Module::new(\"{module_name}\", scope)\n}}\n\
         /// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。\n\
         #[allow(non_snake_case, unused_variables)]\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {{\n{engine_defines}    let _ = engine;\n    Ok(())\n}}\n{items}"
    ))
}

struct Ctx {
    diags: Vec<Diag>,
    /// 宽松模式(探索):缺口降级为占位,不作为整体失败。
    lenient: bool,
    next_id: usize,
    /// 包模式下当前处理的文件路径(体内字符串 import 解析用)。
    current_file: Option<String>,
    /// 包模式的文件集(字符串 import 解析)。
    package_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 包模式:文件路径 → 生成 mod 名(体内字符串 import 取模块值)。
    package_mods: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
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

/// 模块绑定的引用形态:普通槽(LazyLock)或引擎槽(OnceLock + getter)。
#[derive(Clone)]
enum Ref {
    Slot(String),
    Engine(String),
    /// 来自模块值字段的绑定(`import deps.oxifmt: strfmt`)。
    ModuleField { expr: String, name: String },
    /// 直接以表达式求值的绑定(模块值本身或其字段)。
    Value(String),
}

/// 名字解析环境:局部栈 + 可见模块槽(静态作用域,按声明序累积)。
#[derive(Clone)]
struct Env {
    locals: Vec<(String, String)>,
    module: Vec<(String, Ref)>,
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
                        writeln!(body, "    let mut {rust} = rt::param_pos(__args, \"{}\", span)?;", escape(&name));
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {}
                ast::Param::Pos(pattern) => {
                    // 解构参数(对照 eval_closure:"pattern parameter")。
                    let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                    body.push_str(&self.destructure(pattern, value, &mut fn_env, Mode::FnBody, false));
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str().to_string();
                    // 默认值在定义环境求值:不可引用本参数,可引用先前参数。
                    let default = self.expr(named.expr(), &fn_env, Mode::FnBody);
                    let rust = bind_local(&mut fn_env, &name);
                    // 惰性默认值:未提供时在调用点求值(语义边界见文件头)。
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = match __args.named::<Value>(\"{}\")? {{\n\
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
                    format!("Value::Content(rt::content_sequence(vec![{}]))", parts.join(", "))
                }
            }
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
        if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| n == name) {
            return match module_ref {
                Ref::Slot(id) => format!("rt::slot(&{id})"),
                Ref::Engine(id) => format!("{id}_get(engine, context)?"),
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
            self.diag(call.span(), "顶层值中的函数调用需要引擎(暂不支持,请包进函数)".into());
            return "Value::None".into();
        }
        let callee = self.expr(call.callee(), env, mode);
        format!("{{ let __callee = {callee}; let __call_args = {args}; rt::call(&__callee, engine, context, SPAN, __call_args)? }}")
    }

    /// 调用点参数:按源序构造 `Args`。
    fn gen_args(&mut self, args: ast::Args, env: &Env, mode: Mode) -> String {
        let mut out = String::from("{ let mut __call_args_v = Args::new(SPAN, std::iter::empty::<Value>());");
        for arg in args.items() {
            match arg {
                ast::Arg::Pos(expr) => {
                    let value = self.expr(expr, env, mode);
                    let _ = write!(out, " __call_args_v.push(SPAN, {value});");
                }
                ast::Arg::Named(named) => {
                    let name = named.name().as_str();
                    let value = self.expr(named.expr(), env, mode);
                    let _ = write!(
                        out,
                        " __call_args_v.items.push(rt::named_arg(SPAN, \"{}\", {value}));",
                        escape(name)
                    );
                }
                ast::Arg::Spread(spread) => {
                    let value = self.expr(spread.expr(), env, mode);
                    let _ = write!(out, " rt::spread_args(&mut __call_args_v, SPAN, {value})?;");
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
            } else if let Some((_, module_ref)) = env.module.iter().rev().find(|(n, _)| *n == name) {
                match module_ref {
                    Ref::Slot(id) => {
                        captured.push(format!("(\"{}\", rt::slot(&{id}))", escape(&name)))
                    }
                    Ref::Engine(id) => {
                        captured.push(format!(
                            "(\"{}\", {id}_get(engine, context)?)",
                            escape(&name)
                        ))
                    }
                    Ref::ModuleField { expr, name: field } => {
                        captured.push(format!(
                            "(\"{}\", rt::field(&({expr}), \"{field}\", SPAN)?)",
                            escape(&name)
                        ))
                    }
                    Ref::Value(expr) => {
                        captured.push(format!("(\"{}\", {expr})", escape(&name)))
                    }
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
        let defaults = if defaults.is_empty() { "Vec::new()".into() } else { format!("vec![{}]", defaults.join(", ")) };
        let captured = if captured.is_empty() { "&[]".into() } else { format!("&[{}]", captured.join(", ")) };
        format!("Value::Func(rt::closure({src_text:?}, {defaults}, {captured}))")
    }

/// 生成解构语句(对照 typst-eval/src/binding.rs):
/// 数组/字典运行时分派,错误文案与数量校验一致。
/// `assign = true` 时为解构赋值(目标须为已存在局部)。
fn destructure(
    &mut self,
    pattern: ast::Pattern,
    value: String,
    env: &mut Env,
    mode: Mode,
    assign: bool,
) -> String {
    match pattern {
        ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
            let name = ident.as_str().to_string();
            match env.locals.iter().rev().find(|(n, _)| *n == name) {
                // 已有同名:解构叶子写入既有变量(遮蔽在可见性上等价)。
                Some((_, rust)) => format!("{rust} = {value};\n"),
                None => {
                    if assign {
                        self.diag(
                            ident.span(),
                            format!("解构赋值目标 `{name}` 不是局部变量"),
                        );
                        String::new()
                    } else {
                        let rust = bind_local(env, &name);
                        format!("let mut {rust} = {value};\n")
                    }
                }
            }
        }
        ast::Pattern::Placeholder(_) => format!("let _ = {value};\n"),
        ast::Pattern::Parenthesized(p) => self.destructure(p.pattern(), value, env, mode, assign),
        ast::Pattern::Destructuring(d) => {
            let id = self.fresh_id();
            let tmp = format!("__d{id}");
            let n_items = d.items().count();
            let mut out = format!("let {tmp} = {value};\n");
            // 预分配全部绑定名并提升声明到 match 外:数组/字典两分支
            // 以赋值形式写入同一批变量。
            for ident in pattern.bindings() {
                if env.locals.iter().any(|(n, _)| *n == ident.as_str()) {
                    continue;
                }
                let rust = bind_local(env, ident.as_str());
                out.push_str(&format!("let mut {rust} = Value::None;\n"));
            }

            // 数组分支。
            let mut arr = String::new();
            let arr_ref = format!("__a{id}");
            let mut i = 0usize;
            let mut has_spread = false;
            for item in d.items() {
                match item {
                    ast::DestructuringItem::Pattern(p) => {
                        let v = format!("rt::d_at(&{arr_ref}, {i}, SPAN)?");
                        arr.push_str(&self.destructure(p, v, env, mode, true));
                        i += 1;
                    }
                    ast::DestructuringItem::Spread(spread) => {
                        has_spread = true;
                        let v =
                            format!("Value::Array(rt::d_rest(&{arr_ref}, {i}, {n_items}, SPAN)?)");
                        match spread.sink_expr() {
                            Some(expr) => {
                                arr.push_str(&self.destructure(
                                    ast::Pattern::Normal(expr),
                                    v,
                                    env,
                                    mode,
                                    true,
                                ));
                            }
                            None => arr.push_str(&format!("let _ = {v};\n")),
                        }
                    }
                    ast::DestructuringItem::Named(_) => {
                        arr.push_str(
                            "bail!(SPAN, \"cannot destructure named pattern from an array\");\n",
                        );
                    }
                }
            }
            if !has_spread {
                arr.push_str(&format!("rt::d_array_finish(&{arr_ref}, {i}, SPAN)?;\n"));
            }

            // 字典分支。
            let mut dict = String::new();
            let dict_ref = format!("__k{id}");
            let mut used: Vec<String> = Vec::new();
            let mut sink: Option<(ast::Expr, String)> = None;
            for item in d.items() {
                match item {
                    ast::DestructuringItem::Pattern(ast::Pattern::Normal(ast::Expr::Ident(
                        ident,
                    ))) => {
                        let key = ident.as_str().to_string();
                        used.push(key.clone());
                        let v = format!("rt::d_get(&{dict_ref}, \"{}\", SPAN)?", escape(&key));
                        dict.push_str(&self.destructure(
                            ast::Pattern::Normal(ast::Expr::Ident(ident)),
                            v,
                            env,
                            mode,
                            true,
                        ));
                    }
                    ast::DestructuringItem::Named(named) => {
                        let key = named.name().as_str().to_string();
                        used.push(key.clone());
                        let v = format!("rt::d_get(&{dict_ref}, \"{}\", SPAN)?", escape(&key));
                        dict.push_str(&self.destructure(named.pattern(), v, env, mode, true));
                    }
                    ast::DestructuringItem::Spread(spread) => {
                        if let Some(expr) = spread.sink_expr() {
                            sink = Some((expr, format!("__r{id}")));
                        }
                    }
                    ast::DestructuringItem::Pattern(p) => {
                        let _ = p;
                        dict.push_str(
                            "bail!(SPAN, \"cannot destructure unnamed pattern from dictionary\");\n",
                        );
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
                    mode,
                    true,
                ));
            }

            out.push_str(&format!(
                "match &{tmp} {{\n    Value::Array({arr_ref}) => {{\n{arr}    }}\n    Value::Dict({dict_ref}) => {{\n{dict}    }}\n    other => bail!(SPAN, \"cannot destructure {{}}\", other.ty()),\n}}\n"
            ));
            out
        }
        ast::Pattern::Normal(other) => {
            self.diag(other.span(), "解构模式此处须为标识符".into());
            String::new()
        }
    }
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
                match lets.kind() {
                    ast::LetBindingKind::Closure(name) => {
                        let rust = bind_local(&mut env, name.as_str());
                        let _ = writeln!(out, "    let mut {rust} = {init};");
                    }
                    ast::LetBindingKind::Normal(pattern) => {
                        let stmts = self.destructure(pattern, init, &mut env, mode, false);
                        out.push_str("    ");
                        out.push_str(&stmts.replace('\n', "\n    "));
                    }
                }
                continue;
            }
            if let ast::Expr::ModuleImport(import) = stmt {
                // 函数体内局部 import:解析模块值表达式后抄入局部绑定。
                let source = import.source();
                let module_expr: Option<String> = match source {
                    ast::Expr::Ident(_) | ast::Expr::FieldAccess(_) => {
                        match parse_module_access(source) {
                            Some((root, fields)) => {
                                let base = match env.module.iter().rev().find(|(n, _)| *n == root) {
                                    Some((_, module_ref)) => match module_ref {
                                        Ref::Slot(id) => format!("rt::slot(&{id})"),
                                        Ref::Engine(id) => format!("{id}_get(engine, context)?"),
                                        Ref::Value(e) => e.clone(),
                                        Ref::ModuleField { expr, name } => {
                                            format!("rt::field(&({expr}), \"{name}\", SPAN)?")
                                        }
                                    },
                                    None if mode == Mode::FnBody => {
                                        // 全局模块(如 `import calc: abs`)。
                                        format!("rt::global(engine, \"{root}\", SPAN)?")
                                    }
                                    None => {
                                        self.diag(source.span(), format!("import 的模块根 `{root}` 不可见"));
                                        continue;
                                    }
                                };
                                let mut expr = base;
                                for field in &fields {
                                    expr =
                                        format!("rt::field(&({expr}), \"{}\", SPAN)?", escape(field));
                                }
                                Some(expr)
                            }
                            None => {
                                self.diag(source.span(), "体内 import 源须为模块访问".into());
                                continue;
                            }
                        }
                    }
                    ast::Expr::Str(str_expr) => {
                        // 字符串路径:包模式解析到包内文件的模块值。
                        let raw = str_expr.get().to_string();
                        let (Some(file), Some(files), Some(mods)) = (
                            self.current_file.clone(),
                            self.package_files.clone(),
                            self.package_mods.clone(),
                        ) else {
                            self.diag(
                                str_expr.span(),
                                "体内字符串 import 仅在包模式支持(须用模块访问形态)".into(),
                            );
                            continue;
                        };
                        let target = normalize_import(&file, &raw);
                        match (files.contains_key(&target), mods.get(&target)) {
                            (true, Some(mod_name)) => {
                                Some(format!("Value::Module({mod_name}::module())"))
                            }
                            _ => {
                                self.diag(
                                    str_expr.span(),
                                    format!("体内 import \"{raw}\" 不在包文件集"),
                                );
                                continue;
                            }
                        }
                    }
                    _ => {
                        self.diag(source.span(), "体内 import 源暂只支持模块访问".into());
                        continue;
                    }
                };
                let Some(module_expr) = module_expr else { continue };
                // 形态:bare / rename → 模块值;列表 → 字段取值。
                match import.imports() {
                    None => {
                        let name = match import.new_name() {
                            Some(new_name) => new_name.as_str().to_string(),
                            None => source
                                .to_untyped()
                                .full_text()
                                .rsplit('.')
                                .next()
                                .unwrap_or("m")
                                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                                .next()
                                .unwrap_or("m")
                                .to_string(),
                        };
                        let rust = bind_local(&mut env, &name);
                        let _ = writeln!(out, "    let mut {rust} = {module_expr};");
                    }
                    Some(ast::Imports::Wildcard) => {
                        self.diag(stmt.span(), "体内通配 import 暂不支持".into());
                    }
                    Some(ast::Imports::Items(items)) => {
                        for item in items.iter() {
                            let original = item.original_name().as_str().to_string();
                            let bound = item.bound_name().as_str().to_string();
                            let rust = bind_local(&mut env, &bound);
                            let _ = writeln!(
                                out,
                                "    let mut {rust} = rt::field(&({module_expr}), \"{}\", SPAN)?;",
                                escape(&original)
                            );
                        }
                    }
                }
                continue;
            }
            if let ast::Expr::DestructAssignment(assign_) = stmt {
                // 解构赋值:目标须为已存在局部(对照 `Access` 语义)。
                let value = self.expr(assign_.value(), &env, mode);
                let stmts = self.destructure(assign_.pattern(), value, &mut env, mode, true);
                out.push_str("    ");
                out.push_str(&stmts.replace('\n', "\n    "));
                continue;
            }
            if let ast::Expr::Binary(binary) = stmt
                && matches!(
                    binary.op(),
                    ast::BinOp::Assign
                        | ast::BinOp::AddAssign
                        | ast::BinOp::SubAssign
                        | ast::BinOp::MulAssign
                        | ast::BinOp::DivAssign
                )
            {
                // 复合赋值 `x += y` 等价 `x = x + y`(右侧读旧值)。
                if binary.op() != ast::BinOp::Assign
                    && let ast::Expr::Ident(ident) = binary.lhs()
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    let rhs = self.expr(binary.rhs(), &env, mode);
                    let op = match binary.op() {
                        ast::BinOp::AddAssign => "add",
                        ast::BinOp::SubAssign => "sub",
                        ast::BinOp::MulAssign => "mul",
                        ast::BinOp::DivAssign => "div",
                        _ => unreachable!(),
                    };
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {rhs}; {rust} = ops::{op}({rust}, __rhs).at(SPAN)?; }}"
                    );
                    continue;
                }
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
                                format!("赋值目标 `{name}` 不是局��变量(模块槽赋值暂不支持)"),
                            );
                        }
                    }
                } else if let ast::Expr::FieldAccess(access) = binary.lhs()
                    && let ast::Expr::Ident(ident) = access.target()
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    // 字段赋值 `d.k = v`:就地修改局部字典。
                    let field = access.field().as_str();
                    let value = self.expr(binary.rhs(), &env, mode);
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {value}; rt::field_assign(&mut {rust}, \"{}\", __rhs, SPAN)?; }}",
                        escape(field)
                    );
                } else if let ast::Expr::FuncCall(call) = binary.lhs()
                    && let ast::Expr::FieldAccess(access) = call.callee()
                    && let ast::Expr::Ident(ident) = access.target()
                    && matches!(access.field().as_str(), "at" | "first" | "last")
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    // accessor 赋值:`a.at(i) = v` / `a.first() = v`。
                    let method = access.field().as_str();
                    let mut index = "None".to_string();
                    for arg in call.args().items() {
                        if let ast::Arg::Pos(expr) = arg {
                            index = format!("Some({})", self.expr(expr, &env, mode));
                            break;
                        }
                    }
                    let value = self.expr(binary.rhs(), &env, mode);
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {value}; rt::accessor_assign(&mut {rust}, \"{method}\", {index}, __rhs, SPAN)?; }}"
                    );
                } else {
                    self.diag(binary.span(), "字段链/复杂目标赋值暂不支持".into());
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
    // Rust 关键字加原始标识符前缀(typst 允许 `fn` 等作参数名)。
    const KEYWORDS: &[&str] = &[
        "fn", "let", "if", "else", "while", "for", "in", "loop", "match", "return",
        "break", "continue", "struct", "enum", "impl", "trait", "type", "mod", "pub",
        "use", "as", "const", "static", "mut", "ref", "move", "true", "false", "self",
        "super", "crate", "dyn", "where", "unsafe", "async", "await", "box", "union",
    ];
    if KEYWORDS.contains(&out.as_str()) {
        out = format!("r#{out}");
    }
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
    let mut cx = Ctx { diags: Vec::new(), lenient: false, next_id: 0, current_file: None, package_files: None, package_mods: None };
    let code = translate_package_inner(input, module_name, &mut cx);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }
    Ok(code)
}

fn translate_package_inner(
    input: &PackageInput,
    module_name: &str,
    cx: &mut Ctx,
) -> String {
    let entry = input.entry.trim_start_matches('/');
    if !input.files.contains_key(entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: format!("入口文件 {entry} 不在文件集中"),
        });
        return String::new();
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
        external: bool,                   // 外部包("@ns/name:ver")
        // 模块值表达式 import(`deps.oxifmt`):根绑定名 + 字段链
        module_access: Option<(String, Vec<String>)>,
    }
    enum ImportItems {
        Bare,
        Rename(String),
        List(Vec<(String, Option<String>)>), // (名字, rename)
        Wildcard,
    }

    let mut cx = Ctx { diags: Vec::new(), lenient: false, next_id: 0, current_file: None, package_files: None, package_mods: None };
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
                    let (raw, external, module_access) = match import.source() {
                        // 字符串路径:外部包或包内相对/绝对路径。
                        ast::Expr::Str(path_expr) => {
                            let raw = path_expr.get().to_string();
                            let external = raw.starts_with('@');
                            (raw, external, None)
                        }
                        // 模块值表达式 import(`import deps.oxifmt: x`)。
                        other => match parse_module_access(other) {
                            Some(access) => (String::new(), false, Some(access)),
                            None => {
                                cx.diag(expr.span(), "import 源须为字符串路径或模块字段访问".into());
                                continue;
                            }
                        },
                    };
                    let target = if module_access.is_some() {
                        String::new()
                    } else {
                        normalize_import(path, &raw)
                    };
                    if module_access.is_none() && !external && !input.files.contains_key(&target) {
                        cx.diag(
                            expr.span(),
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
                    imports.push(ImportEdge { span: expr.span(), target, items, external, module_access });
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
    if !cx.diags.is_empty() && !cx.lenient {
        // 壳层负责返回 Err。
        return String::new();
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
            cx.diags.push(Diag {
                span: typst_syntax::Span::detached(),
                message: "包内 import 成环(typst 运行时同样会报循环求值)".into(),
            });
            return String::new();
        }
    }

    let package_files = std::rc::Rc::new(input.files.clone());
    let package_mods_map: std::collections::BTreeMap<String, String> = plans
        .iter()
        .map(|p| (p.path.clone(), p.mod_name.clone()))
        .collect();
    let package_mods = std::rc::Rc::new(package_mods_map);
    cx.package_files = Some(package_files.clone());
    cx.package_mods = Some(package_mods.clone());

    // 第二遍:逐文件生成(每个文件的函数体可见:自己的绑定 + import 抄入)。
    // 拓扑序(被依赖者在前):供引擎值 flag 预扫描。
    let mut order: Vec<usize> = Vec::new();
    {
        fn visit(
            idx: usize,
            plans: &[FilePlan],
            order: &mut Vec<usize>,
            state: &mut Vec<u8>,
        ) {
            if state[idx] != 0 {
                return;
            }
            state[idx] = 1;
            for edge in &plans[idx].imports {
                if let Some(next) = plans.iter().position(|p| p.path == edge.target) {
                    visit(next, plans, order, state);
                }
            }
            state[idx] = 2;
            order.push(idx);
        }
        let mut state = vec![0u8; plans.len()];
        for idx in 0..plans.len() {
            visit(idx, &plans, &mut order, &mut state);
        }
    }

    // 文件 tops 里第 nth 个同名绑定的槽(顶层重定义按出现序配对)。
    let nth_slot = |plan: &FilePlan, name: &str, nth: usize| -> Option<String> {
        plan.tops
            .iter()
            .filter(|(n, _)| n == name)
            .map(|(_, slot)| slot.clone())
            .nth(nth)
    };

    // 引擎值 flag 预扫描(拓扑序:被引用文件的 flag 先定,import 解析准确)。
    let mut engine_slots: std::collections::HashSet<String> = std::collections::HashSet::new();
    for &idx in &order {
        let plan = &plans[idx];
        // 该文件的 env:import 边(带跨文件 flag)+ 自身绑定(逐��补 flag)。
        let mut env = Env { locals: Vec::new(), module: Vec::new() };
        for edge in &plan.imports {
            let Some(target_plan) = plans.iter().find(|p| p.path == edge.target) else {
                continue;
            };
            match &edge.items {
                ImportItems::List(list) => {
                    for (item, rename) in list {
                        let as_name = rename.clone().unwrap_or_else(|| item.clone());
                        let slot = nth_slot(target_plan, item, 0).unwrap_or_default();
                        let qualified = format!("{}::{slot}", target_plan.mod_name);
                        let r = if engine_slots.contains(&qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(slot)
                        };
                        env.module.push((as_name, r));
                    }
                }
                ImportItems::Wildcard => {
                    for (name, slot) in &target_plan.tops {
                        let qualified = format!("{}::{slot}", target_plan.mod_name);
                        let r = if engine_slots.contains(&qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(slot.clone())
                        };
                        env.module.push((name.clone(), r));
                    }
                }
                _ => {
                    // bare/rename:模块值槽(engine-free)。
                }
            }
        }
        // 逐绑定两试。
        let mut seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for expr in plan.markup.exprs() {
            let ast::Expr::LetBinding(lets) = expr else { continue };
            let kind = lets.kind();
            let names = kind.bindings();
            let [name_ident] = names.as_slice() else { continue };
            let name = name_ident.as_str().to_string();
            let nth = seen_counts.get(&name).copied().unwrap_or(0);
            seen_counts.insert(name.clone(), nth + 1);
            let Some(slot) = nth_slot(plan, &name, nth) else { continue };
            let qualified = format!("{}::{slot}", plan.mod_name);
            let is_engine = engine_slots.contains(&qualified);
            let mut env2 = env.clone();
            env2.module.push((
                name.clone(),
                if is_engine {
                    Ref::Engine(qualified.clone())
                } else {
                    Ref::Slot(slot.clone())
                },
            ));
            if lets.init().is_some()
                && let ast::LetBindingKind::Normal(pattern) = lets.kind()
                && pattern.bindings().len() == 1
                && let Some(init) = lets.init()
            {
                let before = cx.diags.len();
                let _ = cx.expr(init, &env2, Mode::ModuleInit);
                let needs = cx.diags.len() > before;
                cx.diags.truncate(before);
                if needs {
                    engine_slots.insert(qualified);
                }
            }
        }
    }

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
    let mut engine_init_defines = String::new();
    for plan in &plans {
        cx.current_file = Some(plan.path.clone());
        let is_entry = plan.path == entry;
        let mut env = Env { locals: Vec::new(), module: Vec::new() };
        // 当前文件的 import 抄入(可见绑定 + module() 定义)。
        let mut file_defines = String::new();
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for edge in &plan.imports {
            // 外部包:引擎槽 + 运行时导入(未翻译依赖走解释器)。
            if edge.external {
                let spec = edge.target.clone();
                let name = match &edge.items {
                    ImportItems::Rename(name) => name.clone(),
                    _ => spec_name(&spec),
                };
                let slot = format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(&name));
                let _ = writeln!(
                    items,
                    "\n#[allow(non_snake_case, dead_code)]\npub(super) fn {slot}_get(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    if let Some(v) = {slot}.get() {{ return Ok(v.clone()); }}\n    let v = Value::Module(rt::import_external(engine, \"{spec}\")?);\n    let _ = {slot}.set(v.clone());\n    Ok(v)\n}}\n\
                     #[allow(non_snake_case, dead_code)]\npub(super) static {slot}: std::sync::OnceLock<Value> = std::sync::OnceLock::new();"
                );
                match &edge.items {
                    ImportItems::Bare | ImportItems::Rename(_) => {
                        env.module.push((name.clone(), Ref::Engine(slot.clone())));
                        // 引擎绑定不在 engine-less 的 module() 注册。
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name,
                                Ref::ModuleField {
                                    expr: format!("{slot}_get(engine, context)?"),
                                    name: item.clone(),
                                },
                            ));
                        }
                    }
                    ImportItems::Wildcard => {
                        cx.diag(edge.span, "外部包的通配 import 暂不支持(须翻译该包)".into());
                    }
                }
                continue;
            }
            // 模块值表达式 import(`import deps.oxifmt: strfmt`)。
            if let Some((root, fields)) = &edge.module_access {
                let base = match env.module.iter().rev().find(|(n, _)| n == root) {
                    Some((_, module_ref)) => match module_ref {
                        Ref::Slot(id) => format!("rt::slot(&{id})"),
                        Ref::Engine(id) => format!("{id}_get(engine, context)?"),
                        Ref::Value(e) => e.clone(),
                        Ref::ModuleField { expr, name } => {
                            format!("rt::field(&({expr}), \"{name}\", SPAN)?")
                        }
                    },
                    None => {
                        cx.diag(edge.span, format!("import 的模块根 `{root}` 不可见(须先 import)"));
                        continue;
                    }
                };
                let mut expr = base;
                for field in fields {
                    expr = format!("rt::field(&({expr}), \"{}\", SPAN)?", escape(field));
                }
                match &edge.items {
                    ImportItems::Bare => {
                        let name = fields.last().cloned().unwrap_or_else(|| root.clone());
                        env.module.push((name, Ref::Value(expr)));
                    }
                    ImportItems::Rename(name) => {
                        env.module.push((name.clone(), Ref::Value(expr)));
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name,
                                Ref::ModuleField { expr: expr.clone(), name: item.clone() },
                            ));
                        }
                    }
                    ImportItems::Wildcard => {
                        cx.diag(edge.span, "模块值表达式的通配 import 暂不支持".into());
                    }
                }
                continue;
            }
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
                    let existed = import_slots.contains_key(&edge.target);
                    let slot = match import_slots.get(&edge.target) {
                        Some(slot) => slot.clone(),
                        None => format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(&name)),
                    };
                    import_slots.insert(edge.target.clone(), slot.clone());
                    if !existed {
                        let _ = writeln!(
                            items,
                            "\n#[allow(non_snake_case, dead_code)]\nstatic {slot}: rt::Slot = LazyLock::new(&|| {value});"
                        );
                    }
                    env.module.push((name.clone(), Ref::Slot(format!("{slot}"))));
                    let _ = writeln!(file_defines, "    scope.define(\"{}\", rt::slot(&{slot}));", escape(&name));
                }
                ImportItems::Rename(name) => {
                    let value = format!("Value::Module({target_mod}::module())");
                    let existed = import_slots.contains_key(&edge.target);
                    let slot = match import_slots.get(&edge.target) {
                        Some(slot) => slot.clone(),
                        None => format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(name)),
                    };
                    import_slots.insert(edge.target.clone(), slot.clone());
                    if !existed {
                        let _ = writeln!(
                            items,
                            "\n#[allow(non_snake_case, dead_code)]\nstatic {slot}: rt::Slot = LazyLock::new(&|| {value});"
                        );
                    }
                    env.module.push((name.clone(), Ref::Slot(format!("{slot}"))));
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
                        let r = if engine_slots.contains(qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(qualified.clone())
                        };
                        env.module.push((as_name.clone(), r));
                        if engine_slots.contains(qualified) {
                            let _ = writeln!(
                                engine_init_defines,
                                "    scope.define(\"{}\", {qualified}_get(engine, context)?);",
                                escape(&as_name)
                            );
                        } else {
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::slot(&{qualified}));",
                                escape(&as_name)
                            );
                        }
                    }
                }
                ImportItems::Wildcard => {
                    for (name, qualified) in exports {
                        if env.module.iter().any(|(n, _)| n == name) {
                            continue;
                        }
                        let r = if engine_slots.contains(qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(qualified.clone())
                        };
                        env.module.push((name.clone(), r));
                        if engine_slots.contains(qualified) {
                            let _ = writeln!(
                                engine_init_defines,
                                "    scope.define(\"{}\", {qualified}_get(engine, context)?);",
                                escape(name)
                            );
                        } else {
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::slot(&{qualified}));",
                                escape(name)
                            );
                        }
                    }
                }
            }
        }

        // 本文件顶层绑定(追加在 import 之后:遮蔽语义)。
        let mut file_items = String::new();
        let mut own: Vec<(String, String)> = Vec::new();
        let mut own_engine: Vec<String> = Vec::new();
        let mut own_seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for expr in plan.markup.exprs() {
            let ast::Expr::LetBinding(lets) = expr else { continue };
            match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    let Some(ast::Expr::Closure(closure)) = lets.init() else {
                        cx.diag(lets.span(), "闭包绑定缺少闭包初始化".into());
                        continue;
                    };
                    let nth = own_seen_counts.get(name.as_str()).copied().unwrap_or(0);
                    own_seen_counts.insert(name.as_str().to_string(), nth + 1);
                    let Some(slot) = nth_slot(plan, name.as_str(), nth) else {
                        continue;
                    };
                    let mut env2 = env.clone();
                    for (seen, seen_slot) in &own {
                        let r = if own_engine.iter().any(|e| e == seen_slot) {
                            Ref::Engine(seen_slot.clone())
                        } else {
                            Ref::Slot(seen_slot.clone())
                        };
                        env2.module.push((seen.clone(), r));
                    }
                    env2.module.push((name.as_str().to_string(), Ref::Slot(format!("{slot}"))));
                    let fun = format!("f_{}", lowercase(&slot));
                    let data = format!("D_{}", uppercase(&slot));
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
                    let nth = own_seen_counts.get(name.as_str()).copied().unwrap_or(0);
                    own_seen_counts.insert(name.as_str().to_string(), nth + 1);
                    let Some(slot) = nth_slot(plan, name.as_str(), nth) else {
                        continue;
                    };
                    let qualified_self = format!("{}::{slot}", plan.mod_name);
                    let mut env2 = env.clone();
                    for (seen, seen_slot) in &own {
                        let r = if own_engine.iter().any(|e| e == seen_slot) {
                            Ref::Engine(seen_slot.clone())
                        } else {
                            Ref::Slot(seen_slot.clone())
                        };
                        env2.module.push((seen.clone(), r));
                    }
                    // init 在绑定生效前求值:同名引用解析到外层(如全局
                    // 类型),env2 不含本绑定 → 避免引擎槽自引用递归。
                    if engine_slots.contains(&qualified_self) {
                        // 引擎值(拓扑预扫描已定):本文件惰性槽。
                        let value = cx.expr(init_expr, &env2, Mode::FnBody);
                        file_items.push_str(&format!(
                            "\n#[allow(non_snake_case, dead_code)]\nfn {slot}_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    Ok({value})\n}}\n\
                             #[allow(non_snake_case, dead_code)]\npub(super) static {slot}: std::sync::OnceLock<Value> = std::sync::OnceLock::new();\n\
                             #[allow(non_snake_case, dead_code)]\npub(super) fn {slot}_get(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    if let Some(v) = {slot}.get() {{ return Ok(v.clone()); }}\n    let v = {slot}_init(engine, context)?;\n    let _ = {slot}.set(v.clone());\n    Ok(v)\n}}\n"
                        ));
                        own_engine.push(slot.clone());
                        own.push((name.as_str().to_string(), slot.clone()));
                        continue;
                    }
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
            if own_engine.iter().any(|e| e == slot) {
                // 引擎值:engine-less 的 module() 无法注册,由宿主 module_init 补。
                if is_entry {
                    let _ = writeln!(
                        engine_init_defines,
                        "    scope.define(\"{}\", {slot}_get(engine, context)?);",
                        escape(name)
                    );
                }
                continue;
            }
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
            module_stem(&plan.path)
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

    format!(
        "// 由 trust 从包 `{module_name}`(入口 {entry})生成,勿手改。\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult, bail}};\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\nuse typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         const SPAN: Span = Span::detached();\n\n\
         /// 入口模块的导出面。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{top_defines}    Module::new(\"{module_name}\", scope)\n}}\n\
         /// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。\n\
         #[allow(non_snake_case, unused_variables)]\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {{\n{engine_init_defines}    let _ = engine;\n    Ok(())\n}}\n{items}"
    )
}

/// 从包 spec 取短名(`@preview/oxifmt:1.0.0` → `oxifmt`)。
fn spec_name(spec: &str) -> String {
    spec.split('/').nth(1).unwrap_or("pkg").split(':').next().unwrap_or("pkg").to_string()
}

/// 文件路径的模块名(bare import 的名字 = 文件 stem)。
fn module_stem(path: &str) -> String {
    path.rsplit('/').next().unwrap_or("m").split('.').next().unwrap_or("m").to_string()
}

/// 解析模块值访问链:`deps.oxifmt` → ("deps", ["oxifmt"]);裸 Ident → ("x", [])。
fn parse_module_access(expr: ast::Expr) -> Option<(String, Vec<String>)> {
    fn walk(expr: ast::Expr, fields: &mut Vec<String>) -> Option<String> {
        match expr {
            ast::Expr::Ident(ident) => Some(ident.as_str().to_string()),
            ast::Expr::FieldAccess(access) => {
                let root = walk(access.target(), fields)?;
                fields.push(access.field().as_str().to_string());
                Some(root)
            }
            _ => None,
        }
    }
    let mut fields = Vec::new();
    let root = walk(expr, &mut fields)?;
    fields.reverse();
    Some((root, fields))
}

/// 解析 import 路径(POSIX 风格):以 `/` 开头为包根绝对路径,
/// 其余相对当前文件目录(`..` 归一)。
fn normalize_import(from: &str, raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let dir = if raw.starts_with('/') {
        Vec::new()
    } else {
        match from.rfind('/') {
            Some(idx) => from[..idx].split('/').collect::<Vec<_>>(),
            None => Vec::new(),
        }
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
