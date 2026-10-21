//! 多文件包驱动:一个 typst 包 → **单个** Rust 源文件。
//!
//! 每个源文件对应一个 `mod m<i>_<stem>`(扁平唯一命名),文件内顶层
//! 绑定成为该 mod 的静态槽;包内 `#import` 在翻译期解析为跨 mod 的
//! 槽引用,导出面按 typst 语义抄入(bare → 模块值;`: *`/列表 → 绑定)。
//! 结构分析(import 图/拓扑序/引擎槽预扫描)见 `plan.rs`。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Mode, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, uppercase};
use super::paths::{normalize_import, parse_module_access};
use super::plan::{
    FilePlan, ImportEdge, ImportItems, build_export_table, has_import_cycle, nth_slot,
    scan_engine_slots, topo_order,
};

/// 待翻译的多文件包。
pub struct PackageInput {
    /// 相对路径(POSIX 风格,相对包根)→ 源码。
    pub files: std::collections::BTreeMap<String, String>,
    /// 入口文件相对路径。
    pub entry: String,
}

/// 宽松翻译(探索用):缺口降级为占位,返回 (代码, 诊断清单)。
pub fn translate_package_lenient(input: &PackageInput, module_name: &str) -> (String, Vec<Diag>) {
    let mut cx = Ctx {
        diags: Vec::new(),
        lenient: true,
        next_id: 0,
        current_file: None,
        package_files: None,
        package_mods: None,
    };
    let code = translate_package_inner(input, module_name, &mut cx);
    (code, cx.diags)
}

pub fn translate_package(input: &PackageInput, module_name: &str) -> Result<String, Vec<Diag>> {
    let mut cx = Ctx {
        diags: Vec::new(),
        lenient: false,
        next_id: 0,
        current_file: None,
        package_files: None,
        package_mods: None,
    };
    let code = translate_package_inner(input, module_name, &mut cx);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }
    Ok(code)
}

