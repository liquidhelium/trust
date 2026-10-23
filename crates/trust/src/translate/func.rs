//! 函数体与 `NativeFuncData` 静态的生成。
//!
//! `gen_function` 编译参数解构 + 体直译;`gen_func_data` 手写函数的
//! `NativeFuncData` 描述(参数表按闭包 AST 推导)。

use std::fmt::Write as _;

use typst_syntax::ast::{self};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::{escape, sanitize, sanitize_key};

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
        // 函数体上下文:return 编译为 Rust return,仅此上下文合法。
        self.in_fn += 1;
        // 对照 eval_closure 的绑定协议(声明序逐参绑定,sink 特殊):
        // - sink_size = 位置实参总数 − 显式位置参数数(saturating);
        // - Spread 处按 sink_size 从**队首** consume 位置实参暂存;
        // - 显式位置参数(含在其后的)从剩余里按序 expect;
        // - 末尾 sink = take()(剩余全部,含具名)+ 暂存队首项 extend。
        // 曾错为先绑显式参数再 take 全部剩余:`(..args, body)` 在
        // 多配置实参下 body 错拿队首配置、内容落入 args.pos(),
        // touying 的 merge-dicts 因此收到内容(add-dicts .keys() 崩,
        // touying 驱动;gdb 断点错误构造处定位的调用链)。
        let num_pos_params = closure
            .params()
            .children()
            .filter(|p| matches!(p, ast::Param::Pos(_)))
            .count();
        let mut sink_stash: Vec<(Option<String>, String)> = Vec::new();
        // 位置实参总数须在**任何绑定前**取(sink_size 的分母是全量;
        // 在 Spread 处现算会被先声明的参数扣掉)。
        let has_sink = closure
            .params()
            .children()
            .any(|p| matches!(p, ast::Param::Spread(_)));
        if has_sink {
            let _ = writeln!(body, "    let __n_pos_args = __args.to_pos().len();");
        }
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
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                    // 占位符消费一个位置实参(对照 eval_closure 的
                    // `args.expect::<Value>("pattern parameter")`)。
                    let _ = writeln!(
                        body,
                        "    let _ = rt::param_pos(__args, \"pattern parameter\", span)?;"
                    );
                }
                ast::Param::Pos(pattern) => {
                    // 解构参数(对照 eval_closure:"pattern parameter")。
                    let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                    body.push_str(&self.destructure(pattern, value, &mut fn_env, false));
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str().to_string();
                    // 默认值在定义期(module_init)求值物化进槽,调用期
                    // 只读:求值时机与 typst 一致(创建期一次)。
                    let def_slot = format!("DEF_{slot}_{}", sanitize_key(&name));
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
                    let stash = format!("__sink_pos_{}", sink_stash.len());
                    let _ = writeln!(
                        body,
                        "    let {stash} = __args.consume(__n_pos_args.saturating_sub({num_pos_params}))?;"
                    );
                    let sink = spread.sink_ident().map(|ident| {
                        let name = ident.as_str().to_string();
                        bind_local(&mut fn_env, &name)
                    });
                    sink_stash.push((sink, stash));
                }
            }
        }
        // sink = take()(剩余,含具名)按声明序 + 暂存的队首位置项
        // (对照 eval_closure 末尾 `remaining_args.items.extend(sink_pos)`)。
        for (sink, stash) in sink_stash {
            let target = sink.as_deref().unwrap_or("_sink");
            let _ = writeln!(
                body,
                "    let mut {target} = Value::Args(__args.take());\n    if let Value::Args(__sink_inner) = &mut {target} {{ __sink_inner.items.extend({stash}.into_iter()); }}"
            );
        }
        let _ = writeln!(body, "    rt::finish(__args, span)?;");

        let value = self.expr(closure.body(), &fn_env);
        let _ = writeln!(body, "    Ok({value})");

        // 序言:状态从首个位置参数解包(stub 转发必带,对照原生闭包
        // C 方案:数据随函数值走,全局注册表寻址退场)。必须在参数
        // 解包前——state 占据首个位置参数。
        let prologue = if self.used_state {
            "    let __st = rt::take_state::<__State>(__args)?;\n"
        } else {
            ""
        };
        self.in_fn -= 1;
        format!(
            "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{prologue}{body}}}\n"
        )
    }
