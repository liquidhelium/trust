//! TRUST:把 Typst 包编译成 Rust 的转译器。
//!
//! 技术路线对标 Nuitka:不脱离宿主运行时——生成的 Rust 代码驱动
//! typst/typst-library 的值模型(`Value`/`Func`/`Content`/`Scope`),
//! 而不是重新实现一套类型系统。目标宿主是 typst fork 的
//! `LibraryBuilder::with_native_package`(接线先例见
//! `../typst-video/typst` 的 `cetz-native` 分支),注册后与原版
//! .typ 包在导出面与编译输出上兼容。
//!
//! 阶段规划与技术依据见 `docs/research-route.md`。

/// 前端入口:把 typst 源码解析为官方语法树。
///
/// 使用 `typst-syntax`(与手翻基线同版本 0.15.1),后续翻译阶段
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
}
