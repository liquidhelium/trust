//! 单文件驱动:一个 typst 模块源文件 → 一个 Rust 模块源文件。
//!
//! 生成形态(per-build 槽板,见 `_things/DESIGN-per-build.md`):
//! - `__State`:每个顶层绑定一个写一次槽(`OnceLock<Value>`),
//!   每构建一份实例,按源序在 `module_init` 中求值填充;
//! - `__STATES`:构建键(World 身份)→ 状态实例的注册表;
//! - `module_init(engine, context) -> Module`:模块求值的镜像,
//!   宿主每次构建调用一次;
//! - 函数:体内引用模块槽时编译为 `rt::read(&__st.槽名)?`,
//!   函数体带 `__st` 序言(`__STATES.require(engine)`)。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, uppercase};

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
        return Err(vec![Diag {
            span: typst_syntax::Span::detached(),
            message: "源文件未能解析为 markup 模块".into(),
            file: None,
        }]);
    };

    let mut cx = Ctx {
        diags: Vec::new(),
        lenient: false,
        next_id: 0,
        current_file: None,
        package_files: None,
        package_mods: None,
        used_state: false,
    };
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
                        cx.diag(
                            lets.span(),
                            format!("顶层 `#let {}` 缺少初始化", name.as_str()),
                        );
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
                    format!(
                        "模块顶层暂只支持 `#let` 绑定,遇到 {kind:?}(标记内容/导入等属高级语义)"
                    ),
                );
            }
        }
    }

    // 第二遍:按源序生成求值语句(init 在绑定生效前求值,同名引用
    // 解析到外层,由下方值分支自行处理)。
    let mut init_code = String::new();
    let mut fields = String::new();
    let mut items = String::new();
    for (i, top) in tops.iter().enumerate() {
        // 函数体可见自身(递归)与此前全部绑定;值 init 不含本绑定。
        let slot_of = |j: usize| (tops[j].name.clone(), Ref::Slot(tops[j].rust.clone()));
        let env = Env {
            locals: Vec::new(),
            module: (0..=i).map(slot_of).collect(),
        };
        let init_env = Env {
            locals: Vec::new(),
            module: (0..i).map(slot_of).collect(),
        };
        let slot = &top.rust;
        let _ = writeln!(fields, "    {slot}: std::sync::OnceLock<Value>,");
        match &top.kind {
            TopKind::Value { init } => {
                let expr = cx.expr(*init, &init_env);
                let _ = writeln!(
                    init_code,
                    "    {{ let v = {expr}; rt::fill(&__st.{slot}, v.clone())?; scope.define(\"{}\", v); }}",
                    escape(&top.name)
                );
            }
            TopKind::Fn { closure } => {
                let fun = format!("f_{}", lowercase(&top.rust));
                let data = format!("D_{}", uppercase(&top.rust));
                items.push_str(&cx.gen_function(&fun, *closure, &env));
                items.push_str(&gen_func_data(&data, &fun, &top.name, *closure));
                let _ = writeln!(
                    init_code,
                    "    {{ let v = Value::Func(Func::from(&{data})); rt::fill(&__st.{slot}, v.clone())?; scope.define(\"{}\", v); }}",
                    escape(&top.name)
                );
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
         /// 本模块的求值状态:每个顶层绑定一个写一次槽(每构建一份)。\n\
         #[derive(Default)]\n\
         struct __State {{\n{fields}}}\n\n\
         /// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let mut scope = Scope::new();\n    let __st = rt::state_get_or_insert::<__State>(engine, __State::default);\n    let _ = context;\n{init_code}    Ok(Module::new(\"{module_name}\", scope))\n}}\n{items}"
    ))
}
