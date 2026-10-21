//! 多文件包驱动:一个 typst 包 → **单个** Rust 源文件。
//!
//! 生成形态(per-build 槽板,见 `_things/DESIGN-per-build.md`):
//! - `__State`:包内全部顶层绑定槽 + import 槽(外部包、bare/rename
//!   模块值),每构建一份实例;
//! - 每个源文件一个 `mod m<i>_<stem>`:`eval(&__st, engine, context)`
//!   按源序填本文件槽位(模块求值的镜像),`module(&__st)` 从已填
//!   槽位构建该文件的导出面(含引擎值);
//! - 顶层 `module_init` 按拓扑序驱动各文件求值,返回入口模块。
//!
//! 包内 `#import` 在翻译期解析为跨文件槽引用;结构分析见 `plan.rs`。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, uppercase};
use super::paths::{normalize_import, parse_module_access};
use super::plan::{
    FilePlan, ImportEdge, ImportItems, build_export_table, has_import_cycle, nth_slot, topo_order,
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
    let mut cx = Ctx::new(true);
    let code = translate_package_inner(input, module_name, &mut cx);
    (code, cx.diags)
}

pub fn translate_package(input: &PackageInput, module_name: &str) -> Result<String, Vec<Diag>> {
    let mut cx = Ctx::new(false);
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
    // 经 Source 解析:span 携带 FileId(与生成物 file_span 同源)。
    let mut roots: Vec<(String, typst_syntax::Source)> = Vec::new();
    for (path, src) in input.files.iter() {
        let id = typst_syntax::RootedPath::new(
            typst_syntax::VirtualRoot::Project,
            typst_syntax::VirtualPath::new(path.as_str()).expect("path"),
        )
        .intern();
        roots.push((path.clone(), typst_syntax::Source::new(id, src.clone())));
    }
    for (idx, (path, source)) in roots.iter().enumerate() {
        let root: &'static typst_syntax::SyntaxNode = Box::leak(source.root().clone().into());
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
                        is_include: false,
                    });
                }
                ast::Expr::ModuleInclude(include) => {
                    // include 边:进依赖图(环检测/拓扑),名字不绑定。
                    let Some(ast::Expr::Str(path_expr)) = Some(include.source()) else {
                        cx.diag(expr.span(), "include 源暂只支持字符串路径".into());
                        continue;
                    };
                    let raw = path_expr.get().to_string();
                    let target = normalize_import(path, &raw);
                    if !input.files.contains_key(&target) {
                        cx.diag(
                            expr.span(),
                            format!("include \"{raw}\" 解析到 {target},不在文件集"),
                        );
                        continue;
                    }
                    imports.push(ImportEdge {
                        span: expr.span(),
                        target,
                        items: ImportItems::Bare,
                        external: false,
                        module_access: None,
                        is_include: true,
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
                // 其余顶层表达式(文本/嵌入/set/show)是模块 body
                // 内容,由第二遍收集折叠;此处无需处理。
                _ => {}
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

    // 拓扑序(被依赖者在前):求值顺序。
    let order = topo_order(&plans);
    let export_table = build_export_table(&plans);

    // 第二遍:逐文件生成 eval(填槽)+ module(导出面)+ 函数体。
    let mut items = String::new();
    let mut top_defines = String::new();
    let mut state_fields: Vec<String> = Vec::new();
    let mut eval_calls = String::new();
    for &idx in &order {
        let plan = &plans[idx];
        cx.current_file = Some(plan.path.clone());
        let is_entry = plan.path == entry;
        let mut env = Env {
            locals: Vec::new(),
            module: Vec::new(),
        };
        // 当前文件的 import 抄入:可见绑定(env) + 槽位填充(eval)
        // + 导出面(module)。
        let mut file_defines = String::new();
        let mut import_fills = String::new();
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for edge in &plan.imports {
            if edge.is_include {
                continue;
            }
            // 外部包:槽位 + 装配期运行时导入(未翻译依赖走解释器)。
            if edge.external {
                let spec = edge.target.clone();
                let name = match &edge.items {
                    ImportItems::Rename(name) => name.clone(),
                    _ => spec_name(&spec),
                };
                let slot = format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(&name));
                state_fields.push(slot.clone());
                let _ = writeln!(
                    import_fills,
                    "    rt::fill(&__st.{slot}, Value::Module(rt::import_external(engine, \"{spec}\")?))?;"
                );
                match &edge.items {
                    ImportItems::Bare | ImportItems::Rename(_) => {
                        env.module.push((name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(&name)
                        );
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{slot})?"),
                                    name: item.clone(),
                                },
                            ));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::field(&(rt::read(&__st.{slot})?), \"{}\", SPAN)?);",
                                escape(&as_name),
                                escape(item)
                            );
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
                    Some((_, module_ref)) => cx.ref_value(module_ref),
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
                ImportItems::Bare | ImportItems::Rename(_) => {
                    let bound = match &edge.items {
                        ImportItems::Rename(name) => name.clone(),
                        _ => edge
                            .target
                            .rsplit('/')
                            .next()
                            .unwrap_or("m")
                            .split('.')
                            .next()
                            .unwrap_or("m")
                            .to_string(),
                    };
                    let slot = match import_slots.get(&edge.target) {
                        Some(slot) => slot.clone(),
                        None => format!("IMPORT_{}_{}", sanitize(&plan.mod_name), sanitize(&bound)),
                    };
                    let existed = import_slots
                        .insert(edge.target.clone(), slot.clone())
                        .is_some();
                    if !existed {
                        state_fields.push(slot.clone());
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{slot}, Value::Module({target_mod}::module(&__st)?))?;"
                        );
                    }
                    env.module.push((bound.clone(), Ref::Slot(slot.clone())));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                        escape(&bound)
                    );
                }
                ImportItems::List(list) => {
                    for (name, rename) in list {
                        let Some(slot) = exports.get(name) else {
                            cx.diag(edge.span, format!("`{}` 不包含导出 `{name}`", edge.target));
                            continue;
                        };
                        let as_name = rename.clone().unwrap_or_else(|| name.clone());
                        env.module.push((as_name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(&as_name)
                        );
                    }
                }
                ImportItems::Wildcard => {
                    for (name, slot) in exports {
                        if env.module.iter().any(|(n, _)| n == name) {
                            continue;
                        }
                        env.module.push((name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(name)
                        );
                    }
                }
            }
        }

        // 本文件顶层绑定与内容(按源序):绑定填槽,内容折叠进 body。
        let mut binding_code = String::new();
        let mut file_fns = String::new();
        let mut own: Vec<(String, String)> = Vec::new();
        let mut content_items: Vec<super::expr::ContentItem> = Vec::new();
        let mut own_seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for expr in plan.markup.exprs() {
            // 内容项:顶层文本/嵌入表达式/set/show/include。
            match expr {
                ast::Expr::LetBinding(_) | ast::Expr::ModuleImport(_) => {
                    // 对照 eval_markup:语句并入一个空 display 槽。
                    content_items.push(super::expr::ContentItem::Part(expr));
                }
                ast::Expr::ModuleInclude(include) => {
                    let Some(ast::Expr::Str(path_expr)) = Some(include.source()) else {
                        cx.diag(expr.span(), "include 源暂只支持字符串路径".into());
                        continue;
                    };
                    let raw = path_expr.get().to_string();
                    let target = normalize_import(&plan.path, &raw);
                    let Some(target_mod) = package_mods.get(&target) else {
                        continue;
                    };
                    content_items.push(super::expr::ContentItem::Include(format!(
                        "BODY_{target_mod}"
                    )));
                    continue;
                }
                ast::Expr::SetRule(set) => {
                    content_items.push(super::expr::ContentItem::Set(set));
                    continue;
                }
                ast::Expr::ShowRule(show) => {
                    content_items.push(super::expr::ContentItem::Show(show));
                    continue;
                }
                other => {
                    content_items.push(super::expr::ContentItem::Part(other));
                    continue;
                }
            }
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
                        env2.module
                            .push((seen.clone(), Ref::Slot(seen_slot.clone())));
                    }
                    env2.module
                        .push((name.as_str().to_string(), Ref::Slot(slot.clone())));
                    let fun = format!("f_{}", lowercase(&slot));
                    let data = format!("D_{}", uppercase(&slot));
                    file_fns.push_str(&cx.gen_function(&fun, &slot, closure, &env2));
                    file_fns.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
                    // 默认值创建期物化(对照 typst:闭包创建时逐参数求值;
                    // env2 = import + 此前绑定,不含本函数自身)。
                    for param in closure.params().children() {
                        if let ast::Param::Named(named) = param {
                            let pname = named.name().as_str();
                            let def_slot = format!("DEF_{slot}_{}", sanitize(pname));
                            state_fields.push(def_slot.clone());
                            let value = cx.expr(named.expr(), &env2);
                            let _ = writeln!(
                                binding_code,
                                "    {{ let v = {value}; rt::fill(&__st.{def_slot}, v.clone())?; }}"
                            );
                        }
                    }
                    let _ = writeln!(
                        binding_code,
                        "    rt::fill(&__st.{slot}, Value::Func(Func::from(&{data})))?;"
                    );
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                        escape(name.as_str())
                    );
                    own.push((name.as_str().to_string(), slot.clone()));
                }
                ast::LetBindingKind::Normal(pattern) => {
                    let Some(init_expr) = lets.init() else {
                        if let Some(name) = pattern.bindings().first() {
                            cx.diag(
                                lets.span(),
                                format!("顶层 `#let {}` 缺少初始化", name.as_str()),
                            );
                        }
                        continue;
                    };
                    // 每个模式名字配对本文件的下一个同名槽(解构多名)。
                    let mut bound: Vec<(String, String)> = Vec::new();
                    for ident in pattern.bindings().iter() {
                        let name = ident.as_str();
                        let nth = own_seen_counts.get(name).copied().unwrap_or(0);
                        own_seen_counts.insert(name.to_string(), nth + 1);
                        let Some(slot) = nth_slot(plan, name, nth) else {
                            continue;
                        };
                        bound.push((name.to_string(), slot));
                    }
                    if bound.is_empty() {
                        continue;
                    }
                    let mut env2 = env.clone();
                    for (seen, seen_slot) in &own {
                        env2.module
                            .push((seen.clone(), Ref::Slot(seen_slot.clone())));
                    }
                    // init 在绑定生效前求值:同名引用解析到外层(如全局
                    // 类型),env2 不含本绑定。
                    if let [(name, slot)] = bound.as_slice() {
                        let value = cx.expr(init_expr, &env2);
                        let _ = writeln!(
                            binding_code,
                            "    {{ let v = {value}; rt::fill(&__st.{slot}, v.clone())?; }}"
                        );
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(name)
                        );
                    } else {
                        // 顶层解构:槽名预绑为临时局部,destructure 写回
                        // 这些名字,再逐槽填充注册。
                        let value = cx.expr(init_expr, &env2);
                        let mut tmp_env = env2.clone();
                        let mut decls = String::new();
                        for (name, slot) in &bound {
                            tmp_env.locals.push((name.clone(), slot.clone()));
                            let _ = writeln!(decls, "        let mut {slot} = Value::None;");
                        }
                        let stmts =
                            cx.destructure(pattern, "v.clone()".to_string(), &mut tmp_env, false);
                        let mut fills = String::new();
                        for (name, slot) in &bound {
                            let _ =
                                writeln!(fills, "        rt::fill(&__st.{slot}, {slot}.clone())?;");
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                                escape(name)
                            );
                        }
                        let _ = writeln!(
                            binding_code,
                            "    {{ let v = {value};\n{decls}{stmts}{fills}    }}"
                        );
                    }
                    own.extend(bound);
                }
            }
        }
        for (_, slot) in &plan.tops {
            state_fields.push(slot.clone());
        }
        // 模块 body:顶层内容按源序折叠(规则包装后续内容),入槽。
        let body_slot = format!("BODY_{}", plan.mod_name);
        state_fields.push(body_slot.clone());
        let mut body_env = env.clone();
        for (seen, seen_slot) in &own {
            body_env
                .module
                .push((seen.clone(), Ref::Slot(seen_slot.clone())));
        }
        let body = cx.content_seq(&content_items, &body_env);
        let _ = writeln!(
            binding_code,
            "    rt::fill(&__st.{body_slot}, Value::Content({body}))?;"
        );

        // 文件 mod 块:file_span 锚点 + 函数体 + eval(填槽)+ module(导出面)。
        let mut file_mod = format!(
            "\nmod {} {{\n    use super::*;\n\n    #[allow(non_snake_case, dead_code)]\n    fn file_span() -> Span {{\n        static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n        *CELL.get_or_init(|| {{\n            let id = typst::syntax::RootedPath::new(\n                typst::syntax::VirtualRoot::Project,\n                typst::syntax::VirtualPath::new({:?}).expect(\"path\"),\n            )\n            .intern();\n            typst::syntax::Source::new(id, String::new()).root().span()\n        }})\n    }}\n{file_fns}",
            plan.mod_name, plan.path
        );
        let _ = writeln!(
            file_mod,
            "\n    /// `{}` 的求值:填 import 槽与自身绑定槽(模块求值的镜像)。\n\
             \x20   pub(super) fn eval(__st: &__State, engine: &mut Engine, context: Tracked<Context>) -> SourceResult<()> {{\n\
             \x20       let _ = (&*engine, context);\n{import_fills}{binding_code}        Ok(())\n    }}",
            plan.path
        );
        let _ = writeln!(
            file_mod,
            "\n    /// `{}` 的模块作用域(import 抄入 + 本文件顶层绑定),从已填槽位构建。\n\
             \x20   #[allow(non_snake_case, dead_code)]\n\
             \x20   pub(super) fn module(__st: &__State) -> SourceResult<Module> {{\n\
             \x20       let _ = __st;\n\
             \x20       let mut scope = Scope::new();\n{file_defines}        Ok(Module::new(\"{}\", scope).with_content(rt::read_content(&__st.{body_slot})?))\n    }}",
            plan.path,
            module_stem(&plan.path)
        );
        items.push_str(&file_mod);
        items.push_str("}\n");

        if is_entry {
            top_defines = file_defines;
            let _ = writeln!(
                eval_calls,
                "    // 入口 `{entry}`(被依赖文件已在上方求值)。"
            );
        }
        let _ = writeln!(
            eval_calls,
            "    {}::eval(&__st, engine, context)?;",
            plan.mod_name
        );
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

    let entry_mod = plans
        .iter()
        .find(|p| p.path == entry)
        .map(|p| p.mod_name.clone())
        .expect("entry in plans");
    let mut fields = String::new();
    for slot in &state_fields {
        let _ = writeln!(fields, "    {slot}: std::sync::OnceLock<Value>,");
    }

    let span_consts = cx.span_consts();
    format!(
        "// 由 trust 从包 `{module_name}`(入口 {entry})生成,勿手改。\n{degraded_header}\n\n\
         use std::sync::LazyLock;\n\
         use comemo::Tracked;\n\
         use typst::diag::{{At, SourceResult, bail}};\n\
         use ecow::eco_vec;\n\
         use typst::engine::Engine;\n\
         use typst::foundations::{{\n    Args, CastInfo, Content, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,\n    NativeParamInfo, Scope, Value, ops,\n}};\n\
         use typst::syntax::Span;\n\
         use typst::syntax::ast::Unit;\n\
         use trust_runtime as rt;\n\n\
         const SPAN: Span = Span::detached();\n{span_consts}\n\
         /// 本包的求值状态:全部顶层绑定槽与 import 槽(每构建一份)。\n\
         #[derive(Default)]\n\
         struct __State {{\n{fields}}}\n\n\
         /// 包求值的镜像:宿主每次构建(World)调用一次,按拓扑序求值各文件。\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let __st = rt::state_get_or_insert::<__State>(engine, __State::default);\n{eval_calls}    let mut scope = Scope::new();\n{top_defines}    Ok(Module::new(\"{module_name}\", scope).with_content(rt::read_content(&__st.BODY_{entry_mod})?))\n}}\n{items}"
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
