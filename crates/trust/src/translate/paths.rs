//! Resolution of import-related paths and access chains (shared by in-body
//! imports in single-file mode and by package mode).

use typst_syntax::ast;

/// Parse a module-value access chain: `deps.oxifmt` → ("deps", ["oxifmt"]);
/// a bare Ident → ("x", []).
pub(super) fn parse_module_access(expr: ast::Expr) -> Option<(String, Vec<String>)> {
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
    // walk pushes after its recursive call returns, so fields come out in
    // natural source order (inner first, outer last: `deps.cetz.draw` →
    // [cetz, draw]) — do **not** reverse (a single-field chain hides the
    // inversion; with two or more, `import deps.cetz.draw` would read the
    // fields backwards).
    Some((root, fields))
}

/// Normalize an import path (POSIX style): a leading `/` is an absolute
/// path from the package root; anything else is relative to the current
/// file's directory (`..` segments collapse).
pub(super) fn normalize_import(from: &str, raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let dir = if raw.starts_with('/') {
        // Absolute path: anchored at the owning root. In package key
        // space (`@ns/name:ver/...` keys) that root is the spec segment
        // — typst's vpath join re-anchors inside the same root, never
        // the project. Degenerate ".." pops past the root merely miss
        // the file set (runtime typst then reports the proper escape
        // error through the glue fallback).
        match from.split_once('/') {
            Some((first, _)) if first.starts_with('@') => vec![first],
            _ => Vec::new(),
        }
    } else {
        match from.rfind('/') {
            Some(idx) => from[..idx].split('/').collect::<Vec<_>>(),
            None => Vec::new(),
        }
    };
    parts.extend(dir);
    for seg in raw.split('/') {
        match seg {
            "." | "" => {}
            ".." => {
                parts.pop();
            }
            other => parts.push(other),
        }
    }
    parts.join("/")
}
