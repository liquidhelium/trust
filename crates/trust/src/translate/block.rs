//! Direct translation of code blocks and statements: let bindings, in-body
//! local imports, assignments and destructuring assignments.
//!
//! Non-final statement values accumulate with join semantics; the final
//! expression is the block value (mirrors typst code-block evaluation).

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::escape;
use super::paths::{normalize_import, parse_module_access};

impl Ctx {
    pub(super) fn block<'a>(
        &mut self,
        stmts: impl Iterator<Item = ast::Expr<'a>>,
        env: &mut Env,
    ) -> String {
        let mut env = env.clone();
        // Hoisted slots for expression-position `let` (block-scoped;
        // mirrors typst: let is an expression that binds in the current
        // lexical scope and is discarded when the block exits).
        // Statement-position `#let` goes through bind_local in the
        // statement arm below and stays off the hoisting surface — only its
        // init is scanned (a let expression inside the init still binds
        // this block).
        let stmt_vec: Vec<ast::Expr> = stmts.collect();
        let hoist_decls = self.open_let_hoists(stmt_vec.iter().map(|s| match s {
            ast::Expr::LetBinding(lets) => match lets.init() {
                Some(init) => init.to_untyped(),
                None => s.to_untyped(),
            },
            _ => s.to_untyped(),
        }));
        let mut hoists = String::new();
        for line in hoist_decls.lines() {
            let _ = writeln!(hoists, "    {line}");
        }
        // (tail expression code, its span): non-final statement values
        // accumulate with join semantics, and the final expression is
        // likewise joined into __out before returning — mirrors
        // `eval_code`'s `output = ops::join(output, value).at(span)?`
        // (every expression's value is joined; the error site is that
        // expression's own span).
        let mut out = String::new();
        let mut last: Option<(String, String)> = None;
        // Whether a value has been flushed into `__out` yet (a second
        // expression statement arrived). When nothing has, the block is a
        // single-tail block and collapses below.
        let mut flushed = false;
        let mut iter = stmt_vec.into_iter();
        while let Some(stmt) = iter.next() {
            if let Some((prev, prev_at)) = last.take() {
                flushed = true;
                let _ = writeln!(out, "    __out = rt::join(__out, {prev}, {prev_at})?;");
            }
            // set/show rules inside a code block: the styles/recipe are
            // evaluated first, then **all remaining** statements are
            // evaluated as a code block, displayed into content, and the
            // styles attached (mirrors the SetRule/ShowRule special cases
            // of `eval_code` — the rule wraps the block tail).
            if let ast::Expr::SetRule(set) = stmt {
                let rest: Vec<ast::Expr> = iter.collect();
                let tail = self.block(rest.into_iter(), &mut env);
                let rule_at = self.span_of(set.span());
                let styles = self.set_styles_code(set, &mut env, &rule_at);
                let at = self.span_of(stmt.span());
                last = Some((
                    format!("Value::Content(rt::styled_with_map(({tail}).display(), {styles}))"),
                    at,
                ));
                break;
            }
            if let ast::Expr::ShowRule(show) = stmt {
                let rest: Vec<ast::Expr> = iter.collect();
                let tail = self.block(rest.into_iter(), &mut env);
                let show_at = self.span_of(show.span());
                let (sel_arg, sel_at) = match show.selector() {
                    Some(sel) => {
                        let sel_at = self.span_of(sel.span());
                        (format!("Some({})", self.expr(sel, &mut env)), sel_at)
                    }
                    None => ("None".to_string(), show_at.clone()),
                };
                let recipe = match show.transform() {
                    ast::Expr::SetRule(set) => {
                        let set_at = self.span_of(set.span());
                        let styles = self.set_styles_code(set, &mut env, &set_at);
                        format!(
                            "{{ let __sel = {sel_arg}; let __styles = {styles}; rt::show_rule_style(__sel, {sel_at}, __styles, {show_at}, engine)? }}"
                        )
                    }
                    transform => format!(
                        "{{ let __sel = {sel_arg}; let __tf = {{ {} }}; rt::show_rule(__sel, {sel_at}, __tf, {show_at}, engine)? }}",
                        self.expr(transform, &mut env)
                    ),
                };
                let at = self.span_of(stmt.span());
                last = Some((
                    format!(
                        "Value::Content({{ let __recipe = {recipe}; let __styled_tail = ({tail}).display(); rt::styled_with_recipe(engine, context, __styled_tail, __recipe)? }})"
                    ),
                    at,
                ));
                break;
            }
            if let ast::Expr::LetBinding(lets) = stmt {
                let init = match lets.init() {
                    Some(init) => self.expr(init, &mut env),
                    None => "Value::None".into(),
                };
                match lets.kind() {
                    ast::LetBindingKind::Closure(name) => {
                        let rust = bind_local(&mut env, name.as_str());
                        let _ = writeln!(out, "    let mut {rust} = {init};");
                    }
                    ast::LetBindingKind::Normal(pattern) => {
                        let stmts = self.destructure(pattern, init, &mut env, false);
                        out.push_str("    ");
                        out.push_str(&stmts.replace('\n', "\n    "));
                    }
                }
                continue;
            }
            if let ast::Expr::ModuleImport(import) = stmt {
                // Local import inside a function body: resolve the
                // module-value expression, then copy the bindings into
                // locals.
                let source = import.source();
                let module_expr: Option<String> = match source {
                    ast::Expr::Ident(_) | ast::Expr::FieldAccess(_) => {
                        match parse_module_access(source) {
                            Some((root, fields)) => {
                                let base = if let Some((_, rust)) =
                                    env.locals.iter().rev().find(|(n, _)| *n == root)
                                {
                                    // Local/parameter (e.g. `import style:
                                    // *`): a runtime module value that
                                    // shadows a same-named module binding.
                                    format!("{rust}.clone()")
                                } else {
                                    match env.module.iter().rev().find(|(n, _)| *n == root) {
                                        Some((_, module_ref)) => self.ref_value(module_ref),
                                        None => {
                                            // A global module (e.g.
                                            // `import calc: abs`).
                                            let at = self.span_of(stmt.span());
                                            format!("rt::global(engine, \"{root}\", {at})?")
                                        }
                                    }
                                };
                                let mut expr = base;
                                let at = self.span_of(stmt.span());
                                for field in &fields {
                                    expr = format!(
                                        "rt::field(&({expr}), \"{}\", engine, context, {at})?",
                                        escape(field)
                                    );
                                }
                                Some(expr)
                            }
                            None => {
                                self.diag(source.span(), "in-body import source must be a module access".into());
                                continue;
                            }
                        }
                    }
                    ast::Expr::Str(str_expr) => {
                        // String path: in package mode, resolves to the
                        // module value of a file inside the package.
                        let raw = str_expr.get().to_string();
                        let (Some(file), Some(files), Some(mods)) = (
                            self.current_file.clone(),
                            self.package_files.clone(),
                            self.package_mods.clone(),
                        ) else {
                            self.diag(
                                str_expr.span(),
                                "in-body string import is only supported in package mode (module-access form required)".into(),
                            );
                            continue;
                        };
                        // External package spec (`@preview/meander:0.4.4`):
                        // merged dependencies are looked up in the merged
                        // module table by their `{spec}/` prefix to find
                        // the entry file and assembled statically; others
                        // fall back to runtime import_external
                        // interpretation.
                        if raw.starts_with('@') {
                            if let Some((entry, _)) = mods
                                .iter()
                                .find(|(k, _)| k.starts_with(&format!("{raw}/")))
                            {
                                self.used_state = true;
                                return format!(
                                    "Value::Module({}::module(&__st, engine, context)?)",
                                    mods.get(entry).unwrap_or(&String::new())
                                );
                            }
                            return format!(
                                "Value::Module(rt::import_external(engine, \"{raw}\", SPAN)?)"
                            );
                        }
                        let mut target = normalize_import(&file, &raw);
                        // Absolute imports inside a dependency package are
                        // qualified into that package's merged key space
                        // (same treatment as top-level imports).
                        if raw.starts_with('/')
                            && let Some(colon) = file.find(':')
                            && let Some(rel) = file[colon..].find('/')
                            && file.starts_with('@')
                        {
                            target = format!(
                                "{}/{}",
                                &file[..colon + rel],
                                target.trim_start_matches('/')
                            );
                        }
                        match (files.contains_key(&target), mods.get(&target)) {
                            (true, Some(mod_name)) => {
                                // The target file's module scope is built
                                // from the slot board.
                                self.used_state = true;
                                Some(format!(
                                    "Value::Module({mod_name}::module(&__st, engine, context)?)"
                                ))
                            }
                            _ => {
                                // The world may still resolve it at runtime:
                                // glue (the original file-not-found carries
                                // it naturally).
                                let code =
                                    self.in_body_import_glue(&stmt, import, &mut env);
                                let _ = writeln!(out, "    {{ {code} }}");
                                continue;
                            }
                        }
                    }
                    _ => {
                        // Zone-1 glue: any source expression dispatched
                        // through the original Eval.
                        let code = self.in_body_import_glue(&stmt, import, &mut env);
                        let _ = writeln!(out, "    {{ {code} }}");
                        continue;
                    }
                };
                let Some(module_expr) = module_expr else {
                    continue;
                };
                // Shapes: bare / rename → module value; list → field
                // extraction; wildcard: internal file targets expand
                // statically via the export table (evaluation order
                // guarantees the slots are filled).
                let internal_target = match (&source, import.imports()) {
                    (ast::Expr::Str(str_expr), Some(ast::Imports::Wildcard)) => {
                        let raw = str_expr.get().to_string();
                        self.current_file.as_ref().map(|file| {
                            let mut target = normalize_import(file, &raw);
                            if raw.starts_with('/')
                                && let Some(colon) = file.find(':')
                                && let Some(rel) = file[colon..].find('/')
                                && file.starts_with('@')
                            {
                                target = format!(
                                    "{}/{}",
                                    &file[..colon + rel],
                                    target.trim_start_matches('/')
                                );
                            }
                            target
                        })
                    }
                    _ => None,
                };
                match import.imports() {
                    None => {
                        let name = match import.new_name() {
                            Some(new_name) => new_name.as_str().to_string(),
                            None => super::captures::bare_import_bound_name(source),
                        };
                        let rust = bind_local(&mut env, &name);
                        let _ = writeln!(out, "    let mut {rust} = {module_expr};");
                    }
                    Some(ast::Imports::Wildcard) => {
                        let mut expanded = internal_target.as_ref().and_then(|target| {
                            self.export_table.as_ref().and_then(|t| t.get(target).cloned())
                        });
                        // Module-value source (`import cetz.draw: *`):
                        // slot → file index + chained export-table
                        // resolution to the target file.
                        if expanded.is_none()
                            && let Some((root, fields)) =
                                parse_module_access(source)
                        {
                            let mut cur = env
                                .module
                                .iter()
                                .rev()
                                .find(|(n, _)| *n == root)
                                .and_then(|(_, r)| match r {
                                    super::ctx::Ref::Slot(slot) => {
                                        self.slot_files.as_ref().and_then(|m| m.get(slot).cloned())
                                    }
                                    _ => None,
                                });
                            for field in &fields {
                                cur = cur
                                    .as_ref()
                                    .and_then(|f| {
                                        self.export_table.as_ref().and_then(|t| t.get(f))
                                    })
                                    .and_then(|exports| exports.get(field))
                                    .and_then(|slot| {
                                        self.slot_files
                                            .as_ref()
                                            .and_then(|m| m.get(slot).cloned())
                                    });
                            }
                            expanded = cur
                                .as_ref()
                                .and_then(|f| {
                                    self.export_table.as_ref().and_then(|t| t.get(f).cloned())
                                });
                        }
                        // Module-field binding names (`import cetz: draw` /
                        // top-level wildcard items): the target file carried
                        // by the Ref → static expansion through the export
                        // table, zero runtime cost.
                        if expanded.is_none()
                            && let Some((root, _)) = parse_module_access(source)
                            && let Some((
                                _,
                                super::ctx::Ref::ModuleField { file: Some(f), .. },
                            )) = env.module.iter().rev().find(|(n, _)| *n == root)
                        {
                            expanded = self.export_table.as_ref().and_then(|t| t.get(f).cloned());
                        }
                        match expanded {
                            Some(exports) => {
                                for (name, slot) in exports {
                                    let rust = bind_local(&mut env, &name);
                                    let _ = writeln!(
                                        out,
                                        "    let mut {rust} = rt::read(&__st.{slot})?;"
                                    );
                                }
                            }
                            None => {
                                // Static chain unavailable: runtime wildcard
                                // layer (mirrors the wildcard branch of
                                // typst-eval import.rs — the source
                                // evaluates to a module, and the scope binds
                                // each name into the current scope; from
                                // then on, names in this scope check the
                                // layer first and fall back along the
                                // static chain). Only this scope is
                                // affected; statically reachable wildcard
                                // imports do not take this path.
                                let w = format!("__wild{}", self.fresh_id());
                                let at = self.span_of(stmt.span());
                                let _ = writeln!(
                                    out,
                                    "    let {w} = rt::wild_import(&({module_expr}), {at})?;"
                                );
                                env.wildcards.push(w);
                            }
                        }
                    }
                    Some(ast::Imports::Items(items)) => {
                        for item in items.iter() {
                            let original = item.original_name().as_str().to_string();
                            let bound = item.bound_name().as_str().to_string();
                            let rust = bind_local(&mut env, &bound);
                            let at = self.span_of(stmt.span());
                            let _ = writeln!(
                                out,
                                "    let mut {rust} = rt::field(&({module_expr}), \"{}\", engine, context, {at})?;",
                                escape(&original)
                            );
                        }
                    }
                }
                continue;
            }
            if let ast::Expr::DestructAssignment(assign_) = stmt {
                // Destructuring assignment: targets must be existing locals
                // (mirrors `Access` semantics).
                let value = self.expr(assign_.value(), &mut env);
                let stmts = self.destructure(assign_.pattern(), value, &mut env, true);
                out.push_str("    ");
                out.push_str(&stmts.replace('\n', "\n    "));
                continue;
            }
            if let ast::Expr::Binary(binary) = stmt
                && matches!(
                    binary.op(),
                    ast::BinOp::Assign
                        | ast::BinOp::AddAssign
                        | ast::BinOp::SubAssign
                        | ast::BinOp::MulAssign
                        | ast::BinOp::DivAssign
                )
            {
                // Assignment: uniformly classified via classify_place
                // (local / module slot / captured / unresolved / temporary;
                // mirrors `apply_assignment` + `Access`).
                let code = self.assign_code(binary, &mut env);
                let _ = writeln!(out, "    {code};");
                continue;
            }
            let value = self.expr(stmt, &mut env);
            let at = self.span_of(stmt.span());
            last = Some((value, at));
        }
        // Assemble. Single-tail collapse: when no value has been flushed,
        // the block's value is exactly the tail — `ops::join`'s `(None, b)`
        // arm is total (`b`, no error path), so `join(None, v) ≡ v` and
        // the accumulator never needs to exist. This is the degenerate
        // staging of `eval_code`'s always-join loop (the interpreter
        // cannot know the sequence statically; we can).
        self.close_let_hoists();
        match last {
            Some((tail, _tail_at)) if !flushed => format!("{{\n{hoists}{out}    {tail}\n}}"),
            _ => {
                let mut full = String::from("{ let mut __out = Value::None;\n");
                full.push_str(&hoists);
                full.push_str(&out);
                // Block value = join of all statement values (the tail
                // statement is joined in before returning __out; with no
                // expression statements, __out stays None, matching the
                // empty-block value).
                if let Some((tail, tail_at)) = last {
                    let _ = writeln!(full, "    __out = rt::join(__out, {tail}, {tail_at})?;");
                }
                let _ = writeln!(full, "    let _ = &__out;");
                let _ = writeln!(full, "    __out");
                full.push('}');
                full
            }
        }
    }
    /// Classification of assignment / mutating-method targets (mirrors
    /// typst-eval `access.rs`'s accessible shapes and `Scopes::get_mut`'s
    /// resolution order: local level → module slot → global). Path segments
    /// are field accesses and accessor methods (`at`/`first`/`last`).
    ///
    /// Note: `at`'s argument is evaluated at the segment, target chain
    /// first, argument after (equivalent to typst's "arguments first" for
    /// pure index expressions; only side-effecting indices see an ordering
    /// difference).
    pub(super) fn classify_place(&mut self, expr: ast::Expr, env: &mut Env) -> Place {
        let mut segs = Vec::new();
        let mut root: Option<String> = None;
        if !self.walk_place(expr, env, &mut segs, &mut root) {
            return Place::Temporary;
        }
        let Some(root) = root else {
            return Place::Temporary;
        };
        // Locality is decided before module bindings (captured names bind
        // into the closure environment as locals; mirrors typst's
        // inside-out lexical layers). Captured locals (mirroring the
        // read-only `Binding::capture` marker) form their own class —
        // **only the current closure level's frame is consulted**: an inner
        // param/let of the same name shadows an outer capture and is
        // writable (mirrors `Scopes::get_mut` finding the innermost Normal
        // binding; crossing frames made an inner param sharing an outer
        // capture's name report a bogus read-only error).
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| *n == root) {
            if self
                .captured_locals
                .last()
                .is_some_and(|f| f.contains(rust))
            {
                return Place::Captured;
            }
            return Place::Local { root: rust.clone(), path: segs };
        }
        match env
            .module
            .iter()
            .rev()
            .find(|(n, _)| *n == root)
            .map(|(_, r)| r)
        {
            // Mutable slot on the module's top-level flow (in_closure==0):
            // in-place mutation.
            Some(super::ctx::Ref::Slot(s))
                if self.mutable_slots.contains(s) && self.in_closure == 0 =>
            {
                Place::Slot { slot: s.clone(), path: segs }
            }
            // Inside a function/closure body: module bindings are captures
            // (mirrors typst CapturesVisitor — top-level functions are
            // closures too); writing is a read-only error.
            Some(_) if self.in_closure > 0 => Place::Captured,
            // Runtime references inside the module domain
            // (ModuleField/Value/unmarked-mutable slots): mutable semantics
            // are not carried in this iteration; keep the translate-time
            // diagnostic.
            Some(_) => Place::Deferred,
            // Lexical layers all miss: look up the global at runtime
            // (mirrors the miss branch of `Scopes::get_mut`: a global hit
            // reports constant, otherwise unknown variable).
            None => Place::Unresolved { name: root },
        }
    }

    /// Unified emission for assignments and compound assignments (shared by
    /// the statement and expression sites). Mirrors
    /// typst-eval/src/ops.rs `apply_assignment`: **RHS evaluates first**,
    /// target accessed after; a plain assignment whose target is a field
    /// chain takes the "create-on-missing" dictionary insert at the last
    /// segment (mirrors the `access_dict` + `insert` special case); a
    /// compound assignment does a single addressing pass + mem::take.
    pub(super) fn assign_code(&mut self, binary: ast::Binary, env: &mut Env) -> String {
        use ast::BinOp;
        let compound = match binary.op() {
            BinOp::Assign => None,
            BinOp::AddAssign => Some("add"),
            BinOp::SubAssign => Some("sub"),
            BinOp::MulAssign => Some("mul"),
            BinOp::DivAssign => Some("div"),
            _ => unreachable!("assign_code only accepts assignment operators"),
        };
        let at = self.span_of(binary.span());
        let rhs = self.expr(binary.rhs(), env);
        match self.classify_place(binary.lhs(), env) {
            Place::Local { root, path } => {
                let path_lit = place_path_lit(&path);
                match compound {
                    Some(op) => format!(
                        "{{ let __rhs = {rhs}; let __p = rt::place_mut(&mut {root}, {path_lit}, {at})?; let __old = std::mem::take(__p); *__p = ops::{op}(__old, __rhs).at({at})?; Value::None }}"
                    ),
                    None => format!(
                        "{{ let __rhs = {rhs}; rt::assign_value(&mut {root}, {path_lit}, __rhs, {at})?; Value::None }}"
                    ),
                }
            }
            Place::Slot { slot, path } => {
                // Mutable module slot: read-modify-write (mirrors typst's
                // in-place mutation semantics for top-level bindings); the
                // body's reference to __st must set used_state.
                self.used_state = true;
                let path_lit = place_path_lit(&path);
                match compound {
                    Some(op) => format!(
                        "{{ let __rhs = {rhs}; let mut __msv = rt::read_ms(&__st.{slot})?; let __p = rt::place_mut(&mut __msv, {path_lit}, {at})?; let __old = std::mem::take(__p); *__p = ops::{op}(__old, __rhs).at({at})?; rt::write_ms(&__st.{slot}, __msv)?; Value::None }}"
                    ),
                    None => format!(
                        "{{ let __rhs = {rhs}; let mut __msv = rt::read_ms(&__st.{slot})?; rt::assign_value(&mut __msv, {path_lit}, __rhs, {at})?; rt::write_ms(&__st.{slot}, __msv)?; Value::None }}"
                    ),
                }
            }
            // Write to a capture: mirrors `Binding::write` rejecting
            // Captured (RHS already evaluated first, preserving side-effect
            // order).
            Place::Captured => format!(
                "{{ let __rhs = {rhs}; rt::bail_captured(typst::foundations::Capturer::Function)? }}"
            ),
            // Identifier missing from the lexical layers: at runtime,
            // `Scopes::get_mut` raises the constant / unknown-variable
            // error (message and hint generated on the typst side).
            Place::Unresolved { name } => format!(
                "{{ let __rhs = {rhs}; rt::check_mut_unknown(engine, \"{}\", {at})?; Value::None }}",
                super::naming::escape(&name)
            ),
            // Not an lvalue: evaluate the target's side effects, then
            // report temporary (mirrors the `Access` catch-all arm;
            // FieldAccess evaluates only the target chain, not the field
            // read).
            Place::Temporary => {
                let lhs = self.eval_for_access(binary.lhs(), env);
                format!("{{ let __rhs = {rhs}; let _ = {lhs}; rt::bail_temporary({at})? }}")
            }
            Place::Deferred => {
                self.diag(
                    binary.span(),
                    "assignment target is a runtime module reference (not yet supported)".into(),
                );
                "Value::None".into()
            }
        }
    }

    /// Side-effect evaluation of temporary targets, mirroring the recursive
    /// shapes of the `Access` arms: FieldAccess descends into the **target
    /// only** (does not read the field); everything else evaluates whole.
    pub(super) fn eval_for_access(&mut self, expr: ast::Expr, env: &mut Env) -> String {
        match expr {
            ast::Expr::FieldAccess(access) => self.eval_for_access(access.target(), env),
            ast::Expr::Parenthesized(p) => self.eval_for_access(p.expr(), env),
            _ => self.expr(expr, env),
        }
    }

    /// Unified write-back for destructuring-assignment leaves (shared by
    /// Ident and at-position leaves; mirrors the leaf f of binding.rs
    /// DestructAssignment: `expr.access(vm)?` + write-back, at_mut
    /// semantics do **not** insert missing keys; the value is evaluated by
    /// the caller first).
    pub(super) fn place_write_leaf(
        &mut self,
        expr: ast::Expr,
        value: &str,
        env: &mut Env,
    ) -> String {
        let at = self.span_of(expr.span());
        match self.classify_place(expr, env) {
            Place::Local { root, path } => {
                let path_lit = place_path_lit(&path);
                format!(
                    "{{ let __p = rt::place_mut(&mut {root}, {path_lit}, {at})?; *__p = {value}; }}\n"
                )
            }
            Place::Slot { slot, path } => {
                self.used_state = true;
                let path_lit = place_path_lit(&path);
                format!(
                    "{{ let mut __msv = rt::read_ms(&__st.{slot})?; let __p = rt::place_mut(&mut __msv, {path_lit}, {at})?; *__p = {value}; rt::write_ms(&__st.{slot}, __msv)?; }}\n"
                )
            }
            Place::Captured => format!(
                "{{ let _ = {value}; rt::bail_captured(typst::foundations::Capturer::Function)?; }}\n"
            ),
            Place::Unresolved { name } => format!(
                "{{ let _ = {value}; rt::check_mut_unknown(engine, \"{}\", {at})?; }}\n",
                super::naming::escape(&name)
            ),
            Place::Temporary => {
                let ev = self.eval_for_access(expr, env);
                format!("{{ let _ = {value}; let _ = {ev}; rt::bail_temporary({at})?; }}\n")
            }
            Place::Deferred => {
                self.diag(expr.span(), "destructuring-assignment target is a runtime module reference".into());
                String::new()
            }
        }
    }


    /// Zone-1 glue for in-body imports: the statement text drives the
    /// original `Eval for ast::ModuleImport` via `rt::eval_import`
    /// (source dispatch/validation/warnings all original); visible names
    /// (locals + module domain) are materialized as Vm scopes, and the
    /// difference set writes new Rust locals for the statically known
    /// binding names. A bare dynamic source has an empty expected list
    /// (the original reports the dynamic error at runtime).
    fn in_body_import_glue(
        &mut self,
        stmt: &ast::Expr,
        import: ast::ModuleImport,
        env: &mut Env,
    ) -> String {
        use std::fmt::Write as _;
        let stmt_text = stmt.to_untyped().full_text().to_string();
        // Statically known binding names (the module chains / literal
        // names of bare imports are handled by the fast paths above; here a
        // bare import always gets dynamic semantics: expected empty, the
        // original reports the error).
        let expected: Vec<String> = match import.imports() {
            None => match import.new_name() {
                Some(new_name) => vec![new_name.as_str().to_string()],
                None => Vec::new(),
            },
            Some(ast::Imports::Items(items)) => items
                .iter()
                .map(|item| item.bound_name().as_str().to_string())
                .collect(),
            Some(ast::Imports::Wildcard) => {
                // Wildcard with an arbitrary source: translate-evaluate the
                // source expression → runtime layer.
                let source = import.source();
                let value = self.expr(source, env);
                let w = format!("__wild{}", self.fresh_id());
                let at = self.span_of(stmt.span());
                let code = format!("let {w} = rt::wild_import(&({value}), {at})?;");
                env.wildcards.push(w);
                return code;
            }
        };
        // Visible names: locals + module domain (ref_value needs &mut
        // self, so clone the tables first).
        let locals: Vec<(String, String)> = env.locals.clone();
        let modules: Vec<(String, super::ctx::Ref)> = env.module.clone();
        let mut caps: Vec<String> = locals
            .iter()
            .map(|(n, rust)| {
                format!("(\"{}\", {rust}.clone())", super::naming::escape(n))
            })
            .collect();
        caps.extend(modules.iter().map(|(n, r)| {
            format!(
                "(\"{}\", {})",
                super::naming::escape(n),
                self.ref_value(r)
            )
        }));
        let at = self.span_of(stmt.span());
        let caps_lit = if caps.is_empty() {
            "Vec::new()".to_string()
        } else {
            format!("vec![{}]", caps.join(", "))
        };
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
        let mut binds = String::new();
        let mut names = Vec::new();
        for name in &expected {
            let rust = bind_local(env, name);
            names.push((name.clone(), rust));
        }
        for (i, (_, rust)) in names.iter().enumerate() {
            let _ = write!(binds, "let mut {rust} = __imported[{i}].clone(); ");
        }
        format!(
            "let __caps: Vec<(&'static str, Value)> = {caps_lit}; let __imported = rt::eval_import({stmt_text:?}, {at}, {expected_lit}, &__caps, engine, context)?; {binds} Value::None"
        )
    }

    fn walk_place(
        &mut self,
        expr: ast::Expr,
        env: &mut Env,
        segs: &mut Vec<String>,
        root: &mut Option<String>,
    ) -> bool {
        match expr {
            ast::Expr::Ident(ident) => {
                if root.is_some() {
                    return false;
                }
                *root = Some(ident.as_str().to_string());
                true
            }
            ast::Expr::Parenthesized(p) => self.walk_place(p.expr(), env, segs, root),
            ast::Expr::FieldAccess(access) => {
                if !self.walk_place(access.target(), env, segs, root) {
                    return false;
                }
                segs.push(format!(
                    "rt::PlaceSeg::Field(String::from(\"{}\"))",
                    escape(access.field().as_str())
                ));
                true
            }
            ast::Expr::FuncCall(call) => {
                let ast::Expr::FieldAccess(access) = call.callee() else {
                    return false;
                };
                let seg = match access.field().as_str() {
                    "at" => {
                        let Some(ast::Arg::Pos(index)) = call.args().items().next() else {
                            return false;
                        };
                        // Mirrors typst's FuncCall access: evaluate the
                        // argument first, then walk the target chain
                        // (side-effecting indices keep their evaluation
                        // order).
                        let index_code = self.expr(index, env);
                        if !self.walk_place(access.target(), env, segs, root) {
                            return false;
                        }
                        format!("rt::PlaceSeg::At({index_code})")
                    }
                    "first" | "last" => {
                        if !self.walk_place(access.target(), env, segs, root) {
                            return false;
                        }
                        if access.field().as_str() == "first" {
                            "rt::PlaceSeg::First".into()
                        } else {
                            "rt::PlaceSeg::Last".into()
                        }
                    }
                    _ => return false,
                };
                segs.push(seg);
                true
            }
            _ => false,
        }
    }
}

