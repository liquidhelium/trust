//! 包结构分析(翻译第一遍):每个文件的顶层绑定与 import 边、
//! import 图的环检测与拓扑序、导出查找表。
//!
//! 纯计划数据与图算法,不生成代码;代码生成见 `package.rs`。

use typst_syntax::ast;

use super::naming::{sanitize, sanitize_key};

/// 一个源文件的翻译计划:mod 名、顶层绑定(名字 → 槽)与 import 边。
pub(super) struct FilePlan<'a> {
    pub(super) path: String,
    pub(super) mod_name: String,
    pub(super) markup: ast::Markup<'a>,
    /// 解析诊断(typst-syntax errors_and_warnings 的错误信息;对照
    /// 官方 compile 的 parse 阶段——lenient 翻译曾静默吞掉错误节点,
    /// 解析类错误测试假绿,eval 头部按首条 bail)。
    pub(super) parse_diags: Vec<String>,
    /// 被就地修改的顶层绑定根名(可变方法 push/pop/insert/remove 的
    /// 目标根、复合赋值的目标根):这些绑定发射为可变槽
    /// (std::sync::Mutex),读点/填点/修改点走 *_ms 助手。
    pub(super) mutated_roots: Vec<String>,
    pub(super) tops: Vec<(String, String)>,
    pub(super) imports: Vec<ImportEdge>,
}

pub(super) struct ImportEdge {
    pub(super) span: typst_syntax::Span,
    /// 目标文件路径。
    pub(super) target: String,
    pub(super) items: ImportItems,
    /// 外部包("@ns/name:ver")。
    pub(super) external: bool,
    /// 模块值表达式 import(`deps.oxifmt`):根绑定名 + 字段链。
    pub(super) module_access: Option<(String, Vec<String>)>,
    /// 任意表达式源(`import plugin("…") as m`,touying/hy-dro-gen
    /// 驱动):源节点,二遍经 Ctx 翻译求值为模块值,绑定走模块值
    /// 同款机制(对照 typst-eval:import 源求值为模块即可)。
    pub(super) source_node: Option<typst_syntax::SyntaxNode>,
    /// `#include` 边:参与依赖图,不产生名字绑定。
    pub(super) is_include: bool,
}

pub(super) enum ImportItems {
    Bare,
    Rename(String),
    /// 模块改名 + 成员列表(`import x as m: a, b`,elembic 驱动):
    /// 绑定 m = 模块本身,以及各成员。
    RenameList(String, Vec<(String, Option<String>)>),
    /// (名字, rename)。
    List(Vec<(String, Option<String>)>),
    Wildcard,
}

/// 文件 tops 里第 nth 个同名绑定的槽(顶层重定义按出现序配对)。
pub(super) fn nth_slot(plan: &FilePlan, name: &str, nth: usize) -> Option<String> {
    plan.tops
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, slot)| slot.clone())
        .nth(nth)
}

/// import 环检测(typst 模块循环求值同样报错)。
pub(super) fn has_import_cycle(plans: &[FilePlan], entry: &str) -> bool {
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
    let mut edges: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for plan in plans {
        edges
            .entry(plan.path.clone())
            .or_default()
            .extend(plan.imports.iter().map(|edge| edge.target.clone()));
    }
    let mut stack = Vec::new();
    !dfs(entry, &edges, &mut stack)
}

