//! 代码块与语句直译:let 绑定、体内局部 import、赋值与解构赋值。
//!
//! 非末位语句值按 join 语义累积,末位表达式为块值(对照 typst code
//! block 求值)。

use std::fmt::Write as _;

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, Mode, Ref, bind_local};
use super::naming::escape;
use super::paths::{normalize_import, parse_module_access};

impl Ctx {
    pub(super) fn block<'a>(
        &mut self,
        stmts: impl Iterator<Item = ast::Expr<'a>>,
        env: &Env,
        mode: Mode,
    ) -> String {
        let mut out = String::from("{ let mut __out = Value::None;\n");
        let mut env = env.clone();
        let mut last: Option<String> = None;
        for stmt in stmts {
            if let Some(prev) = last.take() {
                let _ = writeln!(out, "    __out = rt::join(__out, {prev}, SPAN)?;");
            }
            if let ast::Expr::LetBinding(lets) = stmt {
                let init = match lets.init() {
                    Some(init) => self.expr(init, &env, mode),
                    None => "Value::None".into(),
                };
                match lets.kind() {
                    ast::LetBindingKind::Closure(name) => {
                        let rust = bind_local(&mut env, name.as_str());
                        let _ = writeln!(out, "    let mut {rust} = {init};");
                    }
                    ast::LetBindingKind::Normal(pattern) => {
                        let stmts = self.destructure(pattern, init, &mut env, mode, false);
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
                                    Some((_, module_ref)) => match module_ref {
                                        Ref::Slot(id) => format!("rt::slot(&{id})"),
                                        Ref::Engine(id) => format!("{id}_get()?"),
                                        Ref::Value(e) => e.clone(),
                                        Ref::ModuleField { expr, name } => {
                                            format!("rt::field(&({expr}), \"{name}\", SPAN)?")
                                        }
                                    },
                                    None if mode == Mode::FnBody => {
                                        // 全局模块(如 `import calc: abs`)。
                                        format!("rt::global(engine, \"{root}\", SPAN)?")
                                    }
                                    None => {
                                        self.diag(
                                            source.span(),
                                            format!("import 的模块根 `{root}` 不可见"),
                                        );
                                        continue;
                                    }
                                };
                                let mut expr = base;
                                for field in &fields {
                                    expr = format!(
                                        "rt::field(&({expr}), \"{}\", SPAN)?",
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
                                Some(format!("Value::Module({mod_name}::module())"))
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
                            let _ = writeln!(
                                out,
                                "    let mut {rust} = rt::field(&({module_expr}), \"{}\", SPAN)?;",
                                escape(&original)
                            );
                        }
                    }
                }
                continue;
            }
            if let ast::Expr::DestructAssignment(assign_) = stmt {
                // 解构赋值:目标须为已存在局部(对照 `Access` 语义)。
                let value = self.expr(assign_.value(), &env, mode);
                let stmts = self.destructure(assign_.pattern(), value, &mut env, mode, true);
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
                // 复合赋值 `x += y` 等价 `x = x + y`(右侧读旧值)。
                if binary.op() != ast::BinOp::Assign
                    && let ast::Expr::Ident(ident) = binary.lhs()
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    let rhs = self.expr(binary.rhs(), &env, mode);
                    let op = match binary.op() {
                        ast::BinOp::AddAssign => "add",
                        ast::BinOp::SubAssign => "sub",
                        ast::BinOp::MulAssign => "mul",
                        ast::BinOp::DivAssign => "div",
                        _ => unreachable!(),
                    };
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {rhs}; {rust} = ops::{op}({rust}, __rhs).at(SPAN)?; }}"
                    );
                    continue;
                }
                // 局部重赋值 `x = e`(typst 局部可变;遮蔽链上最新槽)。
                if let ast::Expr::Ident(ident) = binary.lhs() {
                    let name = ident.as_str();
                    match env.locals.iter().rev().find(|(n, _)| n == name) {
                        Some((_, rust)) => {
                            let value = self.expr(binary.rhs(), &env, mode);
                            let _ = writeln!(out, "    {rust} = {value};");
                        }
                        None => {
                            self.diag(
                                binary.span(),
                                format!("赋值目标 `{name}` 不是局部变量(模块槽赋值暂不支持)"),
                            );
                        }
                    }
                } else if let Some((rust, fields)) = field_chain_place(binary.lhs(), &env) {
                    // 字段(链)赋值 `d.k = v` / `d.a.b = v`:就地修改局部字典。
                    let path = fields
                        .iter()
                        .map(|f| format!("\"{}\"", escape(f)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let value = self.expr(binary.rhs(), &env, mode);
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {value}; rt::field_assign_path(&mut {rust}, &[{path}], __rhs, SPAN)?; }}"
                    );
                } else if let ast::Expr::FuncCall(call) = binary.lhs()
                    && let ast::Expr::FieldAccess(access) = call.callee()
                    && let ast::Expr::Ident(ident) = access.target()
                    && matches!(access.field().as_str(), "at" | "first" | "last")
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    // accessor 赋值:`a.at(i) = v` / `a.first() = v`。
                    let method = access.field().as_str();
                    let mut index = "None".to_string();
                    for arg in call.args().items() {
                        if let ast::Arg::Pos(expr) = arg {
                            index = format!("Some({})", self.expr(expr, &env, mode));
                            break;
                        }
                    }
                    let value = self.expr(binary.rhs(), &env, mode);
                    let _ = writeln!(
                        out,
                        "    {{ let __rhs = {value}; rt::accessor_assign(&mut {rust}, \"{method}\", {index}, __rhs, SPAN)?; }}"
                    );
                } else {
                    self.diag(binary.span(), "字段链/复杂目标赋值暂不支持".into());
                }
                continue;
            }
            let value = self.expr(stmt, &env, mode);
            last = Some(value);
        }
        match last {
            Some(tail) => {
                let _ = writeln!(out, "    let _ = __out;");
                let _ = writeln!(out, "    {tail}");
                out.push('}');
                out
            }
            None => {
                let _ = writeln!(out, "    let _ = __out;");
                let _ = writeln!(out, "    Value::None");
                out.push('}');
                out
            }
        }
    }
}

/// 赋值目标为 `x.a.b.c` 形态:返回 (根局部 rust 名, 字段链)。
fn field_chain_place(expr: ast::Expr, env: &Env) -> Option<(String, Vec<String>)> {
    fn walk(expr: ast::Expr, fields: &mut Vec<String>) -> Option<String> {
        match expr {
            ast::Expr::Ident(ident) => Some(ident.as_str().to_string()),
            ast::Expr::FieldAccess(access) => {
                let root = walk(access.target(), fields)?;
                fields.push(access.field().as_str().to_string());
                Some(root)
            }
            _ => None,
        }
    }
    let mut fields = Vec::new();
    let root = walk(expr, &mut fields)?;
    let (_, rust) = env.locals.iter().rev().find(|(n, _)| *n == root)?;
    // 后序收集已是从根到叶的字段序,无需 reverse。
    Some((rust.clone(), fields))
}
