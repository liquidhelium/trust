//! Identifier and text helpers for generated code: typst names → valid
//! Rust identifiers, string escaping, case conversion.

pub(super) fn sanitize(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else {
            // Non-ASCII (e.g. Greek parameter names like `θ`) and symbols:
            // encode by code point to keep the mapping injective (`θ` →
            // `u03b8`, no clash with ASCII names, distinct unicode names
            // stay distinct) rather than folding to `_` (which would
            // collide, and `mut _` is illegal anyway).
            out.push_str(&format!("u{:04x}", c as u32));
        }
    }
    // Rust keywords get a raw-identifier prefix (typst allows `fn` etc. as
    // parameter names).
    const KEYWORDS: &[&str] = &[
        "fn", "let", "if", "else", "while", "for", "in", "loop", "match", "return", "break",
        "continue", "struct", "enum", "impl", "trait", "type", "mod", "pub", "use", "as", "const",
        "static", "mut", "ref", "move", "true", "false", "self", "super", "crate", "dyn", "where",
        "unsafe", "async", "await", "box", "union", "extern", "try", "gen",
    ];
    // Reserved words where `r#` does not apply (self/Self/super/crate are
    // forbidden as raw idents): underscore suffix instead (emitting
    // `r#super` gets rejected by rustc). There is also a class of
    // **reserved-but-unused** keywords (lexically reserved by rustc; `r#`
    // is rejected or they are rejected on sight): override/priv/typeof
    // etc. — same suffix.
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

/// Bare identifiers reserved for the generated code's own scopes (function
/// parameters engine/context/span, module aliases rt/ops/std, type names,
/// etc.). A same-named user local would shadow them (a user `let engine =
/// ...` once made generated code use a Value as an &Engine) — hits get a
/// `__trust_` prefix.
pub(super) const RESERVED_RUST_IDENTS: &[&str] = &[
    "engine", "context", "span", "rt", "ops", "std", "SPAN", "Value", "Args", "Dict",
    "Unit", "SourceResult", "Tracked", "Engine", "Context",
];

/// Number same-named locals by order of appearance so generated Rust names
/// stay distinct.
pub(super) fn fresh_rust_ident(name: &str, locals: &[(String, String)]) -> String {
    let mut base = sanitize(name);
    if RESERVED_RUST_IDENTS.contains(&base.as_str()) {
        base = format!("__trust_{base}");
    }
    let mut candidate = base.clone();
    let mut n = 1;
    while locals.iter().any(|(_, rust)| *rust == candidate) {
        n += 1;
        candidate = format!("{base}{n}");
    }
    candidate
}

/// Literal escaping for strings embedded into generated code (backslash and
/// double quote).
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

/// Sanitizing for struct slot names (concatenated identifiers like
/// DEF/G/IMPORT): must not produce an `r#` prefix (illegal inside a
/// concatenation); keywords get a `_kw` suffix instead.
pub(super) fn sanitize_key(name: &str) -> String {
    let mut out = sanitize(name);
    if let Some(stripped) = out.strip_prefix("r#") {
        out = format!("{stripped}_kw");
    }
    out
}
