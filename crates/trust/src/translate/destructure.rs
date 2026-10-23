//! 解构:数组/字典模式 → Rust 语句。
//!
//! 对照 typst-eval/src/binding.rs:数组/字典运行时分派,错误文案与
//! 数量校验一致;`assign = true` 时为解构赋值(目标须为已存在局部)。

use typst_syntax::ast::{self, AstNode};

use super::ctx::{Ctx, Env, bind_local};
use super::naming::escape;

impl Ctx {
    pub(super) fn destructure(
        &mut self,
        pattern: ast::Pattern,
        value: String,
        env: &mut Env,
        assign: bool,
    ) -> String {
        match pattern {
            ast::Pattern::Normal(ast::Expr::Ident(ident)) if !assign => {
                // `let` 恒为**遮蔽**:typst 块内 let 不影响外层同名
                // 绑定(块作用域退出即失效)。复用外层变量会让块内
                // let 泄漏写穿外层——fletcher 分支内 `let result =
                // ...` 曾把外层累积数组覆盖成字典,后续 push 报
                // "type dictionary has no method `push`"。
                let rust = bind_local(env, ident.as_str());
                format!("let mut {rust} = {value};\n")
            }
            ast::Pattern::Normal(expr) if assign => {
                // 解构赋值叶子(Ident 与 at 位置):目标经 `Access`
                // 可变解析,at_mut 写回(无插入创建),见
                // `place_write_leaf`。
                self.place_write_leaf(expr, &value, env)
            }
            ast::Pattern::Normal(other) => {
                // `let` 模式的非标识符叶子(对照 binding.rs `destructure`
                // 的叶子 f:非 Ident 即 "cannot assign to this
                // expression";value 已求,保持副作用)。
                let at = self.span_of(other.span());
                format!(
                    "{{ let _ = {value}; bail!({at}, \"cannot assign to this expression\"); }}\n"
                )
            }
            ast::Pattern::Placeholder(_) => format!("let _ = {value};\n"),
            ast::Pattern::Parenthesized(p) => self.destructure(p.pattern(), value, env, assign),
            ast::Pattern::Destructuring(d) => {
                let at = self.span_of(d.span());
                let id = self.fresh_id();
                let tmp = format!("__d{id}");
                let n_items = d.items().count();
                let mut out = format!("let {tmp} = {value};\n");
                // 预分配全部绑定名并提升声明到 match 外:数组/字典两分支
                // 以赋值形式写入同一批变量。**仅 let 模式**(叶子写预绑
                // 局部);赋值模式一概不预绑——叶子按 classify 写既有
                // 目标(局部/模块槽),预绑局部会把模块槽目标劫持成
                // 新局部,槽永不更新(destructuring-assign 根因)。
                for ident in pattern.bindings() {
                    if assign {
                        continue;
                    }
                    let rust = bind_local(env, ident.as_str());
                    out.push_str(&format!("let mut {rust} = Value::None;\n"));
                }

                // 数组分支(索引为运行期变量,sink 居中时随 sink 大小
                // 前进——对照 binding.rs `i += sink_size`)。
                let mut arr = String::new();
                let arr_ref = format!("__a{id}");
                let idx = format!("__i{id}");
                arr.push_str(&format!("let mut {idx} = 0usize;\n"));
                for item in d.items() {
                    match item {
                        ast::DestructuringItem::Pattern(p) => {
                            let v = format!("rt::d_at(&{arr_ref}, {idx}, {at})?");
                            arr.push_str(&self.destructure(p, v, env, true));
                            arr.push_str(&format!("{idx} += 1usize;\n"));
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            // sink 大小 = 1 + len - 模式项总数(对照
                            // binding.rs 的 `checked_sub` 与数量错误)。
                            let sink_size =
                                format!("rt::d_sink_size(&{arr_ref}, {n_items}, {at})?");
                            let n_plain = n_items - 1;
                            let v = format!(
                                "Value::Array(rt::d_rest(&{arr_ref}, {idx}, {sink_size}, {n_plain}, {at})?)"
                            );
                            match spread.sink_expr() {
                                Some(expr) => {
                                    arr.push_str(&self.destructure(
                                        ast::Pattern::Normal(expr),
                                        v,
                                        env,
                                        true,
                                    ));
                                }
                                None => arr.push_str(&format!("let _ = {v};\n")),
                            }
                            arr.push_str(&format!("{idx} += {sink_size};\n"));
                        }
                        ast::DestructuringItem::Named(_) => {
                            arr.push_str(&format!(
                                "bail!({at}, \"cannot destructure named pattern from an array\");\n"
                            ));
                        }
                    }
                }
                // 收尾校验:无 sink 时剩元即数量错(对照 binding.rs 的
                // `if i < len`;有 sink 时剩余已被 sink 吸收,恒等长)。
                arr.push_str(&format!("rt::d_array_finish(&{arr_ref}, &{idx}, {at})?;\n"));

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
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", {at})?", escape(&key));
                            dict.push_str(&self.destructure(
                                ast::Pattern::Normal(ast::Expr::Ident(ident)),
                                v,
                                env,
                                true,
                            ));
                        }
                        ast::DestructuringItem::Named(named) => {
                            let key = named.name().as_str().to_string();
                            used.push(key.clone());
                            let v = format!("rt::d_get(&{dict_ref}, \"{}\", {at})?", escape(&key));
                            dict.push_str(&self.destructure(named.pattern(), v, env, true));
                        }
                        ast::DestructuringItem::Spread(spread) => {
                            if let Some(expr) = spread.sink_expr() {
                                sink = Some((expr, format!("__r{id}")));
                            }
                        }
                        ast::DestructuringItem::Pattern(p) => {
                            let _ = p;
                            dict.push_str(&format!(
                                "bail!({at}, \"cannot destructure unnamed pattern from dictionary\");\n"
                            ));
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
                        true,
                    ));
                }

                out.push_str(&format!(
                    "match &{tmp} {{\n    Value::Array({arr_ref}) => {{\n{arr}    }}\n    Value::Dict({dict_ref}) => {{\n{dict}    }}\n    other => bail!({at}, \"cannot destructure {{}}\", other.ty()),\n}}\n"
                ));
                out
            }
            ast::Pattern::Normal(other) => {
                // 解构赋值的 at 位置叶子:`(a.at(i), b.c.at(j)) = ...`
                //(DestructAssignment 走 access() 即 at_mut,**不**插入
                // 缺失键——与普通赋值的末段插入语义不同)。let 模式的
                // 非标识符叶子按原版报 "cannot assign to this
                // expression"(对照 binding.rs `destructure` 的叶子 f)。
                if assign {
                    let at = self.span_of(other.span());
                    match self.classify_place(other, env) {
                        super::block::Place::Local { root, path } => {
                            let path_lit = super::block::place_path_lit(&path);
                            return format!(
                                "{{ let __p = rt::place_mut(&mut {root}, {path_lit}, {at})?; *__p = {value}; }}\n"
                            );
                        }
                        super::block::Place::Slot { slot, path } => {
                            self.used_state = true;
                            let path_lit = super::block::place_path_lit(&path);
                            return format!(
                                "{{ let mut __msv = rt::read_ms(&__st.{slot})?; let __p = rt::place_mut(&mut __msv, {path_lit}, {at})?; *__p = {value}; rt::write_ms(&__st.{slot}, __msv)?; }}\n"
                            );
                        }
                        super::block::Place::Captured => {
                            return format!(
                                "{{ let _ = {value}; rt::bail_captured(typest_capturer())?; }}\n"
                            )
                        }
                        super::block::Place::Unresolved { name } => {
                            return format!(
                                "{{ let _ = {value}; rt::check_mut_unknown(engine, \"{}\", {at})?; }}\n",
                                escape(&name)
                            )
                        }
                        super::block::Place::Temporary => {
                            return format!(
                                "{{ let _ = {value}; rt::bail_temporary({at})?; }}\n"
                            )
                        }
                        super::block::Place::Deferred => {
                            self.diag(other.span(), "解构赋值目标为运行期模块引用".into());
                            return String::new();
                        }
                    }
                }
                format!("{{ let _ = {value}; bail!({at_span}, \"cannot assign to this expression\"); }}\n",
                    at_span = self.span_of(other.span()))
            }
        }
    }
}
