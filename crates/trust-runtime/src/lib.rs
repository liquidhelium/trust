//! TRUST 运行时支撑层:机器翻译生成的 Rust 代码在运行期依赖本 crate。
//!
//! 计划提供的支撑(见 `docs/research-route.md` §3):
//!
//! - typst code block 的 **join 语义**(cetz 的命令按此拼进 body 数组);
//! - 函数**参数解构**:位置/命名参数、默认值(调用时求值)、收集参数;
//! - **调用协议**:`Func::call` 的包装与 `SourceResult`/span 错误传播;
//! - dict/array 的 typst 语义操作(COW、字段访问、`in`/`at` 等)。
//!
//! 设计原则:用户可见的值保持 typst-library 的 `Value`,不做私有值
//! 模型——与 Nuitka 复用 CPython 对象模型是同一个选择。类型化提升
//! (f64/结构体)只发生在可证明的机器内部,且必须有 oracle 对拍保护。
