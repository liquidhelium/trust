# trust

把 Typst 包(首个目标:cetz 0.5.2)机器翻译成 Rust 的转译器。技术路线对标
Nuitka:不脱离宿主运行时——产物是驱动 typst-library 值模型(`Value` /
`Func` / `Content`)的 Rust crate,经**自定义 World/Library 注入**接入
typst(原版 0.15.1,零 fork),与原版 .typ 包在导出面与行为上兼容。

研究路线与技术依据:[docs/research-route.md](docs/research-route.md)。
Prior art:`../typst-video/typst` 的 `cetz-native` 分支(cetz 的手工
Rust 翻译版,含约 70 行宿主接线 diff)。

## 工作区

| 路径 | 内容 |
|---|---|
| `crates/trust` | 转译器本体(前端 typst-syntax AST → 后端 Rust 代码生成) |
| `crates/trust-runtime` | 生成代码的运行时支撑层(参数解构、调用协议、闭包构造、content/markup 构造) |
| `crates/trust-world` | 注入式 World:全局模块注入、包虚拟化、`module_init` 引擎值协议(差分测试 harness 地基) |
| `crates/trust-diff` | 差分测试:fixtures 经 trust 翻译(build.rs),与 typst-eval 解释器逐用例对拍 |
| `docs/` | 研究路线与设计文档 |
| `_things/` | 本地资源与探索笔记(不入库) |

构建:`cargo build`;测试:`cargo test`(差分对拍,无网络无外部依赖);
性能基准:`cargo test -p trust-diff --release --test bench -- --ignored --nocapture`。

## 用法

```sh
# 单文件翻译
trust translate input.typ --name mymod -o out.rs

# 生成的源码形态(节选):
#   pub fn module() -> Module          // 导出面照抄源文件
#   pub fn module_init(engine, ...)    // 顶层引擎值补注册(若有)
```

生成代码依赖 `typst` 与 `trust-runtime`;宿主接入见
`crates/trust-world/src/lib.rs` 的文档注释与 `crates/trust-diff` 的用法。

## 当前能力(v0.x,差分测试为证)

- **语言**:let 绑定(值/函数/默认参数/命名参数/sink)、字面量、数组/
  字典(含 spread)、字段访问、方法调用(含可变方法 push/pop/insert/
  remove)、函数调用(含 spread/named)、二元/一元运算(短路 and/or)、
  if/while/for(join 语义)、局部重赋值与遮蔽、递归、闭包(捕获精确
  分析,体由 typst-eval 解释——保真路径)、content 块子集(文本/空白/
  换行/嵌入)、顶层引擎值(惰性槽 + module_init)、多文件包(四种
  import 形态,环检测)
- **实测(过程验证)**:10 个 fixture / 90+ 用例双路逐值一致;性能
  (干净负载,避 comemo memoize 红利):总体 2.0×,纯计算 3.5–7.0×、
  调用密集 7.5×、字符串分配 1.1×(值层瓶颈,属 L1 类型化目标)
- **cetz 0.5.2 整包(探索,不入测试框架)**:严格模式整包翻译通过,
  生成物(37 文件/1.7MB)零错误通过 Rust 编译,注入原版 typst 后与
  解释器 26/26 调用逐值一致(vector/matrix/coordinate、
  path-util、sorting、util 默认参数/闭包、版本常量)
- **已知边界**:markup 样式元素(Strong 等)、`#include`/`#show`、
  参数解构模式、生成物快照测试——见研究路线 §5/§7

## 工程约定

- `target` 是指向 `/mnt/data-hdd/helium/trust-target` 的符号链接
  (大盘可用约 316G,系统盘仅约 48G)。clone 后重建:

  ```sh
  mkdir -p /mnt/data-hdd/helium/trust-target
  ln -s /mnt/data-hdd/helium/trust-target target
  ```
