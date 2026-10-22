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
                    ImportItems::Wildcard => {}
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
                        _ => super::package::spec_name(&edge.target),
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
                    let target_exports = &table[&edge.target];
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
                    for (name, slot) in &table[&edge.target] {
                        exports.entry(name.clone()).or_insert_with(|| slot.clone());
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
