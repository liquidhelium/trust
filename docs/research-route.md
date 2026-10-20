# 研究路线:TRUST —— 把 Typst 包编译成 Rust

日期:2026-10-20 · 状态:初始路线(M0 骨架已建,翻译器未开工)
证据基线:prior art 见文末附录(手翻分支 + 诊断文档),全部论断可溯源。

## 1. 问题定义与验收标准

TRUST = Typst → Rust 转译器。对标 Nuitka:Nuitka 把 Python 编译成 C,
但产物仍然驱动 CPython 对象模型,不另造运行时;TRUST 把 typst 包编译成
Rust crate,产物仍然驱动 typst/typst-library 的值模型
(`Value`/`Func`/`Content`/`Scope`/comemo)。要消灭的不是"typst 值",
而是**解释器的每调用常数**(FINDINGS 实测 2.0~3.3µs/调用,一张
cetz 图被放大到 10~16 万次原语调用——排版本身不是堵点)。

终态验收(以 cetz 0.5.2 为标尺):

1. **注册兼容**:`#import "@preview/cetz:0.5.2": canvas` 与 `: *`
   全量导入无感;下游包(cetz-plot 0.1.4,只用 `: *` 公开面)不改一字可用。
2. **输出兼容**:同一 typst 二进制双路(原生注册 / 原 .typ 包)编译同一
   语料,SVG/PDF 逐字节一致(oracle 法,§5)。
3. **性能**:达到手翻版量级(NATIVE-API-EVAL §7:native-bench 口径
   10~150×)。

明确不在 v1 目标内:一般 typst 程序的完全覆盖(以 cetz 生态所需子集为准)、
脱离 typst fork 独立运行(上游不会链接具体包的原生实现,"fork 即发行物")。

## 2. 已有资产与手翻版的教训

| 资产 | 位置 | 对 TRUST 的用处 |
|---|---|---|
| 宿主接线机制(约 70 行 diff) | cetz-native 分支:`typst-cli/src/world.rs` + `typst-eval/src/import.rs` + `typst-library/src/lib.rs` | **宿主侧已解决**:`LibraryBuilder::with_module/with_native_package`,求值器在 `import_package` 处短路;另加 `Func::with_args` |
| 手翻版 `typst-cetz` | cetz-native 分支,+17.8k 行 | 生成代码的目标形态参考(`#[func]` + `Scope` 逐名绑定 + `module()` 构建器) |
| FUNCTION-MAP(332 条函数级对照) | cetz-diagnose/FUNCTION-MAP.md | 手翻 FAITHFUL 仅 12%、MISSING 34% → **手翻走样是常态**;机器翻译必须按函数清单核对 + oracle 对拍双保险 |
| FINDINGS(性能诊断) | cetz-diagnose/FINDINGS.md | 慢因账本(四条机制)、VM 原语单价表 → 编译收益的量化依据 |
| NATIVE-API-EVAL(手翻设计书) | cetz-diagnose/NATIVE-API-EVAL.md | 架构原则("用户可见值=Value,机器几何=struct")、迁移顺序、实测数字 |
| FAST-PATH-DESIGN(行为钉子) | cetz-diagnose/FAST-PATH-DESIGN.md | 翻译器保真规格:浮点常量/求值顺序/边界行为逐条钉死;oracle 方法论 |
| 基准/语料脚本 | cetz-diagnose/{native-bench,native-corpus}.py | 验收工具直接复用(改注册路径) |
| 原版包副本 | cetz-diagnose/cetz-0.5.2-orig、cetz-plot-0.1.4-orig | golden 语义标准(只读,勿作素材改写) |

手翻版踩过的坑 = 翻译器的 hard cases 清单(FINDINGS §GROUP-5、
cetz-native 提交史):

- **浮点路径**:椭圆贝塞尔常数 0.551784 ≠ 0.5522847、`Scalar::powi`
  与 `pow` 的选择、sqrt 的写法、catmull tension 的映射与两点特例;
- **注册顺序**:锚点列表、compass ring 按**声明序**而非集合序;
- **形式覆盖**:padding 的 dict/array 形式、坐标语法 `_|_`/`⟂`、
  `rel:`/`to:` 复合坐标的分支;
- **join 语义**:draw 命令作为原生值按 code-block join 拼进 body 数组。

这些全是"机械纪律"问题——恰是编译器相对人手的优势所在,也是本项目的
核心动机。

## 3. 架构

### 3.1 分层策略(Nuitka 式)

