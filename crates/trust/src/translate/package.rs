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
    topo_order, spec_name, spec_prefix, resolve_import_slot_file};

/// 待翻译的多文件包。
pub struct PackageInput {
    /// 相对路径(POSIX 风格,相对包根)→ 源码。递归依赖形态下,
    /// 被翻译的依赖包文件以 `{spec}/{包内相对路径}` 为键并入
    /// (spec 形如 `@preview/cetz:0.3.4`;与文件内路径规范化兼容,
    /// mod 名带序号前缀天然无槽名冲突)。
    pub files: std::collections::BTreeMap<String, String>,
    /// 入口文件相对路径。
    pub entry: String,
    /// 已并入翻译的外部包:spec → 其入口的合并键(`{spec}/{entry}`)。
    /// 命中这些 spec 的 import 边**静态化**(求值期直接装配对应生成
    /// 模块,不再运行时 import_external 解释);未命中的保持运行时
    /// 回退(混合执行)。
    pub dep_entries: std::collections::BTreeMap<String, String>,
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
    registry_flat: String,
    registry_tree: String,
    files: Vec<FileOut>,
    /// C2 值位置闭包的原生体汇集(去重后跨文件共用,发射到产物根部)。
    closure_items: String,
    /// 每调用点节点静态(零锁创建协议,发射到根)。
    closure_nodes: String,
}

/// 宽松翻译(探索用):缺口降级为占位,返回 (代码, 诊断清单)。
pub fn translate_package_lenient(input: &PackageInput, module_name: &str) -> (String, Vec<Diag>) {
    let mut cx = Ctx::new(true);
    let mut parts = translate_package_parts(input, module_name, &mut cx);
    (render_single(&parts), cx.diags)
}

pub fn translate_package(input: &PackageInput, module_name: &str) -> Result<String, Vec<Diag>> {
    let mut cx = Ctx::new(false);
    let mut parts = translate_package_parts(input, module_name, &mut cx);
    if !cx.diags.is_empty() {
        return Err(cx.diags);
    }
    Ok(render_single(&parts))
}


// ─── 分阶段装配(对照 typst-eval `eval()` 的目录式入口:每步全部
// 委托,读完入口即知全流程)────────────────────────────────

/// 解析:每文件 `Source`(span 携带 FileId,与生成物 file_span 同源)。
fn parse_sources(input: &PackageInput) -> Vec<(String, typst_syntax::Source)> {
    let mut roots: Vec<(String, typst_syntax::Source)> = Vec::new();
    for (path, src) in input.files.iter() {
        let id = typst_syntax::RootedPath::new(
            typst_syntax::VirtualRoot::Project,
            typst_syntax::VirtualPath::new(path.as_str()).expect("path"),
        )
        .intern();
        roots.push((path.clone(), typst_syntax::Source::new(id, src.clone())));
    }
    roots
}

/// 第一遍:每个文件的计划(import 边/顶层绑定槽/解析诊断/可变根)。
/// markup 取自 `Box::leak` 的静态根,计划全 'static。
fn plan_files(
    input: &PackageInput,
    roots: &[(String, typst_syntax::Source)],
    cx: &mut Ctx,
) -> Vec<FilePlan<'static>> {
    let mut plans: Vec<FilePlan> = Vec::new();
    for (idx, (path, source)) in roots.iter().enumerate() {
        let root: &'static typst_syntax::SyntaxNode = Box::leak(source.root().clone().into());
        let Some(markup) = root.cast::<ast::Markup>() else {
            cx.diag(
                typst_syntax::Span::detached(),
                format!("{path} 未能解析为 markup 模块"),
            );
            continue;
        };
        let mut mutated_roots = Vec::new();
        collect_mutated_roots(source.root(), &mut mutated_roots);
        let parse_diags = source
            .root()
            .errors_and_warnings()
            .0
            .into_iter()
            .map(|d| d.message.to_string())
            .collect::<Vec<_>>();
        let stem = path
            .rsplit('/')
            .next()
            .unwrap_or("f")
            .split('.')
            .next()
            .unwrap_or("f");
        // 复合模块名(槽前缀/路径引用):sanitize_key——裸 sanitize 对
            // 关键字产出 r# 前缀,嵌在复合里是词法错(hy-dro-gen 的
            // mod.typ → m21_r#mod,touying 驱动)。
            let mod_name = format!("m{idx}_{}", sanitize_key(stem));
        let mut tops = Vec::new();
        let mut imports = Vec::new();
        for expr in markup.exprs() {
            match expr {
                ast::Expr::Space(_) | ast::Expr::Parbreak(_) => {}
                // (其内的 <Label> 等标记不是代码)。
                ast::Expr::ModuleImport(import) => {
                    let mut source_node: Option<typst_syntax::SyntaxNode> = None;
                    let (raw, external, module_access) = match import.source() {
                        // 字符串路径:外部包或包内相对/绝对路径。
                        ast::Expr::Str(path_expr) => {
                            let raw = path_expr.get().to_string();
                            let external = raw.starts_with('@');
                            (raw, external, None)
                        }
                        // 模块值表达式 import(`import deps.oxifmt: x`)。
                        // 其余任意表达式(如 `import plugin("…")`,
                        // touying/hy-dro-gen 驱动)存源节点,二遍求值
                        // 为模块值再绑定(对照 typst-eval:源求值为模块
                        // 即可 import)。
                        other => match parse_module_access(other) {
                            Some(access) => (String::new(), false, Some(access)),
                            None => {
                                source_node = Some(other.to_untyped().clone());
                                (String::new(), false, None)
                            }
                        },
                    };
                    let target = if module_access.is_some() {
                        String::new()
                    } else if external {
                        // 外部包:已并入翻译的依赖指向其入口合并键
                        // (依赖图/求值序随之打通);否则保持原始 spec
                        // (运行时 import_external 解释)。
                        input
                            .dep_entries
                            .get(&raw)
                            .cloned()
                            .unwrap_or_else(|| raw.clone())
                    } else {
                        let mut target = normalize_import(path, &raw);
                        // 依赖包内的**绝对** import(`/src/x.typ`)限定在
                        // 该包的合并键空间内(相对 import 经 dir 拼接
                        // 天然带前缀;绝对 import 会落到顶层键,丢失
                        // 包归属)。
                        if raw.starts_with('/') && let Some(colon) = path.find(':')
                            && let Some(rel) = path[colon..].find('/')
                        {
                            let prefix_end = colon + rel;
                            if path.starts_with('@') && prefix_end < path.len() {
                                target = format!(
                                    "{}/{}",
                                    &path[..prefix_end],
                                    target.trim_start_matches('/')
                                );
                            }
                        }
                        target
                    };
                    if module_access.is_none() && source_node.is_none() && !external
                        && !input.files.contains_key(&target)
                    {
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
                        // 模块改名 + 成员列表:双绑定(elembic 驱动,
                        // `import "base.typ" as base: ok, err`)。
                        Some(ast::Imports::Items(items)) => match import.new_name() {
                            Some(new_name) => ImportItems::RenameList(
                                new_name.as_str().to_string(),
                                items
                                    .iter()
                                    .map(|item| {
                                        let original = item
                                            .path()
                                            .iter()
                                            .map(|seg| seg.as_str())
                                            .collect::<Vec<_>>()
                                            .join(".");
                                        let bound = match item {
                                            ast::ImportItem::Simple(_) => original
                                                .rsplit('.')
                                                .next()
                                                .unwrap_or(&original)
                                                .to_string(),
                                            ast::ImportItem::Renamed(renamed) => {
                                                renamed.new_name().as_str().to_string()
                                            }
                                        };
                                        (original, Some(bound))
                                    })
                                    .collect(),
                            ),
                            None => ImportItems::List(
                                items
                                    .iter()
                                    .map(|item| {
                                        // 完整名字含点号路径(`a.b as c`):
                                        // original_name() 只给末段,这里按
                                        // 段连出全名(hydra 驱动);未改名时
                                        // 绑定名= 末段(typst 语义)。
                                        let original = item
                                            .path()
                                            .iter()
                                            .map(|seg| seg.as_str())
                                            .collect::<Vec<_>>()
                                            .join(".");
                                        let bound = match item {
                                            ast::ImportItem::Simple(_) => original
                                                .rsplit('.')
                                                .next()
                                                .unwrap_or(&original)
                                                .to_string(),
                                            ast::ImportItem::Renamed(renamed) => {
                                                renamed.new_name().as_str().to_string()
                                            }
                                        };
                                        (original, Some(bound))
                                    })
                                    .collect(),
                            ),
                        },
                    };
                    imports.push(ImportEdge {
                        span: expr.span(),
                        target,
                        items,
                        external,
                        module_access,
                        source_node,
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
                        source_node: None,
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
                        tops.push((name.clone(), format!("V{id}_{}", sanitize_key(&name))));
                    }
                }
                // 其余顶层表达式(文本/嵌入/set/show)是模块 body
                // 内容,由第二遍收集折叠;此处无需处理。
                _ => {}
            }
        }
        plans.push(FilePlan {
            parse_diags,
            mutated_roots,
            path: path.clone(),
            mod_name,
            markup,
            tops,
            imports,
        });
    }
    plans
}

