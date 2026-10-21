//! 翻译器本体:typst 源码 → Rust 源码(nuitka 路线的 L0 直译层)。
//!
//! 每个顶层 `#let` 绑定编译为:
//! - 函数 → `NativeFuncData` 静态(手写字段,不经宏):体为参数解构 +
//!   表达式直译(全程 `Value`,运算走 `typst::foundations::ops`,报错
//!   文案与 span 行为对照 typst-eval);
//! - 值 → `LazyLock<Value>` 槽,模块构建时按源序注册。
//!
//! 语义边界(v1,详见 docs/research-route.md):
//! - 顶层值表达式必须免引擎(字面量/集合/闭包/槽引用);需要引擎的
//!   构造(函数调用、标准库引用)请包进函数;
//! - 命名参数默认值在每次调用时求值(原版在定义期求值一次;纯代码
//!   等价,求值次数有差);
//! - 闭包体由 typst-eval 解释执行(保真路径);捕获在创建期物化;
//! - 错误的 span 使用 detached(差分对拍比较值,不比较定位)。
//!
//! 模块布局(自顶向下阅读):
//! - `ctx`:翻译上下文与名字解析环境(`Ctx`/`Env`/`Ref`/`Mode`);
//! - `expr`:表达式直译(字面量/集合/调用/闭包/二元运算);
//! - `block`:代码块与语句(let/体内 import/赋值);
//! - `destructure`:解构(数组/字典模式);
//! - `func`:函数体与 `NativeFuncData` 静态的生成;
//! - `captures`:闭包捕获分析;
//! - `naming`/`paths`:标识符与 import 路径的工具函数;
//! - `file`:单文件驱动(`translate`);
//! - `plan` + `package`:多文件包驱动(结构分析 + 代码生成)。

mod block;
mod captures;
mod ctx;
mod destructure;
mod expr;
mod file;
mod func;
mod naming;
mod package;
mod paths;
mod plan;

pub use file::translate;
pub use package::{PackageInput, translate_package, translate_package_lenient};

/// 翻译诊断:语法 span + 消息 + 所属文件(包模式)。
#[derive(Debug, Clone)]
pub struct Diag {
    pub span: typst_syntax::Span,
    pub message: String,
    pub file: Option<String>,
}
