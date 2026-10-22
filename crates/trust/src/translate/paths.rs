//! import 相关的路径与访问链解析(单文件体内 import 与包模式共用)。

use typst_syntax::ast;

/// 解析模块值访问链:`deps.oxifmt` → ("deps", ["oxifmt"]);裸 Ident → ("x", [])。
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
    // walk 的 push 发生在递归返回之后,天然按源序(inner 先 outer 后:
    // `deps.cetz.draw` → [cetz, draw])——**不要**再 reverse(单字段链
    // 看不出颠倒,两条以上如 fletcher 的 `import deps.cetz.draw` 会把
    // 字段读反)。
    Some((root, fields))
}

/// 解析 import 路径(POSIX 风格):以 `/` 开头为包根绝对路径,
/// 其余相对当前文件目录(`..` 归一)。
pub(super) fn normalize_import(from: &str, raw: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    let dir = if raw.starts_with('/') {
        Vec::new()
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