- **L0 直译层**:每个 `#let f(..)` → 一个 Rust fn,**全程 `Value`**。
  控制流、常量、分支编译成 Rust;调用协议走 typst-library
  (`Args` 构造 + `Func::call`)。保真优先;仅此一层已消除 eval 分派、
  span 记录、作用域栈维护的每调用常数。
- **L1 类型化层**:仅对可证明的机器内部值提升为原生类型(f64、结构体
  几何)。原则沿 NATIVE-API-EVAL §1:**用户可见的值保持 `Value`**
  (样式表、闭包参数、content),机器几何进结构体;翻译税只发生在
  闭包真读字段时,不摊到每次运算。
- **L2 跨过程层**:内联、去装箱、样式按代缓存(对应 FINDINGS 机制 2)、
  wasm 叶子数学内联(手翻版已验证可行)。每项优化必须在 oracle 对拍
  保护下单独开启。

L0 是正确性的载体,L1/L2 是性能的载体;通过率不足时永远可以退回 L0
重新对拍——优化不改变语义承诺。

### 3.2 crates 划分(与仓库结构对应)

```
crates/trust           转译器:前端(typst-syntax 0.15.1 官方解析器)
                       → 中端(包级模块图、调用/常量分析、导出面规格)
                       → 后端(生成 Rust crate 源码)
crates/trust-runtime   生成代码的运行时支撑:join 语义、参数解构、
                       调用协议、错误包装、dict/array shim
生成物 typst-<name>     每个源包一个 crate:`pub fn module() -> Module`,
                       按原包 typst.toml 与 lib.typ 的导出面逐名绑定
宿主 typst fork         with_native_package 按版本分流:被编译的版本
                       走原生模块,其余版本走原 .typ 路径(版本钉死)
```

前端钉 typst-syntax 0.15.1(与手翻基线、crates.io 发布版一致);语义
准绳是同版 typst-eval(约 3.5k 行,全语义的权威定义)。

### 3.3 关键语义的翻译策略(v1 决策)

| 语言构造 | v1 策略 | 依据/风险 |
|---|---|---|
| 动态类型/一等值 | 全程 `Value`,零私有值模型 | Nuitka 先例;dict/array COW 由 typst-library 保证 |
| 顶层 `#let`(含默认值/命名参数) | 生成 `#[func]` 原生函数;默认表达式按声明序在调用时求值 | 与手翻版 `Scope::define_func` 面一致 |
| 闭包(作为值传递、被用户调用) | 生成 Rust fn + 包装成 `Func`;宿主可能需小扩展(fork 目前只加了 `Func::with_args`,无"原生闭包"公开入口) | **M2 前必须定案**,见 §7.1 |
| 模块/`#import` | 包内 import 图平铺为 Rust mod;初始化顺序照 typst 文件求值序(自顶向下) | cetz 的 re-export 面(lib.typ ~16 个子模块)机械照抄 |
| markup/content 字面量 | 生成 `Content` 构造序列,走 typst-library 元素构造器,不经 eval | NATIVE-API-EVAL §0 第 3 类 |
| set/show 规则 | 语料面内仅 content 局部 `#show linebreak: []` 与函数内 `#set text(...)`:按 content 作用域翻译;全局规则不支持则显式报错 | 已 grep 全语料确认;不支持必须报错不静默(FAST-PATH 原则) |
| plugin(wasm 叶子数学) | v1 **原样保留**:生成代码按 typst 同一路径调用 wasm 插件(cetz 5 个文件用 `plugin()`) | 保真零风险;L2 再评估内联 |
| 错误与 span | 生成代码携带原 span;错误文案逐字复刻 | 生态里有人 assert 错误文本 |
| comemo/增量 | 原生路径天然绕过包级 eval 缓存;按 canvas 元素 memoize 属 P6 级增强,非保真必需 | 手翻版同此 |

