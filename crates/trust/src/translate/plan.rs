//! Package structure analysis (translation pass one): each file's
//! top-level bindings and import edges, cycle detection and topological
//! order of the import graph, and the export lookup table.
//!
//! Pure planning data and graph algorithms; no code generation — see
//! `package.rs` for that.

use typst_syntax::ast;

use super::naming::{sanitize, sanitize_key};

/// One source file's translation plan: mod name, top-level bindings
/// (name → slot), and import edges.
pub(super) struct FilePlan<'a> {
    pub(super) path: String,
    pub(super) mod_name: String,
    pub(super) markup: ast::Markup<'a>,
    /// Parse diagnostics (error messages from typst-syntax's
    /// errors_and_warnings; mirrors the parse stage of the official
    /// compile — lenient translation once silently swallowed error nodes,
    /// making parse-error tests falsely green; eval heads bail on the
    /// first error).
    pub(super) parse_diags: Vec<String>,
    /// Root names of top-level bindings mutated in place (targets of
    /// mutating methods push/pop/insert/remove and of compound
    /// assignments): these bindings are emitted as mutable slots
    /// (std::sync::Mutex); reads/fills/mutations go through the *_ms
    /// helpers.
    pub(super) mutated_roots: Vec<String>,
    pub(super) tops: Vec<(String, String)>,
    pub(super) imports: Vec<ImportEdge>,
}

pub(super) struct ImportEdge {
    pub(super) span: typst_syntax::Span,
    /// Target file path.
    pub(super) target: String,
    pub(super) items: ImportItems,
    /// External package ("@ns/name:ver").
    pub(super) external: bool,
    /// Module-value expression import (`deps.oxifmt`): root binding name +
    /// field chain.
    pub(super) module_access: Option<(String, Vec<String>)>,
    /// Arbitrary-expression source (`import plugin("…") as m`): the source
    /// node, translated and evaluated into a module value in pass two,
    /// with bindings following the same mechanism as module-value imports
    /// (mirrors typst-eval: an import source just has to evaluate to a
    /// module).
    pub(super) source_node: Option<typst_syntax::SyntaxNode>,
    /// Statement text (for zone-1 glue: re-parsed by rt::eval_import to
    /// drive the original `Eval for ast::ModuleImport`, with the span
    /// synthesized onto the original statement).
    pub(super) stmt_text: Option<String>,
    /// Raw literal of a Str source (used when a wildcard goes out of range
    /// through the import_external layer).
    pub(super) raw: String,
    /// `#include` edge: participates in the dependency graph, binds no
    /// names.
    pub(super) is_include: bool,
}

pub(super) enum ImportItems {
    Bare,
    Rename(String),
    /// Module rename + member list (`import x as m: a, b`): binds
    /// m = the module itself, plus each member.
    RenameList(String, Vec<(String, Option<String>)>),
    /// (name, rename).
    List(Vec<(String, Option<String>)>),
    Wildcard,
}

/// The slot of the nth same-named binding in a file's tops (top-level
/// redefinitions pair up by order of appearance).
pub(super) fn nth_slot(plan: &FilePlan, name: &str, nth: usize) -> Option<String> {
    plan.tops
        .iter()
        .filter(|(n, _)| n == name)
        .map(|(_, slot)| slot.clone())
        .nth(nth)
}

/// Import cycle detection (typst likewise errors on circular module
/// evaluation).
pub(super) fn has_import_cycle(
    plans: &[FilePlan],
    entry: &str,
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> bool {
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
        let targets = plan
            .imports
            .iter()
            .filter_map(|edge| edge_effective_target(edge, dep_entries));
        edges.entry(plan.path.clone()).or_default().extend(targets);
    }
    let mut stack = Vec::new();
    !dfs(entry, &edges, &mut stack)
}

