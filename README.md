# trust

把 Typst 包(首个目标:cetz 0.5.2)机器翻译成 Rust 的转译器。技术路线对标
Nuitka:不脱离宿主运行时——产物是驱动 typst-library 值模型(`Value` /
`Func` / `Content`)的 Rust crate,经 typst fork 的
`LibraryBuilder::with_native_package` 注册进 typst,与原版 .typ 包在
导出面、行为与编译输出上兼容。

研究路线与技术依据:[docs/research-route.md](docs/research-route.md)。
Prior art:`../typst-video/typst` 的 `cetz-native` 分支(cetz 的手工
Rust 翻译版,含约 70 行宿主接线 diff)。

## 工作区

| 路径 | 内容 |
|---|---|
| `crates/trust` | 转译器本体(前端 typst-syntax AST → 后端 Rust 代码生成) |
| `crates/trust-runtime` | 生成代码的运行时支撑层(join 语义、参数解构、调用协议) |
| `docs/` | 研究路线与设计文档 |
| `_things/` | 本地资源与探索笔记(不入库) |

构建:`cargo build`;测试:`cargo test`。

## 工程约定

- `target` 是指向 `/mnt/data-hdd/helium/trust-target` 的符号链接
  (大盘可用约 316G,系统盘仅约 48G)。clone 后重建:

  ```sh
  mkdir -p /mnt/data-hdd/helium/trust-target
  ln -s /mnt/data-hdd/helium/trust-target target
  ```