/// Array literal for path segments (an empty path yields `&[]`, inferred as
/// an empty slice).
pub(super) fn place_path_lit(path: &[String]) -> String {
    if path.is_empty() {
        "&[]".into()
    } else {
        format!("&[{}]", path.join(", "))
    }
}

/// Classification of assignment / mutating-method targets (mirrors
/// typst-eval `access.rs`: the accessible shapes are a finite enumeration,
/// everything else is Temporary; `Scopes::get_mut` decides the resolution
/// order).
pub(super) enum Place {
    /// Root is a local variable (or a field/accessor chain landing on a
    /// local).
    Local { root: String, path: Vec<String> },
    /// Root is a mutable module slot (read-modify-write).
    Slot { slot: String, path: Vec<String> },
    /// Root is a closure-captured name (mirrors the read-only
    /// `Binding::capture` marker; runtime reports "variables from outside
    /// the function are read-only").
    Captured,
    /// Identifier missing from the lexical layers (runtime global lookup:
    /// constant / unknown-variable error).
    Unresolved { name: String },
    /// Not an lvalue expression (side effects evaluated, then a temporary
    /// error).
    Temporary,
    /// Runtime reference inside the module domain (mutable semantics not
    /// carried in this iteration; translate-time diagnostic).
    Deferred,
}