/// 包世界装配产物:求值拓扑序、导出查找表、路径→mod 名表。
struct PkgWorld {
    order: Vec<usize>,
    export_table: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    package_mods: std::rc::Rc<std::collections::BTreeMap<String, String>>,
}

/// 世界装配:环检测(None = 成环)→ 拓扑序/导出表 → cx 侧的
/// files/mods/export/slot_files 注册与可变槽标记。
fn assemble_package_world(
    input: &PackageInput,
    plans: &[FilePlan],
    entry: &str,
    cx: &mut Ctx,
) -> Option<PkgWorld> {
    // import 环检测(typst 模块循环求值同样报错)。
    if has_import_cycle(plans, entry) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: "包内 import 成环(typst 运行时同样会报循环求值)".into(),
            file: None,
        });
        return None;
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
    let order = topo_order(plans);
    let export_table = build_export_table(plans, &order, &input.dep_entries);
        cx.export_table = Some(std::rc::Rc::new(export_table.clone()));

    // 可变槽:被就地修改的根名并集,标记各计划自身定义的同名槽
    // (对照 typst 顶层绑定可就地修改的语义;跨文件导入名不改本槽)。
    {
        let roots: std::collections::HashSet<String> = plans
            .iter()
            .flat_map(|p| p.mutated_roots.iter().cloned())
            .collect();
        for p in plans {
            for (name, slot) in &p.tops {
                if roots.contains(name) {
                    cx.mutable_slots.insert(slot.clone());
                }
            }
        }
    }

    // 槽名→文件索引(第二遍开始前一次性构建,体内模块值链通配用):
    // 槽名→文件索引(一次性构建,体内模块值链通配用):
    // V{i}_{name} → plans[i].path;IMPORT_{mod}_{bound} → 边目标。
    if cx.slot_files.is_none() {
    let mut idx = std::collections::BTreeMap::new();
    for (i, p) in plans.iter().enumerate() {
    for (name, slot) in &p.tops {
    let _ = slot;
    idx.insert(
    format!("V{i}_{}", sanitize_key(name)),
    p.path.clone(),
    );
    }
    for edge in &p.imports {
    if edge.is_include {
    continue;
    }
    let bound = if edge.external {
    match &edge.items {
    ImportItems::Rename(n) | ImportItems::RenameList(n, _) => {
    n.clone()
    }
    _ => spec_name(&edge.target),
    }
    } else {
    match &edge.items {
    ImportItems::Bare => module_stem_pub(&edge.target),
    ImportItems::Rename(n) | ImportItems::RenameList(n, _) => {
    n.clone()
    }
    _ => continue,
    }
    };
    idx.insert(
    format!("IMPORT_{}_{}", p.mod_name, bound),
    edge.target.clone(),
    );
    }
    }
    // 回填:LIST/RenameList 项槽与跨文件链式身份,经通用解析器
    // (finite 的 `import t4t: *`,t4t 为内部 list 项引入的模块)。
    for p in plans.iter() {
        for edge in &p.imports {
            if edge.is_include {
                continue;
            }
            let names: Vec<String> = match &edge.items {
                ImportItems::List(list) => list
                    .iter()
                    .map(|(item, rename)| {
                        rename.clone().unwrap_or_else(|| item.clone())
                    })
                    .collect(),
                ImportItems::RenameList(n, list) => {
                    let mut v = vec![n.clone()];
                    v.extend(
                        list.iter()
                            .map(|(i, r)| r.clone().unwrap_or_else(|| i.clone())),
                    );
                    v
                }
                _ => continue,
            };
            for name in names {
                let slot = format!(
                    "IMPORT_{}_{}",
                    sanitize_key(&p.mod_name),
                    sanitize_key(&name)
                );
                if !idx.contains_key(&slot)
                    && let Some(file) = resolve_import_slot_file(
                        &slot,
                        &plans,
                        &export_table,
                        &input.dep_entries,
                        plans.len(),
                    )
                {
                    idx.insert(slot, file);
                }
            }
        }
    }
    cx.slot_files = Some(std::rc::Rc::new(idx));
    }
    Some(PkgWorld { order, export_table, package_mods })
}

/// 第二遍单文件累积器(Vm 等价物:一文件一束,顶层语句生成共享;
/// package 级共享的 `state_fields` 由调用方传参)。
struct TopGen {
    /// 当前可见 env(import 注入与绑定可见性按源序演进)。
    env: Env,
    /// 本文件已处理的顶层绑定(名,槽)。
    own: Vec<(String, String)>,
    /// 同名绑定的第 n 次出现(解构多名配对槽用)。
    own_seen: std::collections::HashMap<String, usize>,
    /// 求值序绑定代码(eval 体内)。
    binding_code: String,
    /// 导出面 define 序列(module 体内)。
    file_defines: String,
    /// 函数体与静态汇集(文件 body)。
    file_fns: String,
    /// import 槽填充序列(eval 体头部)。
    import_fills: String,
}

