//! Multi-file package driver: one typst package → Rust output in two
//! forms, a single file or a directory tree.
//!
//! Output shape (per-build slot board, see `_things/DESIGN-per-build.md`):
//! - `__State`: all top-level binding slots of the package + import slots
//!   (external packages, bare/rename module values), one instance per
//!   build;
//! - one module per source file: `eval(&__st, engine, context)` fills this
//!   file's slots in source order (the mirror of module evaluation);
//!   `module(&__st)` builds the file's export surface from the filled
//!   slots (including engine values);
//! - the top-level `module_init` drives the files' evaluation in
//!   topological order and returns the entry module.
//!
//! Single-file form (`render_single`): all file bodies inlined as flat
//! `mod` blocks — the locked baseline for snapshots; directory form
//! (`translate_package_tree`): mirrors the source tree, one rust file per
//! typ file, cross-file references through the root's `__State` and
//! `use super::*` chains (a private use is visible to descendants). When
//! `x.typ` and an `x/` directory coexist, the directory declaration is
//! appended to `x.rs` (a rust file module looks for child modules in
//! exactly the `x/` directory); otherwise the directory gets `mod.rs`.
//!
//! In-package `#import` resolves at translation time into cross-file slot
//! references; structure analysis lives in `plan.rs`.

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

/// The multi-file package to translate.
pub struct PackageInput {
    /// Relative paths (POSIX style, relative to the package root) →
    /// source. In the recursive-dependency form, translated dependency
    /// files are merged in under `{spec}/{path-inside-that-package}` keys
    /// (spec like `@preview/cetz:0.3.4`; compatible with in-file path
    /// normalization, and the numbered mod-name prefix avoids slot-name
    /// collisions naturally).
    pub files: std::collections::BTreeMap<String, String>,
    /// Entry file's relative path.
    pub entry: String,
    /// External packages merged into the translation: spec → the merge key
    /// of their entry (`{spec}/{entry}`). Import edges hitting these specs
    /// are **staticized** (at evaluation time the corresponding generated
    /// module is assembled directly, no runtime import_external
    /// interpretation); misses keep the runtime fallback (mixed
    /// execution).
    pub dep_entries: std::collections::BTreeMap<String, String>,
}

/// The emittable output for one typ file: the file body + module names for
/// both the single-file and directory forms.
struct FileOut {
    /// File body (the inside of a `mod` block; byte-identical to the
    /// single-file output).
    body: String,
    /// Flat mod name for the single-file form (`m{i}_{stem}`).
    flat_name: String,
    /// Full module path segments for the directory form (mirroring the typ
    /// path, e.g. `[src, draw, shapes]`).
    tree_full: Vec<String>,
}

/// Intermediate product of package translation: header (with
/// `{EVAL_CALLS}`/`{ITEMS}` placeholders) + the two-form eval drivers +
/// per-file bodies, assembled into the final output by
/// `render_single`/`translate_package_tree`. The empty state (empty
/// header, no files) corresponds to an early exit.
#[derive(Default)]
struct PackageParts {
    header: String,
    /// slot → nested access path prefix (`dir.file`), applied to the
    /// assembled outputs by `nest_state_access`.
    slot_paths: std::collections::HashMap<String, String>,
    eval_flat: String,
    eval_tree: String,
    /// module_by_path registry entries: (file path, dependency closure
    /// as (flat, tree) module-path pairs in topological order, self
    /// last). The arm text is derived at render time — the product only
    /// ever carries one form; the dual strings previously lived in
    /// translator memory.
    registry: Vec<(String, Vec<(String, String)>)>,
    files: Vec<FileOut>,
    /// Collection of C2 value-position closure native bodies (deduped,
    /// shared across files; emitted at the output root).
    closure_items: String,
    /// Per-call-site node statics (zero-lock creation protocol; emitted at
    /// the root).
    closure_nodes: String,
}

/// Lenient translation (for exploration): gaps degrade to placeholders;
/// returns (code, diagnostic list).
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


// ─── Staged assembly (mirrors typst-eval `eval()`'s directory-style entry:
// every step fully delegates; reading the entry shows the whole
// flow)──────────────────────────────────────

/// Parsing: one `Source` per file (spans carry a FileId, same origin as
/// the output's file_span).
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

