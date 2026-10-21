//! 函数体与 `NativeFuncData` 静态的生成。
//!
//! `gen_function` 编译参数解构 + 体直译;`gen_func_data` 手写函数的
//! `NativeFuncData` 描述(参数表按闭包 AST 推导)。

use std::fmt::Write as _;

use typst_syntax::ast::{self};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::{escape, sanitize};

impl Ctx {
    /// 生成一个函数:参数解构 + 体直译。
    pub(super) fn gen_function(
        &mut self,
        fun: &str,
        slot: &str,
        closure: ast::Closure,
        env: &Env,
    ) -> String {
        let mut body = String::new();
        let mut fn_env = env.clone();
        // 是否需要 `__st`(函数体/默认值引用了模块槽)在生成中追踪。
        self.used_state = false;
        // sink 参数延后解包(take 收剩余,须在显式参数之后)。
        let mut sinks: Vec<Option<String>> = Vec::new();

        for param in closure.params().children() {
            match param {
                ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                    let name = ident.as_str().to_string();
                    let rust = bind_local(&mut fn_env, &name);
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = rt::param_pos(__args, \"{}\", span)?;",
                        escape(&name)
                    );
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {}
                ast::Param::Pos(pattern) => {
                    // 解构参数(对照 eval_closure:"pattern parameter")。
                    let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                    body.push_str(&self.destructure(pattern, value, &mut fn_env, false));
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str().to_string();
                    // 默认值在定义期(module_init)求值物化进槽,调用期
                    // 只读:求值时机与 typst 一致(创建期一次)。
                    let def_slot = format!("DEF_{slot}_{}", sanitize(&name));
                    self.used_state = true;
                    let rust = bind_local(&mut fn_env, &name);
                    let _ = writeln!(
                        body,
                        "    let mut {rust} = match __args.named::<Value>(\"{}\")? {{\n\
                         \x20       Some(value) => value,\n\
                         \x20       None => rt::read(&__st.{def_slot})?,\n\
                         \x20   }};",
                        escape(&name)
                    );
                }
                ast::Param::Spread(spread) => {
                    // sink 收**剩余**参数(对照 typst 调用协议:显式参数
                    // 先匹配,sink 最后取走剩余)——延后到其余参数解包完
                    // 再 take,否则吞掉后续具名参数的传值。
                    let sink = match spread.sink_ident() {
                        Some(ident) => {
                            let name = ident.as_str().to_string();
                            let rust = bind_local(&mut fn_env, &name);
                            Some(rust)
                        }
                        None => None,
                    };
                    sinks.push(sink);
                }
            }
        }
        // sink 最后收剩余(typst 调用协议:显式参数先匹配)。
        for sink in sinks {
            let target = sink.as_deref().unwrap_or("_sink");
            let _ = writeln!(body, "    let mut {target} = Value::Args(__args.take());");
        }
        let _ = writeln!(body, "    rt::finish(__args, span)?;");

        let value = self.expr(closure.body(), &fn_env);
        let _ = writeln!(body, "    Ok({value})");

        let prologue = if self.used_state {
            "    let __st = rt::state_require::<__State>(engine)?;\n"
        } else {
            ""
        };
        format!(
            "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{prologue}{body}}}\n"
        )
    }
}

/// `NativeParamInfo` 字面量(元数据按闭包参数形态推导)。
fn format_param(name: &str, named: bool, positional: bool, variadic: bool) -> String {
    format!(
        "NativeParamInfo {{ name: \"{}\", docs: \"\", def_site: None, input: CastInfo::Any, default: None, positional: {positional}, named: {named}, variadic: {variadic}, required: {}, settable: false }}",
        escape(name),
        positional && !variadic
    )
}

/// 生成函数的 `NativeFuncData` 静态(手写字段,不经宏)。
pub(super) fn gen_func_data(data: &str, fun: &str, name: &str, closure: ast::Closure) -> String {
    let mut params = Vec::new();
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                params.push(format_param(ident.as_str(), false, true, false));
            }
            ast::Param::Pos(_) => params.push(format_param("_", false, true, false)),
            ast::Param::Named(named) => {
                // 命名参数仅按名绑定(args.named),元数据保持一致(KNOWN-ISSUES #10)。
                params.push(format_param(named.name().as_str(), true, false, false))
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
