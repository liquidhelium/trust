//! 包结构分析(翻译第一遍):每个文件的顶层绑定与 import 边、
//! import 图的环检测与拓扑序、导出查找表。
//!
//! 纯计划数据与图算法,不生成代码;代码生成见 `package.rs`。

use typst_syntax::ast;

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
    /// `#include` 边:参与依赖图,不产生名字绑定。
    pub(super) is_include: bool,
}

pub(super) enum ImportItems {
    Bare,
    Rename(String),
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

/// 顶层绑定查找表:路径 → (名字 → 槽名)。
pub(super) fn build_export_table(
    plans: &[FilePlan],
) -> std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> {
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
        .collect()
}
