//! 包结构分析(翻译第一遍与预扫描):每个文件的顶层绑定与 import 边、
//! import 图的环检测与拓扑序、引擎值槽预扫描、导出查找表。
//!
//! 纯计划数据与图算法,不生成代码;代码生成见 `package.rs`。

use typst_syntax::ast;

use super::ctx::{Ctx, Env, Mode, Ref};

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

/// 引擎值槽预扫描(拓扑序:被引用文件的 flag 先定,import 解析准确)。
///
/// 判定方式:以 `Mode::ModuleInit` 试译 init,新增诊断即需要引擎
/// (两次生成,翻译纯函数)。
pub(super) fn scan_engine_slots(
    plans: &[FilePlan],
    order: &[usize],
    cx: &mut Ctx,
) -> std::collections::HashSet<String> {
    let mut engine_slots: std::collections::HashSet<String> = std::collections::HashSet::new();
    for &idx in order {
        let plan = &plans[idx];
        // 该文件的 env:import 边(带跨文件 flag)+ 自身绑定(逐个补 flag)。
        let mut env = Env {
            locals: Vec::new(),
            module: Vec::new(),
        };
        for edge in &plan.imports {
            let Some(target_plan) = plans.iter().find(|p| p.path == edge.target) else {
                continue;
            };
            match &edge.items {
                ImportItems::List(list) => {
                    for (item, rename) in list {
                        let as_name = rename.clone().unwrap_or_else(|| item.clone());
                        let slot = nth_slot(target_plan, item, 0).unwrap_or_default();
                        let qualified = format!("{}::{slot}", target_plan.mod_name);
                        let r = if engine_slots.contains(&qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(slot)
                        };
                        env.module.push((as_name, r));
                    }
                }
                ImportItems::Wildcard => {
                    for (name, slot) in &target_plan.tops {
                        let qualified = format!("{}::{slot}", target_plan.mod_name);
                        let r = if engine_slots.contains(&qualified) {
                            Ref::Engine(qualified.clone())
                        } else {
                            Ref::Slot(slot.clone())
                        };
                        env.module.push((name.clone(), r));
                    }
                }
                _ => {
                    // bare/rename:模块值槽(engine-free)。
                }
            }
        }
        // 逐绑定两试。
        let mut seen_counts: std::collections::HashMap<String, usize> =
            std::collections::HashMap::new();
        for expr in plan.markup.exprs() {
            let ast::Expr::LetBinding(lets) = expr else {
                continue;
            };
            let kind = lets.kind();
            let names = kind.bindings();
            let [name_ident] = names.as_slice() else {
                continue;
            };
            let name = name_ident.as_str().to_string();
            let nth = seen_counts.get(&name).copied().unwrap_or(0);
            seen_counts.insert(name.clone(), nth + 1);
            let Some(slot) = nth_slot(plan, &name, nth) else {
                continue;
            };
            let qualified = format!("{}::{slot}", plan.mod_name);
            let is_engine = engine_slots.contains(&qualified);
            let mut env2 = env.clone();
            env2.module.push((
                name.clone(),
                if is_engine {
                    Ref::Engine(qualified.clone())
                } else {
                    Ref::Slot(slot.clone())
                },
            ));
            if lets.init().is_some()
                && let ast::LetBindingKind::Normal(pattern) = lets.kind()
                && pattern.bindings().len() == 1
                && let Some(init) = lets.init()
            {
                let before = cx.diags.len();
                let _ = cx.expr(init, &env2, Mode::ModuleInit);
                let needs = cx.diags.len() > before;
                cx.diags.truncate(before);
                if needs {
                    engine_slots.insert(qualified);
                }
            }
        }
    }
    engine_slots
}

/// 顶层绑定查找表:路径 → (名字 → qualified 槽)。
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
                    .map(|(name, slot)| (name.clone(), format!("{}::{slot}", plan.mod_name)))
                    .collect(),
            )
        })
        .collect()
}
