//! TRUST:把 Typst 包编译成 Rust 的转译器。
//!
//! 技术路线对标 Nuitka:不脱离宿主运行时——生成的 Rust 代码驱动
//! typst/typst-library 的值模型(`Value`/`Func`/`Content`/`Scope`),
//! 而不是重新实现一套类型系统。宿主接入用自定义 World 注入(见
//! `trust-world`),不改 typst 一行代码。
//!
//! 分层与语义边界见 `docs/research-route.md`,语言子集的判定以
//! `crates/trust-diff` 的差分测试为准(解释器 golden ↔ 生成物)。

pub mod translate;

pub use translate::{translate, translate_package, Diag, PackageInput};

/// 前端入口:把 typst 源码解析为官方语法树。
///
/// 使用 `typst-syntax`(与宿主基线同版本 0.15.1),后续翻译阶段
/// 在这棵树上工作,语义以 `typst-eval` 的实现为准绳。
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
        assert!(out.contains("pub fn module()"), "generated:\n{out}");
        assert!(out.contains("rt::param(__args, \"a\", span)?"), "generated:\n{out}");
    }
}
