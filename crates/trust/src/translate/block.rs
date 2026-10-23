//! 代码块与语句直译:let 绑定、体内局部 import、赋值与解构赋值。
//!
//! 非末位语句值按 join 语义累积,末位表达式为块值(对照 typst code
//! block 求值)。

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
        let mut out = String::from("{ let mut __out = Value::None;\n");
        let mut env = env.clone();
        // 表达式位置 `let` 的提升槽(块作用域;对照 typst:let 是
        // 表达式,在当前词法作用域绑定,块退出即弃)。语句位置的
        // `#let` 由下方语句臂走 bind_local,不入提升面——只扫其
        // init(init 内的 let 表达式仍绑定本块)。
        let stmt_vec: Vec<ast::Expr> = stmts.collect();
        let hoist_decls = self.open_let_hoists(stmt_vec.iter().map(|s| match s {
            ast::Expr::LetBinding(lets) => match lets.init() {
                Some(init) => init.to_untyped(),
                None => s.to_untyped(),
            },
            _ => s.to_untyped(),
        }));
        if !hoist_decls.is_empty() {
            for line in hoist_decls.lines() {
                let _ = writeln!(out, "    {line}");
            }
        }
        // (尾表达式代码, 其 span):非末位语句值按 join 语义累积,
        // 末位表达式同样并入 __out 后返回——对照 `eval_code` 的
        // `output = ops::join(output, value).at(span)?`(每个表达式的
        // 值都 join,错误站点为该表达式自身 span)。
        let mut last: Option<(String, String)> = None;
        let mut iter = stmt_vec.into_iter();
        while let Some(stmt) = iter.next() {
            if let Some((prev, prev_at)) = last.take() {
                let _ = writeln!(out, "    __out = rt::join(__out, {prev}, {prev_at})?;");
            }
            // 代码块内的 set/show 规则:样式/规则先求值,**剩余全部**
            // 语句作为代码块求值、display 成内容后挂上(对照
            // `eval_code` 的 SetRule/ShowRule 特例——规则包装块尾)。
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
                // 函数体内局部 import:解析模块值表达式后抄入局部绑定。
                let source = import.source();
                let module_expr: Option<String> = match source {
                    ast::Expr::Ident(_) | ast::Expr::FieldAccess(_) => {
                        match parse_module_access(source) {
                            Some((root, fields)) => {
                                let base = if let Some((_, rust)) =
                                    env.locals.iter().rev().find(|(n, _)| *n == root)
                                {
                                    // 局部/参数(tidy 的 `import style: *`):
                                    // 运行期模块值,遮蔽同名模块绑定。
                                    format!("{rust}.clone()")
                                } else {
                                    match env.module.iter().rev().find(|(n, _)| *n == root) {
                                        Some((_, module_ref)) => self.ref_value(module_ref),
                                        None => {
                                            // 全局模块(如 `import calc: abs`)。
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
                                self.diag(source.span(), "体内 import 源须为模块访问".into());
                                continue;
                            }
                        }
                    }
                    ast::Expr::Str(str_expr) => {
                        // 字符串路径:包模式解析到包内文件的模块值。
                        let raw = str_expr.get().to_string();
                        let (Some(file), Some(files), Some(mods)) = (
                            self.current_file.clone(),
                            self.package_files.clone(),
                            self.package_mods.clone(),
                        ) else {
                            self.diag(
                                str_expr.span(),
                                "体内字符串 import 仅在包模式支持(须用模块访问形态)".into(),
                            );
                            continue;
                        };
                        // 外部包 spec(`@preview/meander:0.4.4`,touying
                        // 驱动):已并入翻译的依赖在合并模块表里按
                        // `{spec}/` 前缀查入口文件,静态装配;未并入的
                        // 回退运行时 import_external 解释。
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
                        // 依赖包内的绝对 import 限定在该包合并键空间
                        // (同顶层 import 的处理)。
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
                                // 目标文件的模块作用域从槽板构建。
                                self.used_state = true;
                                Some(format!(
                                    "Value::Module({mod_name}::module(&__st, engine, context)?)"
                                ))
                            }
                            _ => {
                                // 世界可能仍能运行期解析:胶水(原版
                                // file-not-found 自然承担)。
                                let code =
                                    self.in_body_import_glue(&stmt, import, &mut env);
                                let _ = writeln!(out, "    {{ {code} }}");
                                continue;
                            }
                        }
                    }
                    _ => {
                        // 区 1 胶水:任意源表达式经原版 Eval 分派。
                        let code = self.in_body_import_glue(&stmt, import, &mut env);
                        let _ = writeln!(out, "    {{ {code} }}");
                        continue;
                    }
                };
                let Some(module_expr) = module_expr else {
                    continue;
                };
                // 形态:bare / rename → 模块值;列表 → 字段取值;
                // 通配:内部文件目标按导出表静态展开(求值序保证槽已填)。
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
                            None => source
                                .to_untyped()
                                .full_text()
                                .rsplit('.')
                                .next()
                                .unwrap_or("m")
                                .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                                .next()
                                .unwrap_or("m")
                                .to_string(),
                        };
                        let rust = bind_local(&mut env, &name);
                        let _ = writeln!(out, "    let mut {rust} = {module_expr};");
                    }
                    Some(ast::Imports::Wildcard) => {
                        let mut expanded = internal_target.as_ref().and_then(|target| {
                            self.export_table.as_ref().and_then(|t| t.get(target).cloned())
                        });
                        // 模块值源(import cetz.draw: *,alchemist 驱动):
                        // 槽→文件索引 + 导出表链式解析到目标文件。
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
                        // 模块字段绑定名(`import cetz: draw` / 顶层通配项,
                        // alchemist/diagraph 驱动):Ref 携带的目标文件 →
                        // 导出表静态展开,零运行期开销。
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
                                // 静态链不通:运行期通配层(对照 typst-eval
                                // import.rs 通配分支——源求值为模块,scope
                                // 逐名 bind 进当前作用域;此后本作用域的
                                // 名字先查层再沿静态链回落)。仅此作用域
                                // 受影响;静态可达的通配 import 不走此路。
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
                // 解构赋值:目标须为已存在局部(对照 `Access` 语义)。
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
                // 赋值:统一走 classify_place 分类(局部/模块槽/捕获/
                // 未命中/临时值,对照 `apply_assignment` + `Access`)。
                let code = self.assign_code(binary, &mut env);
                let _ = writeln!(out, "    {code};");
                continue;
            }
            let value = self.expr(stmt, &mut env);
            let at = self.span_of(stmt.span());
            last = Some((value, at));
        }
        // 块值 = 全部语句值的 join(尾语句并入后返回 __out;无表达式
        // 语句时 __out 保持 None,与空块值一致)。
        if let Some((tail, tail_at)) = last {
            let _ = writeln!(out, "    __out = rt::join(__out, {tail}, {tail_at})?;");
        }
        let _ = writeln!(out, "    let _ = &__out;");
        let _ = writeln!(out, "    __out");
        self.close_let_hoists();
        out.push('}');
        out
    }
    /// 赋值/可变方法目标的分类(对照 typst-eval `access.rs` 的可
    /// access 形态与 `Scopes::get_mut` 的解析序:局部层 → 模块槽 →
    /// 全局)。路径段为字段访问与 accessor 方法(`at`/`first`/`last`)。
    ///
    /// 注:`at` 的参数在段处求值,先走目标链后求参数(纯下标表达式
    /// 下与 typst 的"参数先求"等价;带副作用的下标才有顺序差)。
    pub(super) fn classify_place(&mut self, expr: ast::Expr, env: &mut Env) -> Place {
        let mut segs = Vec::new();
        let mut root: Option<String> = None;
        if !self.walk_place(expr, env, &mut segs, &mut root) {
            return Place::Temporary;
        }
        let Some(root) = root else {
            return Place::Temporary;
        };
        // 局部判定先于模块(捕获名以局部形态绑定进闭包环境;对照
        // typst 词法层由内向外)。捕获局部(对照 `Binding::capture`
        // 只读标)单独成类。
        if let Some((_, rust)) = env.locals.iter().rev().find(|(n, _)| *n == root) {
            if self.captured_locals.iter().any(|f| f.contains(rust)) {
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
            // 模块顶层流(in_closure==0)的可变槽:就地修改。
            Some(super::ctx::Ref::Slot(s))
                if self.mutable_slots.contains(s) && self.in_closure == 0 =>
            {
                Place::Slot { slot: s.clone(), path: segs }
            }
            // 函数/闭包体内:模块绑定是捕获(对照 typst CapturesVisitor
            // ——顶层函数同样是闭包),写即只读错。
            Some(_) if self.in_closure > 0 => Place::Captured,
            // 模块域内的运行期引用(ModuleField/Value/未标可变的槽):
            // 可变语义本轮不承载,保留翻译期诊断。
            Some(_) => Place::Deferred,
            // 词法层全部未命中:运行期查全局(对照 `Scopes::get_mut`
            // 的 miss 分支:命中全局报常量,否则未知变量)。
            None => Place::Unresolved { name: root },
        }
    }

    /// 赋值/复合赋值的统一发射(语句与表达式两处共用)。对照
    /// typst-eval/src/ops.rs `apply_assignment`:**RHS 先求**,目标后
    /// 访问;普通赋值且目标为字段链时末段走"缺失即创建"的字典插入
    /// (对照 `access_dict` + `insert` 特例);复合赋值单次寻址 +
    /// mem::take。
    pub(super) fn assign_code(&mut self, binary: ast::Binary, env: &mut Env) -> String {
        use ast::BinOp;
        let compound = match binary.op() {
            BinOp::Assign => None,
            BinOp::AddAssign => Some("add"),
            BinOp::SubAssign => Some("sub"),
            BinOp::MulAssign => Some("mul"),
            BinOp::DivAssign => Some("div"),
            _ => unreachable!("assign_code 只接赋值族"),
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
                // 可变模块槽:读改写回(对照 typst 顶层绑定的就地修改
                // 语义);体引用 __st 须置 used_state。
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
            // 写捕获:对照 `Binding::write` 对 Captured 的拒绝
            //(RHS 已先求,保持副作用顺序)。
            Place::Captured => format!(
                "{{ let __rhs = {rhs}; rt::bail_captured(typst::foundations::Capturer::Function)? }}"
            ),
            // 词法层未命中的标识符:运行期经 `Scopes::get_mut`
            // 触发常量/未知变量错(文案与 hint 由 typst 侧生成)。
            Place::Unresolved { name } => format!(
                "{{ let __rhs = {rhs}; rt::check_mut_unknown(engine, \"{}\", {at})?; Value::None }}",
                super::naming::escape(&name)
            ),
            // 非左值:求目标副作用后报 temporary(对照 `Access`
            // 兜底臂;FieldAccess 只求目标链不求字段读)。
            Place::Temporary => {
                let lhs = self.eval_for_access(binary.lhs(), env);
                format!("{{ let __rhs = {rhs}; let _ = {lhs}; rt::bail_temporary({at})? }}")
            }
            Place::Deferred => {
                self.diag(
                    binary.span(),
                    "赋值目标为运行期模块引用(本轮不承载)".into(),
                );
                "Value::None".into()
            }
        }
    }

    /// 临时值目标的副作用求值,对照 `Access` 各臂的递归形态:
    /// FieldAccess 只深入**目标**(不读字段),其余表达式整体求值。
    pub(super) fn eval_for_access(&mut self, expr: ast::Expr, env: &mut Env) -> String {
        match expr {
            ast::Expr::FieldAccess(access) => self.eval_for_access(access.target(), env),
            ast::Expr::Parenthesized(p) => self.eval_for_access(p.expr(), env),
            _ => self.expr(expr, env),
        }
    }

    /// 解构赋值叶子的统一写回(Ident 与 at 位置叶子共用;对照
    /// binding.rs DestructAssignment 的叶子 f:`expr.access(vm)?` +
    /// 写回,at_mut 语义**不**插入缺失键;value 由调用方先求)。
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
                self.diag(expr.span(), "解构赋值目标为运行期模块引用".into());
                String::new()
            }
        }
    }


    /// 体内 import 的区 1 胶水:语句原文经 `rt::eval_import` 驱动原版
    /// `Eval for ast::ModuleImport`(源分派/校验/警告全原版);可见名
    /// (局部 + 模块域)物化为 Vm 作用域,差集按静态可知的绑定名写回
    /// 新的 Rust 局部。bare 动态源 expected 为空(原版运行期报
    /// dynamic 错)。
    fn in_body_import_glue(
        &mut self,
        stmt: &ast::Expr,
        import: ast::ModuleImport,
        env: &mut Env,
    ) -> String {
        use std::fmt::Write as _;
        let stmt_text = stmt.to_untyped().full_text().to_string();
        // 静态可知的绑定名(bare 的模块链/字面量名由上方快路径承担,
        // 此处 bare 一律动态语义:expected 空,原版报错)。
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
                // 通配的任意源:源表达式翻译求值 → 运行期层。
                let source = import.source();
                let value = self.expr(source, env);
                let w = format!("__wild{}", self.fresh_id());
                let at = self.span_of(stmt.span());
                let code = format!("let {w} = rt::wild_import(&({value}), {at})?;");
                env.wildcards.push(w);
                return code;
            }
        };
        // 可见名:局部 + 模块域(ref_value 需要 &mut self,先克隆表)。
        let locals: Vec<(String, String)> = env.locals.clone();
        let modules: Vec<(String, super::ctx::Ref)> = env.module.clone();
        let mut caps: Vec<String> = locals
            .iter()
            .map(|(n, rust)| {
                format!("(\"{}\", {rust}.clone())", super::naming::escape(n))
            })
            .collect();
        caps.extend(modules.iter().map(|(n, r)| {
            format!("\"{}\", {}", super::naming::escape(n), self.ref_value(r))
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
                        // 对照 typst 的 FuncCall access:先求参数,后走
                        // 目标链(带副作用的下标保求值顺序)。
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

/// 路径段的数组字面量(空路径给 `&[]`,推断为空切片)。
pub(super) fn place_path_lit(path: &[String]) -> String {
    if path.is_empty() {
        "&[]".into()
    } else {
        format!("&[{}]", path.join(", "))
    }
}

/// 赋值/可变方法目标的分类(对照 typst-eval `access.rs`:可 access
/// 形态有限枚举,其余一律 Temporary;`Scopes::get_mut` 决定解析序)。
pub(super) enum Place {
    /// 根为局部变量(或字段/accessor 链落在局部上)。
    Local { root: String, path: Vec<String> },
    /// 根为可变模块槽(读改写回)。
    Slot { slot: String, path: Vec<String> },
    /// 根为闭包捕获名(对照 `Binding::capture` 只读标,运行期报
    /// "variables from outside the function are read-only")。
    Captured,
    /// 标识符在词法层未命中(运行期查全局:常量/未知变量错)。
    Unresolved { name: String },
    /// 非左值表达式(求副作用后报 temporary)。
    Temporary,
    /// 模块域内的运行期引用(本轮不承载可变语义,翻译期诊断)。
    Deferred,
}
