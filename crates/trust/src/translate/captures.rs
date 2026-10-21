//! 闭包捕获分析:确定闭包子树引用了哪些外层名字。
//!
//! 收集闭包子树中的自由变量名(对照 typst-eval 的 `CapturesVisitor`,
//! 但目标是从"外层视角"确定捕获集):
//! - 闭包名/参数/内部 let/for 变量/内层闭包的绑定是内部名,跳过;
//! - 命名参数默认值在外层作用域求值,按外层绑定集分析;
//! - 字段访问的字段名、命名参数与字典的键名不是变量引用,跳过。

use typst_syntax::ast::{self, AstNode};

pub(super) fn collect_idents(
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
        K::Ident => {
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
