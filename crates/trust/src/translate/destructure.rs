//! 解构:数组/字典模式 → Rust 语句。
//!
//! 对照 typst-eval/src/binding.rs:数组/字典运行时分派,错误文案与
//! 数量校验一致;`assign = true` 时为解构赋值(目标须为已存在局部)。

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, Mode, bind_local};
use super::naming::escape;

impl Ctx {
    pub(super) fn destructure(
        &mut self,
        pattern: ast::Pattern,
        value: String,
        env: &mut Env,
        mode: Mode,
        assign: bool,
    ) -> String {
        match pattern {
            ast::Pattern::Normal(ast::Expr::Ident(ident)) => {
                let name = ident.as_str().to_string();
                match env.locals.iter().rev().find(|(n, _)| *n == name) {
                    // 已有同名:解构叶子写入既有变量(遮蔽在可见性上等价)。
                    Some((_, rust)) => format!("{rust} = {value};\n"),
                    None => {
                        if assign {
                            self.diag(ident.span(), format!("解构赋值目标 `{name}` 不是局部变量"));
                            String::new()
                        } else {
                            let rust = bind_local(env, &name);
                            format!("let mut {rust} = {value};\n")
                        }
                    }
                }
            }
            ast::Pattern::Placeholder(_) => format!("let _ = {value};\n"),
            ast::Pattern::Parenthesized(p) => {
                self.destructure(p.pattern(), value, env, mode, assign)
            }
            ast::Pattern::Destructuring(d) => {
                let id = self.fresh_id();
                let tmp = format!("__d{id}");
                let n_items = d.items().count();
                let mut out = format!("let {tmp} = {value};\n");
                // 预分配全部绑定名并提升声明到 match 外:数组/字典两分支
                // 以赋值形式写入同一批变量。
                for ident in pattern.bindings() {
                    if env.locals.iter().any(|(n, _)| *n == ident.as_str()) {
                        continue;
                    }
                    let rust = bind_local(env, ident.as_str());
                    out.push_str(&format!("let mut {rust} = Value::None;\n"));
                }

                // 数组分支。
                let mut arr = String::new();
                let arr_ref = format!("__a{id}");
                let mut i = 0usize;
                let mut has_spread = false;
                for item in d.items() {
                    match item {
                        ast::DestructuringItem::Pattern(p) => {
                            let v = format!("rt::d_at(&{arr_ref}, {i}, SPAN)?");
                            arr.push_str(&self.destructure(p, v, env, mode, true));
                            i += 1;
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            has_spread = true;
                            let v = format!(
                                "Value::Array(rt::d_rest(&{arr_ref}, {i}, {n_items}, SPAN)?)"
                            );
                            match spread.sink_expr() {
                                Some(expr) => {
                                    arr.push_str(&self.destructure(
                                        ast::Pattern::Normal(expr),
                                        v,
                                        env,
                                        mode,
                                        true,
                                    ));
                                }
                                None => arr.push_str(&format!("let _ = {v};\n")),
                            }
                        }
                        ast::DestructuringItem::Named(_) => {
                            arr.push_str(
                                "bail!(SPAN, \"cannot destructure named pattern from an array\");\n",
                            );
                        }
                    }
                }
                if !has_spread {
                    arr.push_str(&format!("rt::d_array_finish(&{arr_ref}, {i}, SPAN)?;\n"));
                }

                // 字典分支。
                let mut dict = String::new();
                let dict_ref = format!("__k{id}");
                let mut used: Vec<String> = Vec::new();
                let mut sink: Option<(ast::Expr, String)> = None;
                for item in d.items() {
                    match item {
                        ast::DestructuringItem::Pattern(ast::Pattern::Normal(
                            ast::Expr::Ident(ident),
                        )) => {
                            let key = ident.as_str().to_string();
                            used.push(key.clone());
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", SPAN)?", escape(&key));
                            dict.push_str(&self.destructure(
                                ast::Pattern::Normal(ast::Expr::Ident(ident)),
                                v,
                                env,
                                mode,
                                true,
                            ));
                        }
                        ast::DestructuringItem::Named(named) => {
                            let key = named.name().as_str().to_string();
                            used.push(key.clone());
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", SPAN)?", escape(&key));
                            dict.push_str(&self.destructure(named.pattern(), v, env, mode, true));
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            if let Some(expr) = spread.sink_expr() {
                                sink = Some((expr, format!("__r{id}")));
                            }
                        }
                        ast::DestructuringItem::Pattern(p) => {
                            let _ = p;
                            dict.push_str(
                                "bail!(SPAN, \"cannot destructure unnamed pattern from dictionary\");\n",
                            );
                        }
                    }
                }
                if let Some((expr, sink_tmp)) = sink {
                    let used_list = used
                        .iter()
                        .map(|k| format!("\"{}\"", escape(k)))
                        .collect::<Vec<_>>()
                        .join(", ");
                    dict.push_str(&format!(
                        "let {sink_tmp} = rt::d_dict_rest(&{dict_ref}, &[{used_list}]);\n"
                    ));
                    dict.push_str(&self.destructure(
                        ast::Pattern::Normal(expr),
                        format!("Value::Dict({sink_tmp})"),
                        env,
                        mode,
                        true,
                    ));
                }

                out.push_str(&format!(
                    "match &{tmp} {{\n    Value::Array({arr_ref}) => {{\n{arr}    }}\n    Value::Dict({dict_ref}) => {{\n{dict}    }}\n    other => bail!(SPAN, \"cannot destructure {{}}\", other.ty()),\n}}\n"
                ));
                out
            }
            ast::Pattern::Normal(other) => {
                // 解构赋值的 accessor 目标:`(a.at(i), b.at(j)) = ...`。
                if assign
                    && let ast::Expr::FuncCall(call) = other
                    && let ast::Expr::FieldAccess(access) = call.callee()
                    && let ast::Expr::Ident(ident) = access.target()
                    && matches!(access.field().as_str(), "at" | "first" | "last")
                    && let Some((_, rust)) =
                        env.locals.iter().rev().find(|(n, _)| *n == ident.as_str())
                {
                    let method = access.field().as_str();
                    let mut index = "None".to_string();
                    for arg in call.args().items() {
                        if let ast::Arg::Pos(expr) = arg {
                            index = format!("Some({})", self.expr(expr, env, mode));
                            break;
                        }
                    }
                    return format!(
                        "rt::accessor_assign(&mut {rust}, \"{method}\", {index}, {value}, SPAN)?;\n"
                    );
                }
                self.diag(other.span(), "解构模式此处须为标识符".into());
                String::new()
            }
        }
    }
}