impl TopGen {
    fn new() -> Self {
        Self {
            env: Env { locals: Vec::new(), module: Vec::new(), wildcards: Vec::new() },
            own: Vec::new(),
            own_seen: std::collections::HashMap::new(),
            binding_code: String::new(),
            file_defines: String::new(),
            file_fns: String::new(),
            import_fills: String::new(),
        }
    }

    /// 内容项的可见 env(当前 env + 此前顶层绑定)。
    fn content_env(&self) -> Env {
        let mut e = self.env.clone();
        for (seen, seen_slot) in &self.own {
            e.module.push((seen.clone(), Ref::Slot(seen_slot.clone())));
        }
        e
    }

    /// 顶层 `#let`(函数/闭包/普通绑定/解构):计划槽配对、全局名
    /// 创建期物化、绑定代码与导出面写入。对照 eval 的 LetBinding 分支。
    fn gen_let(
        &mut self,
        cx: &mut Ctx,
        plan: &super::plan::FilePlan,
        state_fields: &mut Vec<String>,
        lets: ast::LetBinding,
    ) {
            match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    let Some(ast::Expr::Closure(closure)) = lets.init() else {
                        cx.diag(lets.span(), "闭包绑定缺少闭包初始化".into());
return;
                    };
                    let nth = self.own_seen.get(name.as_str()).copied().unwrap_or(0);
                    self.own_seen.insert(name.as_str().to_string(), nth + 1);
                    let Some(slot) = nth_slot(plan, name.as_str(), nth) else {
return;
                    };
                    let mut env2 = self.env.clone();
                    for (seen, seen_slot) in &self.own {
                        env2.module
                            .push((seen.clone(), Ref::Slot(seen_slot.clone())));
                    }
                    env2.module
                        .push((name.as_str().to_string(), Ref::Slot(slot.clone())));
                    let fun = format!("f_{}", lowercase(&slot));
                    let data = format!("D_{}", uppercase(&slot));
                    // 全局名创建期物化(对照 typst 闭包捕获)。先注入
                    // 闭包自身绑定(名字/参数/sink)——参数不是自由名
                    // (codly 驱动:`__merge(base, …)` 的 base 曾被收成
                    // 全局槽;对照 CapturesVisitor 的 Closure 分支)。
                    let mut g_env = env2.clone();
                    let mut names = Vec::new();
                    let mut g_bound: Vec<String> = Vec::new();
                    if let Some(cname) = closure.name() {
                        g_bound.push(cname.as_str().to_string());
                    }
                    for param in closure.params().children() {
                        match param {
                            ast::Param::Pos(pattern) => g_bound
                                .extend(pattern.bindings().into_iter().map(|i| i.as_str().to_string())),
                            ast::Param::Named(named) => {
                                g_bound.push(named.name().as_str().to_string())
                            }
                            ast::Param::Spread(spread) => {
                                if let Some(ident) = spread.sink_ident() {
                                    g_bound.push(ident.as_str().to_string());
                                }
                            }
                        }
                    }
                    super::captures::collect_idents(
                        closure.body().to_untyped(),
                        &mut g_bound,
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
                            self.binding_code,
                            "    if let Some(v) = rt::capture_opt(engine, \"{gname_escaped}\")? {{ rt::fill(&__st.{gslot}, v)?; }}"
                        );
                        g_env.module.push((
                            gname,
                            Ref::GlobalFallback {
                                slot: gslot,
                                name: gname_escaped,
                            },
                        ));
                    }
                    self.file_fns.push_str(&cx.gen_function(&fun, &slot, closure, &mut g_env));
                    self.file_fns.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
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
                            let value = cx.expr(named.expr(), &mut env2);
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
                        self.binding_code,
                        "    rt::fill(&__st.{slot}, Value::Func({func_value}))?;"
                    );
                    let _ = writeln!(
                        self.file_defines,
                        "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                        escape(name.as_str())
                    );
                    self.own.push((name.as_str().to_string(), slot.clone()));
                }
                ast::LetBindingKind::Normal(pattern) => {
                    // 无初值声明(`#let x`):绑定 none(对照 typst-eval
                    // LetBinding 无 init 的 define(name, None))。
                    let Some(init_expr) = lets.init().map(|e| e) else {
                        if let Some(name) = pattern.bindings().first() {
                            let nth = self.own_seen.get(name.as_str()).copied().unwrap_or(0);
                            self.own_seen.insert(name.as_str().to_string(), nth + 1);
                            if let Some(slot) = nth_slot(plan, name.as_str(), nth) {
                                let fill_none = if cx.mutable_slots.contains(&slot) {
                                    format!("rt::fill_ms(&__st.{slot}, Value::None)?")
                                } else {
                                    format!("rt::fill(&__st.{slot}, Value::None)?")
                                };
                                let _ = writeln!(self.binding_code, "    {fill_none};");
                                let _ = writeln!(
                                    self.file_defines,
                                    "    scope.define(\"{}\", Value::None);",
                                    escape(name.as_str())
                                );
                                self.own.push((name.as_str().to_string(), slot));
                            }
                        }
return;
                    };
                    // 每个模式名字配对本文件的下一个同名槽(解构多名)。
                    let mut bound: Vec<(String, String)> = Vec::new();
                    for ident in pattern.bindings().iter() {
                        let name = ident.as_str();
                        let nth = self.own_seen.get(name).copied().unwrap_or(0);
                        self.own_seen.insert(name.to_string(), nth + 1);
                        let Some(slot) = nth_slot(plan, name, nth) else {
return;
                        };
                        bound.push((name.to_string(), slot));
                    }
                    if bound.is_empty() {
return;
                    }
                    let mut env2 = self.env.clone();
                    for (seen, seen_slot) in &self.own {
                        env2.module
                            .push((seen.clone(), Ref::Slot(seen_slot.clone())));
                    }
                    // init 在绑定生效前求值:同名引用解析到外层(如全局
                    // 类型),env2 不含本绑定。单名快速路径只适用于
                    // **纯标识符模式**——`(a,)` 这类单绑定解构模式仍须
                    // 走解构(整值绑定曾是 destructuring-let 单元数组
                    // 与字典 sink 族的根因)。
                    let plain_ident = matches!(
                        pattern,
                        ast::Pattern::Normal(ast::Expr::Ident(_))
                            | ast::Pattern::Parenthesized(_)
                    );
                    if let [(name, slot)] = bound.as_slice() && plain_ident {
                        let value = cx.expr(init_expr, &mut env2);
                        let fill_call = if cx.mutable_slots.contains(slot) {
                            format!("rt::fill_ms(&__st.{slot}, v.clone())?")
                        } else {
                            format!("rt::fill(&__st.{slot}, v.clone())?")
                        };
                        let read_call = if cx.mutable_slots.contains(slot) {
                            format!("rt::read_ms(&__st.{slot})?")
                        } else {
                            format!("rt::read(&__st.{slot})?")
                        };
                        let _ = writeln!(
                            self.binding_code,
                            "    {{ let v = {value}; {fill_call}; }}"
                        );
                        let _ = writeln!(
                            self.file_defines,
                            "    scope.define(\"{}\", {read_call});",
                            escape(name)
                        );
                    } else {
                        // 顶层解构:槽名预绑为临时局部,destructure 写回
                        // 这些名字,再逐槽填充注册。
                        let value = cx.expr(init_expr, &mut env2);
                        let mut tmp_env = env2.clone();
                        let mut decls = String::new();
                        for (name, slot) in &bound {
                            tmp_env.locals.push((name.clone(), slot.clone()));
                            let _ = writeln!(decls, "        let mut {slot} = Value::None;");
                        }
                        let stmts =
                            cx.destructure(pattern, "v.clone()".to_string(), &mut tmp_env, true);
                        let mut fills = String::new();
                        for (name, slot) in &bound {
                            // 可变槽(fill_ms/read_ms)与写一次槽(fill/read)
                            // 按计划期标记分流。
                            let (fill_call, read_call) =
                                if cx.mutable_slots.contains(slot) {
                                    (
                                        format!("rt::fill_ms(&__st.{slot}, {slot}.clone())?;"),
                                        format!("rt::read_ms(&__st.{slot})?"),
                                    )
                                } else {
                                    (
                                        format!("rt::fill(&__st.{slot}, {slot}.clone())?;"),
                                        format!("rt::read(&__st.{slot})?"),
                                    )
                                };
                            let _ = writeln!(fills, "        {fill_call}");
                            let _ = writeln!(
                                self.file_defines,
                                "    scope.define(\"{}\", {read_call});",
                                escape(name)
                            );
                        }
                        let _ = writeln!(
                            self.binding_code,
                            "    {{ let v = {value};\n{decls}{stmts}{fills}    }}"
                        );
                    }
                    self.own.extend(bound);
                }
            }
    }
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

    // 解析:每文件 Source(span 带 FileId,与生成物 file_span 同源)。
    let roots = parse_sources(input);
    // 第一遍:每个文件的计划(import 边/顶层绑定槽/解析诊断/可变根)。
    // 注意:计划期直接使用形参 cx(此前误建局部同名 Ctx 遮蔽形参,
    // 导致包模式的诊断从未传回壳层,严格性失效)。
    let plans = plan_files(input, &roots, cx);
    if !cx.diags.is_empty() && !cx.lenient {
        // 壳层负责返回 Err。
        return PackageParts::default();
    }
    // 世界装配:环检测 → 拓扑序/导出表 → 可变槽标记 → 槽→文件表。
    let Some(world) = assemble_package_world(input, &plans, entry, cx) else {
        return PackageParts::default();
    };
    let PkgWorld { order, export_table, package_mods } = world;
    // 第二遍:逐文件生成 eval(填槽)+ module(导出面)+ 函数体。
    let mut top_defines = String::new();
    let mut state_fields: Vec<String> = Vec::new();
    let mut files: Vec<FileOut> = Vec::new();
    let mut eval_flat = String::new();
    let mut eval_tree = String::new();
    let mut registry_flat = String::new();
    let mut registry_tree = String::new();
    for &idx in &order {
        let plan = &plans[idx];
        cx.current_file = Some(plan.path.clone());
        let is_entry = plan.path == entry;
        // 单文件累积器(env 按源序演进;import 槽表与边游标本文件局部)。
        let mut top = TopGen::new();
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // 顶层 import 边不再整体前置:由下方源序流水在语句位置处理
        //(见 process_import_edge 与 ModuleImport 臂的 edge 游标)。
        let mut edge_cursor = 0usize;
        let mut content_items: Vec<super::expr::TopItem> = Vec::new();
        for expr in plan.markup.exprs() {
            // 内容项:顶层文本/嵌入表达式/set/show/include(逐项可见性)。
            match expr {
                ast::Expr::ModuleImport(_) => {
                    // 源序流水:在此语句位置处理对应 import 边(env 注入
                    // 即时生效——此后的 let 才能看见它;此前的 let 仍
                    // 解析到外层,对照原生逐语句求值)。
                    if let Some(edge) = plan.imports.get(edge_cursor) {
                        edge_cursor += 1;
                        cx.process_import_edge(
                            edge,
                            plan,
                            input,
                            &plans,
                            &export_table,
                            &package_mods,
                            &mut top.env,
                            &mut state_fields,
                            &mut top.import_fills,
                            &mut top.file_defines,
                            &mut import_slots,
                        );
                    }
                    // 对照 eval_markup:语句并入一个空 display 槽。
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(expr),
                        env: top.content_env(),
                    });
                }
                ast::Expr::LetBinding(lets) => {
                    // 对照 eval_markup:语句并入一个空 display 槽。
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(expr),
                        env: top.content_env(),
                    });
                    top.gen_let(cx, plan, &mut state_fields, lets);
                }
                ast::Expr::ModuleInclude(include) => {
                    let item_env = top.content_env();
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
                        env: top.content_env(),
                    });
                    continue;
                }
                ast::Expr::ShowRule(show) => {
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Show(show),
                        env: top.content_env(),
                    });
                    continue;
                }
                other => {
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(other),
                        env: top.content_env(),
                    });
                    continue;
                }
            }
        }
        // 累积器移出:文件装配段按原局部名消费。
        let TopGen {
            env,
            own,
            mut binding_code,
            file_defines,
            file_fns,
            import_fills,
            ..
        } = top;

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
            &mut binding_code,
            "    rt::fill(&__st.{body_slot}, Value::Content({body}))?;"
        );

        // 文件体:file_span 锚点 + 函数体 + eval(填槽)+ module(导出面);
        // 单文件形态包进 `mod` 块,目录形态独立成文件(镜像源码树)。
        let mut body = format!(
            "\n    #[allow(non_snake_case, dead_code)]\n    pub(crate) fn file_span() -> Span {{\n        static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();\n        *CELL.get_or_init(|| {{\n            let id = typst::syntax::RootedPath::new(\n                typst::syntax::VirtualRoot::Project,\n                typst::syntax::VirtualPath::new({:?}).expect(\"path\"),\n            )\n            .intern();\n            typst::syntax::Source::new(id, String::new()).root().span()\n        }})\n    }}\n{file_fns}",
            plan.path
        );
        // 解析诊断(对照官方 compile parse 阶段):首条错误即 bail;
        // 多条时信息集合口径的差由错误测试自然承载。
        let parse_guard = if plan.parse_diags.is_empty() {
            String::new()
        } else {
            let msgs: Vec<String> = plan
                .parse_diags
                .iter()
                .map(|m| format!("{m:?}"))
                .collect();
            // 多条解析诊断一次带全(Err 携 EcoVec;对照官方 compile
            // 的 parse 阶段收集行为——单条 bail 曾让多错误测试失配)。
            format!(
                "        return Err(eco_vec![{}]);\n",
                msgs.iter()
                    .map(|m| format!("typst::diag::SourceDiagnostic::error(file_span(), ecow::eco_format!(\"{{}}\", {m}))"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        let _ = writeln!(
            body,
            "\n    /// `{}` 的求值:填 import 槽与自身绑定槽(模块求值的镜像)。\n\
             \x20   pub(super) fn eval(__st: &std::sync::Arc<__State>, engine: &mut Engine, context: Tracked<Context>) -> SourceResult<()> {{\n\
             \x20       let _ = (&*engine, context);\n{parse_guard}{import_fills}{binding_code}        Ok(())\n    }}",
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
            // 树段名兼作文件名与模块声明:`r#` 前缀会进文件名(非法)
            // 与 mod 声明/路径不一致——用 sanitize_key(剥 r# 加 _kw)。
            .map(sanitize_key)
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
        // 按路径装配臂:依赖闭包拓扑序求值 + 自身求值与装配(fresh
        // 槽板,单测试只装配该文件——与解释器单文件编译同阶)。
        let closure = dep_closure(&plans, idx);
        let mut arm_flat = format!("        \"{}\" => Some((|| {{\n", plan.path);
        let mut arm_tree = arm_flat.clone();
        for &j in &closure {
            let _ = writeln!(
                arm_flat,
                "            {}::eval(board, engine, context)?;",
                plans[j].mod_name
            );
            let dep_tree = plans[j]
                .path
                .strip_suffix(".typ")
                .unwrap_or(&plans[j].path)
                .split('/')
                .map(sanitize_key)
                .collect::<Vec<_>>()
                .join("::");
            let _ = writeln!(arm_tree, "            {dep_tree}::eval(board, engine, context)?;");
        }
        let _ = writeln!(
            arm_flat,
            "            {}::module(board, engine, context)\n        }})()),",
            plan.mod_name
        );
        let _ = writeln!(
            arm_tree,
            "            {tree_path}::module(board, engine, context)\n        }})()),"
        );
        registry_flat.push_str(&arm_flat);
        registry_tree.push_str(&arm_tree);
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
    // 去重:源面的重复参数名(如 `f(v: 1, v: 2)` 的错误测试)会对
    // 同名默认槽登记两次,重复字段让生成物无法编译(scripting-closure
    // 驱动);重复参数的语义分歧由该错误测试自然承载。
    let mut seen_fields = std::collections::BTreeSet::new();
    for slot in &state_fields {
        if seen_fields.insert(slot.clone()) {
            let ty = if cx.mutable_slots.contains(slot) {
                "std::sync::Mutex<Value>"
            } else {
                "std::sync::OnceLock<Value>"
            };
            let _ = writeln!(fields, "    {slot}: {ty},");
        }
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
         pub struct __State {{\n{fields}}}\n\n\
         /// 包求值的镜像:宿主每次构建(World)调用一次,按拓扑序求值各文件。\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let __st = std::sync::Arc::new(__State::default());\n{{EVAL_CALLS}}    let mut scope = Scope::new();\n{top_defines}    Ok(Module::new(\"{module_name}\", scope).with_content(rt::read_content(&__st.BODY_{entry_mod})?))\n}}\n\n\
         /// 一行接入:驱动 module_init 求值并返回模块(注入宿主 library 用\n\
         /// `rt::bind_module`;测试世界 InjectWorld::init_native_module 已内置)。\n\
         pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {{\n    rt::init_module(world, library, module_init)\n}}\n\n\
         /// 新建求值槽板(批驱动自管生命周期时用)。\n\
         pub fn new_state() -> std::sync::Arc<__State> {{ std::sync::Arc::new(__State::default()) }}\n\n\
         /// 按文件路径按需装配:在给定槽板上先按拓扑序求值该文件的\n\
         /// 依赖闭包,再求值自身并装配模块。官方 tests/suite 批驱动:\n\
         /// 每测试 fresh 槽板 + 单文件装配,全程只构建**一个**二进制\n\
         /// (翻译产物整体一次编译,编译次数与测试数无关)。\n\
         pub fn module_by_path(path: &str, board: &std::sync::Arc<__State>, engine: &mut Engine, context: Tracked<Context>) -> Option<SourceResult<Module>> {{\n    match path {{\n{{REGISTRY}}        _ => None,\n    }}\n}}\n{{ITEMS}}"
    );
    PackageParts {
        header,
        eval_flat,
        eval_tree,
        registry_flat,
        registry_tree,
        files,
        closure_items: std::mem::take(&mut cx.closure_items),
        closure_nodes: std::mem::take(&mut cx.closure_nodes),
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
    // C2 闭包原生体与每调用点节点静态(根级,可见 __State 与导入)。
    items.push_str(&parts.closure_items);
    items.push_str(&parts.closure_nodes);
    parts
        .header
        .replace("{EVAL_CALLS}", &parts.eval_flat)
        .replace("{REGISTRY}", &parts.registry_flat)
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
    let mut parts = translate_package_parts(input, module_name, &mut cx);

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
    let mut root = parts
        .header
        .replace("{EVAL_CALLS}", &parts.eval_tree)
        .replace("{REGISTRY}", &parts.registry_tree)
        .replace("{ITEMS}", &root_decls);
    let cx_nodes = std::mem::take(&mut parts.closure_nodes);
    // C2 闭包段与文件体同款:体内对其他文件的扁平模块引用
    // (`{flat}::module(` 与提升体限定的 `{flat}::file_span(`)
    // 换为树路径。
    let mut closure_items = parts.closure_items.clone();
    for g in &parts.files {
        closure_items = closure_items.replace(
            &format!("{}::module(", g.flat_name),
            &format!("{}::module(", g.tree_full.join("::")),
        );
        closure_items = closure_items.replace(
            &format!("{}::file_span(", g.flat_name),
            &format!("{}::file_span(", g.tree_full.join("::")),
        );
    }
    root.push_str(&closure_items);
    root.push_str(&cx_nodes);
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



/// 收集被就地修改的绑定根名:可变方法调用(push/pop/insert/remove)
/// 与赋值/复合赋值的目标根标识符(对照 typst 的 access 可变语义——
/// 模块绑定同样可就地修改)。
pub(super) fn collect_mutated_roots(node: &typst_syntax::SyntaxNode, out: &mut Vec<String>) {
    use typst_syntax::SyntaxKind as K;
    if let Some(fc) = node.cast::<typst_syntax::ast::FuncCall>()
        && let typst_syntax::ast::Expr::FieldAccess(access) = fc.callee()
        && matches!(access.field().as_str(), "push" | "pop" | "insert" | "remove")
    {
        if let Some(root) = place_root(access.target().to_untyped()) {
            out.push(root);
        }
    }
    if let Some(bin) = node.cast::<typst_syntax::ast::Binary>()
        && matches!(
            bin.op(),
            typst_syntax::ast::BinOp::Assign
                | typst_syntax::ast::BinOp::AddAssign
                | typst_syntax::ast::BinOp::SubAssign
                | typst_syntax::ast::BinOp::MulAssign
                | typst_syntax::ast::BinOp::DivAssign
        )
        && let Some(root) = place_root(bin.lhs().to_untyped())
    {
        out.push(root);
    }
    // 解构赋值 `(a, b, ..c) = ...` / `(a.at(i),) = ...`:模式内全部
    // 可放叶子的根(对照 binding.rs DestructAssignment 叶子走 access)。
    if let Some(assign) = node.cast::<typst_syntax::ast::DestructAssignment>() {
        collect_pattern_roots(assign.pattern().to_untyped(), out);
    }
    for child in node.children() {
        collect_mutated_roots(child, out);
    }
}

/// 解构模式的可放叶子根收集(叶子 = Ident 或以 Ident 根的
/// 字段/accessor 链;重复无害,消费侧为集合)。
fn collect_pattern_roots(node: &typst_syntax::SyntaxNode, out: &mut Vec<String>) {
    if let Some(root) = place_root(node) {
        out.push(root);
    }
    for child in node.children() {
        collect_pattern_roots(child, out);
    }
}

/// 可放路径的根名(Ident 或以 Ident 开头的字段/accessor 链
/// ——含 `at`/`first`/`last` 存取器调用;其余形态 None)。
fn place_root(node: &typst_syntax::SyntaxNode) -> Option<String> {
    use typst_syntax::ast::AstNode;
    if let Some(ident) = node.cast::<typst_syntax::ast::Ident>() {
        return Some(ident.as_str().to_string());
    }
    if let Some(access) = node.cast::<typst_syntax::ast::FieldAccess>() {
        return place_root(access.target().to_untyped());
    }
    if let Some(paren) = node.cast::<typst_syntax::ast::Parenthesized>() {
        return place_root(paren.expr().to_untyped());
    }
    if let Some(call) = node.cast::<typst_syntax::ast::FuncCall>()
        && let typst_syntax::ast::Expr::FieldAccess(access) = call.callee()
        && matches!(access.field().as_str(), "at" | "first" | "last")
    {
        return place_root(access.target().to_untyped());
    }
    None
}

/// 文件的依赖闭包(含自身),依赖在前(与 topo_order 的访问一致,
/// include 边亦为依赖)。
fn dep_closure(plans: &[super::plan::FilePlan], idx: usize) -> Vec<usize> {
    fn visit(
        i: usize,
        plans: &[super::plan::FilePlan],
        order: &mut Vec<usize>,
        state: &mut Vec<u8>,
    ) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for edge in &plans[i].imports {
            if let Some(j) = plans.iter().position(|p| p.path == edge.target) {
                visit(j, plans, order, state);
            }
        }
        state[i] = 2;
        order.push(i);
    }
    let mut order = Vec::new();
    let mut state = vec![0u8; plans.len()];
    visit(idx, plans, &mut order, &mut state);
    order
}

/// 项绑定(模块字段)的目标文件:基础模块入口导出表 → 项槽 →
/// 文件索引(仅 IMPORT 槽——其边目标即模块文件身份)。供体内
/// `import 项名: *` 静态展开(alchemist/diagraph 驱动);链不通
/// 返回 None,通配落运行期层。
fn item_file(
    export_table: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    slot_files: &Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    base_target: &str,
    item: &str,
) -> Option<String> {
    let slot = export_table.get(base_target)?.get(item)?;
    if !slot.starts_with("IMPORT_") {
        return None;
    }
    slot_files.as_ref()?.get(slot).cloned()
}

/// 从包 spec 取短名(`@preview/oxifmt:1.0.0` → `oxifmt`)。
/// 合并键 `{spec}/{包内路径}` 的 spec 前缀提取(spec 自含 `/`,
/// 以冒号后的首个 `/` 为界);非合并键返回空串。

/// 槽名→所属文件(`V{i}_…` 的 i 是 plan 序号;`IMPORT_{mod}_…`
/// 反查该 mod 的内部 import 边目标)。模块值链通配解析用。
fn slot_target_file(
    slot: &str,
    plans: &[super::plan::FilePlan],
) -> Option<String> {
    if let Some(rest) = slot.strip_prefix('V')
        && let Some((idx, _)) = rest.split_once('_')
        && let Ok(i) = idx.parse::<usize>()
        && i < plans.len()
    {
        return Some(plans[i].path.clone());
    }
    if let Some(rest) = slot.strip_prefix("IMPORT_") {
        // mod_name 自身含下划线(m55_anchors)——按"plan 前缀 + _"剥离。
        let plan = plans.iter().find(|p| {
            rest.strip_prefix(&p.mod_name)
                .and_then(|r| r.strip_prefix('_'))
                .is_some()
        })?;
        let plan = &plans[plans.iter().position(|p| std::ptr::eq(p, plan)).unwrap()];
        for edge in &plan.imports {
            if edge.is_include {
                continue;
            }
            // 外部包(含已并入翻译的依赖):绑定名按 spec 名或改名。
            let bound = if edge.external {
                match &edge.items {
                    super::plan::ImportItems::Rename(name)
                    | super::plan::ImportItems::RenameList(name, _) => name.clone(),
                    _ => spec_name(&edge.target),
                }
            } else {
                match &edge.items {
                    super::plan::ImportItems::Bare => module_stem_pub(&edge.target),
                    super::plan::ImportItems::Rename(name)
                    | super::plan::ImportItems::RenameList(name, _) => name.clone(),
                    _ => continue,
                }
            };
            if format!("IMPORT_{}_{}", plan.mod_name, bound) == *slot {
                return Some(edge.target.clone());
            }
        }
    }
    None
}

/// module_stem 的模块级可达副本(plan.rs 内为 pub(super))。
fn module_stem_pub(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("m")
        .split('.')
        .next()
        .unwrap_or("m")
        .to_string()
}

impl Ctx {
    /// 单条顶层 import 边的处理(env 注入 + 槽填充 + 导出面;由第二遍
    /// 的**源序流水**在 ModuleImport 语句位置调用——import 在 let 之前
    /// 还是之后影响名字解析,与原生逐语句求值一致,不能整体前置)。
    #[allow(clippy::too_many_arguments)]

    fn process_import_edge(
        &mut self,
        edge: &super::plan::ImportEdge,
        plan: &super::plan::FilePlan,
        input: &PackageInput,
        plans: &[super::plan::FilePlan],
        export_table: &std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        >,
        package_mods: &std::collections::BTreeMap<String, String>,
        env: &mut Env,
        state_fields: &mut Vec<String>,
        import_fills: &mut String,
        file_defines: &mut String,
        import_slots: &mut std::collections::HashMap<String, String>,
    ) {
        let cx = self;
        // 单次迭代壳:方法体内的裸 `continue` 语义 == 跳过本条边
        //(原属边循环体;内层循环的 continue 不受影响)。
        for _ in std::iter::once(()) {

            if edge.is_include {
                continue;
            }
            // 外部包:槽位 + 装配期运行时导入(未翻译依赖走解释器)。
            if edge.external {
                // 已并入翻译的依赖:spec 在 target(入口合并键)前缀
                // (spec 自含 `/`,取到冒号后的首个 `/` 为止);求值期
                // 静态装配生成模块,不经 import_external。
                let spec = spec_prefix(&edge.target);
                let name = match &edge.items {
                    ImportItems::Rename(name) => name.clone(),
                    _ => spec_name(&spec),
                };
                let slot = format!("IMPORT_{}_{}", sanitize_key(&plan.mod_name), sanitize_key(&name));
                state_fields.push(slot.clone());
                let ext_at = cx.span_of(edge.span);
                if input.dep_entries.contains_key(&spec) {
                    let target_mod = &plans
                        .iter()
                        .find(|p| p.path == edge.target)
                        .expect("dep entry plan")
                        .mod_name;
                    let _ = writeln!(
                        import_fills,
                        "    rt::fill(&__st.{slot}, Value::Module({target_mod}::module(&__st, engine, context)?))?;"
                    );
                } else {
                    let _ = writeln!(
                        import_fills,
                        "    rt::fill(&__st.{slot}, Value::Module(rt::import_external(engine, \"{spec}\", {ext_at})?))?;"
                    );
                }
                match &edge.items {
                    ImportItems::Bare | ImportItems::Rename(_) => {
                        env.module.push((name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(&name)
                        );
                    }
                    // 模块改名 + 成员列表:双绑定(elembic 驱动)。
                    ImportItems::RenameList(mname, list) => {
                        env.module.push((mname.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(mname)
                        );
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{slot})?"),
                                    name: item.clone(),
                                    file: item_file(export_table, &cx.slot_files, &edge.target, item),
                                },
                            ));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", {});",
                                escape(&as_name),
                                super::ctx::field_access_chain(
                                    &format!("rt::read(&__st.{slot})?"),
                                    item
                                )
                            );
                        }
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{slot})?"),
                                    name: item.clone(),
                                    file: item_file(export_table, &cx.slot_files, &edge.target, item),
                                },
                            ));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", {});",
                                escape(&as_name),
                                super::ctx::field_access_chain(
                                    &format!("rt::read(&__st.{slot})?"),
                                    item
                                )
                            );
                        }
                    }
                    ImportItems::Wildcard => {
                        if input.dep_entries.contains_key(&spec) {
                            // 合并依赖的通配:按入口导出表展开(与 LIST
                            // 同形,字段访问物化;gentle-clues 驱动)。
                            for (wname, _) in &export_table[&edge.target] {
                                if env.module.iter().any(|(n, _)| n == wname) {
                                    continue;
                                }
                                env.module.push((
                                    wname.clone(),
                                    Ref::ModuleField {
                                        expr: format!("rt::read(&__st.{slot})?"),
                                        name: wname.clone(),
                                        file: item_file(export_table, &cx.slot_files, &edge.target, wname),
                                    },
                                ));
                                let _ = writeln!(
                                    file_defines,
                                    "    scope.define(\"{}\", {});",
                                    escape(wname),
                                    super::ctx::field_access_chain(
                                        &format!("rt::read(&__st.{slot})?"),
                                        wname
                                    )
                                );
                            }
                        } else {
                            cx.diag(edge.span, "外部包的通配 import 暂不支持(须翻译该包)".into());
                        }
                    }
                }
                continue;
            }
            // 任意表达式源(`import plugin("…") as hypher`,
            // touying/hy-dro-gen 驱动):源翻译求值为模块值,后续绑定
            // 与模块值 import 同款(对照 typst-eval:import 源求值为
            // 模块即可)。
            if let Some(node) = &edge.source_node
                && let Some(expr_node) = node.cast::<ast::Expr>()
            {
                let mut env_m = env.clone();
                let value = cx.expr(expr_node, &mut env_m);
                let base_name = match &edge.items {
                    ImportItems::Rename(name) | ImportItems::RenameList(name, _) => name.clone(),
                    ImportItems::Bare => "trust-source".to_string(),
                    _ => "trust-source".to_string(),
                };
                let mslot = format!(
                    "IMPORT_{}_{}",
                    sanitize_key(&plan.mod_name),
                    sanitize_key(&base_name)
                );
                state_fields.push(mslot.clone());
                let _ = writeln!(import_fills, "    rt::fill(&__st.{mslot}, {value})?;");
                match &edge.items {
                    // 裸表达式源无名可绑(typst 语义须 `as`;hy-dro-gen
                    // 即 `as hypher`)——不产生绑定。
                    ImportItems::Bare => {}
                    ImportItems::Rename(name) => {
                        env.module.push((name.clone(), Ref::Slot(mslot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{mslot})?);",
                            escape(name)
                        );
                    }
                    ImportItems::RenameList(mname, list) => {
                        env.module.push((mname.clone(), Ref::Slot(mslot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{mslot})?);",
                            escape(mname)
                        );
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{mslot})?"),
                                    name: item.clone(),
                                    file: None,
                                },
                            ));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", {});",
                                escape(&as_name),
                                super::ctx::field_access_chain(
                                    &format!("rt::read(&__st.{mslot})?"),
                                    item
                                )
                            );
                        }
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            env.module.push((
                                as_name.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{mslot})?"),
                                    name: item.clone(),
                                    file: None,
                                },
                            ));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", {});",
                                escape(&as_name),
                                super::ctx::field_access_chain(
                                    &format!("rt::read(&__st.{mslot})?"),
                                    item
                                )
                            );
                        }
                    }
                    ImportItems::Wildcard => {
                        // 任意表达式源的通配:运行时按模块作用域展开——
                        // 槽里存模块值,define 后由 scope 镜像(此处以
                        // 模块值本身导出,binding 靠字段访问;纯通配场景
                        // 罕见,降级为模块绑定 + 诊断提示)。
                        cx.diag(edge.span, "任意表达式源的通配 import 暂不支持".into());
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
                let mut expr = base.clone();
                for field in fields {
                    expr = format!(
                        "rt::field(&({expr}), \"{}\", engine, context, SPAN)?",
                        escape(field)
                    );
                }
                // 绑定进**槽**(eval 期填充 + 模块作用域转出):与外部包
                // import 同款形态——下游 `import "本文件": 名字` 可达
                // (对照原生:模块作用域含 import 引入的名字)。此前绑定
                // 为即时表达式,文件内可用但不可转出,fletcher 0.5.9 的
                // `#import cetz: vector`(外部模块的子模块值)踩中此缺口。
                match &edge.items {
                    ImportItems::Bare => {
                        let name = fields.last().cloned().unwrap_or_else(|| root.clone());
                        let slot = format!(
                            "IMPORT_{}_{}",
                            sanitize(&plan.mod_name),
                            sanitize(&name)
                        );
                        state_fields.push(slot.clone());
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{slot}, {expr})?;"
                        );
                        env.module.push((name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(&name)
                        );
                    }
                    ImportItems::Rename(name) => {
                        let slot = format!(
                            "IMPORT_{}_{}",
                            sanitize(&plan.mod_name),
                            sanitize(&name)
                        );
                        state_fields.push(slot.clone());
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{slot}, {expr})?;"
                        );
                        env.module.push((name.clone(), Ref::Slot(slot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                            escape(&name)
                        );
                    }
                    // 模块改名 + 成员列表:双绑定(elembic 驱动)。
                    ImportItems::RenameList(mname, list) => {
                        let mslot = format!(
                            "IMPORT_{}_{}",
                            sanitize(&plan.mod_name),
                            sanitize(mname)
                        );
                        state_fields.push(mslot.clone());
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{mslot}, {expr})?;"
                        );
                        env.module.push((mname.clone(), Ref::Slot(mslot.clone())));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::read(&__st.{mslot})?);",
                            escape(mname)
                        );
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            let slot = format!(
                                "IMPORT_{}_{}",
                                sanitize(&plan.mod_name),
                                sanitize(&as_name)
                            );
                            state_fields.push(slot.clone());
                            let item_expr = format!(
                                "rt::field(&({expr}), \"{}\", engine, context, SPAN)?",
                                escape(item)
                            );
                            let _ = writeln!(
                                import_fills,
                                "    rt::fill(&__st.{slot}, {item_expr})?;"
                            );
                            env.module.push((as_name.clone(), Ref::Slot(slot.clone())));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                                escape(&as_name)
                            );
                        }
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            let slot = format!(
                                "IMPORT_{}_{}",
                                sanitize(&plan.mod_name),
                                sanitize(&as_name)
                            );
                            state_fields.push(slot.clone());
                            let item_expr = format!(
                                "rt::field(&({expr}), \"{}\", engine, context, SPAN)?",
                                escape(item)
                            );
                            let _ = writeln!(
                                import_fills,
                                "    rt::fill(&__st.{slot}, {item_expr})?;"
                            );
                            env.module.push((as_name.clone(), Ref::Slot(slot.clone())));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                                escape(&as_name)
                            );
                        }
                    }
                    ImportItems::Wildcard => {
                        // 模块值链的通配:静态解析链(root→文件,逐字段
                        // 经导出表到目标文件)后按其导出表展开
                        // (alchemist 的 `import cetz.draw: *` 等)。
                        let mut cur: Option<String> = None;
                        if let Some((_, r#ref)) =
                            env.module.iter().rev().find(|(n, _)| n == root)
                        {
                            if let super::ctx::Ref::Slot(slot) = r#ref {
                                cur = slot_target_file(slot, &plans);
                                if std::env::var_os("TRUST_TRACE_WILD").is_some() {
                                    eprintln!("[wild] root={root} slot={slot} → {cur:?}");
                                }
                            }
                        }
                        for field in fields {
                            let next = cur
                                .as_ref()
                                .and_then(|f| export_table.get(f.as_str()))
                                .and_then(|exports| exports.get(field))
                                .and_then(|slot| slot_target_file(slot, &plans));
                            match next {
                                Some(f) => cur = Some(f),
                                None => {
                                    cur = None;
                                    break;
                                }
                            }
                        }
                        if let Some(target) = cur
                            && let Some(exports) = export_table.get(target.as_str())
                        {
                            for (wname, _wslot) in exports {
                                if env.module.iter().any(|(n, _)| n == wname) {
                                    continue;
                                }
                                env.module.push((
                                    wname.clone(),
                                    Ref::ModuleField {
                                        expr: base.clone(),
                                        name: wname.clone(),
                                        file: item_file(export_table, &cx.slot_files, &target, wname),
                                    },
                                ));
                                let _ = writeln!(
                                    file_defines,
                                    "    scope.define(\"{}\", rt::field(&({base}), \"{}\", engine, context, SPAN)?);",
                                    escape(wname),
                                    escape(wname)
                                );
                            }
                        } else {
                            cx.diag(edge.span, "模块值表达式的通配 import 暂不支持(链未静态解析)".into());
                        }
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
                        None => format!("IMPORT_{}_{}", sanitize_key(&plan.mod_name), sanitize_key(&bound)),
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
                // 模块改名 + 成员列表:模块槽绑定 + 逐成员字段访问
                // (elembic 驱动:`import "base.typ" as base: ok, err`)。
                ImportItems::RenameList(mname, list) => {
                    let mslot = match import_slots.get(&edge.target) {
                        Some(slot) => slot.clone(),
                        None => format!(
                            "IMPORT_{}_{}",
                            sanitize(&plan.mod_name),
                            sanitize(mname)
                        ),
                    };
                    let existed = import_slots
                        .insert(edge.target.clone(), mslot.clone())
                        .is_some();
                    if !existed {
                        state_fields.push(mslot.clone());
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{mslot}, Value::Module({target_mod}::module(&__st, engine, context)?))?;"
                        );
                    }
                    env.module.push((mname.clone(), Ref::Slot(mslot.clone())));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::read(&__st.{mslot})?);",
                        escape(mname)
                    );
                    for (name, rename) in list {
                        let as_name = rename.clone().unwrap_or_else(|| name.clone());
                        env.module.push((
                            as_name.clone(),
                            Ref::ModuleField {
                                expr: format!("rt::read(&__st.{mslot})?"),
                                name: name.clone(),
                                file: item_file(export_table, &cx.slot_files, &edge.target, name),
                            },
                        ));
                        let _ = writeln!(
                            file_defines,
                            "    scope.define(\"{}\", rt::field(&(rt::read(&__st.{mslot})?), \"{}\", engine, context, SPAN)?);",
                            escape(&as_name),
                            escape(name)
                        );
                    }
                }
                ImportItems::List(list) => {
                    for (name, rename) in list {
                        // 点分项(`a.b as c`,hydra 驱动:首段按导出表
                        // 解析,余段字段链物化进独立 IMPORT 槽)。
                        let (first, rest): (&str, &[&str]) = match name.split_once('.') {
                            Some((f, r)) => {
                                (f, r.split('.').collect::<Vec<_>>().leak())
                            }
                            None => (name.as_str(), &[]),
                        };
                        let Some(slot) = exports.get(first) else {
                            cx.diag(edge.span, format!("`{}` 不包含导出 `{first}`", edge.target));
                            continue;
                        };
                        let as_name = rename.clone().unwrap_or_else(|| name.clone());
                        if rest.is_empty() {
                            env.module.push((as_name.clone(), Ref::Slot(slot.clone())));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                                escape(&as_name)
                            );
                        } else {
                            let dot_slot = format!(
                                "IMPORT_{}_{}",
                                sanitize_key(&plan.mod_name),
                                sanitize_key(&as_name)
                            );
                            state_fields.push(dot_slot.clone());
                            let mut chain = format!("rt::read(&__st.{slot})?");
                            for field in rest {
                                chain = format!(
                                    "rt::field(&({chain}), \"{}\", engine, context, SPAN)?",
                                    escape(field)
                                );
                            }
                            let _ = writeln!(
                                import_fills,
                                "    rt::fill(&__st.{dot_slot}, {chain})?;"
                            );
                            env.module.push((as_name.clone(), Ref::Slot(dot_slot.clone())));
                            let _ = writeln!(
                                file_defines,
                                "    scope.define(\"{}\", rt::read(&__st.{dot_slot})?);",
                                escape(&as_name)
                            );
                        }
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
    }
}
