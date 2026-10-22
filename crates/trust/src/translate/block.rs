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
        env: &Env,
    ) -> String {
        let mut out = String::from("{ let mut __out = Value::None;\n");
        let mut env = env.clone();
        // (尾表达式代码, 其 span):非末位语句值按 join 语义累积,
        // 末位表达式同样并入 __out 后返回——对照 `eval_code` 的
        // `output = ops::join(output, value).at(span)?`(每个表达式的
        // 值都 join,错误站点为该表达式自身 span)。
        let mut last: Option<(String, String)> = None;
        let mut iter = stmts;
        while let Some(stmt) = iter.next() {
            if let Some((prev, prev_at)) = last.take() {
                let _ = writeln!(out, "    __out = rt::join(__out, {prev}, {prev_at})?;");
            }
            // 代码块内的 set/show 规则:样式/规则先求值,**剩余全部**
            // 语句作为代码块求值、display 成内容后挂上(对照
            // `eval_code` 的 SetRule/ShowRule 特例——规则包装块尾)。
            if let ast::Expr::SetRule(set) = stmt {
                let rest: Vec<ast::Expr> = iter.collect();
                let tail = self.block(rest.into_iter(), &env);
                let rule_at = self.span_of(set.span());
                let styles = self.set_styles_code(set, &env, &rule_at);
                let at = self.span_of(stmt.span());
                last = Some((
                    format!("Value::Content(rt::styled_with_map(({tail}).display(), {styles}))"),
                    at,
                ));
                break;
            }
            if let ast::Expr::ShowRule(show) = stmt {
                let rest: Vec<ast::Expr> = iter.collect();
                let tail = self.block(rest.into_iter(), &env);
                let show_at = self.span_of(show.span());
                let (sel_arg, sel_at) = match show.selector() {
                    Some(sel) => {
                        let sel_at = self.span_of(sel.span());
                        (format!("Some({})", self.expr(sel, &env)), sel_at)
                    }
                    None => ("None".to_string(), show_at.clone()),
                };
                let recipe = match show.transform() {
                    ast::Expr::SetRule(set) => {
                        let set_at = self.span_of(set.span());
                        let styles = self.set_styles_code(set, &env, &set_at);
                        format!(
                            "rt::show_rule_style({sel_arg}, {sel_at}, {styles}, {show_at}, engine)?"
                        )
                    }
                    transform => format!(
                        "rt::show_rule({sel_arg}, {sel_at}, {}, {show_at}, engine)?",
                        self.expr(transform, &env)
                    ),
                };
                let at = self.span_of(stmt.span());
                last = Some((
                    format!(
                        "Value::Content({{ let __recipe = {recipe}; rt::styled_with_recipe(engine, context, ({tail}).display(), __recipe)? }})"
                    ),
                    at,
                ));
                break;
            }
            if let ast::Expr::LetBinding(lets) = stmt {
                let init = match lets.init() {
                    Some(init) => self.expr(init, &env),
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
                                let base = match env.module.iter().rev().find(|(n, _)| *n == root) {
                                    Some((_, module_ref)) => self.ref_value(module_ref),
                                    None => {
                                        // 全局模块(如 `import calc: abs`)。
                                        let at = self.span_of(stmt.span());
                                        format!("rt::global(engine, \"{root}\", {at})?")
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
                        let target = normalize_import(&file, &raw);
                        match (files.contains_key(&target), mods.get(&target)) {
                            (true, Some(mod_name)) => {
                                // 目标文件的模块作用域从槽板构建。
                                self.used_state = true;
                                Some(format!(
                                    "Value::Module({mod_name}::module(&__st, engine, context)?)"
                                ))
                            }
                            _ => {
                                self.diag(
                                    str_expr.span(),
                                    format!("体内 import \"{raw}\" 不在包文件集"),
                                );
                                continue;
                            }
                        }
                    }
                    _ => {
                        self.diag(source.span(), "体内 import 源暂只支持模块访问".into());
                        continue;
                    }
                };
                let Some(module_expr) = module_expr else {
                    continue;
                };
                // 形态:bare / rename → 模块值;列表 → 字段取值。
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
                        self.diag(stmt.span(), "体内通配 import 暂不支持".into());
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
                let value = self.expr(assign_.value(), &env);
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
                // 目标:局部变量(空路径)或可变位置(字段/accessor 链),
                // 对照 typst 的 `Expr::access`;复合赋值读旧值再写回。
                if let Some((root, path)) = self.parse_place(binary.lhs(), &env) {
                    let path_lit = place_path_lit(&path);
                    let at = self.span_of(binary.span());
                    let rhs = self.expr(binary.rhs(), &env);
                    let op = match binary.op() {
                        ast::BinOp::Assign => None,
                        ast::BinOp::AddAssign => Some("add"),
                        ast::BinOp::SubAssign => Some("sub"),
                        ast::BinOp::MulAssign => Some("mul"),
                        ast::BinOp::DivAssign => Some("div"),
                        _ => unreachable!(),
                    };
                    match op {
                        Some(op) => {
                            // 复合赋值:单次寻址 + mem::take(对照原生
                            // `let lhs = std::mem::take(&mut *location)`,
                            // 免一次旧值克隆)。
                            let _ = writeln!(
                                out,
                                "    {{ let __rhs = {rhs}; let __p = rt::place_mut(&mut {root}, {path_lit}, {at})?; let __old = std::mem::take(__p); *__p = ops::{op}(__old, __rhs).at({at})?; }}"
                            );
                        }
                        None => {
                            // 普通赋值:末段字段在字典上缺失即创建
                            // (对照 `apply_assignment` 的 FieldAccess
                            // 特例:access_dict + insert,单次写入)。
                            let _ = writeln!(
                                out,
                                "    {{ let __rhs = {rhs}; rt::assign_value(&mut {root}, {path_lit}, __rhs, {at})?; }}"
                            );
                        }
                    }
                } else if let ast::Expr::Ident(ident) = binary.lhs() {
                    self.diag(
                        binary.span(),
                        format!(
                            "赋值目标 `{}` 不是局部变量(模块槽赋值暂不支持)",
                            ident.as_str()
                        ),
                    );
                } else {
                    self.diag(
                        binary.span(),
                        "赋值目标须为局部变量或字段/accessor 路径".into(),
                    );
                }
                continue;
            }
            let value = self.expr(stmt, &env);
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
        out.push('}');
        out
    }
    /// 解析可变位置(赋值/可变方法目标):根须为局部变量,路径段为
    /// 字段访问与 accessor 方法(`at`/`first`/`last`),对照 typst-eval
    /// `access.rs` 可 access 的形态。返回 (根局部 rust 名, 路径段表达式)。
    ///
    /// 注:`at` 的参数在段处求值,先走目标链后求参数(纯下标表达式
    /// 下与 typst 的"参数先求"等价;带副作用的下标才有顺序差)。
    pub(super) fn parse_place(
        &mut self,
        expr: ast::Expr,
        env: &Env,
    ) -> Option<(String, Vec<String>)> {
        let mut segs = Vec::new();
        let mut root: Option<String> = None;
        if !self.walk_place(expr, env, &mut segs, &mut root) {
            return None;
        }
        let root = root?;
        let (_, rust) = env.locals.iter().rev().find(|(n, _)| *n == root)?;
        Some((rust.clone(), segs))
    }

    fn walk_place(
        &mut self,
        expr: ast::Expr,
        env: &Env,
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