fn translate_package_inner(input: &PackageInput, module_name: &str, cx: &mut Ctx) -> String {
    let entry = input.entry.trim_start_matches('/');
    if !input.files.contains_key(entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: format!("入口文件 {entry} 不在文件集中"),
            file: None,
        });
        return String::new();
    }

    // 第一遍:每个文件的顶层绑定与 import 边。
    // 注意:此处直接使用形参 cx(此前误建局部同名 Ctx 遮蔽形参,
    // 导致包模式的诊断从未传回壳层,严格性失效)。
    let mut plans: Vec<FilePlan> = Vec::new();
    let mut roots: Vec<(String, typst_syntax::SyntaxNode)> = Vec::new();
    for (path, src) in input.files.iter() {
        roots.push((path.clone(), typst_syntax::parse(src)));
    }
    for (idx, (path, root)) in roots.iter().enumerate() {
        let Some(markup) = root.cast::<ast::Markup>() else {
            cx.diag(
                typst_syntax::Span::detached(),
                format!("{path} 未能解析为 markup 模块"),
            );
            continue;
        };
        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or("f")
            .split('.')
            .next()
            .unwrap_or("f");
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
                                cx.diag(
                                    expr.span(),
                                    "import 源须为字符串路径或模块字段访问".into(),
                                );
                                continue;
                            }
                        },
                    };
                    let target = if module_access.is_some() {
                        String::new()
                    } else if external {
                        // 外部包:target 即原始 spec(勿路径归一,否则
                        // rt::import_external 解析失败)。
                        raw.clone()
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
                    imports.push(ImportEdge {
                        span: expr.span(),
                        target,
                        items,
                        external,
                        module_access,
                    });
                }
                ast::Expr::LetBinding(lets) => {
                    let names = match lets.kind() {
                        ast::LetBindingKind::Closure(name) => vec![name.as_str().to_string()],
                        ast::LetBindingKind::Normal(pattern) => pattern
                            .bindings()
                            .iter()
                            .map(|i| i.as_str().to_string())
                            .collect(),
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
        plans.push(FilePlan {
            path: path.clone(),
            mod_name,
            markup,
            tops,
            imports,
        });
    }
    if !cx.diags.is_empty() && !cx.lenient {
        // 壳层负责返回 Err。
        return String::new();
    }

    // import 环检测(typst 模块循环求值同样报错)。
    if has_import_cycle(&plans, entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: "包内 import 成环(typst 运行时同样会报循环求值)".into(),
            file: None,
        });
        return String::new();
    }

    let package_files = std::rc::Rc::new(input.files.clone());
    let package_mods_map: std::collections::BTreeMap<String, String> = plans
        .iter()
        .map(|p| (p.path.clone(), p.mod_name.clone()))
        .collect();
    let package_mods = std::rc::Rc::new(package_mods_map);
    cx.package_files = Some(package_files.clone());
    cx.package_mods = Some(package_mods.clone());

    // 拓扑序(被依赖者在前)+ 引擎值 flag 预扫描。
    let order = topo_order(&plans);
    let engine_slots = scan_engine_slots(&plans, &order, cx);

    // 顶层绑定查找表:路径 → (名字 → qualified 槽)。
    let export_table = build_export_table(&plans);

    // 第二遍:逐文件生成(每个文件的函数体可见:自己的绑定 + import 抄入)。
    let mut items = String::new();
    let mut top_defines = String::new();
    let mut engine_init_defines = String::new();
    let mut package_engine_fills: Vec<String> = Vec::new();
    let mut external_fills: Vec<String> = Vec::new();
    for plan in &plans {
        cx.current_file = Some(plan.path.clone());
        let is_entry = plan.path == entry;
        let mut env = Env {
            locals: Vec::new(),
            module: Vec::new(),
        };
        // 当前文件的 import 抄入(可见绑定 + module() 定义)。
        let mut file_defines = String::new();
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // import 段生成的前置项(外部包槽等),并入本文件 mod。
        let mut file_prelude = format!(
            "\n#[allow(non_snake_case, dead_code)]\nfn file_span() -> Span {{\n    static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n    *CELL.get_or_init(|| {{\n        let id = typst::syntax::RootedPath::new(\n            typst::syntax::VirtualRoot::Project,\n            typst::syntax::VirtualPath::new({:?}).expect(\"path\"),\n        )\n        .intern();\n        typst::syntax::Source::new(id, String::new()).root().span()\n    }})\n}}\n",
            plan.path
        );
        for edge in &plan.imports {
            // 外部包:引擎槽 + 运行时导入(未翻译依赖走解释器)。
            if edge.external {
                let spec = edge.target.clone();
                let name = match &edge.items {
                    ImportItems::Rename(name) => name.clone(),
                    _ => spec_name(&spec),
                };
                let slot = format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(&name));
                file_prelude.push_str(&format!(
                    "\n#[allow(non_snake_case, dead_code)]\npub(crate) fn {slot}_init(engine: &mut Engine, _context: Tracked<Context>) -> SourceResult<()> {{\n    let v = Value::Module(rt::import_external(engine, \"{spec}\")?);\n    let _ = {slot}.set(v);\n    Ok(())\n}}\n\
                     #[allow(non_snake_case, dead_code)]\npub(crate) static {slot}: std::sync::OnceLock<Value> = std::sync::OnceLock::new();\n\
                     #[allow(non_snake_case, dead_code)]\npub(crate) fn {slot}_get() -> SourceResult<Value> {{\n    {slot}.get().cloned().ok_or_else(|| eco_vec![typst::diag::SourceDiagnostic::error(SPAN, typst::foundations::Str::from(\"外部包未初始化:宿主未调用 module_init(装配期导入)\"))])\n}}\n"
                ));
                external_fills.push(format!(
                    "    let _ = {plan_mod}::{slot}_init(engine, context)?;",
                    plan_mod = plan.mod_name
                ));
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
                                    expr: format!("{slot}_get()?"),
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
                        Ref::Engine(id) => format!("{id}_get()?"),
                        Ref::Value(e) => e.clone(),
                        Ref::ModuleField { expr, name } => {
                            format!("rt::field(&({expr}), \"{name}\", SPAN)?")
                        }
                    },
                    None => {
                        cx.diag(
                            edge.span,
                            format!("import 的模块根 `{root}` 不可见(须先 import)"),
                        );
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
                                Ref::ModuleField {
                                    expr: expr.clone(),
                                    name: item.clone(),
                                },
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
                    env.module.push((name.clone(), Ref::Slot(slot.clone())));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::slot(&{slot}));",
                        escape(&name)
                    );
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
                    env.module.push((name.clone(), Ref::Slot(slot.clone())));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::slot(&{slot}));",
                        escape(name)
                    );
                }
                ImportItems::List(list) => {
                    for (name, rename) in list {
                        let Some(qualified) = exports.get(name) else {
                            cx.diag(edge.span, format!("`{}` 不包含导出 `{name}`", edge.target));
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
                                "    scope.define(\"{}\", {qualified}_get()?);",
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
                                "    scope.define(\"{}\", {qualified}_get()?);",
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
        let mut file_items = file_prelude;
        let mut own: Vec<(String, String)> = Vec::new();
        let mut own_engine: Vec<String> = Vec::new();
        let mut own_seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for expr in plan.markup.exprs() {
            let ast::Expr::LetBinding(lets) = expr else {
                continue;
            };
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
                    env2.module
                        .push((name.as_str().to_string(), Ref::Slot(slot.clone())));
                    let fun = format!("f_{}", lowercase(&slot));
                    let data = format!("D_{}", uppercase(&slot));
                    file_items.push_str(&cx.gen_function(&fun, closure, &env2));
                    file_items.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
                    let _ = writeln!(
                        file_items,
                        "\n#[allow(non_snake_case, dead_code)]\nfn {slot}_init() -> Value {{\n    Value::Func(Func::from(&{data}))\n}}\n\
                         #[allow(non_snake_case, dead_code)]\npub(crate) static {slot}: rt::Slot = LazyLock::new(&{slot}_init);"
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
                        cx.diag(
                            lets.span(),
                            format!("顶层 `#let {}` 缺少初始化", name.as_str()),
                        );
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
                            "\n#[allow(non_snake_case, dead_code)]\npub(crate) fn {slot}_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Value> {{\n    Ok({value})\n}}\n\
                             #[allow(non_snake_case, dead_code)]\npub(crate) static {slot}: std::sync::OnceLock<Value> = std::sync::OnceLock::new();\n\
                             #[allow(non_snake_case, dead_code)]\npub(crate) fn {slot}_get() -> SourceResult<Value> {{\n    {slot}.get().cloned().ok_or_else(|| eco_vec![typst::diag::SourceDiagnostic::error(SPAN, typst::foundations::Str::from(\"引擎值未初始化:宿主未调用 module_init(装配期求值)\"))])\n}}\n"
                        ));
                        package_engine_fills.push(format!(
                            "    let _ = {mod_name}::{slot}.set({mod_name}::{slot}_init(engine, context)?);",
                            mod_name = plan.mod_name
                        ));
                        own_engine.push(slot.clone());
                        own.push((name.as_str().to_string(), slot.clone()));
                        continue;
                    }
                    let value = cx.expr(init_expr, &env2, Mode::ModuleInit);
                    file_items.push_str(&format!(
                        "\n#[allow(non_snake_case, dead_code)]\nfn {slot}_init() -> Value {{\n    {value}\n}}\n\
                         #[allow(non_snake_case, dead_code)]\npub(crate) static {slot}: rt::Slot = LazyLock::new(&{slot}_init);\n"
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
                        "    scope.define(\"{}\", {slot}_get()?);",
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

    let mut package_fills = String::new();
    for fill in external_fills.iter().chain(package_engine_fills.iter()) {
        package_fills.push_str(fill);
        package_fills.push('\n');
    }
    let degraded_header = if cx.lenient && !cx.diags.is_empty() {
        let mut header = String::from("// 降级清单(宽松模式,功能缺失点,运行时将发 warning):\n");
        for d in &cx.diags {
            header.push_str(&format!(
                "// - [{}] {}\n",
                d.file.as_deref().unwrap_or("?"),
                d.message.replace('\n', " ")
            ));
        }
        header
    } else {
        String::new()
    };
    format!(
        "// 由 trust 从包 `{module_name}`(入口 {entry})生成,勿手改。\n{degraded_header}\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult, bail}};\n\
use ecow::eco_vec;\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
use typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         const SPAN: Span = Span::detached();\n\n\
         /// 入口模块的导出面。\n\
         pub fn module() -> Module {{\n    let mut scope = Scope::new();\n{top_defines}    Module::new(\"{module_name}\", scope)\n}}\n\
         /// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。\n\
         #[allow(non_snake_case, unused_variables)]\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {{\n{package_fills}    let _ = (engine, context);\n{engine_init_defines}    Ok(())\n}}\n{items}"
    )
}

/// 从包 spec 取短名(`@preview/oxifmt:1.0.0` → `oxifmt`)。
fn spec_name(spec: &str) -> String {
    spec.split('/')
        .nth(1)
        .unwrap_or("pkg")
        .split(':')
        .next()
        .unwrap_or("pkg")
        .to_string()
}

/// 文件路径的模块名(bare import 的名字 = 文件 stem)。
fn module_stem(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("m")
        .split('.')
        .next()
        .unwrap_or("m")
        .to_string()
}