/// Pass one: each file's plan (import edges / top-level binding slots /
/// parse diagnostics / mutable roots). The markup comes from `Box::leak`ed
/// static roots, so plans are entirely 'static.
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
                format!("{path} failed to parse as a markup module"),
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
        // Composite module names (slot prefixes / path references) use
        // sanitize_key — plain sanitize produces an r# prefix for
        // keywords, which is a lexical error when embedded in a composite
        // (mod.typ → m21_r#mod).
        let mod_name = format!("m{idx}_{}", sanitize_key(stem));
        let mut tops = Vec::new();
        let mut imports = Vec::new();
        for expr in markup.exprs() {
            match expr {
                ast::Expr::Space(_) | ast::Expr::Parbreak(_) => {}
                // (Markup like <Label> inside is not code.)
                ast::Expr::ModuleImport(import) => {
                    let mut source_node: Option<typst_syntax::SyntaxNode> = None;
                    let (raw, external, module_access) = match import.source() {
                        // String path: an external package or an
                        // in-package relative/absolute path.
                        ast::Expr::Str(path_expr) => {
                            let raw = path_expr.get().to_string();
                            let external = raw.starts_with('@');
                            (raw, external, None)
                        }
                        // Module-value expression import (`import
                        // deps.oxifmt: x`). Any other expression (e.g.
                        // `import plugin("…")`) stores the source node,
                        // evaluated into a module value and bound in pass
                        // two (mirrors typst-eval: an import source just
                        // has to evaluate to a module).
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
                        // External package: a merged dependency points at
                        // its entry merge key (the dependency graph /
                        // evaluation order follow); otherwise the raw spec
                        // is kept (runtime import_external interpretation).
                        input
                            .dep_entries
                            .get(&raw)
                            .cloned()
                            .unwrap_or_else(|| raw.clone())
                    } else {
                        let mut target = normalize_import(path, &raw);
                        // An **absolute** import inside a dependency package
                        // (`/src/x.typ`) is qualified into that package's
                        // merged key space (relative imports get the prefix
                        // naturally via directory joining; absolute ones
                        // would land at top-level keys and lose package
                        // ownership).
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
                    // A non-external Str not in the file set: no
                    // translate-time interception anymore — the world may
                    // still resolve it at runtime (suite fixtures etc.);
                    // hand it to the glue path and let the original
                    // import's file-not-found carry it.
                    let _ = &raw;
                    let items = match import.imports() {
                        None => match import.new_name() {
                            Some(new_name) => ImportItems::Rename(new_name.as_str().to_string()),
                            None => ImportItems::Bare,
                        },
                        Some(ast::Imports::Wildcard) => ImportItems::Wildcard,
                        // Module rename + member list: double binding
                        // (`import "base.typ" as base: ok, err`).
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
                                        // Full names carry a dotted path
                                        // (`a.b as c`):
                                        // original_name() gives only the
                                        // last segment, so the full name is
                                        // rejoined here; without a rename
                                        // the bound name = the last segment
                                        // (typst semantics).
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
                        // Statement text: zone-1 glue (rt::eval_import
                        // re-parses and drives the original Eval; the span
                        // is synthesized back onto the original statement).
                        stmt_text: Some(expr.to_untyped().full_text().to_string()),
                        raw: raw.clone(),
                        is_include: false,
                    });
                }
                ast::Expr::ModuleInclude(include) => {
                    // Include edge: enters the dependency graph (cycle
                    // detection / topo order), binds no names.
                    let Some(ast::Expr::Str(path_expr)) = Some(include.source()) else {
                        cx.diag(expr.span(), "include source must currently be a string path".into());
                        continue;
                    };
                    let raw = path_expr.get().to_string();
                    let target = normalize_import(path, &raw);
                    if !input.files.contains_key(&target) {
                        cx.diag(
                            expr.span(),
                            format!("include \"{raw}\" resolves to {target}, which is not in the file set"),
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
                        stmt_text: None,
                        raw: String::new(),
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
                // Other top-level expressions (text/embedded/set/show) are
                // module body content, collected and folded by pass two;
                // nothing to do here.
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

/// Package world assembly product: evaluation topological order, export
/// lookup table, path → mod name table.
struct PkgWorld {
    order: Vec<usize>,
    export_table: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
    package_mods: std::rc::Rc<std::collections::BTreeMap<String, String>>,
}

/// World assembly: cycle detection (None = cycle) → topo order / export
/// table → cx-side files/mods/export/slot_files registration and mutable
/// slot marking.
fn assemble_package_world(
    input: &PackageInput,
    plans: &[FilePlan],
    entry: &str,
    cx: &mut Ctx,
) -> Option<PkgWorld> {
    // Import cycle detection (typst likewise errors on circular module
    // evaluation).
    if has_import_cycle(plans, entry, &input.dep_entries) {
        cx.diags.push(Diag {
            span: typst_syntax::Span::detached(),
            message: "imports form a cycle within the package (typst's runtime likewise reports circular evaluation)".into(),
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

    // Topological order (dependencies first): evaluation order. The export
    // lookup table expands in the same order, so downstream list/wildcard
    // imports can reach upstream import re-exports.
    let order = topo_order(plans, &input.dep_entries);
    let export_table = build_export_table(plans, &order, &input.dep_entries);
        cx.export_table = Some(std::rc::Rc::new(export_table.clone()));

    // Mutable slots: the union of in-place-mutated root names, marking each
    // plan's own same-named slots (mirrors typst's in-place mutation
    // semantics for top-level bindings; cross-file imported names do not
    // mutate their own slot).
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

    // Slot name → file index (built once before pass two starts, used by
    // in-body module-value-chain wildcards):
    // Slot name → file index (built once, used by in-body
    // module-value-chain wildcards):
    // V{i}_{name} → plans[i].path; IMPORT_{mod}_{bound} → edge target.
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
    if let Some((root, fields)) = &edge.module_access {
    let via_bound = match &edge.items {
    ImportItems::Rename(n) | ImportItems::RenameList(n, _) => n.clone(),
    ImportItems::Bare => fields.last().cloned().unwrap_or_else(|| root.clone()),
    _ => bound.clone(),
    };
    idx.insert(
    super::plan::module_list_slot(&p.mod_name, root, fields, &via_bound),
    edge.target.clone(),
    );
    }
    }
    }
    // Backfill: LIST/RenameList item slots and cross-file chained
    // identities, via the generic resolver (e.g. `import t4t: *` where t4t
    // is a module introduced via an internal list item).
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
                        plans,
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

/// Pass-two single-file accumulator (the Vm equivalent: one bundle per
/// file, top-level statement generation shared; the package-level shared
/// `state_fields` is passed in by the caller).
struct TopGen {
    /// The currently visible env (import injection and binding visibility
    /// evolve in source order).
    env: Env,
    /// This file's processed top-level bindings (name, slot).
    own: Vec<(String, String)>,
    /// nth occurrence of same-named bindings (pairing destructured names
    /// with slots).
    own_seen: std::collections::HashMap<String, usize>,
    /// Evaluation-order binding code (inside the eval body).
    binding_code: String,
    /// Export-surface define sequence (inside the module body).
    file_defines: String,
    /// Function bodies and statics collection (file body).
    file_fns: String,
    /// Import slot fill sequence (head of the eval body).
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

    /// The visible env for content items (current env + prior top-level
    /// bindings).
    fn content_env(&self) -> Env {
        let mut e = self.env.clone();
        for (seen, seen_slot) in &self.own {
            e.module.push((seen.clone(), Ref::Slot(seen_slot.clone())));
        }
        e
    }

    /// Top-level `#let` (function/closure/plain binding/destructuring):
    /// plan-slot pairing, creation-time materialization of global names,
    /// binding code and export-surface writes. Mirrors eval's LetBinding
    /// branch.
    fn gen_let(
        &mut self,
        cx: &mut Ctx,
        plan: &super::plan::FilePlan,
        state_fields: &mut Vec<(String, String)>,
        lets: ast::LetBinding,
    ) {
            match lets.kind() {
                ast::LetBindingKind::Closure(name) => {
                    let Some(ast::Expr::Closure(closure)) = lets.init() else {
                        cx.diag(lets.span(), "closure binding is missing its closure initializer".into());
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
                    // Global names materialize at creation time (mirrors
                    // typst closure capture). Inject the closure's own
                    // bindings first (name/params/sink) — parameters are
                    // not free names (otherwise `__merge(base, …)`'s base
                    // was collected as a global slot; mirrors
                    // CapturesVisitor's Closure branch).
                    let mut g_env = env2.clone();
                    // Global names materialize at creation time (mirrors
                    // typst closure capture). The routing rules live with
                    // the capture analysis (see global_capture_slots).
                    let closure_body = closure.body().to_untyped();
                    let g_captures = super::captures::global_capture_slots(
                        super::captures::top_closure_free_names(closure),
                        &env2.module,
                        super::captures::has_wildcard_import(closure_body),
                        &slot,
                    );
                    for g in &g_captures {
                        state_fields.push((plan.path.clone(), g.slot.clone()));
                        let gname_escaped = escape(&g.name);
                        let resolver = if g.in_math { "capture_math_opt" } else { "capture_code_opt" };
                        let _ = writeln!(
                            self.binding_code,
                            "    if let Some(v) = rt::{resolver}(engine, \"{gname_escaped}\")? {{ rt::fill(&__st.{}, v)?; }}",
                            g.slot
                        );
                        g_env.module.push((
                            g.name.clone(),
                            Ref::GlobalFallback {
                                slot: g.slot.clone(),
                                name: gname_escaped,
                            },
                        ));
                    }
                    self.file_fns.push_str(&cx.gen_function(&fun, &slot, closure, &mut g_env));
                    self.file_fns.push_str(&gen_func_data(&data, &fun, name.as_str(), closure));
                    // Defaults materialize at creation time (mirrors typst:
                    // a closure evaluates its parameters when created; env2
                    // = imports + prior bindings, without the function
                    // itself). The same expression batch feeds the stub's
                    // defaults (used for missing arguments at call time,
                    // mirroring Closure.defaults semantics).
                    let mut defaults = Vec::new();
                    for param in closure.params().children() {
                        if let ast::Param::Named(named) = param {
                            let pname = named.name().as_str();
                            let def_slot = format!("DEF_{slot}_{}", sanitize_key(pname));
                            state_fields.push((plan.path.clone(), def_slot.clone()));
                            let value = cx.expr(named.expr(), &mut env2);
                            defaults.push(format!(
                                "{{ let v = {value}; rt::fill(&__st.{def_slot}, v.clone())?; v }}"
                            ));
                        }
                    }
                    // Native closure (scheme C): signature-faithful stub +
                    // state travels with the value. Pure functions that
                    // reference no module slots stay bare native (no state
                    // prefix, no take_state prologue; both sides decided by
                    // the same used_state flag).
                    let params_src = closure.params().to_untyped().full_text();
                    let defaults = if defaults.is_empty() {
                        "Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", defaults.join(", "))
                    };
                    // Uniform Closure-form stub: whether state is carried
                    // is an internal detail and must not leak into the
                    // external identity (a bare Native's scope()=Some would
                    // be dispatched as a native function by imports;
                    // mirrors the original's None for user closures).
                    let state_arg = if cx.used_state {
                        "Some(&rt::StateSlot::new(__st.clone()))"
                    } else {
                        "None"
                    };
                    let func_value = format!(
                        "rt::native_closure(&{data}, {params_src:?}, {defaults}, {state_arg})"
                    );
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
                    // Declaration without initializer (`#let x`): binds
                    // none (mirrors typst-eval LetBinding's define(name,
                    // None) for the no-init case).
                    let Some(init_expr) = lets.init() else {
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
                    // Each pattern name pairs with this file's next
                    // same-named slot (destructuring binds several).
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
                    // The init is evaluated before the binding takes
                    // effect: same-name references resolve to the outer
                    // scope (e.g. a global type); env2 excludes this
                    // binding. The single-name fast path applies only to
                    // **plain identifier patterns** — single-binding
                    // destructuring patterns like `(a,)` must still go
                    // through destructuring (binding the whole value was
                    // once the root cause of the destructuring-let
                    // single-element-array and dictionary-sink failures).
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
                        // Top-level destructuring: slot names are pre-bound
                        // as temporary locals; destructure writes back to
                        // these names, then each slot is filled and
                        // registered.
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
                            // Mutable slots (fill_ms/read_ms) and write-once
                            // slots (fill/read) split by the plan-time
                            // marking.
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
            message: format!("entry file {entry} is not in the file set"),
            file: None,
        });
        return PackageParts::default();
    }

    // Parsing: one Source per file (spans carry a FileId, same origin as
    // the output's file_span).
    let roots = parse_sources(input);
    // Pass one: each file's plan (import edges / top-level binding slots /
    // parse diagnostics / mutable roots). Note: the plan phase uses the cx
    // parameter directly (a locally shadowed same-named Ctx once swallowed
    // the parameter, so package-mode diagnostics never reached the shell
    // and strictness silently failed).
    let plans = plan_files(input, &roots, cx);
    if !cx.diags.is_empty() && !cx.lenient {
        // The shell returns Err.
        return PackageParts::default();
    }
    // World assembly: cycle detection → topo order / export table →
    // mutable-slot marking → slot→file table.
    let Some(world) = assemble_package_world(input, &plans, entry, cx) else {
        return PackageParts::default();
    };
    let PkgWorld { order, export_table, package_mods } = world;
    // Pass two: per file, generate eval (slot filling) + module (export
    // surface) + function bodies.
    let mut top_defines = String::new();
    let mut state_fields: Vec<(String, String)> = Vec::new();
    let mut files: Vec<FileOut> = Vec::new();
    let mut eval_flat = String::new();
    let mut eval_tree = String::new();
    let mut registry: Vec<(String, Vec<(String, String)>)> = Vec::new();
    for &idx in &order {
        let plan = &plans[idx];
        cx.current_file = Some(plan.path.clone());
        let is_entry = plan.path == entry;
        // Single-file accumulator (env evolves in source order; the import
        // slot table and edge cursor are local to this file).
        let mut top = TopGen::new();
        let mut import_slots: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        // Top-level import edges are no longer hoisted wholesale: handled
        // at their statement position by the source-order stream below (see
        // process_import_edge and the ModuleImport arm's edge cursor).
        let mut edge_cursor = 0usize;
        let mut content_items: Vec<super::expr::TopItem> = Vec::new();
        for expr in plan.markup.exprs() {
            // Content items: top-level text / embedded expressions /
            // set/show/include (per-item visibility).
            match expr {
                ast::Expr::ModuleImport(_) => {
                    // Source-order stream: the corresponding import edge is
                    // processed at this statement position (the env
                    // injection takes effect immediately — lets after it
                    // can see it; lets before it still resolve to the outer
                    // scope, mirroring native statement-by-statement
                    // evaluation).
                    if let Some(edge) = plan.imports.get(edge_cursor) {
                        edge_cursor += 1;
                        cx.process_import_edge(
                            edge,
                            plan,
                            input,
                            &plans,
                            &export_table,
                            &mut top.env,
                            &top.own,
                            &mut state_fields,
                            &mut top.import_fills,
                            &mut top.file_defines,
                            &mut import_slots,
                            &mut top.binding_code,
                        );
                    }
                    // Mirrors eval_markup: the statement joins the output
                    // as an empty display slot.
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(expr),
                        env: top.content_env(),
                    });
                }
                ast::Expr::LetBinding(lets) => {
                    // Mirrors eval_markup: the statement joins the output
                    // as an empty display slot.
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::Part(expr),
                        env: top.content_env(),
                    });
                    top.gen_let(cx, plan, &mut state_fields, lets);
                }
                ast::Expr::ModuleInclude(inc) => {
                    let item_env = top.content_env();
                    if let ast::Expr::Str(path_expr) = inc.source() {
                        let raw = path_expr.get().to_string();
                        let target = normalize_import(&plan.path, &raw);
                        if let Some(target_mod) = package_mods.get(&target) {
                            content_items.push(super::expr::TopItem {
                                item: super::expr::ContentItem::Include(format!(
                                    "BODY_{target_mod}"
                                )),
                                env: item_env,
                            });
                            continue;
                        }
                    }
                    // Dynamic source / out of range: zone-1 glue item
                    // (rt::eval_include drives the original Eval; the world
                    // may still resolve it, file-not-found reported by the
                    // original).
                    let stmt_text = expr.to_untyped().full_text().trim().to_string();
                    let at = cx.span_of(expr.span());
                    let refs: Vec<(String, Ref)> = top.env.module.clone();
                    let mut caps: Vec<String> = refs
                        .iter()
                        .map(|(n, r)| {
                            format!("(\"{}\", {})", escape(n), cx.ref_value(r))
                        })
                        .collect();
                    for (seen, seen_slot) in &top.own {
                        caps.push(format!(
                            "(\"{}\", rt::read(&__st.{seen_slot})?)",
                            escape(seen)
                        ));
                    }
                    let caps_lit = if caps.is_empty() {
                        "Vec::new()".to_string()
                    } else {
                        format!("vec![{}]", caps.join(", "))
                    };
                    content_items.push(super::expr::TopItem {
                        item: super::expr::ContentItem::DynamicInclude(format!(
                            "rt::eval_include({stmt_text:?}, {at}, &{caps_lit}, engine, context)?"
                        )),
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
        // Move out of the accumulator: the file assembly section consumes
        // it under the original local names.
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
            state_fields.push((plan.path.clone(), slot.clone()));
        }
        // Module body: top-level content folds in source order (rules wrap
        // subsequent content) into the slot.
        let body_slot = format!("BODY_{}", plan.mod_name);
        state_fields.push((plan.path.clone(), body_slot.clone()));
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

        let parse_guard_src = if plan.parse_diags.is_empty() {
            String::new()
        } else {
            let msgs: Vec<String> = plan
                .parse_diags
                .iter()
                .map(|m| format!("{m:?}"))
                .collect();
            // All parse diagnostics in one go (Err carries an EcoVec;
            // mirrors the official compile parse stage's collection — a
            // single bail once made multi-error tests mismatch).
            format!(
                "        return Err(eco_vec![{}]);\n",
                msgs.iter()
                    .map(|m| format!("typst::diag::SourceDiagnostic::error(file_span(), ecow::eco_format!(\"{{}}\", {m}))"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        // File body: the scaffolding macro (file_span anchor + eval/module
        // signature wrappers) around per-file bodies; module_body owns its
        // scope/FileId/Module construction (ident passthrough keeps the
        // pasted bodies hygiene-compatible with their own locals).
        let eval_body_src = format!(
            "let _ = (&*engine, context);\n{parse_guard_src}{import_fills}{binding_code}        Ok(())"
        );
        let body_slot_name = format!("BODY_{}", plan.mod_name);
        let module_body_src = format!(
            "let _ = __st;\n    let mut scope = Scope::new();\n{file_defines}    let __fid = typst::syntax::RootedPath::new(\n        typst::syntax::VirtualRoot::Project,\n        typst::syntax::VirtualPath::new({path_repr:?}).expect(\"path\"),\n    )\n    .intern();\n    Ok(Module::new({stem_repr:?}, scope).with_content(rt::read_content(&__st.{body_slot_name})?).with_file_id(__fid))",
            path_repr = plan.path,
            stem_repr = module_stem(&plan.path),
        );
        let mut body = format!(
            "\n    rt::file_module! {{\n\
            \x20       path: {path_repr:?},\n\
            \x20       state_ty: __State,\n\
            \x20       eval_body(__st, engine, context) {{ {eval_body_src} }}\n\
            \x20       module_body(__st, engine, context) {{ {module_body_src} }}\n\
            \x20       file_fns {{ {file_fns_src} }}\n\
            \x20   }}",
            path_repr = plan.path,
            file_fns_src = file_fns,
        );
        body.push('\n');

        let tree_full: Vec<String> = plan
            .path
            .strip_suffix(".typ")
            .unwrap_or(&plan.path)
            .split('/')
            // Tree segments double as file names and module declarations:
            // an `r#` prefix would end up in a file name (illegal) and
            // disagree with the mod declaration/path — use sanitize_key
            // (strips r#, adds _kw).
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
            let _ = writeln!(eval_flat, "    // Entry `{entry}` (dependencies were evaluated above).");
            let _ = writeln!(eval_tree, "    // Entry `{entry}` (dependencies were evaluated above).");
        }
        let _ = writeln!(
            eval_flat,
            "    {}::eval(&__st, engine, context)?;",
            plan.mod_name
        );
        let _ = writeln!(eval_tree, "    {tree_path}::eval(&__st, engine, context)?;");
        // Assemble-by-path arm: evaluate the dependency closure in
        // topological order + the file's own evaluation and assembly (a
        // fresh slot board; a single test assembles just that file — same
        // order of magnitude as the interpreter's single-file compile).
        let closure = dep_closure(&plans, idx, &input.dep_entries);
        let mut pairs: Vec<(String, String)> = Vec::new();
        for &j in &closure {
            let tree = plans[j]
                .path
                .strip_suffix(".typ")
                .unwrap_or(&plans[j].path)
                .split('/')
                .map(sanitize_key)
                .collect::<Vec<_>>()
                .join("::");
            pairs.push((plans[j].mod_name.clone(), tree));
        }
        registry.push((plan.path.clone(), pairs));
    }

    let degraded_header = if cx.lenient && !cx.diags.is_empty() {
        let mut header = String::from("// Degradation list (lenient mode; missing features, warned at runtime):\n");
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
    // Per-file nested state: slots group by their owning file, mirroring
    // the source tree (directory levels become intermediate structs, a
    // file node holds its slots; a file sharing a stem with a sibling
    // directory absorbs the directory's children as extra fields).
    // Access stays O(1) — nested structs have no indirection — and the
    // grouping keeps thousands of slots readable (`__st.dir.file.SLOT`).
    let mut seen_fields = std::collections::BTreeSet::new();
    let mut root = StateNode::default();
    let mut slot_paths: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    // Dedup: duplicate parameter names in the source (e.g. the error test
    // `f(v: 1, v: 2)`) register the same default slot twice; a duplicate
    // field makes the output uncompilable. The semantic divergence for
    // duplicate parameters is naturally carried by that error test.
    for (file, slot) in &state_fields {
        if !seen_fields.insert(slot.clone()) {
            continue;
        }
        let node = root.node_for(file);
        node.slots.push((
            slot.clone(),
            cx.mutable_slots.contains(slot.as_str()),
        ));
        slot_paths.insert(
            slot.clone(),
            file_segments(file).join("."),
        );
    }
    let mut decls = String::new();
    let mut fields = String::new();
    root.emit("__S", &mut decls, &mut fields, true);

    let span_consts = cx.span_consts();
    let header = format!(
        "// Generated by trust from package `{module_name}` (entry {entry}); do not edit.\n{degraded_header}\n\n\
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
{decls}\
         /// Evaluation state of this package: all top-level binding slots and import slots (one instance per build).\n\
         #[derive(Default)]\n\
         pub struct __State {{\n{fields}}}\n\n\
         /// Mirror of package evaluation: the host calls this once per build (World); evaluates the files in topological order.\n\
         pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {{\n    let __st = std::sync::Arc::new(__State::default());\n{{EVAL_CALLS}}    let mut scope = Scope::new();\n{top_defines}    Ok(Module::new(\"{module_name}\", scope).with_content(rt::read_content(&__st.BODY_{entry_mod})?))\n}}\n\n\
         /// One-line integration: drives module_init and returns the module (to inject into a host library use\n\
         /// `rt::bind_module`; the test world's InjectWorld::init_native_module already wires it).\n\
         pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {{\n    rt::init_module(world, library, module_init)\n}}\n\n\
         /// Creates a fresh evaluation slot board (for batch drivers managing their own lifecycle).\n\
         pub fn new_state() -> std::sync::Arc<__State> {{ std::sync::Arc::new(__State::default()) }}\n\n\
         /// Assembles on demand by file path: on the given slot board, first evaluates the file's\n\
         /// dependency closure in topological order, then evaluates the file itself and assembles its\n\
         /// module. Drives the official tests/suite batch runner: each test gets a fresh board +\n\
         /// single-file assembly, all inside **one** binary (the translated output is compiled once;\n\
         /// the compile count is independent of the test count).\n\
         pub fn module_by_path(path: &str, board: &std::sync::Arc<__State>, engine: &mut Engine, context: Tracked<Context>) -> Option<SourceResult<Module>> {{\n    match path {{\n{{REGISTRY}}        _ => None,\n    }}\n}}\n{{ITEMS}}"
    );
    PackageParts {
        header,
        slot_paths,
        eval_flat,
        eval_tree,
        registry,
        files,
        closure_items: std::mem::take(&mut cx.closure_items),
        closure_nodes: std::mem::take(&mut cx.closure_nodes),
    }
}

/// Single-file form: the header's placeholders are replaced with the flat
/// eval driver and all `mod` blocks. Assembled from `push_str` segments
/// (the `{{}}}}` escapes of a `writeln!` format string are error-prone);
/// byte-identical to the pre-refactor output and locked by snapshots.
///
/// Render module_by_path arms from the registry table. `module_path`
/// maps a plan to its reference path (flat `m5_foo` or tree
/// `dir::foo`); the arm assembles the dependency closure in
/// topological order, then the file's own eval+module.
fn render_registry(
    registry: &[(String, Vec<(String, String)>)],
    tree: bool,
) -> String {
    let pick = |p: &(String, String)| if tree { p.1.clone() } else { p.0.clone() };
    let mut out = String::new();
    for (path, closure) in registry {
        let _ = writeln!(out, "        \"{path}\" => Some((|| {{");
        for pair in closure {
            let _ = writeln!(out, "            {}::eval(board, engine, context)?;", pick(pair));
        }
        let _ = writeln!(
            out,
            "            {}::module(board, engine, context)\n        }})()),",
            pick(closure.last().expect("closure includes self"))
        );
    }
    out
}

fn render_single(parts: &PackageParts) -> String {
    let mut items = String::new();
    for f in &parts.files {
        items.push_str("\nmod ");
        items.push_str(&f.flat_name);
        items.push_str(" {\n    use super::*;\n");
        items.push_str(&f.body);
        items.push_str("}\n");
    }
    // C2 closure native bodies and per-call-site node statics (at the root,
    // with visibility of __State and the imports).
    items.push_str(&parts.closure_items);
    items.push_str(&parts.closure_nodes);
    let rendered = parts
        .header
        .replace("{EVAL_CALLS}", &parts.eval_flat)
        .replace("{REGISTRY}", &render_registry(&parts.registry, false))
        .replace("{ITEMS}", &items);
    nest_state_access(&rendered, &parts.slot_paths)
}

/// Directory form: a multi-file output mirroring the source tree (relative
/// path → content, root being `mod.rs`). One rust file per typ file (first
/// line `use super::*;`, the file body dedented one level from the
/// single-file `mod` block, `pub(super)` widened to `pub(crate)`, and the
/// flat module path `m<i>_<stem>::module` replaced by the tree path); when
/// `x.typ` and an `x/` directory coexist, the child-module declarations
/// are appended to the tail of `x.rs`, otherwise the directory gets a
/// `mod.rs` (declaring `use super::*;` + `pub mod child;`). The glob chain
/// relies on "a parent's private use is visible to descendants", the same
/// mechanism as the single-file form.
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

/// Lenient directory form: gaps degrade to placeholders; returns (file
/// tree, diagnostic list).
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

    // Root `mod.rs`: header (tree-form eval driver) + top-level module
    // declarations.
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
        .replace("{REGISTRY}", &render_registry(&parts.registry, true))
        .replace("{ITEMS}", &root_decls);
    let cx_nodes = std::mem::take(&mut parts.closure_nodes);
    // The C2 closure section gets the same treatment as file bodies:
    // flat-module references to other files inside the bodies
    // (`{flat}::module(` and the hoisted-body-qualified `{flat}::file_span(`)
    // are replaced with tree paths.
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
    tree.insert("mod.rs".to_string(), nest_state_access(&root, &parts.slot_paths));

    // Directory → direct child segments (ordered, deduped); the root is
    // the empty segment, depth = segment index.
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

    // typ file → mirrored `.rs`; a file with the same name as a directory
    // doubles as that directory's declaration.
    let by_tree: std::collections::HashMap<Vec<String>, ()> = parts
        .files
        .iter()
        .map(|f| (f.tree_full.clone(), ()))
        .collect();
    for f in &parts.files {
        let mut body = dedent4(&f.body);
        // References to other files inside the body (`{flat}::module(`,
        // import fills and module-path expressions) are replaced with tree
        // paths; flat names are unique, so the prefix replacement is
        // unambiguous.
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
        // Rust file names use sanitize segments (consistent with the
        // module declarations: `mark-shapes.typ` → `mark_shapes.rs`).
        let rel = format!("{}.rs", f.tree_full.join("/"));
        let body = nest_state_access(&body, &parts.slot_paths);
        tree.insert(rel, format!("use super::*;\n{body}"));
    }
    // Directories with no same-named file: `dir/mod.rs`.
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

/// Strip one level of indentation (the single-file `mod` block's 4 spaces)
/// for the directory form (lines with fewer leading spaces pass through
/// unchanged).
fn dedent4(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for line in s.split_inclusive('\n') {
        out.push_str(line.strip_prefix("    ").unwrap_or(line));
    }
    out
}



/// Collect the root names of bindings mutated in place: targets of
/// mutating method calls (push/pop/insert/remove) and of
/// assignments/compound assignments (mirrors typst's mutable access
/// semantics — module bindings are likewise mutable in place).
pub(super) fn collect_mutated_roots(node: &typst_syntax::SyntaxNode, out: &mut Vec<String>) {
    if let Some(fc) = node.cast::<typst_syntax::ast::FuncCall>()
        && let typst_syntax::ast::Expr::FieldAccess(access) = fc.callee()
        && matches!(access.field().as_str(), "push" | "pop" | "insert" | "remove")
        && let Some(root) = place_root(access.target().to_untyped()) {
            out.push(root);
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
    // Destructuring assignment `(a, b, ..c) = ...` / `(a.at(i),) = ...`:
    // all placeable-leaf roots in the pattern (mirrors binding.rs
    // DestructAssignment's leaves going through access).
    if let Some(assign) = node.cast::<typst_syntax::ast::DestructAssignment>() {
        collect_pattern_roots(assign.pattern().to_untyped(), out);
    }
    for child in node.children() {
        collect_mutated_roots(child, out);
    }
}

/// Collect placeable-leaf roots of a destructuring pattern (leaf = Ident,
/// or a field/accessor chain rooted at an Ident; duplicates are harmless —
/// the consumer is a set).
fn collect_pattern_roots(node: &typst_syntax::SyntaxNode, out: &mut Vec<String>) {
    if let Some(root) = place_root(node) {
        out.push(root);
    }
    for child in node.children() {
        collect_pattern_roots(child, out);
    }
}

/// The root name of a placeable path (an Ident, or a field/accessor chain
/// starting at an Ident — including `at`/`first`/`last` accessor calls;
/// None otherwise).
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

/// A file's dependency closure (including itself), dependencies first
/// (consistent with topo_order's visit; include edges count as
/// dependencies too).
fn dep_closure(
    plans: &[super::plan::FilePlan],
    idx: usize,
    dep_entries: &std::collections::BTreeMap<String, String>,
) -> Vec<usize> {
    fn visit(
        i: usize,
        plans: &[super::plan::FilePlan],
        dep_entries: &std::collections::BTreeMap<String, String>,
        order: &mut Vec<usize>,
        state: &mut Vec<u8>,
    ) {
        if state[i] != 0 {
            return;
        }
        state[i] = 1;
        for edge in &plans[i].imports {
            if let Some(target) =
                super::plan::edge_effective_target(edge, dep_entries)
                && let Some(j) = plans.iter().position(|p| p.path == target)
            {
                visit(j, plans, dep_entries, order, state);
            }
        }
        state[i] = 2;
        order.push(i);
    }
    let mut order = Vec::new();
    let mut state = vec![0u8; plans.len()];
    visit(idx, plans, dep_entries, &mut order, &mut state);
    order
}

/// Target file of an item binding (module field): base module entry export
/// table → item slot → file index (IMPORT slots only — their edge target
/// is the module's file identity). Feeds the static expansion of in-body
/// `import item: *`; returns None when the chain is unavailable and the
/// wildcard falls to the runtime layer.
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

/// Take the short name from a package spec (`@preview/oxifmt:1.0.0` →
/// `oxifmt`). Extracting the spec prefix of a merge key
/// `{spec}/{path-inside-package}` (the spec itself contains `/`; bounded
/// by the first `/` after the colon); returns an empty string for
/// non-merge keys.
///
/// Slot name → owning file (`V{i}_…` where i is the plan index;
/// `IMPORT_{mod}_…` reverse-looks-up that mod's internal import edge
/// target). Used by module-value-chain wildcard resolution.
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
        // The mod_name itself contains underscores (m55_anchors) — strip
        // by "plan prefix + _".
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
            // External packages (including merged dependencies): the
            // binding name is the spec name or the rename.
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
            let via = edge.module_access.as_ref().map(|(root, fields)| {
                let via_bound = match &edge.items {
                    super::plan::ImportItems::Rename(name)
                    | super::plan::ImportItems::RenameList(name, _) => name.clone(),
                    super::plan::ImportItems::Bare => {
                        fields.last().cloned().unwrap_or_else(|| root.clone())
                    }
                    _ => bound.clone(),
                };
                super::plan::module_list_slot(&plan.mod_name, root, fields, &via_bound)
            });
            if format!("IMPORT_{}_{}", plan.mod_name, bound) == *slot
                || via.as_deref() == Some(slot)
            {
                return Some(edge.target.clone());
            }
        }
    }
    None
}

/// A module-level reachable copy of module_stem (pub(super) inside
/// plan.rs).
fn module_stem_pub(path: &str) -> String {
    path.rsplit('/')
        .next()
        .unwrap_or("m")
        .split('.')
        .next()
        .unwrap_or("m")
        .to_string()
}

#[allow(clippy::too_many_arguments)]
fn import_edge_via_glue(
    cx: &mut Ctx,
    plan: &super::plan::FilePlan,
    edge: &super::plan::ImportEdge,
    env: &mut Env,
    own: &[(String, String)],
    state_fields: &mut Vec<(String, String)>,
    // The statement-position sink (binding_code): the glue evaluation
    // depends on the top-level let slots above it, so it must not be
    // hoisted with import_fills (static edges' fill order is irrelevant,
    // hoisting them is harmless; a glue edge's evaluation position = its
    // source-order statement position, mirroring the original's
    // statement-by-statement evaluation).
    inline_sink: &mut String,
    file_defines: &mut String,
    bare_expected: Option<&str>,
) {
    use std::fmt::Write as _;
    let stmt = edge
        .stmt_text
        .as_deref()
        .expect("glue edge carries its statement text (captured at plan time)");
    let expected: Vec<String> = match &edge.items {
        ImportItems::Bare => bare_expected
            .map(|n| vec![n.to_string()])
            .unwrap_or_default(),
        ImportItems::Rename(n) => vec![n.clone()],
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
            .map(|(i, r)| r.clone().unwrap_or_else(|| i.clone()))
            .collect(),
        ImportItems::Wildcard => unreachable!("wildcards do not take the set-difference glue (layer mechanism)"),
    };
    // Materialize the visible names (the current env's module domain;
    // mirrors the original: the lexical surface at the statement position).
    let refs: Vec<(String, Ref)> = env.module.clone();
    let mut caps: Vec<String> = refs
        .iter()
        .map(|(n, r)| format!("(\"{}\", {})", escape(n), cx.ref_value(r)))
        .collect();
    // This file's prior top-level bindings (env.module excludes own; they
    // must be merged in explicitly).
    for (seen, seen_slot) in own {
        caps.push(format!(
            "(\"{}\", rt::read(&__st.{seen_slot})?)",
            escape(seen)
        ));
    }
    let at = cx.span_of(edge.span);
    let expected_lit = if expected.is_empty() {
        "&[]".to_string()
    } else {
        format!(
            "&[{}]",
            expected
                .iter()
                .map(|n| format!("{n:?}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };
    let caps_lit = if caps.is_empty() {
        "Vec::new()".to_string()
    } else {
        format!("vec![{}]", caps.join(", "))
    };
    let mut slot_fills = String::new();
    // Glue-minted slots are chain-discriminated (module_list_slot): a
    // module-value List (`import cetz: draw, util`) can bind the same
    // name as a later static import (`import "util.typ"`) — typst's
    // last-binding-wins needs two distinct slots (the shared formula made
    // both edges fill one cell; the fill guard caught it as a refill).
    // env/module define order keeps the last binding effective; the
    // export table uses the same formula verbatim.
    let (root, fields) = edge
        .module_access
        .clone()
        .unwrap_or_else(|| (String::new(), Vec::new()));
    for (i, name) in expected.iter().enumerate() {
        let slot = super::plan::module_list_slot(&plan.mod_name, &root, &fields, name);
        state_fields.push((plan.path.clone(), slot.clone()));
        let _ = writeln!(
            slot_fills,
            "        rt::fill(&__st.{slot}, __imported[{i}].clone())?;"
        );
        env.module.push((name.clone(), Ref::Slot(slot.clone())));
        let _ = writeln!(
            file_defines,
            "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
            escape(name)
        );
    }
    let _ = writeln!(
        inline_sink,
        "    {{ let __caps: Vec<(&'static str, Value)> = {caps_lit}; let __imported = rt::eval_import({stmt:?}, {at}, {expected_lit}, &__caps, engine, context)?; {slot_fills}    }}"
    );
}


impl Ctx {
    /// Handling of a single top-level import edge (env injection + slot
    /// filling + export surface; called by pass two's **source-order
    /// stream** at the ModuleImport statement position — whether an import
    /// precedes or follows a let affects name resolution, consistent with
    /// native statement-by-statement evaluation, so it must not be hoisted
    /// wholesale).
    #[allow(clippy::too_many_arguments)]
/// Zone-1 glue edge handling: the statement text drives the original
/// `Eval for ast::ModuleImport` via `rt::eval_import` (source
/// dispatch/loading including file-not-found plus the cycle/bare-name
/// tiers/Items nesting and unresolved/all warnings — all original code);
/// the difference set fills slots and re-exports by the statically known
/// binding names.
/// `bare_expected`: the static name for the bare form (the stem of a Str
/// literal / the last segment of a module chain or the root name); pass
/// None for a dynamic-source bare (the original reports the dynamic error
/// at runtime).
    /// One top-level import edge: env injection + slot/define emission,
    /// dispatched by edge kind (external package / arbitrary-expression
    /// source / module-value chain / internal file target). `continue` in
    /// the former single-pass body means "skip this edge" — each processor
    /// returns instead.
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
        env: &mut Env,
        own: &[(String, String)],
        state_fields: &mut Vec<(String, String)>,
        import_fills: &mut String,
        file_defines: &mut String,
        import_slots: &mut std::collections::HashMap<String, String>,
        binding_code: &mut String,
    ) {
        if edge.is_include {
            return;
        }
        let mut io = ImportEdgeIo {
            env,
            own,
            state_fields,
            import_fills,
            file_defines,
            import_slots,
            binding_code,
        };
        if edge.external {
            self.import_edge_external(edge, plan, input, plans, export_table, &mut io);
        } else if edge.source_node.is_some() {
            self.import_edge_source_expr(edge, plan, &mut io);
        } else if edge.module_access.is_some() {
            self.import_edge_module_value(edge, plan, plans, export_table, &mut io);
        } else {
            self.import_edge_internal(edge, plan, input, plans, export_table, &mut io);
        }
    }

    /// External package import (incl. merged dependencies): slot + fill
    /// via static assembly or `import_external`, bindings by item shape.
    #[allow(clippy::too_many_arguments)]
    fn import_edge_external(
        &mut self,
        edge: &super::plan::ImportEdge,
        plan: &super::plan::FilePlan,
        input: &PackageInput,
        plans: &[super::plan::FilePlan],
        export_table: &std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        >,
        io: &mut ImportEdgeIo<'_>,
    ) {
        let ImportEdgeIo {
            env,
            own: _,
            state_fields,
            import_fills,
            file_defines,
            import_slots: _,
            binding_code: _,
        } = io;
        if edge.external {
            // A merged dependency: the spec prefixes the target (the
            // entry merge key; the spec itself contains `/`, taken up
            // to the first `/` after the colon); at evaluation time
            // the generated module is assembled statically, without
            // import_external.
            let spec = spec_prefix(&edge.target);
            let name = match &edge.items {
                ImportItems::Rename(name) => name.clone(),
                _ => spec_name(&spec),
            };
            let slot = format!("IMPORT_{}_{}", sanitize_key(&plan.mod_name), sanitize_key(&name));
            state_fields.push((plan.path.clone(), slot.clone()));
            let ext_at = self.span_of(edge.span);
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
                // Module rename + member list: double binding.
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
                                file: item_file(export_table, &self.slot_files, &edge.target, item),
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
                                file: item_file(export_table, &self.slot_files, &edge.target, item),
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
                        // Wildcard of a merged dependency: expanded via
                        // the entry export table (same shape as LIST,
                        // materializing field accesses).
                        for wname in export_table[&edge.target].keys() {
                            if env.module.iter().any(|(n, _)| n == wname) {
                                continue;
                            }
                            env.module.push((
                                wname.clone(),
                                Ref::ModuleField {
                                    expr: format!("rt::read(&__st.{slot})?"),
                                    name: wname.clone(),
                                    file: item_file(export_table, &self.slot_files, &edge.target, wname),
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
                        self.diag(edge.span, "wildcard import from an external package is not supported (the package must be translated)".into());
                    }
                }
            }
        }
    }

    /// Arbitrary-expression source (`import plugin("…") as hypher`): the
    /// source is translated and evaluated into a module value, and the
    /// bindings then follow the module-value import shape (mirrors
    /// typst-eval: an import source just has to evaluate to a module).
    fn import_edge_source_expr(
        &mut self,
        edge: &super::plan::ImportEdge,
        plan: &super::plan::FilePlan,
        io: &mut ImportEdgeIo<'_>,
    ) {
        let ImportEdgeIo {
            env,
            own,
            state_fields,
            import_fills,
            file_defines,
            import_slots: _,
            binding_code,
        } = io;
        if edge.source_node.is_some() {
            // Zone-1 glue: the arbitrary-expression source dispatched
            // through the original Eval (module/Str/Path/validation/
            // warnings all original), difference set filling slots;
            // wildcards take the layer + export-surface enumeration.
            if matches!(edge.items, ImportItems::Wildcard) {
                let expr_node = edge
                    .source_node
                    .as_ref()
                    .and_then(|n| n.cast::<ast::Expr>())
                    .expect("source_node casts");
                let mut env_m = env.clone();
                let value = self.expr(expr_node, &mut env_m);
                let mslot = format!(
                    "IMPORT_{}_WILD_SRC",
                    sanitize_key(&plan.mod_name)
                );
                state_fields.push((plan.path.clone(), mslot.clone()));
                let at = self.span_of(edge.span);
                let _ = writeln!(import_fills, "    rt::fill(&__st.{mslot}, {value})?;");
                let w = format!("__wild{}", self.fresh_id());
                let _ = writeln!(
                    import_fills,
                    "    let {w} = rt::wild_import(&({value}), {at})?;"
                );
                env.wildcards.push(w);
                let _ = writeln!(
                    file_defines,
                    "    if let Some(__sc) = rt::read(&__st.{mslot})?.scope() {{ for (__k, __b) in __sc.iter() {{ scope.bind(__k.clone(), __b.clone()); }} }}"
                );
            } else {
                import_edge_via_glue(
                    self,
                    plan,
                    edge,
                    env,
                    own,
                    state_fields,
                    binding_code,
                    file_defines,
                    None,
                );
            }
        }
    }

    /// Module-value expression import (`import deps.oxifmt: strfmt`):
    /// non-wildcard shapes go through the runtime glue; wildcards expand
    /// via the static resolution chain with a runtime-layer fallback.
    fn import_edge_module_value(
        &mut self,
        edge: &super::plan::ImportEdge,
        plan: &super::plan::FilePlan,
        plans: &[super::plan::FilePlan],
        export_table: &std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        >,
        io: &mut ImportEdgeIo<'_>,
    ) {
        let ImportEdgeIo {
            env,
            own,
            state_fields,
            import_fills,
            file_defines,
            import_slots: _,
            binding_code,
        } = io;
        if let Some((root, fields)) = &edge.module_access {
            // Non-wildcard shapes always go through glue (the value's
            // type is only known at runtime: module/Str/Path/global
            // type's scope; the original's dispatch + validation covers
            // everything, including the no-effect warning for `import
            // enum` and the dynamic error for `import p` where p is a
            // string).
            if !matches!(edge.items, ImportItems::Wildcard) {
                let bare =
                    fields.last().cloned().unwrap_or_else(|| root.clone());
                import_edge_via_glue(
                    self,
                    plan,
                    edge,
                    env,
                    own,
                    state_fields,
                    binding_code,
                    file_defines,
                    Some(&bare),
                );
                return;
            }
            // Wildcard: expanded via the static resolution chain (the
            // root must be visible); a root that is not (the scope of a
            // global type/function, e.g. `import assert.with(true): *`)
            // takes the runtime layer + runtime export-surface
            // enumeration (mirroring the original wildcard branch's
            // per-name `scope.iter()` bind).
            let base = match env.module.iter().rev().find(|(n, _)| n == root) {
                Some((_, module_ref)) => self.ref_value(module_ref),
                None => {
                    let at = self.span_of(edge.span);
                    format!("rt::global(engine, \"{root}\", {at})?")
                }
            };
            let mut expr = base.clone();
            for field in fields {
                expr = format!(
                    "rt::field(&({expr}), \"{}\", engine, context, SPAN)?",
                    escape(field)
                );
            }
            // Bindings go into **slots** (filled at eval time +
            // re-exported through the module scope): the same shape as
            // external package imports — a downstream `import
            // "this-file": name` can reach them (mirroring the
            // original: the module scope contains names introduced by
            // imports). These used to be immediate expressions — usable
            // within the file but not re-exportable, which hit the
            // `#import cetz: vector` case (a sub-module value of an
            // external module).
            match &edge.items {
                ImportItems::Bare => {
                    let name = fields.last().cloned().unwrap_or_else(|| root.clone());
                    // module_list_slot verbatim (same formula as the
                    // glue minter and the export table — see plan.rs).
                    let slot = super::plan::module_list_slot(
                        &plan.mod_name,
                        root,
                        fields,
                        &name,
                    );
                    state_fields.push((plan.path.clone(), slot.clone()));
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
                    let slot = super::plan::module_list_slot(
                        &plan.mod_name,
                        root,
                        fields,
                        name,
                    );
                    state_fields.push((plan.path.clone(), slot.clone()));
                    let _ = writeln!(
                        import_fills,
                        "    rt::fill(&__st.{slot}, {expr})?;"
                    );
                    env.module.push((name.clone(), Ref::Slot(slot.clone())));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::read(&__st.{slot})?);",
                        escape(name)
                    );
                }
                // Module rename + member list: double binding.
                ImportItems::RenameList(mname, list) => {
                    let mslot = format!(
                        "IMPORT_{}_{}",
                        sanitize(&plan.mod_name),
                        sanitize(mname)
                    );
                    state_fields.push((plan.path.clone(), mslot.clone()));
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
                        state_fields.push((plan.path.clone(), slot.clone()));
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
                        state_fields.push((plan.path.clone(), slot.clone()));
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
                    // Wildcard of a module-value chain: after the
                    // static resolution chain (root → file, then field
                    // by field through the export table to the target
                    // file), expand by that file's export table (e.g.
                    // `import cetz.draw: *`).
                    let mut cur: Option<String> = None;
                    if let Some((_, r#ref)) =
                        env.module.iter().rev().find(|(n, _)| n == root)
                        && let super::ctx::Ref::Slot(slot) = r#ref {
                            cur = slot_target_file(slot, plans);
                            if std::env::var_os("TRUST_TRACE_WILD").is_some() {
                                eprintln!("[wild] root={root} slot={slot} → {cur:?}");
                            }
                        }
                    for field in fields {
                        let next = cur
                            .as_ref()
                            .and_then(|f| export_table.get(f.as_str()))
                            .and_then(|exports| exports.get(field))
                            .and_then(|slot| slot_target_file(slot, plans));
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
                        for wname in exports.keys() {
                            if env.module.iter().any(|(n, _)| n == wname) {
                                continue;
                            }
                            env.module.push((
                                wname.clone(),
                                Ref::ModuleField {
                                    expr: base.clone(),
                                    name: wname.clone(),
                                    file: item_file(export_table, &self.slot_files, &target, wname),
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
                        // Static chain unavailable: runtime layer
                        // (in-body names resolve through the layer) +
                        // runtime export-surface enumeration (per-name
                        // scope bind, mirroring the original wildcard
                        // branch).
                        let mslot = format!(
                            "IMPORT_{}_WILD_{}",
                            sanitize_key(&plan.mod_name),
                            sanitize_key(root)
                        );
                        state_fields.push((plan.path.clone(), mslot.clone()));
                        let at = self.span_of(edge.span);
                        let _ = writeln!(
                            import_fills,
                            "    rt::fill(&__st.{mslot}, {expr})?;"
                        );
                        let w = format!("__wild{}", self.fresh_id());
                        let _ = writeln!(
                            import_fills,
                            "    let {w} = rt::wild_import(&({expr}), {at})?;"
                        );
                        env.wildcards.push(w);
                        let _ = writeln!(
                            file_defines,
                            "    if let Some(__sc) = rt::read(&__st.{mslot})?.scope() {{ for (__k, __b) in __sc.iter() {{ scope.bind(__k.clone(), __b.clone()); }} }}"
                        );
                    }
                }
            }
        }
    }

    /// Internal file-target import: unknown targets fall back to the
    /// runtime glue (the world may still resolve them at runtime; the
    /// original file-not-found carries through); known targets emit
    /// slot/define pairs per item shape, behind the cyclic-import guard.
    #[allow(clippy::too_many_arguments)]
    fn import_edge_internal(
        &mut self,
        edge: &super::plan::ImportEdge,
        plan: &super::plan::FilePlan,
        input: &PackageInput,
        plans: &[super::plan::FilePlan],
        export_table: &std::collections::BTreeMap<
            String,
            std::collections::BTreeMap<String, String>,
        >,
        io: &mut ImportEdgeIo<'_>,
    ) {
        let ImportEdgeIo {
            env,
            own,
            state_fields,
            import_fills,
            file_defines,
            import_slots,
            binding_code: _,
        } = io;
        let Some(target_plan) = plans.iter().find(|p| p.path == edge.target) else {
            // Wildcard out of range: the module value is materialized
            // via import_external (public import, including
            // file-not-found); runtime layer + runtime export-surface
            // enumeration.
            if matches!(edge.items, ImportItems::Wildcard) && !edge.raw.is_empty() {
                let mslot = format!(
                    "IMPORT_{}_WILD",
                    sanitize_key(&plan.mod_name)
                );
                state_fields.push((plan.path.clone(), mslot.clone()));
                let at = self.span_of(edge.span);
                let raw_escaped = escape(&edge.raw);
                let _ = writeln!(
                    import_fills,
                    "    rt::fill(&__st.{mslot}, Value::Module(rt::import_external(engine, \"{raw_escaped}\", {at})?))?;"
                );
                let w = format!("__wild{}", self.fresh_id());
                let _ = writeln!(
                    import_fills,
                    "    let {w} = rt::wild_import(&rt::read(&__st.{mslot})?, {at})?;"
                );
                env.wildcards.push(w);
                let _ = writeln!(
                    file_defines,
                    "    if let Some(__sc) = rt::read(&__st.{mslot})?.scope() {{ for (__k, __b) in __sc.iter() {{ scope.bind(__k.clone(), __b.clone()); }} }}"
                );
                return;
            }
            let stem = edge
                .target
                .rsplit('/')
                .next()
                .unwrap_or("m")
                .split('.')
                .next()
                .unwrap_or("m")
                .to_string();
            import_edge_via_glue(
                self,
                plan,
                edge,
                env,
                own,
                state_fields,
                import_fills,
                file_defines,
                Some(&stem),
            );
            return;
        };
        let target_mod = &target_plan.mod_name;
        let exports = &export_table[edge.target.as_str()];
        // Cyclic import: typst errors when module evaluation re-enters
        // a file already on the route (import.rs:231-233); an edge that
        // closes a cycle bails at its own import statement — whichever
        // file of the cycle evaluates first reports it.
        if super::plan::import_reaches(plans, &edge.target, &plan.path, &input.dep_entries) {
            let message = "cyclic import".to_string();
            self.diag(edge.span, message.clone());
            let _ = writeln!(import_fills, "    rt::bail_value(engine, \"{message}\")?;");
            return;
        }
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
                // A bare import's derived name must be a valid
                // identifier (upstream BareImportError::PathInvalid,
                // import.rs:99-101; `as` renames escape the check).
                if matches!(edge.items, ImportItems::Bare) && !typst_syntax::is_ident(&bound)
                {
                    let message = "module name would not be a valid identifier".to_string();
                    self.diag(edge.span, message.clone());
                    let _ =
                        writeln!(import_fills, "    rt::bail_value(engine, \"{message}\")?;");
                    return;
                }
                let slot = match import_slots.get(&edge.target) {
                    Some(slot) => slot.clone(),
                    None => format!("IMPORT_{}_{}", sanitize_key(&plan.mod_name), sanitize_key(&bound)),
                };
                let existed = import_slots
                    .insert(edge.target.clone(), slot.clone())
                    .is_some();
                if !existed {
                    state_fields.push((plan.path.clone(), slot.clone()));
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
            // Module rename + member list: module slot binding + member
            // field accesses one by one (`import "base.typ" as base:
            // ok, err`).
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
                    state_fields.push((plan.path.clone(), mslot.clone()));
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
                    // Dotted item (`as base: ok, util.resolve-number`):
                    // the first segment resolves through the module
                    // value, the remaining chain walks fields — the
                    // same split as the plain List arm (a dotted name
                    // used as a single field would never match).
                    let (first, rest): (&str, &[&str]) =
                        match name.split_once('.') {
                            Some((f, r)) => {
                                (f, r.split('.').collect::<Vec<_>>().leak())
                            }
                            None => (name.as_str(), &[]),
                        };
                    let as_name =
                        rename.clone().unwrap_or_else(|| name.clone());
                    // `Ref::ModuleField` semantics: `expr` is the module
                    // value, `name` the field to access (ref_value adds
                    // that one hop) — so the chain covers all segments
                    // except the last, which is the name.
                    let segments: Vec<&str> =
                        std::iter::once(first).chain(rest.iter().copied()).collect();
                    let (field_name, chain) = (
                        segments.last().copied().unwrap_or(first),
                        {
                            let mut c = format!("rt::read(&__st.{mslot})?");
                            for seg in segments[..segments.len() - 1].iter() {
                                c = format!(
                                    "rt::field(&({c}), \"{}\", engine, context, SPAN)?",
                                    escape(seg)
                                );
                            }
                            c
                        },
                    );
                    env.module.push((
                        as_name.clone(),
                        Ref::ModuleField {
                            expr: chain.clone(),
                            name: field_name.to_string(),
                            file: item_file(
                                export_table,
                                &self.slot_files,
                                &edge.target,
                                field_name,
                            ),
                        },
                    ));
                    let _ = writeln!(
                        file_defines,
                        "    scope.define(\"{}\", rt::field(&({chain}), \"{}\", engine, context, SPAN)?);",
                        escape(&as_name),
                        escape(field_name)
                    );
                }
            }
            ImportItems::List(list) => {
                for (name, rename) in list {
                    // Dotted item (`a.b as c`): the first segment
                    // resolves via the export table, the remaining
                    // field chain is materialized into an independent
                    // IMPORT slot.
                    let (first, rest): (&str, &[&str]) = match name.split_once('.') {
                        Some((f, r)) => {
                            (f, r.split('.').collect::<Vec<_>>().leak())
                        }
                        None => (name.as_str(), &[]),
                    };
                    let Some(slot) = exports.get(first) else {
                        // Upstream reports the unresolved component as an
                        // eval error (import.rs:121); the detailed note
                        // stays in the translator's degradation log.
                        self.diag(edge.span, format!("`{}` does not export `{first}`", edge.target));
                        let _ = writeln!(
                            import_fills,
                            "    rt::bail_value(engine, \"unresolved import\")?;"
                        );
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
                        state_fields.push((plan.path.clone(), dot_slot.clone()));
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

// ----------------------------------------------------- nested state tree

/// Source path → sanitized tree segments ("dir/sub/file.typ" →
/// ["dir", "sub", "file"]; a dependency merge key keeps its spec segment
/// sanitized as one segment).
fn file_segments(file: &str) -> Vec<String> {
    file.split('/')
        .map(|seg| {
            let stem = seg.strip_suffix(".typ").unwrap_or(seg);
            sanitize_key(stem)
        })
        .collect()
}

/// One node of the per-file state tree: a directory level (children) or
/// a file (slots); a node can be both (file stem == sibling dir name).
#[derive(Default)]
struct StateNode {
    slots: Vec<(String, bool)>,
    children: std::collections::BTreeMap<String, StateNode>,
}

impl StateNode {
    fn node_for(&mut self, file: &str) -> &mut StateNode {
        let mut node = self;
        for seg in file_segments(file) {
            node = node.children.entry(seg).or_default();
        }
        node
    }

    /// Post-order emission: children structs first, then this one.
    /// `prefix` is the accumulating struct-type name; writes this node's
    /// field lines (children + slots) into `body`.
    fn emit(&self, prefix: &str, decls: &mut String, body: &mut String, root: bool) {
        let mut inner = String::new();
        for (seg, child) in &self.children {
            child.emit(&format!("{prefix}_{seg}"), decls, &mut inner, false);
            let _ = writeln!(inner, "    {seg}: {prefix}_{seg},");
        }
        for (slot, mutable) in &self.slots {
            let ty = if *mutable {
                "std::sync::Mutex<Value>"
            } else {
                "std::sync::OnceLock<Value>"
            };
            let _ = writeln!(inner, "    {slot}: {ty},");
        }
        if root {
            // The root's body becomes __State's fields directly.
            body.push_str(&inner);
        } else {
            let _ = writeln!(
                decls,
                "#[derive(Default)]\nstruct {prefix} {{\n{inner}}}\n"
            );
        }
    }
}

/// Rewrite `__st.<slot>` accesses to their nested paths
/// (`__st.dir.file.<slot>`). Single scan; idents that are not slots
/// (e.g. `__st.clone()`) pass through. Generated string literals never
/// contain `__st.` (the emitter owns every literal shape involved).
fn nest_state_access(text: &str, paths: &std::collections::HashMap<String, String>) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut pos = 0usize;
    while let Some(hit) = text[pos..].find("__st.") {
        let at = pos + hit;
        out.push_str(&text[pos..at]);
        let mut end = at + 5;
        let bytes = text.as_bytes();
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
        {
            end += 1;
        }
        let ident = &text[at + 5..end];
        match paths.get(ident) {
            Some(path) => {
                out.push_str("__st.");
                out.push_str(path);
                out.push('.');
                out.push_str(ident);
            }
            None => out.push_str(&text[at..end]),
        }
        pos = end;
    }
    out.push_str(&text[pos..]);
    out
}
/// Mutable emission targets shared by the import-edge processors
/// (destructured into the original local names inside each processor).
struct ImportEdgeIo<'a> {
    env: &'a mut Env,
    own: &'a [(String, String)],
    state_fields: &'a mut Vec<(String, String)>,
    import_fills: &'a mut String,
    file_defines: &'a mut String,
    import_slots: &'a mut std::collections::HashMap<String, String>,
    binding_code: &'a mut String,
}

