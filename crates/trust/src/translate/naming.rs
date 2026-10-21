//! 生成代码的标识符与文本工具:typst 名字 → 合法 rust 标识符、
//! 字符串转义、大小写变形。

pub(super) fn sanitize(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    // Rust 关键字加原始标识符前缀(typst 允许 `fn` 等作参数名)。
    const KEYWORDS: &[&str] = &[
        "fn", "let", "if", "else", "while", "for", "in", "loop", "match", "return",
        "break", "continue", "struct", "enum", "impl", "trait", "type", "mod", "pub",
        "use", "as", "const", "static", "mut", "ref", "move", "true", "false", "self",
        "super", "crate", "dyn", "where", "unsafe", "async", "await", "box", "union",
    ];
    if KEYWORDS.contains(&out.as_str()) {
        out = format!("r#{out}");
    }
    if out.is_empty() {
        out.push('v');
    }
    if !(out.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')) {
        out.insert(0, '_');
    }
    out
}

/// 同名局部按出现序加序号,保证生成 rust 名互异。
pub(super) fn fresh_rust_ident(name: &str, locals: &[(String, String)]) -> String {
    let base = sanitize(name);
    let mut candidate = base.clone();
    let mut n = 1;
    while locals.iter().any(|(_, rust)| *rust == candidate) {
        n += 1;
        candidate = format!("{base}{n}");
    }
    candidate
}

/// 字符串嵌入生成代码的字面量转义(反斜杠与双引号)。
pub(super) fn escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

pub(super) fn lowercase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_lowercase().to_string() + c.as_str(),
        None => String::new(),
    }
}

pub(super) fn uppercase(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_ascii_uppercase().to_string() + c.as_str(),
        None => String::new(),
    }
}
