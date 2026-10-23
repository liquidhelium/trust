//! 闭包捕获分析:确定闭包子树引用了哪些外层名字。
//!
//! **对照原版**:`typst-eval/src/call.rs:708` 的 `CapturesVisitor`
//! (pub)。走查结构逐分支同构(各分支注释带行号锚点),类型不同故
//! 不复用:原版持**运行期** `Scopes`,每个 ident 当场解析并以
//! `Binding::capture` 快照**绑定**(miss 即不捕获,留给调用期);本
//! crate 翻译期无运行期作用域(仅依赖 typst-syntax),只能产出
//! **名字级**自由集 `(名字, 是否数学名)`,由 `capture_pairs` 按我们的
//! env 模型四分流(局部克隆/模块槽/通配层/pending),运行期物化走
//! `rt::capture_code_opt` / `capture_math_opt`(直接调 pub 的
//! `Scopes::get` / `get_in_math`,与原版分档一致)。
//!
//! 对应关系总表(我们 → 原版 visit() 的分支):
//! - Closure(默认值先走,绑定罩体)      → call.rs:764-792
//! - LetBinding(init 先走,绑定留到块边界)→ call.rs:795-806(不 enter,
//!   绑定进当前层,块退出才失效——我们压栈不弹、块边界 truncate 同义)
//! - ForLoop(iterable 外,模式罩体)      → call.rs:809-820
//! - ModuleImport(源走查,列表项绑定)    → call.rs:823-830
//! - FieldAccess / MathFieldAccess(只走 target)→ call.rs:755-762
//! - CodeBlock / ContentBlock(作用域边界)→ call.rs:746-753
//! - Named(名字部分不是引用,只走 expr)  → call.rs:836-841(原版在兜底臂)
//! - Ident / MathIdent(分档捕获)        → call.rs:739-743
//! - Keyed(键+值都走)——原版落兜底的通用
//!   children 走查,行为一致;此处显式化并注释理由(计算键是真实求值)
//! - 兜底 children 走查                  → call.rs:843-847
//! 原版以 `internal.enter()/exit()` 管作用域,我们以 bound 栈的
//! push/pop 与块边界 truncate 等价实现;成员查询语义相同
//! (原版 `internal.get().is_ok()`,我们 `bound.contains`)。

use typst_syntax::ast::{self, AstNode};

pub(super) fn collect_idents(
    node: &typst_syntax::SyntaxNode,
    bound: &mut Vec<String>,
    out: &mut Vec<(String, bool)>,
) {
    use typst_syntax::SyntaxKind as K;
    match node.kind() {
        K::Closure => {
            // 对照 call.rs:764-792:命名参数默认值在 internal.enter()
            // **之前**访问(创建期于外层作用域求值,且不得看见前面的
            // 参数绑定)。
            let Some(cl) = node.cast::<ast::Closure>() else {
                return;
            };
            for param in cl.params().children() {
                if let ast::Param::Named(named) = param {
                    collect_idents(named.expr().to_untyped(), bound, out);
                }
            }
            // 对照 call.rs:771-790:自身名 + 三类参数绑定罩住体。
            let mut pushed = 0;
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
            // 对照 call.rs:795-806:init 先走(绑定生效前同名引用解析
            // 外层),绑定进当前层——语句级 let 持续到所在块结束
            //(弹出交给块边界,同原版不 enter 的语义)。
            let Some(lets) = node.cast::<ast::LetBinding>() else {
                return;
            };
            if let Some(init) = lets.init() {
                collect_idents(init.to_untyped(), bound, out);
            }
            for ident in lets.kind().bindings() {
                bound.push(ident.as_str().to_string());
            }
        }
        K::ModuleImport => {
            // 对照 call.rs:823-830:源表达式照常遍历,列表 import 的
            // 名字是内部绑定(调用期由体内 import 自身绑定;bare/rename
            // 不作处理,与原版一致)。
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
            // 对照 call.rs:809-820:iterable 在外,模式绑定罩住体。
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
            // 对照 call.rs:755-758:字段名不是变量引用,只走 target。
            let Some(access) = node.cast::<ast::FieldAccess>() else {
                return;
            };
            collect_idents(access.target().to_untyped(), bound, out);
        }
        // 数学字段访问(`$arrow.t$` / 三段链 `$arrow.t.twohead$`),
        // 对照 call.rs:759-762:只访问目标,字段名不算捕获(commute
        // 驱动:`$arrow.t$` 的 t 被当变量,调用期报 unknown variable)。
        K::MathFieldAccess => {
            if let Some(access) = node.cast::<ast::MathFieldAccess>() {
                collect_idents(access.target().to_untyped(), bound, out);
            }
        }
        K::Named => {
            // 对照 call.rs:836-841(原版兜底臂内特判):命名参数的
            // 名字部分不是变量引用,只走值表达式。
            let Some(named) = node.cast::<ast::Named>() else {
                return;
            };
            collect_idents(named.expr().to_untyped(), bound, out);
        }
        K::Keyed => {
            // 原版落兜底的通用 children 走查(键+值都访):计算键
            // `("pt-" + str(i)): v` 里的 str 等外层引用是真实求值
            // ——跳过会让解释闭包运行期 unknown variable,此处显式
            // 保留同形行为。
            let Some(keyed) = node.cast::<ast::Keyed>() else {
                return;
            };
            collect_idents(keyed.key().to_untyped(), bound, out);
            collect_idents(keyed.expr().to_untyped(), bound, out);
        }
        K::CodeBlock | K::ContentBlock => {
            // 对照 call.rs:746-753:块作用域,离开时恢复绑定集
            //(原版 internal.enter/exit,我们 mark/truncate)。
            let mark = bound.len();
            for child in node.children() {
                collect_idents(child, bound, out);
            }
            bound.truncate(mark);
        }
        K::Ident | K::MathIdent => {
            // 对照 call.rs:739-743 的分档:Ident → capture(get)
            //(局部→全局→std),MathIdent → capture(get_in_math)
            //(局部→数学作用域→std)。原版此处当场解析快照;我们记
            //(名字, 类别),类别贯穿到运行期物化(capture_code_opt /
            // capture_math_opt)——数学名不回退全局(issue-2885:
            // `$rgb$` 该报 unknown variable),代码名不回退数学作用域。
            let in_math = node.kind() == K::MathIdent;
            let name = node.full_text().to_string();
            if !bound.contains(&name) {
                out.push((name, in_math));
            }
        }
        _ => {
            // 对照 call.rs:843-847:其余自左向右遍历。
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
            !out.iter().any(|(n, _)| n == "from-check"),
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
            !out.iter().any(|(n, _)| n == "from-check"),
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
            !out.iter().any(|(n, _)| n == "from-check") && !out.iter().any(|(n, _)| n == "from-cast"),
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
            !out.iter().any(|(n, _)| n == "from-check"),
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
            !out.iter().any(|(n, _)| n == "twohead") && !out.iter().any(|(n, _)| n == "tt"),
            "数学字段名不应被收集: {out:?}"
        );
        assert!(out.iter().any(|(n, _)| n == "arrow"), "目标名应收集: {out:?}");
    }
}