pub(super) fn gen_closure_fn(
    &mut self,
    fun: &str,
    data: &str,
    closure: ast::Closure,
    capture_names: &[String],
    self_name: Option<&str>,
    params_src: &str,
) -> String {
    // 闭包体的求值环境 = 自身参数 + 捕获(全部自由名都已入捕获元组,
    // 不继承外层 env)。has_st(体引用 __st:模块槽读取或体内字符串
    // import)在体生成后判定——序言与元组类型随后组。
    let mut fn_env = super::ctx::Env {
        locals: Vec::new(),
        module: Vec::new(),
        wildcards: Vec::new(),
    };
    let saved = self.used_state;
    self.used_state = false;
    self.in_fn += 1;
    self.in_closure += 1;

    // 先绑定捕获名(体的自由名解析到这些局部)。
    for name in capture_names {
        bind_local(&mut fn_env, name);
    }
    // 自引用名在体生成**前**注入 env(此前在序言构造段才绑定,
    // 体生成时名字不可见,递归点落到 global 回退——unknown variable)。
    let mut self_binding: Option<(String, String)> = None;
    if let Some(name) = self_name {
        let rust = bind_local(&mut fn_env, name);
        self_binding = Some((name.to_string(), rust));
    }
    // 参数绑定:位置/解构/sink 与顶层同款;命名参数 stub 已填齐,
    // 直接必取。
    // 声明序绑定,sink 按队首 consume 暂存(与顶层函数序言同协议,
    // 对照 eval_closure;详见顶层函数处的协议注释)。
    let mut params_code = String::new();
    let num_pos_params = closure
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    let mut sink_stash: Vec<(Option<String>, String)> = Vec::new();
    let has_sink = closure
        .params()
        .children()
        .any(|p| matches!(p, ast::Param::Spread(_)));
    if has_sink {
        let _ = writeln!(params_code, "    let __n_pos_args = __args.to_pos().len();");
    }
    for param in closure.params().children() {
        match param {
            ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(ident))) => {
                let name = ident.as_str().to_string();
                let rust = bind_local(&mut fn_env, &name);
                let _ = writeln!(
                    params_code,
                    "    let mut {rust} = rt::param_pos(__args, \"{}\", span)?;",
                    escape(&name)
                );
            }
            ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                // 占位符消费一个位置实参(同顶层函数序言)。
                let _ = writeln!(
                    params_code,
                    "    let _ = rt::param_pos(__args, \"pattern parameter\", span)?;"
                );
            }
            ast::Param::Pos(pattern) => {
                let value = "rt::param_pos(__args, \"pattern parameter\", span)?".to_string();
                params_code.push_str(&self.destructure(pattern, value, &mut fn_env, false));
            }
            ast::Param::Named(named) => {
                let name = named.name().as_str().to_string();
                let rust = bind_local(&mut fn_env, &name);
                let _ = writeln!(
                    params_code,
                    "    let mut {rust} = rt::param_named(__args, \"{}\", span)?;",
                    escape(&name)
                );
            }
            ast::Param::Spread(spread) => {
                let stash = format!("__sink_pos_{}", sink_stash.len());
                let _ = writeln!(
                    params_code,
                    "    let {stash} = __args.consume(__n_pos_args.saturating_sub({num_pos_params}))?;"
                );
                let sink = spread.sink_ident().map(|ident| {
                    let name = ident.as_str().to_string();
                    bind_local(&mut fn_env, &name)
                });
                sink_stash.push((sink, stash));
            }
        }
    }
    for (sink, stash) in sink_stash {
        let target = sink.as_deref().unwrap_or("_sink");
        let _ = writeln!(
            params_code,
            "    let mut {target} = Value::Args(__args.take());\n    if let Value::Args(__sink_inner) = &mut {target} {{ __sink_inner.items.extend({stash}.into_iter()); }}"
        );
    }
    let _ = writeln!(params_code, "    rt::finish(__args, span)?;");

    let value = self.expr(closure.body(), &fn_env);
    let body_code = format!("    Ok({value})\n");

    // has_st 判定(体/参数代码生成期间的标记),恢复外层计数并留下
    // 本闭包的判定给调用方读取。
    // has_st = 体自身判定;used_state 留给调用方读取后恢复外层计数
    //(勿在此恢复——外层再读会被污染,创建元组与序言形态不一致)。
    let has_st = self.used_state;
    self.in_fn -= 1;
    self.in_closure = self.in_closure.saturating_sub(1);

    // 状态元组与解包序言(has_st 后置判定)。自递归闭包:���部追加
    // SelfFuncCell(自身 stub 的每实例一次重建缓存),序言保留槽。
    let mut tuple_t: Vec<&str> = Vec::new();
    let mut destruct: Vec<String> = Vec::new();
    if has_st {
        tuple_t.push("std::sync::Arc<__State>");
        destruct.push("__st".to_string());
    }
    for i in 0..capture_names.len() {
        tuple_t.push("Option<Value>");
        destruct.push(format!("__c{i}"));
    }
    let self_destruct;
    if self_name.is_some() {
        tuple_t.push("std::sync::Arc<rt::SelfFuncCell>");
        self_destruct = "__selfcell".to_string();
        destruct.push(self_destruct.clone());
    }
    let tuple_t = if tuple_t.is_empty() {
        "()".to_string()
    } else {
        tuple_t.join(", ")
    };
    let mut prologue = if self_name.is_some() {
        format!(
            "    let (({}), __slot) = rt::take_caps_self::<({tuple_t})>(__args)?;\n",
            destruct.join(", ")
        )
    } else if destruct.is_empty() {
        "    rt::take_caps::<()>(__args)?;\n".to_string()
    } else {
        format!(
            "    let ({}) = rt::take_caps::<({tuple_t})>(__args)?;\n",
            destruct.join(", ")
        )
    };
    // 捕获按名解包(None 即创建期不可见名字,调用期报 unknown
    // variable,与原生捕获语义一致)。按 env 绑定序取局部名。
    for (i, name) in capture_names.iter().enumerate() {
        let rust = fn_env
            .locals
            .iter()
            .rev()
            .find(|(n, _)| n == name)
            .map(|(_, r)| r.clone())
            .unwrap_or_else(|| format!("__cap_{i}"));
        let _ = writeln!(
            prologue,
            "    let mut {rust} = rt::cap(__c{i}, \"{}\", span)?;",
            escape(name)
        );
    }
    if let Some((name, rust)) = &self_binding {
        // 自引用绑定发射(绑定时机见上方 env 注入;序言在此)。
        // 重建走每调用点静态节点(与创建点同一份,零查找零锁)。
        let stub_src = trust_runtime::compose_stub(name, params_src);
        let node_name = self.closure_node_static(&stub_src);
        let _ = writeln!(
            prologue,
            "    let mut {rust} = Value::Func(rt::self_func_node(&__selfcell, &{data}, &{node_name}, &__slot));"
        );
    }

    format!(
        "\n#[allow(non_snake_case, dead_code, unused_variables)]\nfn {fun}(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {{\n    let span = __args.span;\n{prologue}{params_code}{body_code}}}\n"
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