/// 文件依赖的拓扑序(被依赖者在前)。
pub(super) fn topo_order(plans: &[FilePlan]) -> Vec<usize> {
    fn visit(idx: usize, plans: &[FilePlan], order: &mut Vec<usize>, state: &mut Vec<u8>) {
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
    let mut order = Vec::new();
    let mut state = vec![0u8; plans.len()];
    for idx in 0..plans.len() {
        visit(idx, plans, &mut order, &mut state);
    }
    order
}

/// 顶层绑定查找表:路径 → (名字 → 槽名)。对照 typst 模块导出面:
/// 全部顶层绑定,**含 import 引入的名字(转出名)**——下游的具名与
/// 通配 import 从这里展开(如 `shapes.typ` 经 `import "grouping.typ": *`
/// 拿到 grouping 转出的 `intersection`)。
///
/// 按拓扑序增量并入:被依赖者的导出面先定型,list/wildcard 引用的
/// 目标槽随之递归可达。bare/rename 与**外部包** import 的 IMPORT 槽名
/// 与 `package.rs` 的生成规则逐字一致(外部包每边独立槽、无 dedup;
/// 内部同 target 复用首个槽;`deps.typ` 一行裸外部 import 转出 `cetz`
/// 即此形态,fletcher 的跨文件传递依赖靠它)。已有名字(本地绑定或
/// 先前的 import)优先,后到不覆盖。模块值 import(`import
/// deps.oxifmt: strfmt`)引入的名字无槽形态,暂不转出。
pub(super) fn build_export_table(
    plans: &[FilePlan],
    order: &[usize],
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
    let mut table: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
        plans
            .iter()
            .map(|plan| {
                (
                    plan.path.clone(),
                    plan.tops
                        .iter()
                        .map(|(name, slot)| (name.clone(), slot.clone()))
                        .collect(),
                )
            })
            .collect();
    for &idx in order {
        let plan = &plans[idx];
        let mut exports = table[&plan.path].clone();
        // 与 package.rs 同款:同 target 的重复 import 复用首个槽名。
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for edge in &plan.imports {
            if edge.is_include {
                continue;
            }
            // 模块值 import(`import cetz: vector`)的绑定同样转出:
            // 槽名与 package.rs 的生成规则逐字一致(eval 期填充)。
            if let Some((root, fields)) = &edge.module_access {
                match &edge.items {
                    ImportItems::Bare | ImportItems::Rename(_) => {
                        let bound = match &edge.items {
                            ImportItems::Rename(name) => name.clone(),
                            _ => fields.last().cloned().unwrap_or_else(|| root.clone()),
                        };
                        let slot = format!(
                            "IMPORT_{}_{}",
                            sanitize_key(&plan.mod_name),
                            sanitize_key(&bound)
                        );
                        exports.entry(bound).or_insert(slot);
                    }
                    ImportItems::RenameList(mname, list) => {
                        exports.entry(mname.clone()).or_insert_with(|| {
                            format!(
                                "IMPORT_{}_{}",
                                sanitize_key(&plan.mod_name),
                                sanitize_key(mname)
                            )
                        });
                        for (item, rename) in list {
                            let as_name = rename.clone().unwrap_or_else(|| item.clone());
                            let slot = format!(
                                "IMPORT_{}_{}",
                                sanitize_key(&plan.mod_name),
                                sanitize_key(&as_name)
                            );
                            exports.entry(as_name).or_insert(slot);
                        }
                    }
                    ImportItems::List(list) => {
                        for (item, rename) in list {
                            let as_name =
                                rename.clone().unwrap_or_else(|| item.clone());
                            let slot = format!(
                                "IMPORT_{}_{}",
                                sanitize(&plan.mod_name),
                                sanitize(&as_name)
                            );
                            exports.entry(as_name).or_insert(slot);
                        }
                    }
                    ImportItems::Wildcard => {
                        // 模块值链通配(`import t4t: *`,finite 驱动):
                        // 根名槽 → 文件身份(通用解析器),逐字段经导出
                        // 表到目标文件,按其导出面**转出**(下游具名/通配
                        // import 可达;此前此臂为空,`src/util.typ 不包含
                        // 导出 get` 即此缺口)。与 package.rs
                        // process_import_edge 的同型链一致。
                        let root_slot = exports.get(root).cloned();
                        let mut cur = root_slot.as_deref().and_then(|slot| {
                            resolve_import_slot_file(slot, plans, &table, dep_entries, plans.len())
                        });
                        for field in fields {
                            let next = cur
                                .as_deref()
                                .and_then(|f| table.get(f))
                                .and_then(|exports| exports.get(field))
                                .and_then(|slot| {
                                    resolve_import_slot_file(
                                        slot,
                                        plans,
                                        &table,
                                        dep_entries,
                                        plans.len(),
                                    )
                                });
                            match next {
                                Some(f) => cur = Some(f),
                                None => {
                                    cur = None;
                                    break;
                                }
                            }
                        }
                        if let Some(target) = cur.as_deref()
                            && let Some(target_exports) = table.get(target)
                        {
                            for (wname, wslot) in target_exports {
                                exports.entry(wname.clone()).or_insert_with(|| wslot.clone());
                            }
                        }
                    }
                }
                continue;
            }
            // 外部包 bare/rename:转出绑定到外部模块槽(package.rs 的
            // 外部 import 每边独立生成 IMPORT_{mod}_{name} 槽并运行时
            // 装配,此处按同规则登记,下游 import 即可达)。
            if edge.external {
                if let ImportItems::Bare | ImportItems::Rename(_) = &edge.items {
                    let bound = match &edge.items {
                        ImportItems::Rename(name) => name.clone(),
                        _ => spec_name(&edge.target),
                    };
                    let slot = format!(
                        "IMPORT_{}_{}",
                        sanitize_key(&plan.mod_name),
                        sanitize_key(&bound)
                    );
                    exports.entry(bound).or_insert(slot);
                }
                // 外部包 LIST:绑定名同样转出(hydra 驱动:`_pkgs.typ`
                // 从 oxifmt 取 strfmt 再被下游 import——槽公式与
                // package.rs 的字段访问生成侧逐字一致)。
                if let ImportItems::List(list) = &edge.items {
                    for (item, rename) in list {
                        let as_name = rename.clone().unwrap_or_else(|| item.clone());
                        let slot = format!(
                            "IMPORT_{}_{}",
                            sanitize_key(&plan.mod_name),
                            sanitize_key(&as_name)
                        );
                        exports.entry(as_name).or_insert(slot);
                    }
                }
                continue;
            }
            match &edge.items {
                ImportItems::RenameList(mname, list) => {
                    let slot = format!(
                        "IMPORT_{}_{}",
                        sanitize_key(&plan.mod_name),
                        sanitize_key(mname)
                    );
                    exports.entry(mname.clone()).or_insert(slot);
                    for (item, rename) in list {
                        let as_name = rename.clone().unwrap_or_else(|| item.clone());
                        let slot = format!(
                            "IMPORT_{}_{}",
                            sanitize_key(&plan.mod_name),
                            sanitize_key(&as_name)
                        );
                        exports.entry(as_name).or_insert(slot);
                    }
                }
                ImportItems::Bare | ImportItems::Rename(_) => {
                    let bound = match &edge.items {
                        ImportItems::Rename(name) => name.clone(),
                        _ => module_stem(&edge.target),
                    };
                    let slot = match import_slots.get(&edge.target) {
                        Some(slot) => slot.clone(),
                        None => format!(
                            "IMPORT_{}_{}",
                            sanitize_key(&plan.mod_name),
                            sanitize_key(&bound)
                        ),
                    };
                    import_slots
                        .entry(edge.target.clone())
                        .or_insert_with(|| slot.clone());
                    exports.entry(bound).or_insert(slot);
                }
                ImportItems::List(items) => {
                    // 目标不在计划集(模板占位 spec 等未解析边,eggs
                    // 驱动曾在此裸索引 panic):安全查找,miss 时该项
                    // 无从展开,留空——真正的缺口由后续具名使用点报。
                    let Some(target_exports) = table.get(&edge.target) else {
                        continue;
                    };
                    let target_exports = &target_exports.clone();
                    for (original, rename) in items {
                        // 点分项:首段解析,余段物化进独立槽(package.rs
                        // 同公式;hydra 驱动)。
                        let (first, rest): (&str, Vec<&str>) = match original.split_once('.') {
                            Some((f, r)) => (f, r.split('.').collect()),
                            None => (original.as_str(), Vec::new()),
                        };
                        if rest.is_empty() {
                            if let Some(slot) = target_exports.get(first) {
                                let as_name = rename.clone().unwrap_or_else(|| original.clone());
                                exports.entry(as_name).or_insert_with(|| slot.clone());
                            }
                        } else if target_exports.contains_key(first) {
                            let as_name = rename.clone().unwrap_or_else(|| original.clone());
                            let slot = format!(
                                "IMPORT_{}_{}",
                                sanitize_key(&plan.mod_name),
                                sanitize_key(&as_name)
                            );
                            exports.entry(as_name).or_insert(slot);
                        }
                    }
                }
                ImportItems::Wildcard => {
                    // 同 List:目标不在计划集时安全跳过(eggs 模板边)。
                    if let Some(target_exports) = table.get(&edge.target) {
                        for (name, slot) in target_exports {
                            exports.entry(name.clone()).or_insert_with(|| slot.clone());
                        }
                    }
                }
            }
        }
        table.insert(plan.path.clone(), exports);
    }
    table
}

/// 文件路径的模块名(bare import 的名字 = 文件 stem)。
pub(super) fn module_stem(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("m")
        .split('.')
        .next()
        .unwrap_or("m")
        .to_string()
}

pub(super) fn spec_prefix(target: &str) -> String {
    if !target.starts_with('@') {
        return String::new();
    }
    match target.find(':').and_then(|colon| {
        target[colon..].find('/').map(|rel| colon + rel)
    }) {
        Some(end) => target[..end].to_string(),
        // 无后续路径:整个串就是 spec(未翻译外部边,target 即原始
        // spec,如 "@preview/oxifmt:1.0.0")。
        None => target.to_string(),
    }
}

pub(super) fn spec_name(spec: &str) -> String {
    spec.split('/')
        .nth(1)
        .unwrap_or("pkg")
        .split(':')
        .next()
        .unwrap_or("pkg")
        .to_string()
}

/// IMPORT 槽 → 该绑定模块的**文件身份**(通用递归解析,深度以计划
/// 数为界):内部 bare/rename → 边目标文件;内部 list 项 → 目标文件
/// 导出表该项的槽再解一跳;外部边 → 已并入依赖的入口合并键。供
/// slot_files 索引与体内/顶层模块值链通配使用(finite 的
/// `import t4t: *`,t4t 经内部 list 引入)。
pub(super) fn resolve_import_slot_file(
    slot: &str,
    plans: &[super::plan::FilePlan],
    export_table: &std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    dep_entries: &std::collections::BTreeMap<String, String>,
    depth: usize,
) -> Option<String> {
    if depth == 0 {
        return None;
    }
    // IMPORT_{mod}_{bound}:mod 名可含下划线(sanitize_key 不去下划
    // 线),按计划前缀匹配定界(与前缀匹配 plan 的既有做法一致)。
    let (plan, bound): (&super::plan::FilePlan, String) = plans
        .iter()
        .find_map(|p| {
            let prefix = format!("IMPORT_{}_", sanitize_key(&p.mod_name));
            slot.strip_prefix(&prefix)
                .map(|b| (p, b.to_string()))
        })
        .or_else(|| {
            // 模块名不含下划线的常规形态也要兜底(前缀匹配按计划序
            // 首个命中;无可行分隔时返回 None)。
            None
        })?;
    let bound = &bound[..];
    for edge in &plan.imports {
        if edge.is_include {
            continue;
        }
        // 本边可能产出该槽的绑定名集合(bare/rename/list/rename-list)。
        let names: Vec<String> = if edge.external {
            match &edge.items {
                ImportItems::Rename(n) | ImportItems::RenameList(n, _) => vec![n.clone()],
                ImportItems::Bare => vec![spec_name(&edge.target)],
                ImportItems::List(list) => list
                    .iter()
                    .map(|(item, rename)| rename.clone().unwrap_or_else(|| item.clone()))
                    .collect(),
                ImportItems::Wildcard => Vec::new(),
            }
        } else {
            match &edge.items {
                ImportItems::Rename(n) => vec![n.clone()],
                ImportItems::Bare => vec![module_stem(&edge.target)],
                ImportItems::RenameList(n, list) => {
                    let mut v = vec![n.clone()];
                    v.extend(
                        list.iter()
                            .map(|(i, r)| r.clone().unwrap_or_else(|| i.clone())),
                    );
                    v
                }
                ImportItems::List(list) => list
                    .iter()
                    .map(|(item, rename)| {
                        let as_name = rename.clone().unwrap_or_else(|| item.clone());
                        // 点分项只取首段为绑定名。
                        as_name.split('.').next().unwrap_or(&as_name).to_string()
                    })
                    .collect(),
                ImportItems::Wildcard => Vec::new(),
            }
        };
        if !names.iter().any(|n| sanitize_key(n) == bound) {
            continue;
        }
        if !edge.external {
            // 模块名绑定(bare 的 stem / rename / rename-list 的模块名)
            // → 边目标文件即模块身份。
            let module_binding = match &edge.items {
                ImportItems::Bare => module_stem(&edge.target),
                ImportItems::Rename(n) | ImportItems::RenameList(n, _) => n.clone(),
                _ => String::new(),
            };
            if !module_binding.is_empty() && sanitize_key(&module_binding) == bound {
                return Some(edge.target.clone());
            }
            // list/rename-list 项:目标文件导出表该项 → 槽,再解一跳。
            let table_key = if bound.contains('.') {
                bound.split('.').next().unwrap_or(bound)
            } else {
                bound
            };
            let next = export_table.get(&edge.target)?.get(table_key)?.clone();
            return resolve_import_slot_file(&next, plans, export_table, dep_entries, depth - 1);
        }
        // 外部边:已并入依赖 → 入口合并键。
        let spec = spec_prefix(&edge.target);
        return dep_entries.get(&spec).cloned();
    }
    None
}