/// Topological order of file dependencies (dependents of nothing first,
/// i.e. dependencies first).
pub(super) fn topo_order(
    plans: &[FilePlan],
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> Vec<usize> {
    fn visit(
        idx: usize,
        plans: &[FilePlan],
        dep_entries: &std::collections::BTreeMap<String, String>,
        order: &mut Vec<usize>,
        state: &mut Vec<u8>,
    ) {
        if state[idx] != 0 {
            return;
        }
        state[idx] = 1;
        for edge in &plans[idx].imports {
            if let Some(target) = edge_effective_target(edge, dep_entries)
                && let Some(next) = plans.iter().position(|p| p.path == target)
            {
                visit(next, plans, dep_entries, order, state);
            }
        }
        state[idx] = 2;
        order.push(idx);
    }
    let mut order = Vec::new();
    let mut state = vec![0u8; plans.len()];
    for idx in 0..plans.len() {
        visit(idx, plans, dep_entries, &mut order, &mut state);
    }
    order
}

/// Top-level binding lookup table: path → (name → slot name). Mirrors the
/// typst module export surface: all top-level bindings, **including names
/// introduced by imports (re-exports)** — downstream named and wildcard
/// imports expand from here (e.g. `shapes.typ` gets the re-exported
/// `intersection` from `grouping.typ` via `import "grouping.typ": *`).
///
/// Merged in incrementally along the topological order: a dependency's
/// export surface settles first, so the target slots referenced by
/// list/wildcard become recursively reachable. IMPORT slot names for
/// bare/rename and **external package** imports match the generation rules
/// in `package.rs` verbatim (external packages get an independent slot per
/// edge, no dedup; internal same-target imports reuse the first slot; a
/// one-line bare external import in `deps.typ` re-exporting `cetz` is this
/// shape, and cross-file transitive dependencies rely on it). Existing
/// names (local bindings or earlier imports) win; later ones do not
/// override. Names introduced by module-value imports (`import
/// deps.oxifmt: strfmt`) have no slot form and are not re-exported yet.
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
        // Same as package.rs: repeated imports of the same target reuse the
        // first slot name.
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for edge in &plan.imports {
            if edge.is_include {
                continue;
            }
            // Bindings of module-value imports (`import cetz: vector`) are
            // re-exported too: slot names match the generation rules in
            // package.rs verbatim (filled at eval time). List bindings go
            // through the glue, whose slots carry the module-chain
            // discriminator (see module_list_slot) — a later same-named
            // static import must not collide with the glue cell.
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
                            let slot = module_list_slot(
                                &plan.mod_name,
                                root,
                                fields,
                                &as_name,
                            );
                            exports.entry(as_name).or_insert(slot);
                        }
                    }
                    ImportItems::Wildcard => {
                        // Module-value-chain wildcard (`import t4t: *`):
                        // root-name slot → file identity (the generic
                        // resolver), then field by field through the
                        // export table to the target file, and re-exported
                        // by **its** export surface (reachable by downstream
                        // named/wildcard imports; this arm used to be empty,
                        // which was the "src/util.typ does not export get"
                        // gap). Same-shaped chain as package.rs's
                        // process_import_edge.
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
            // External package bare/rename: re-export the binding to the
            // external module slot (package.rs generates an independent
            // IMPORT_{mod}_{name} slot per external import edge and
            // assembles it at runtime; registered here by the same rule so
            // downstream imports can reach it).
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
                // External package LIST: binding names re-exported too
                // (e.g. `_pkgs.typ` taking strfmt from oxifmt and being
                // imported downstream — the slot formula matches the
                // field-access generation side of package.rs verbatim).
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
                // External package WILDCARD: a pure re-export shim
                // (`#import "@preview/cetz:0.5.2": *` — its whole export
                // surface is the dependency's). When the spec was merged
                // into the translation (--with-deps), expand the merged
                // entry file's exports here (topological order has the
                // dependency settled first); downstream item imports from
                // the shim then resolve statically. Unmerged specs keep
                // the runtime wildcard layer.
                if let ImportItems::Wildcard = &edge.items {
                    let spec = spec_prefix(&edge.target);
                    if std::env::var_os("TRUST_TRACE_WILD").is_some() {
                        eprintln!(
                            "[ext-wild] {} -> spec={spec:?} entry={:?} row_has={:?}",
                            plan.path,
                            dep_entries.get(&spec),
                            table
                                .get(dep_entries.get(&spec).map(String::as_str).unwrap_or(""))
                                .map(|r| r.len())
                        );
                    }
                    // dep_entries maps spec → the dependency entry's full
                    // merge key ("@preview/cetz:0.5.2/src/lib.typ").
                    if let Some(entry_path) = dep_entries.get(&spec) {
                        if let Some(target_exports) = table.get(entry_path) {
                            for (wname, wslot) in target_exports {
                                exports
                                    .entry(wname.clone())
                                    .or_insert_with(|| wslot.clone());
                            }
                        }
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
                    // Target not in the plan set (unresolved edges like
                    // template placeholder specs; a bare index here once
                    // panicked): look up safely; on a miss the item cannot
                    // be expanded and is left empty — the real gap is
                    // reported at the later named use site.
                    let Some(target_exports) = table.get(&edge.target) else {
                        continue;
                    };
                    let target_exports = &target_exports.clone();
                    for (original, rename) in items {
                        // Dotted item: first segment resolved, remaining
                        // segments materialized into an independent slot
                        // (same formula as package.rs).
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
                    // Same as List: skip safely when the target is not in
                    // the plan set (template edges).
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

/// Module name for a file path (a bare import's name = the file stem).
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
        // No trailing path: the whole string is the spec (an untranslated
        // external edge; the target is the raw spec, e.g.
        // "@preview/oxifmt:1.0.0").
        None => target.to_string(),
    }
}

/// The plan-path an import edge effectively points at: internal edges →
/// their target; **merged** external edges (spec present in dep_entries)
/// → the dependency's entry merge key; unmerged externals → None (no
/// plan node — runtime layer). Feeding this through the graph walks
/// (topo order, cycle detection, dependency closures) is what makes
/// merged dependencies real graph citizens: their evaluation order, the
/// export-table settlement order, and the per-file assembly closures all
/// see them.
pub(super) fn edge_effective_target(
    edge: &ImportEdge,
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> Option<String> {
    if !edge.external {
        return Some(edge.target.clone());
    }
    let spec = spec_prefix(&edge.target);
    match dep_entries.get(&spec) {
        // The target may already be entry-qualified
        // ("@ns/name:ver/lib.typ" — path-form imports into a merged
        // package); use it as-is.
        Some(_) if edge.target.len() > spec.len() => Some(edge.target.clone()),
        // Bare spec: qualify with the dependency's entry merge key.
        Some(entry) => Some(entry.clone()),
        None => None,
    }
}

/// Slot name for a module-value List binding (`import cetz: draw, util`):
/// the module chain (`root.fields`) discriminates the cell from a
/// same-named static-import binding in the same file (typst's
/// last-binding-wins runs on two distinct slots; env/scope define order
/// picks the winner). Shared verbatim by the export table and the glue
/// generator.
pub(super) fn module_list_slot(
    plan_mod: &str,
    root: &str,
    fields: &[String],
    bound: &str,
) -> String {
    let mut chain = String::new();
    for f in fields {
        chain.push('_');
        chain.push_str(&sanitize_key(f));
    }
    format!(
        "IMPORT_{}_{}_VIA_{}{chain}",
        sanitize_key(plan_mod),
        sanitize_key(bound),
        sanitize_key(root)
    )
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

/// IMPORT slot → the **file identity** of that binding's module (generic
/// recursive resolution, depth bounded by the plan count): internal
/// bare/rename → the edge's target file; internal list item → the target
/// file's export-table slot for that item, resolved one more hop;
/// external edge → the entry merge key of the merged dependency. Feeds the
/// slot_files index and in-body/top-level module-value-chain wildcards
/// (e.g. `import t4t: *` where t4t came in via an internal list).
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
    // IMPORT_{mod}_{bound}: the mod name may contain underscores
    // (sanitize_key keeps underscores), so delimit by matching plan
    // prefixes (consistent with the existing plan-prefix matching).
    let (plan, bound): (&super::plan::FilePlan, String) = plans
        .iter()
        .find_map(|p| {
            let prefix = format!("IMPORT_{}_", sanitize_key(&p.mod_name));
            slot.strip_prefix(&prefix)
                .map(|b| (p, b.to_string()))
        })
        .or({
            // The regular shape (module name without underscores) also
            // needs a fallback (prefix matching takes the first plan in
            // order; None when no split is possible).
            None
        })?;
    let bound = &bound[..];
    for edge in &plan.imports {
        if edge.is_include {
            continue;
        }
        // The binding-name set this edge may produce for that slot
        // (bare/rename/list/rename-list).
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
                        // Dotted items use only the first segment as the
                        // binding name.
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
            // Module-name binding (bare stem / rename / rename-list module
            // name) → the edge's target file is the module identity.
            let module_binding = match &edge.items {
                ImportItems::Bare => module_stem(&edge.target),
                ImportItems::Rename(n) | ImportItems::RenameList(n, _) => n.clone(),
                _ => String::new(),
            };
            if !module_binding.is_empty() && sanitize_key(&module_binding) == bound {
                return Some(edge.target.clone());
            }
            // list/rename-list item: the target file's export table entry
            // for that item → slot, resolved one more hop.
            let table_key = if bound.contains('.') {
                bound.split('.').next().unwrap_or(bound)
            } else {
                bound
            };
            let next = export_table.get(&edge.target)?.get(table_key)?.clone();
            return resolve_import_slot_file(&next, plans, export_table, dep_entries, depth - 1);
        }
        // External edge: merged dependency → entry merge key.
        let spec = spec_prefix(&edge.target);
        return dep_entries.get(&spec).cloned();
    }
    None
}

/// Whether `from` reaches `to` through internal import edges (a `to == from`
/// hit only counts via a real edge chain; callers pass an edge's target and
/// the edge's source file — a hit means the edge closes an import cycle).
pub(super) fn import_reaches(
    plans: &[FilePlan],
    from: &str,
    to: &str,
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> bool {
    let owned: Vec<Option<String>> = plans
        .iter()
        .flat_map(|p| p.imports.iter().map(|e| edge_effective_target(e, dep_entries)))
        .collect();
    let mut edges: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
    let mut it = owned.iter();
    for plan in plans {
        for _ in &plan.imports {
            if let Some(Some(t)) = it.next() {
                edges.entry(plan.path.as_str()).or_default().push(t.as_str());
            }
        }
    }
    let mut visited: std::collections::BTreeSet<&str> = Default::default();
    let mut stack: Vec<&str> = edges.get(from).cloned().unwrap_or_default();
    while let Some(node) = stack.pop() {
        if node == to {
            return true;
        }
        if visited.insert(node) {
            if let Some(nexts) = edges.get(node) {
                stack.extend(nexts.iter().copied());
            }
        }
    }
    false
}
