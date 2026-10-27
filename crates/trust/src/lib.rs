//! TRUST: a transpiler that compiles Typst packages into Rust.
//!
//! The technical route follows Nuitka: never leave the host runtime — the
//! generated Rust code drives typst/typst-library's value model
//! (`Value`/`Func`/`Content`/`Scope`) instead of re-implementing a type
//! system. Host integration injects a custom World (see `trust-world`)
//! without changing a single line of typst.

pub mod fmt;
pub mod translate;

pub use fmt::format_generated;
pub use translate::{
    Diag, PackageInput, translate, translate_package, translate_package_lenient,
    translate_package_tree, translate_package_tree_lenient,
};

/// Front-end entry: parse typst source into the official syntax tree.
///
/// Uses `typst-syntax` (same version as the host baseline, 0.15.1); the
/// later translation stages work on this tree, with `typst-eval`'s
/// implementation as the semantic reference.
pub fn parse(src: &str) -> typst_syntax::SyntaxNode {
    typst_syntax::parse(src)
}

#[cfg(test)]
mod tests {
    use typst_syntax::SyntaxKind;

    #[test]
    fn parses_a_let_binding() {
        let root = super::parse("#let x = 1 + 2");
        assert!(root.cast::<typst_syntax::ast::Markup>().is_some());
        assert!(root.children().any(|n| n.kind() == SyntaxKind::LetBinding));
    }

    #[test]
    fn translates_simple_function() {
        let out = super::translate("#let add(a, b) = a + b", "test").expect("translate ok");
        assert!(out.contains("pub fn module_init"), "generated:\n{out}");
        assert!(
            out.contains("rt::param_pos(__args, \"a\", span)?"),
            "generated:\n{out}"
        );
    }
}
