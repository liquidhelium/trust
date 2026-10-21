//! 单文件驱动:一个 typst 模块源文件 → 一个 Rust 模块源文件。
//!
//! 两遍扫描:先收集顶层绑定(名字 → 槽)并预判每个顶层值是否需要
//! 引擎(决定槽形态),再逐绑定生成函数/槽与模块装配代码。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, Mode, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, uppercase};
use super::Diag;

/// 顶层绑定:名字 + 槽名 + 形态(函数闭包节点或值 init)。
struct TopBinding<'a> {
    name: String,
    rust: String,
    kind: TopKind<'a>,
}

enum TopKind<'a> {
    Value { init: ast::Expr<'a> },
    Fn { closure: ast::Closure<'a> },
}

/// 把一个 typst 模块源文件翻译为 Rust 源码文本。
pub fn translate(src: &str, module_name: &str) -> Result<String, Vec<Diag>> {
    let root = typst_syntax::parse(src);
    let Some(markup) = root.cast::<ast::Markup>() else {
        return Err(vec![Diag { span: typst_syntax::Span::detached(), message: "源文件未能解析为 markup 模块".into(), file: None }]);
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
    let mut engine_fills = String::new();
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
                         #[allow(non_snake_case, dead_code)]\nfn {id}_get() -> SourceResult<Value> {{\n    {id}.get().cloned().ok_or_else(|| eco_vec![typst::diag::SourceDiagnostic::error(SPAN, typst::foundations::Str::from(\"引擎值未初始化:宿主未调用 module_init(装配期求值)\"))])\n}}"
                    );
                    let _ = writeln!(engine_fills, "    let _ = {id}.set({id}_init(engine, context)?);");
                    let _ = writeln!(
                        engine_defines,
                        "    scope.define(\"{}\", {id}_get()?);",
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
use ecow::eco_vec;\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
use typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         /// 生成的错误 span:detached(差分对拍只比较值)。\n\
         const SPAN: Span = Span::detached();\n\
         /// 当前文件的锚点 span(带 FileId;plugin 等按 span 定位的全局函数依赖它)。\n\
         #[allow(non_snake_case, dead_code)]\n\
         fn file_span() -> Span {{\n    static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n    *CELL.get_or_init(|| {{\n        let id = typst::syntax::RootedPath::new(\n            typst::syntax::VirtualRoot::Project,\n            typst::syntax::VirtualPath::new(\"main.typ\").expect(\"path\"),\n        )\n        .intern();\n        typst::syntax::Source::new(id, String::new()).root().span()\n    }})\n}}\n\n\
         /// 构建该模块(名字与导出面照抄源文件)。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{defines}    Module::new(\"{module_name}\", scope)\n}}\n\
         /// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。\n\
         #[allow(non_snake_case, unused_variables)]\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {{\n{engine_fills}    let _ = (engine, context);\n{engine_defines}    Ok(())\n}}\n{items}"
    ))
}
