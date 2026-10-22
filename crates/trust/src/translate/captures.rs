//! 闭包捕获分析:确定闭包子树引用了哪些外层名字。
//!
//! 收集闭包子树中的自由变量名(对照 typst-eval 的 `CapturesVisitor`,
//! 但目标是从"外层视角"确定捕获集):
//! - 闭包名/参数/内部 let/for 变量/内层闭包的绑定是内部名,跳过;
//! - 命名参数默认值在外层作用域求值,按外层绑定集分析;
//! - 字段访问的字段名、命名参数与字典的键名不是变量引用,跳过。

use typst_syntax::ast::{self, AstNode};

pub(super) fn collect_idents(  // WALK 计数供 TRUST_TRACE_CAPS 调试
    node: &typst_syntax::SyntaxNode,
    bound: &mut Vec<String>,
    out: &mut Vec<String>,
) {
    use typst_syntax::SyntaxKind as K;
    match node.kind() {
        K::Closure => {
            let Some(cl) = node.cast::<ast::Closure>() else {
                return;
            };
            // 默认值在外层作用域求值。
            for param in cl.params().children() {
                if let ast::Param::Named(named) = param {
                    collect_idents(named.expr().to_untyped(), bound, out);
                }
            }
            let mut pushed = 0;
            if std::env::var_os("TRUST_TRACE_CAPS").is_some() {
                static WALK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
                let w = WALK.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let has_fc = cl.params().children().any(|p| matches!(p, ast::Param::Pos(pat) if pat.bindings().into_iter().any(|i| i.as_str() == "from-check")));
                if has_fc { eprintln!("[closure#{w}] from-check 闭包入 Closure 分支,参数绑定后 in-bound=TBD"); }
            }
            if let Some(name) = cl.name() {
                bound.push(name.as_str().to_string());
                pushed += 1;
            }
            for param in cl.params().children() {
                match param {
                    ast::Param::Pos(pattern) => {
                        for ident in pattern.bindings() {
                            bound.push(ident.as_str().to_string());
                            pushed += 1;
                        }
                    }
                    ast::Param::Named(named) => {
                        bound.push(named.name().as_str().to_string());
                        pushed += 1;
                    }
                    ast::Param::Spread(spread) => {
                        if let Some(ident) = spread.sink_ident() {
                            bound.push(ident.as_str().to_string());
                            pushed += 1;
                        }
                    }
                }
            }
            collect_idents(cl.body().to_untyped(), bound, out);
            for _ in 0..pushed {
                bound.pop();
            }
        }
        K::LetBinding => {
            let Some(lets) = node.cast::<ast::LetBinding>() else {
                return;
            };
            if let Some(init) = lets.init() {
                collect_idents(init.to_untyped(), bound, out);
            }
            // 语句级 let 的绑定持续到所在块结束(弹出交给块边界处理)。
            for ident in lets.kind().bindings() {
                bound.push(ident.as_str().to_string());
            }
        }
        K::ModuleImport => {
            // 对照 typst-eval 的 CapturesVisitor:源表达式照常遍历,
            // 列表 import 的名字是内部绑定(调用期由体内 import 自身
            // 绑定;bare/rename 不作处理,与原版一致)。
            let Some(import) = node.cast::<ast::ModuleImport>() else {
                return;
            };
            collect_idents(import.source().to_untyped(), bound, out);
            if let Some(ast::Imports::Items(items)) = import.imports() {
                for item in items.iter() {
                    bound.push(item.bound_name().as_str().to_string());
                }
            }
        }
        K::ForLoop => {
            let Some(for_loop) = node.cast::<ast::ForLoop>() else {
                return;
            };
            collect_idents(for_loop.iterable().to_untyped(), bound, out);
            let mut pushed = 0;
            for ident in for_loop.pattern().bindings() {
                bound.push(ident.as_str().to_string());
                pushed += 1;
            }
            collect_idents(for_loop.body().to_untyped(), bound, out);
            for _ in 0..pushed {
                bound.pop();
            }
        }
        K::FieldAccess => {
            let Some(access) = node.cast::<ast::FieldAccess>() else {
                return;
            };
            collect_idents(access.target().to_untyped(), bound, out);
        }
        // 数学字段访问(`$arrow.t$` / 三段链 `$arrow.t.twohead$`):
        // **对照 typst-eval CapturesVisitor 同名分支逐字同形**——只访
        // 问目标(access.target()),字段名不算捕获(commute 驱动:
        // `$arrow.t$` 的 t 被当变量,调用期报 unknown variable)。
        K::MathFieldAccess => {
            if let Some(access) = node.cast::<ast::MathFieldAccess>() {
                collect_idents(access.target().to_untyped(), bound, out);
            }
        }
        K::Named => {
            let Some(named) = node.cast::<ast::Named>() else {
                return;
            };
            collect_idents(named.expr().to_untyped(), bound, out);
        }
        K::Keyed => {
            let Some(keyed) = node.cast::<ast::Keyed>() else {
                return;
            };
            // 键表达式照常遍历(对照原生 CapturesVisitor:只有命名参数
            // 跳过名字部分;计算键 `("pt-" + str(i)): v` 里的 str 等
            // 外层引用是真实求值——跳过会让解释闭包运行期 unknown
            // variable)。
            collect_idents(keyed.key().to_untyped(), bound, out);
            collect_idents(keyed.expr().to_untyped(), bound, out);
        }
        K::CodeBlock | K::ContentBlock => {
            // 块作用域:离开时恢复绑定集。
            let mark = bound.len();
            for child in node.children() {
                collect_idents(child, bound, out);
            }
            bound.truncate(mark);
        }
        K::Ident | K::MathIdent => {
            // 对照原生 CapturesVisitor:Ident 与 MathIdent 都是潜在
            // 捕获(数学名先查常规作用域再查数学作用域;仅存于数学
            // 作用域的符号名捕获落空,调用期由数学作用域解析)。
            let name = node.full_text().to_string();
            if !bound.contains(&name) {
                out.push(name);
            }
        }
        _ => {
            for child in node.children() {
                collect_idents(child, bound, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inline_closure_params_not_collected() {
        let src = "#let outer(a) = { call(c: from-check => if from-check == none { none } else { from-check(1) }, d: 2); a }";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.contains(&"from-check".to_string()),
            "内联闭包参数不应被收集: {out:?}"
        );
    }

    #[test]
    fn multiline_inline_closure_not_collected() {
        let src = r#"#let declare(from, cast-check) = {
  let from-cast = from.cast

  types.wrap(
    from,
    check: from-check => if from-check == none {
      if cast-check == none { none } else { from.cast }
    } else {
      from-check(1)
    },
  )
  from
}"#;
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.contains(&"from-check".to_string()),
            "多行内联闭包参数不应被收集: {out:?}"
        );
    }

    #[test]
    fn elembic_real_closure_shape() {
        let src = "            false,
            message: \"elembic: types.declare: cast 'with' can only be omitted for 'from: dictionary'. It must receive the default constructor and return a function 'casted value => your type'.\"
          )
        }

        if type(with) != function {
          assert(
            false,
            message: \"elembic: types.declare: cast 'with' must receive the default constructor and return a function 'casted value => your type'. Received \" + base.typename(with)
          )
        }

        let from-cast = from.cast

        types.wrap(
          from,
          check: from-check => if from-check == none {
            if cast-check == none {
              none
            } else if from.cast == none {
              cast-check
            } else {
              value => cast-check(from-cast(value))
            }
          } else if cast-check == none {
            from-check
          } else if from.cast == none {
            value => from-check(value) and cast-check(value)
          } else {
            value => from-check(value) and cast-check(from-cast(value))
          },

          output: (typeid,),

          cast: from-cast => if from-cast == none {
            with
          } else {
";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.contains(&"from-check".to_string()) && !out.contains(&"from-cast".to_string()),
            "elembic 真实形态:内联闭包参数不应被收集: {out:?}"
        );
    }

    #[test]
    fn elembic_declare_slice() {
        let src = include_str!("../../tests/fixtures/elembic-declare.txt");
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.contains(&"from-check".to_string()),
            "declare 切片:from-check 不应被收集: {out:?}"
        );
    }

    #[test]
    fn math_field_names_not_collected() {
        let src = "#let f(x) = { box($arrow.t.twohead$); $arrows.tt$; $arrow.t$; x }";
        let root = typst_syntax::parse_code(src);
        let mut out = Vec::new();
        collect_idents(&root, &mut Vec::new(), &mut out);
        assert!(
            !out.contains(&"twohead".to_string()) && !out.contains(&"tt".to_string()),
            "数学字段名不应被收集: {out:?}"
        );
        assert!(out.contains(&"arrow".to_string()), "目标名应收集: {out:?}");
    }
}
