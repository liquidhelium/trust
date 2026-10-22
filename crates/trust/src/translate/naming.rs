//! 生成代码的标识符与文本工具:typst 名字 → 合法 rust 标识符、
//! 字符串转义、大小写变形。

pub(super) fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            // 非 ASCII(如希腊字母参数名 `θ`)与符号:按码点编码,
            // 保名单射(`θ`→`u03b8`,不与 ASCII 名冲突、不同
            // unicode 名互异),而非折叠成 `_`(会撞名且 `mut _`
            // 非法)。
            out.push_str(&format!("u{:04x}", c as u32));
        }
    }
    // Rust 关键字加原始标识符前缀(typst 允许 `fn` 等作参数名)。
    const KEYWORDS: &[&str] = &[
        "fn", "let", "if", "else", "while", "for", "in", "loop", "match", "return", "break",
        "continue", "struct", "enum", "impl", "trait", "type", "mod", "pub", "use", "as", "const",
        "static", "mut", "ref", "move", "true", "false", "self", "super", "crate", "dyn", "where",
        "unsafe", "async", "await", "box", "union",
    ];
    // r# 不适用的保留字(self/Self/super/crate 禁作 raw ident):
    // 加下划线后缀(cmarker 驱动:`super` 生成 `r#super` 被 rustc 拒)。
    // 另有一类**保留未用**关键字(rustc 词法保留,r# 也拒或列入即拒):
    // override/priv/typeof 等——同样后缀(codly 驱动:`override` 裸出)。
    const RAW_ILLEGAL: &[&str] = &[
        "self", "Self", "super", "crate", "override", "priv", "typeof", "unsized", "virtual",
        "yield", "abstract", "become", "do", "final", "macro",
    ];
    if RAW_ILLEGAL.contains(&out.as_str()) {
        out.push('_');
    } else if KEYWORDS.contains(&out.as_str()) {
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

/// 结构体槽名(DEF/G/IMPORT 等标识符拼接)用的清洗:不产出 `r#`
/// 前缀(拼接后非法),关键字转为 `_kw` 后缀。
pub(super) fn sanitize_key(name: &str) -> String {
    let mut out = sanitize(name);
    if let Some(stripped) = out.strip_prefix("r#") {
        out = format!("{stripped}_kw");
    }
    out
}
