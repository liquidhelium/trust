//! 多文件包驱动:一个 typst 包 → 单文件或目录树两种 Rust 产物。
//!
//! 生成形态(per-build 槽板,见 `_things/DESIGN-per-build.md`):
//! - `__State`:包内全部顶层绑定槽 + import 槽(外部包、bare/rename
//!   模块值),每构建一份实例;
//! - 每个源文件一个模块:`eval(&__st, engine, context)` 按源序填本
//!   文件槽位(模块求值的镜像),`module(&__st)` 从已填槽位构建该文件
//!   的导出面(含引擎值);
//! - 顶层 `module_init` 按拓扑序驱动各文件求值,返回入口模块。
//!
//! 单文件形态(`render_single`):全部文件体内联为扁平 `mod` 块,是
//! 差分与快照的锁定基准;目录形态(`translate_package_tree`):镜像
//! 源码树,每个 typ 文件一个 rust 文件,跨文件引用经根的 `__State`
//! 与 `use super::*` 链(私有 use 对后代可见)。`x.typ` 与 `x/` 目录
//! 并存时,目录声明并入 `x.rs`(rust 文件模块的子模块查找目录恰为
//! `x/`),否则目录用 `mod.rs`。
//!
//! 包内 `#import` 在翻译期解析为跨文件槽引用;结构分析见 `plan.rs`。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::Diag;
use super::ctx::{Ctx, Env, Ref};
use super::func::gen_func_data;
use super::naming::{escape, lowercase, sanitize, sanitize_key, uppercase};
use super::paths::{normalize_import, parse_module_access};
use super::plan::{
    FilePlan, ImportEdge, ImportItems, build_export_table, has_import_cycle, module_stem, nth_slot,
    topo_order,
};

/// 待翻译的多文件包。
pub struct PackageInput {
    /// 相对路径(POSIX 风格,相对包根)→ 源码。
    pub files: std::collections::BTreeMap<String, String>,
    /// 入口文件相对路径。
    pub entry: String,
}

/// 一个 typ 文件的可发射产物:文件体 + 单文件/目录两种模块名。
struct FileOut {
    /// 文件体(`mod` 块内部形态,与单文件产物逐字节同形)。
    body: String,
    /// 单文件形态的扁平 mod 名(`m{i}_{stem}`)。
    flat_name: String,
    /// 目录形态的完整模块路径段(镜像 typ 路径,如 `[src, draw, shapes]`)。
    tree_full: Vec<String>,
}

/// 包翻译的中间产物:header(带 `{EVAL_CALLS}`/`{ITEMS}` 占位)+
/// 双形态 eval 驱动 + 各文件体,由 `render_single`/`translate_package_tree`
/// 组装为最终产物。空态(header 空、files 空)对应早退。
#[derive(Default)]
struct PackageParts {
    header: String,
    eval_flat: String,
    eval_tree: String,
    files: Vec<FileOut>,
}

/// 宽松翻译(探索用):缺口降级为占位,返回 (代码, 诊断清单)。
pub fn translate_package_lenient(input: &PackageInput, module_name: &str) -> (String, Vec<Diag>) {
    let mut cx = Ctx::new(true);
    let parts = translate_package_parts(input, module_name, &mut cx);
    (render_single(&parts), cx.diags)
}

pub fn translate_package(input: &PackageInput, module_name: &str) -> Result<String, Vec<Diag>> {
    let mut cx = Ctx::new(false);
    let parts = translate_package_parts(input, module_name, &mut cx);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }
    Ok(render_single(&parts))
}

