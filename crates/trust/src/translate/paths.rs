//! Resolution of import-related paths and access chains (shared by in-body
//! imports in single-file mode and by package mode).

use typst_syntax::ast;
use typst_syntax::package::PackageSpec;
use typst_syntax::{RootedPath, VirtualPath, VirtualRoot};

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
/// file's directory. Resolution runs through typst's own join
/// (`PathOrStr::resolve`'s normalization: `.`/`..` folding); a degenerate
/// key or an escaping `..` falls back to the legacy string join — the
/// resulting key then simply misses the file set and runtime typst
/// reports the proper error through the glue path.
pub(super) fn normalize_import(from: &str, raw: &str) -> String {
    if let Some((root, vpath)) = rooted_of(from) {
        let base = vpath.parent().unwrap_or_else(|| vpath.clone());
        let joined: Option<VirtualPath> = if let Some(rel) = raw.strip_prefix('/') {
            // Absolute: anchored at the owning root.
            VirtualPath::new(rel).ok()
        } else {
            base.join(raw).ok()
        };
        if let Some(joined) = joined {
            let rooted = RootedPath::new(root, joined);
            let rel = rooted.vpath().get_with_slash().trim_start_matches('/');
            return match rooted.root() {
                VirtualRoot::Project => rel.to_string(),
                VirtualRoot::Package(spec) => format!("{spec}/{rel}"),
            };
        }
    }
    normalize_import_segments(from, raw)
}

/// The rooted identity of a merge key: `@ns/name:ver/rel` keys root at
/// the package, anything else at the project.
fn rooted_of(from: &str) -> Option<(VirtualRoot, VirtualPath)> {
    if from.starts_with('@') {
        let colon = from.find(':')?;
        let slash = from[colon..].find('/').map(|rel| colon + rel)?;
        let spec: PackageSpec = from[..slash].parse().ok()?;
        let vpath = VirtualPath::new(&from[slash + 1..]).ok()?;
        Some((VirtualRoot::Package(spec), vpath))
    } else {
        Some((VirtualRoot::Project, VirtualPath::new(from).ok()?))
    }
}

/// The legacy string join (fallback for degenerate keys and escapes).
fn normalize_import_segments(from: &str, raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let dir = if raw.starts_with('/') {
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

/// Re-qualifies an **absolute** import made from inside a dependency
/// package into that package's merged key space: `from` is the importing
/// file's merge key (`@ns/name:ver/rel`), and the raw import's leading `/`
/// anchors at the package root (the spec prefix), not the top level —
/// otherwise the target would lose package ownership. Returns `None` for
/// non-absolute imports or non-package importing files.
pub(super) fn qualify_package_absolute(from: &str, raw: &str, target: &str) -> Option<String> {
    if !raw.starts_with('/') {
        return None;
    }
    let colon = from.find(':')?;
    let rel = from[colon..].find('/')?;
    if !from.starts_with('@') {
        return None;
    }
    Some(format!(
        "{}/{}",
        &from[..colon + rel],
        target.trim_start_matches('/')
    ))
}