**不可表达即报错**:转译器遇到不认识的构造,报错并指认源码位置与缺口,
绝不静默回退或生成近似代码(FAST-PATH-DESIGN §0 的"编码不了的东西
不存在"原则)。

## 4. 翻译管线(v1)

1. **载入包**:`typst.toml` + 入口文件 + import 图(路径解析复用
   typst-syntax 的 package/path 模块,磁盘侧最小实现);
2. **建模块图**:每文件 `parse` → AST;收集 re-export,得到**导出面
   规格**(名字 × 绑定物 = 未来 `Scope` 的逐名清单);
3. **逐 `#let` 生成**:签名分析(位置/命名/默认值/收集参数)→ Rust fn
   签名;函数体直译:表达式 → `Value` 表达式,控制流 → Rust 控制流,
   调用 → `Args` 构造 + `call`;
4. **常量预构造**:模块级字面量(dict/array/枚举表)预构建为
   `LazyHash` 值,消除每次求值;
5. **生成 `module()`**:按 lib.rs 的嵌套结构逐名 `define_func` /
   `define`,含 `name`/`version` 元数据;
6. **生成测试壳**:把原包 `tests/**/test.typ` 挂进生成 crate 的
   `cargo test`,跑 oracle 对拍(§5)。

## 5. 保真与验收方法(沿 fast 里程碑法)

- **golden**:原版 .typ 包在同一 typst 二进制下的编译输出。语料四级:
  cetz tests(102 例,`native-corpus.py` 口径)、cetz-diagnose v0~v9
  消融系列、cetz-plot showcase、cetz gallery;
- **对拍口径**:SVG/PDF 逐字节;差异按 FUNCTION-MAP 的七类根因归类
  (结构差异无视觉差者不计缺陷);
- **工具**:复用 cetz-diagnose 脚本,仅改注册路径;通过率曲线驱动
  开发节奏(手翻版基线:起步 5/102);
- **测量纪律**:沿用 diagnose 的 A/B 方法——同机同时间窗、同一二进制
  双路对比,共享机器负载漂移大,跨时段绝对值不可比;
- **函数清单核对**:FUNCTION-MAP 的 332 条对照表机器化——转译器自检
  "原包每个顶层 `#let` 在生成物中有同名绑定",缺口即编译错误。

## 6. 里程碑

| 阶段 | 内容 | 完成判据 |
|---|---|---|
| **M0 骨架**(本仓已就绪) | 工作区、前端可 parse、运行时 crate 占位 | `cargo build && cargo test` 绿;空模块可注册进 fork 编译 |
| **M1 语言子集** | 无闭包、无 content 的纯函数模块(vector/matrix/sorting/aabb) | 生成物替换原模块后语料该子集对拍通过 |
| **M2 语义完备** | 闭包、content、默认参数、join、局部 show/set → draw.* 主链 | cetz tests 通过率 ≥ 50% |
| **M3 整包** | cetz 0.5.2 全导出面(含 plugin 保留路径) | 通过率 ≥ 手翻版当前水平,SVG 逐字节 |
| **M4 生态** | cetz-plot 兼容;版本钉死分流(编译版本 vs 其他版本) | showcase 全量双路一致 |
| **M5 性能** | L1 类型化;L2 按账本逐项(样式代缓存、插件内联) | native-bench 对齐手翻 10~150× |

M1~M3 每步:翻一块、语料对拍一块、通过率入档——手翻 P1~P6 的
里程碑纪律直接沿用。

## 7. 风险与开放问题

1. **闭包的 `Func` 构造**:typst-library 无公开的"原生闭包"入口
   (fork 仅为 `palette.new` 加了 `Func::with_args`)。M2 前需定案:
   宿主小扩展(`Func::from_native_closure` 之类)或直构 eval 的
   `Closure` 值(闭包体同样在生成代码里,直构可能更保真)。
2. **typst 上游漂移**:fork 同步上游时 `Scope`/`Module`/`#[func]`
   API 可能变;生成器锁定 fork rev,升级时全量重跑 oracle。
3. **错误文案**:typst 的运行时错误消息与 span 定位产生在 eval 内;
   翻译后由生成代码复刻,文案需逐字(含位置)——工作量在细节,不在架构。
4. **wasm 插件依赖**:v1 保留插件调用要求宿主环境有插件文件;L2 内联
   则需对齐 cetz-core 的语义版本(手翻版内联成功的先例在 scene.rs)。
5. **`#show` 全局规则**:语料面内未用,故 v1 不支持;遇到即显式报错
   (策略同"不可表达即报错"),不做半吊子模拟。
6. **同盘竞争**:`trust-target` 与手翻 fork 的 `typst-target` 同在
   大盘,容量与 inode 需留意(大盘当前 ~316G 可用,typst-target 已有
   先例规模)。

## 附录:证据位置(绝对路径)

- 手翻分支:`/home/helium/code/typst-video/typst` 分支 `cetz-native`
  (+`crates/typst-cetz` 17.8k 行;接线 diff 见 §2 表);
- 诊断与设计:`/home/helium/code/typst-video/cetz-diagnose/`
  (FINDINGS / NATIVE-API-EVAL / FAST-PATH-DESIGN / FUNCTION-MAP);
- 基准与语料:同上目录 `native-bench.py`、`native-corpus.py`、
  `cetz-0.5.2-orig/`、`cetz-plot-0.1.4-orig/`;
- 性能锚点:NATIVE-API-EVAL §7(10~150×)、FINDINGS VM 原语单价表。
