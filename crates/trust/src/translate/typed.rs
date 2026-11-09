//! L1 typed internals, first slice: float locals that never leave `f64`.
//!
//! A binding is a float local when every value written to it is built from
//! float literals and other float locals through `+`, `-`, `*`, `/`, and
//! unary `+`/`-`, and no read of it escapes that fragment. Escaping means
//! the value is returned, passed to a call, stored in a collection,
//! compared, or used as a condition: the binding then stays `Value` and the
//! expression boxes at the boundary.
//!
//! Integers are excluded. Typst's `ops::add` on ints is a checked add that
//! errors on overflow; a native wrapping add would change that.

use std::collections::BTreeSet;

use typst_syntax::ast;

/// Names proven to be float locals over the whole of `root`.
pub(super) fn float_locals(root: &typst_syntax::SyntaxNode) -> BTreeSet<String> {
    if has_expr_let(root) {
        return BTreeSet::new();
    }
    let mut proven = BTreeSet::new();
    loop {
        let mut next = BTreeSet::new();
        let mut rejected = BTreeSet::new();
        collect(root, &proven, &mut next, &mut rejected);
        mark_escaping_reads(root, &next, &mut rejected);
        next.retain(|name| !rejected.contains(name));
        if next == proven {
            return proven;
        }
        proven = next;
    }
}

/// An expression-position `let` binds through a hoisted `Value` slot, which
/// cannot hold an unboxed float. A body containing one stays on the `Value`
/// path entirely. Statement-level lets (directly inside a code block,
/// including the function body itself) bind plain locals and are fine.
fn has_expr_let(root: &typst_syntax::SyntaxNode) -> bool {
    fn walk(node: &typst_syntax::SyntaxNode, stmt_level: bool) -> bool {
        if !stmt_level && node.cast::<ast::LetBinding>().is_some() {
            return true;
        }
        // A code block's statements sit under an inner `Code` node, not
        // directly under the `CodeBlock`; both levels hold statements.
        let stmt_level = node.cast::<ast::CodeBlock>().is_some()
            || node.kind() == typst_syntax::SyntaxKind::Code;
        node.children().any(|child| walk(child, stmt_level))
    }
    walk(root, root.cast::<ast::CodeBlock>().is_some())
}

fn collect(
    node: &typst_syntax::SyntaxNode,
    proven: &BTreeSet<String>,
    next: &mut BTreeSet<String>,
    rejected: &mut BTreeSet<String>,
) {
    if let Some(binary) = node.cast::<ast::Binary>() {
        if matches!(
            binary.op(),
            ast::BinOp::Assign | ast::BinOp::AddAssign | ast::BinOp::SubAssign
                | ast::BinOp::MulAssign | ast::BinOp::DivAssign
        ) {
            if let ast::Expr::Ident(ident) = binary.lhs() {
                let name = ident.as_str().to_string();
                if float_shape(binary.rhs(), proven) {
                    next.insert(name);
                } else {
                    rejected.insert(name);
                }
            }
        }
    }
    if let Some(lets) = node.cast::<ast::LetBinding>() {
        if let ast::LetBindingKind::Normal(ast::Pattern::Normal(ast::Expr::Ident(ident))) =
            lets.kind()
        {
            let name = ident.as_str().to_string();
            match lets.init() {
                Some(init) if float_shape(init, proven) => {
                    next.insert(name);
                }
                Some(_) => {
                    rejected.insert(name);
                }
                None => {}
            }
        }
    }
    for child in node.children() {
        collect(child, proven, next, rejected);
    }
}

/// Whether `expr` computes a float from float literals, unary `+`/`-`,
/// `+` `-` `*` `/`, parentheses, and names already proven to be float locals.
pub(super) fn is_float_expr(expr: ast::Expr, proven: &BTreeSet<String>) -> bool {
    !proven.is_empty() && float_shape(expr, proven)
}

fn float_shape(expr: ast::Expr, proven: &BTreeSet<String>) -> bool {
    match expr {
        ast::Expr::Float(_) => true,
        ast::Expr::Parenthesized(p) => float_shape(p.expr(), proven),
        ast::Expr::Unary(unary) => {
            matches!(unary.op(), ast::UnOp::Pos | ast::UnOp::Neg)
                && float_shape(unary.expr(), proven)
        }
        ast::Expr::Binary(binary) => {
            matches!(
                binary.op(),
                ast::BinOp::Add | ast::BinOp::Sub | ast::BinOp::Mul | ast::BinOp::Div
            ) && float_shape(binary.lhs(), proven)
                && float_shape(binary.rhs(), proven)
        }
        ast::Expr::Ident(ident) => proven.contains(ident.as_str()),
        _ => false,
    }
}

/// A float local read anywhere except inside a float expression escapes
/// into `Value` land and cannot stay unboxed.
fn mark_escaping_reads(
    node: &typst_syntax::SyntaxNode,
    candidates: &BTreeSet<String>,
    rejected: &mut BTreeSet<String>,
) {
    let covered = if let Some(expr) = node.cast::<ast::Expr>() {
        is_float_expr(expr, candidates)
    } else {
        false
    };
    if covered {
        return;
    }
    if let Some(ident) = node.cast::<ast::Ident>() {
        let name = ident.as_str();
        if candidates.contains(name) {
            rejected.insert(name.to_string());
        }
    }
    for child in node.children() {
        mark_escaping_reads(child, candidates, rejected);
    }
}