fn translate_package_parts(input: &PackageInput, module_name: &str, cx: &mut Ctx) -> PackageParts {
    let entry = input.entry.trim_start_matches('/');
    if !input.files.contains_key(entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: format!("入口文件 {entry} 不在文件集中"),
            file: None,
        });
        return PackageParts::default();
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
        return PackageParts::default();
    }

    // import 环检测(typst 模块循环求值同样报错)。
    if has_import_cycle(&plans, entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: "包内 import 成环(typst 运行时同样会报循环求值)".into(),
            file: None,
        });
        return PackageParts::default();
    }

    let package_files = std::rc::Rc::new(input.files.clone());
    let package_mods_map: std::collections::BTreeMap<String, String> = plans
        .iter()
        .map(|p| (p.path.clone(), p.mod_name.clone()))
        .collect();
    let package_mods = std::rc::Rc::new(package_mods_map);
    cx.package_files = Some(package_files.clone());
    cx.package_mods = Some(package_mods.clone());

    // 拓扑序(被依赖者在前):求值顺序。导出查找表按同序展开,
    // 下游 list/wildcard import 可达上游的 import 转出名。
    let order = topo_order(&plans);
    let export_table = build_export_table(&plans, &order);

    // 第二遍:逐文件生成 eval(填槽)+ module(导出面)+ 函数体。
    let mut top_defines = String::new();
    let mut state_fields: Vec<String> = Vec::new();
    let mut files: Vec<FileOut> = Vec::new();
    let mut eval_flat = String::new();
    let mut eval_tree = String::new();
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
                let ext_at = cx.span_of(edge.span);
                let _ = writeln!(
                    import_fills,
                    "    rt::fill(&__st.{slot}, Value::Module(rt::import_external(engine, \"{spec}\", {ext_at})?))?;"
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
                                "    scope.define(\"{}\", rt::field(&(rt::read(&__st.{slot})?), \"{}\", engine, context, SPAN)?);",
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
                    expr = format!(
                        "rt::field(&({expr}), \"{}\", engine, context, SPAN)?",
                        escape(field)
                    );
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
                            "    rt::fill(&__st.{slot}, Value::Module({target_mod}::module(&__st, engine, context)?))?;"
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
        let mut content_items: Vec<super::expr::TopItem> = Vec::new();
        let mut own_seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        let content_env = |env: &Env, own: &[(String, String)]| {
            let mut e = env.clone();
            for (seen, seen_slot) in own {
                e.module.push((seen.clone(), Ref::Slot(seen_slot.clone())));
            }
            e
        };
        for expr in plan.markup.exprs() {
            // 内容项:顶层文本/嵌入表达式/set/show/include(逐项可见性)。
            match expr {
                ast::Expr::LetBinding(_) | ast::Expr::ModuleImport(_) => {
                    // 对照 eval_markup:语句并入一个空 display 槽。
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(expr),
                        env: content_env(&env, &own),
                    });
                }
                ast::Expr::ModuleInclude(include) => {
                    let item_env = content_env(&env, &own);
                    let Some(ast::Expr::Str(path_expr)) = Some(include.source()) else {
                        cx.diag(expr.span(), "include 源暂只支持字符串路径".into());
                        continue;
                    };
                    let raw = path_expr.get().to_string();
                    let target = normalize_import(&plan.path, &raw);
                    let Some(target_mod) = package_mods.get(&target) else {
                        continue;
                    };
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Include(format!("BODY_{target_mod}")),
                        env: item_env,
                    });
                    continue;
                }
                ast::Expr::SetRule(set) => {
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Set(set),
                        env: content_env(&env, &own),
                    });
                    continue;
                }
                ast::Expr::ShowRule(show) => {
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Show(show),
                        env: content_env(&env, &own),
                    });
                    continue;
                }
                other => {
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(other),
                        env: content_env(&env, &own),
                    });
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
                    // 全局名创建期物化(对照 typst 闭包捕获)。
                    let mut g_env = env2.clone();
                    let mut names = Vec::new();
                    super::captures::collect_idents(
                        closure.body().to_untyped(),
                        &mut Vec::new(),
                        &mut names,
                    );
                    let mut g_seen = std::collections::HashSet::new();
                    for gname in names {
                        if !g_seen.insert(gname.clone()) {
                            continue;
                        }
                        if env2.module.iter().any(|(n, _)| *n == gname) {
                            continue;
                        }
                        let gslot = format!("G_{slot}_{}", sanitize_key(&gname));
                        state_fields.push(gslot.clone());
                        let gname_escaped = escape(&gname);
                        let _ = writeln!(
                            binding_code,
                            "    if let Some(v) = rt::global_opt(engine, \"{gname_escaped}\")? {{ rt::fill(&__st.{gslot}, v)?; }}"
                        );
                        g_env.module.push((
                            gname,
                            Ref::GlobalFallback {
                                slot: gslot,
                                name: gname_escaped,
                            },
                        ));
                    }
                    file_fns.push_str(&cx.gen_function(&fun, &slot, closure, &g_env));
                    file_fns.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
                    // 默认值创建期物化(对照 typst:闭包创建时逐参数求值;
                    // env2 = import + 此前绑定,不含本函数自身)。同一批
                    // 表达式进 stub 的 defaults(调用期缺失参数取值用,
                    // 对照 Closure.defaults 语义)。
                    let mut defaults = Vec::new();
                    for param in closure.params().children() {
                        if let ast::Param::Named(named) = param {
                            let pname = named.name().as_str();
                            let def_slot = format!("DEF_{slot}_{}", sanitize_key(pname));
                            state_fields.push(def_slot.clone());
                            let value = cx.expr(named.expr(), &env2);
                            defaults.push(format!(
                                "{{ let v = {value}; rt::fill(&__st.{def_slot}, v.clone())?; v }}"
                            ));
                        }
                    }
                    // 原生闭包(C 方案):签名保真 stub + 状态随值走。
                    // 不引用模块槽的纯函数保持裸 native(无状态前缀,
                    // 序言无 take_state,两边按同一 used_state 判定)。
                    let params_src = closure.params().to_untyped().full_text();
                    let defaults = if defaults.is_empty() {
                        "Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", defaults.join(", "))
                    };
                    let func_value = if cx.used_state {
                        format!(
                            "rt::native_closure(&{data}, {params_src:?}, {defaults}, &rt::StateSlot::new(__st.clone()))"
                        )
                    } else {
                        format!("Func::from(&{data})")
                    };
                    let _ = writeln!(
                        binding_code,
                        "    rt::fill(&__st.{slot}, Value::Func({func_value}))?;"
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
        let body = cx.content_seq_top(&content_items);
        let _ = writeln!(
            binding_code,
            "    rt::fill(&__st.{body_slot}, Value::Content({body}))?;"
        );

        // 文件体:file_span 锚点 + 函数体 + eval(填槽)+ module(导出面);
        // 单文件形态包进 `mod` 块,目录形态独立成文件(镜像源码树)。
        let mut body = format!(
            "\n    #[allow(non_snake_case, dead_code)]\n    fn file_span() -> Span {{\n        static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n        *CELL.get_or_init(|| {{\n            let id = typst::syntax::RootedPath::new(\n                typst::syntax::VirtualRoot::Project,\n                typst::syntax::VirtualPath::new({:?}).expect(\"path\"),\n            )\n            .intern();\n            typst::syntax::Source::new(id, String::new()).root().span()\n        }})\n    }}\n{file_fns}",
            plan.path
        );
        let _ = writeln!(
            body,
            "\n    /// `{}` 的求值:填 import 槽与自身绑定槽(模块求值的镜像)。\n\
             \x20   pub(super) fn eval(__st: &std::sync::Arc<__State>, engine: &mut Engine, context: Tracked<Context>) -> SourceResult<()> {{\n\
             \x20       let _ = (&*engine, context);\n{import_fills}{binding_code}        Ok(())\n    }}",
            plan.path
        );
        let _ = writeln!(
            body,
            "\n    /// `{}` 的模块作用域(import 抄入 + 本文件顶层绑定),从已填槽位构建。\n\
             \x20   #[allow(non_snake_case, dead_code)]\n\
             \x20   pub(super) fn module(__st: &std::sync::Arc<__State>, engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n\
             \x20       let _ = __st;\n\
             \x20       let mut scope = Scope::new();\n{file_defines}        let __fid = typst::syntax::RootedPath::new(\n\
             \x20           typst::syntax::VirtualRoot::Project,\n\
             \x20           typst::syntax::VirtualPath::new({path_repr:?}).expect(\"path\"),\n\
             \x20       )\n\
             \x20       .intern();\n\
             \x20       Ok(Module::new(\"{}\", scope).with_content(rt::read_content(&__st.{body_slot})?).with_file_id(__fid))\n    }}",
            plan.path,
            module_stem(&plan.path),
            path_repr = plan.path
        );
        // 目录形态的完整模块路径段(镜像 typ 路径,目录消歧)。
        let tree_full: Vec<String> = plan
            .path
            .strip_suffix(".typ")
            .unwrap_or(&plan.path)
            .split('/')
            .map(sanitize)
            .collect();
        let tree_path = tree_full.join("::");
        files.push(FileOut {
            body,
            flat_name: plan.mod_name.clone(),
            tree_full,
        });

        if is_entry {
            top_defines = file_defines;
            let _ = writeln!(eval_flat, "    // 入口 `{entry}`(被依赖文件已在上方求值)。");
            let _ = writeln!(eval_tree, "    // 入口 `{entry}`(被依赖文件已在上方求值)。");
        }
        let _ = writeln!(
            eval_flat,
            "    {}::eval(&__st, engine, context)?;",
            plan.mod_name
        );
        let _ = writeln!(eval_tree, "    {tree_path}::eval(&__st, engine, context)?;");
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
    let header = format!(
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
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let __st = std::sync::Arc::new(__State::default());\n{{EVAL_CALLS}}    let mut scope = Scope::new();\n{top_defines}    Ok(Module::new(\"{module_name}\", scope).with_content(rt::read_content(&__st.BODY_{entry_mod})?))\n}}\n{{ITEMS}}"
    );
    PackageParts {
        header,
        eval_flat,
        eval_tree,
        files,
    }
}

/// 单文件形态:header 的占位替换为扁平 eval 驱动与全部 `mod` 块。
/// 逐段 `push_str` 拼接(`writeln!` 格式串的 `{{}}}}` 转义易错),
/// 与重构前产物逐字节同形,是差分与快照的锁定基准。
fn render_single(parts: &PackageParts) -> String {
    let mut items = String::new();
    for f in &parts.files {
        items.push_str("\nmod ");
        items.push_str(&f.flat_name);
        items.push_str(" {\n    use super::*;\n");
        items.push_str(&f.body);
        items.push_str("}\n");
    }
    parts
        .header
        .replace("{EVAL_CALLS}", &parts.eval_flat)
        .replace("{ITEMS}", &items)
}

/// 目录形态:镜像源码树的多文件产物(相对路径 → 内容,根为 `mod.rs`)。
/// 每个 typ 文件一个 rust 文件(首行 `use super::*;`,文件体剥掉单
/// 文件 `mod` 块的一层缩进,`pub(super)` 放宽为 `pub(crate)`,扁平
/// 模块路径 `m<i>_<stem>::module` 换为树路径);`x.typ` 与 `x/` 目录
/// 并存时子模块声明并入 `x.rs` 尾部,否则目录用 `mod.rs`(声明
/// `use super::*;` + `pub mod 子;`)。glob 链靠"父的私有 use 对后代
/// 可见",与单文件形态同机制。
pub fn translate_package_tree(
    input: &PackageInput,
    module_name: &str,
) -> Result<std::collections::BTreeMap<String, String>, Vec<Diag>> {
    let (tree, diags) = tree_impl(input, module_name, false);
    if !diags.is_empty() {
        return Err(diags);
    }
    Ok(tree)
}

/// 宽松目录形态:缺口降级为占位,返回 (文件树, 诊断清单)。
pub fn translate_package_tree_lenient(
    input: &PackageInput,
    module_name: &str,
) -> (std::collections::BTreeMap<String, String>, Vec<Diag>) {
    tree_impl(input, module_name, true)
}

fn tree_impl(
    input: &PackageInput,
    module_name: &str,
    _lenient: bool,
) -> (std::collections::BTreeMap<String, String>, Vec<Diag>) {
    let mut cx = Ctx::new(_lenient);
    let parts = translate_package_parts(input, module_name, &mut cx);

    let mut tree = std::collections::BTreeMap::new();

    // 根 `mod.rs`:header(树形态 eval 驱动)+ 顶层模块声明。
    let mut root_segs: Vec<String> = Vec::new();
    for f in &parts.files {
        let seg = f.tree_full[0].clone();
        if !root_segs.contains(&seg) {
            root_segs.push(seg);
        }
    }
    let mut root_decls = String::new();
    for seg in &root_segs {
        let _ = writeln!(root_decls, "mod {seg};");
    }
    let root = parts
        .header
        .replace("{EVAL_CALLS}", &parts.eval_tree)
        .replace("{ITEMS}", &root_decls);
    tree.insert("mod.rs".to_string(), root);

    // 目录 → 直接子段(有序去重);根为空段,深度 = 段下标。
    let mut dir_subs: std::collections::BTreeMap<Vec<String>, Vec<String>> =
        std::collections::BTreeMap::new();
    for f in &parts.files {
        for depth in 0..f.tree_full.len() {
            let dir = f.tree_full[..depth].to_vec();
            let seg = f.tree_full[depth].clone();
            let subs = dir_subs.entry(dir).or_default();
            if !subs.contains(&seg) {
                subs.push(seg);
            }
        }
    }

    // typ 文件 → 镜像 `.rs`;与目录同名的文件兼作目录声明。
    let by_tree: std::collections::HashMap<Vec<String>, ()> = parts
        .files
        .iter()
        .map(|f| (f.tree_full.clone(), ()))
        .collect();
    for f in &parts.files {
        let mut body = dedent4(&f.body).replace("pub(super) fn", "pub(crate) fn");
        // body 内对其他文件的引用(`{flat}::module(`,import 填充与
        // 模块路径表达式)换为树路径;扁平名唯一,前缀替换无歧义。
        for g in &parts.files {
            if g.flat_name != f.flat_name {
                body = body.replace(
                    &format!("{}::module(", g.flat_name),
                    &format!("{}::module(", g.tree_full.join("::")),
                );
            }
        }
        if let Some(subs) = dir_subs.get(&f.tree_full) {
            let mut decls = String::new();
            for seg in subs {
                let _ = writeln!(decls, "pub mod {seg};");
            }
            body.push_str(&decls);
        }
        // rust 文件名用 sanitize 段(与模块声明一致:`mark-shapes.typ`
        // → `mark_shapes.rs`)。
        let rel = format!("{}.rs", f.tree_full.join("/"));
        tree.insert(rel, format!("use super::*;\n{body}"));
    }
    // 无同名文件的目录:`dir/mod.rs`。
    for (dir, subs) in &dir_subs {
        if dir.is_empty() || by_tree.contains_key(dir) {
            continue;
        }
        let mut content = String::from("use super::*;\n");
        for seg in subs {
            let _ = writeln!(content, "pub mod {seg};");
        }
        tree.insert(format!("{}/mod.rs", dir.join("/")), content);
    }
    (tree, cx.diags)
}

/// 目录形态剥掉单文件 `mod` 块的一层缩进(每行前导 4 空格,不足则原样)。
fn dedent4(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        out.push_str(line.strip_prefix("    ").unwrap_or(line));
    }
    out
}

/// 从包 spec 取短名(`@preview/oxifmt:1.0.0` → `oxifmt`)。
pub(super) fn spec_name(spec: &str) -> String {
    spec.split('/')
        .nth(1)
        .unwrap_or("pkg")
        .split(':')
        .next()
        .unwrap_or("pkg")
        .to_string()
}
