# 生成物的宿主接入契约(现状盘点与演进路线)

问题:如何让**任何一个已经在工作的 typst 程序**(如 typst-cli)干净、
直接地加入转译模块?本文盘点当前对宿主的全部要求(以代码为据),
分析差距,给出演进排序。

## 一、当前给宿主的要求(逐条)

### 1. 宿主必须是 Rust 程序,链接生成物 crate

生成物的唯一入口是 `pub fn module_init(engine, context) -> SourceResult<Module>`
(`gen/mod.rs` 根)。没有 dylib/wasm 等可加载形态;非 Rust 宿主
(纯编译好的 typst-cli 二进制)当前完全无法使用。

### 2. 注入协议(trust-world `InjectWorld`,或宿主逐条复刻)

- **驱动 module_init**:构造 `Engine`(library + world +
  `EmptyIntrospector` + 新 `Sink` + `Route::default`),`Context::none()`,
  调 `module_init`。经 comemo memoize 壳缓存(`eval_native_module`,
  按 (world 读取约束, library 内容, init 函数) 失效;`rt::fill` 对
  同值幂等,驱逐重入安全)。
- **注入 library**:`library.global.scope_mut().bind(name,
  Binding::detached(module))`,并且**重建 `library.std` 绑定**(std
  持有 global 的另一 Arc 副本,scope 写时复制会让两者分叉),最后
  重包 `LazyHash<Library>`(否则 comemo 缓存哈希不感知注入)。

### 3. ~~FileId 预 intern 顺序约定~~ **已消除**(2026-10-22,span 自包含)

生成物里 `S{n}` 原先是翻译进程 interner 的绝对序号(span 高 16 位
= FileId 编号,只在产生它的进程里有意义),要求宿主在创建任何
World 之前按同序预 intern,错了还**静默**。现在 `S{n}` 是
`LazyLock<Span>`,首次使用经 `rt::source_span(路径, 原始位置号)`
在**当前进程** intern 路径并换上当前编号(低 48 位位置号对同一
文本确定性,原样保留;以 `id()` 相等性自检布局漂移)。宿主无任何
interner 义务。验证:cetz/fletcher 全部宿主在**零预 intern**下
26/26 值对拍、18/25 渲染逐字节一致(plugin 路径解析依赖 span 的
FileId,此前缺预 intern 会直接失败)。

### 4. 文件服务(InjectWorld 三条回退)

- `TRUST_WORLD_ROOT` 环境变量:Project 根文件回退(wasm 插件、
  数据文件,如 cetz 的 `../cetz-core/cetz_core.wasm`);
- 外部包(未翻译依赖,如 fletcher→cetz 0.3.4)按标准缓存布局
  `~/.cache/typst/packages/{ns}/{name}/{ver}` 解析;
- 虚拟源 `add_source`:宿主程序还会 import 的其余源文件
  (如 `tests/helper.typ`)需显式注入。

### 5. 杂项

- 字体显式注入(`set_fonts(typst_assets::fonts())`,
  InjectWorld 初始零字体);
- `sys.inputs` 经 `with_inputs`。

## 二、对"任意已工作的 typst 程序"的差距

| 差距 | 说明 |
| --- | --- |
| **span 绝对编号耦合**(最深) | 预 intern 要求宿主进程的 interner 前缀完全受控——对任意程序不可满足。即使解决了注入点,这项也会让接入即坏(且静默)。 |
| **必须换 World** | typst-cli 用 SystemWorld(磁盘、包下载、字体发现、增量);要求换 `InjectWorld` 等于换心脏。InjectWorld 是测试世界,不是生产世界。 |
| **无注入钩子** | typst-cli 每次 compile 从 `Library::builder()` 新建 library,没有第三方模块挂载点;所有 Rust typst 程序倒都会在某处构造 Library——这是天然挂载点。 |
| **无动态加载形态** | 生成物是 Rust crate;跨进程/非 Rust 宿主为零支持。 |

关键观察:**注入完成后,模块值是自包含的**——状态随函数值走
(StateSlot,无全局注册表),后续调用使用**宿主自己的真实 Engine**
(realize/layout 期)。所以对 Rust 宿主,最小契约其实只有三条:
链接 crate、注入 library(§2)、预 intern(§3)。§4 的文件服务,
任何真实 World(SystemWorld)本来就有——那是 InjectWorld 的缺陷,
不是生成物的要求。

## 三、演进路线(按性价比排序)

1. ~~**span 自包含,删除预 intern 约定**~~ **已完成**(见 §3):
   `rt::source_span` 首次使用自 intern,位置号照抄,产物自包含。
2. **注入协议泛化**:把"驱动 + 绑定 + std 重建 + LazyHash 重包"
   提成对任意 `&mut Library`(或宿主 World)的函数
   `install(library)`;Engine 构造参数化(introspector/sink/route
   由宿主给,module_init 只需 world 能服务包文件)。InjectWorld
   退化为测试实现。此后 Rust 宿主接入 = 链接 + 一行调用。
3. **包目录形态**:生成物同时产出一个可直接放置的包目录
   (`typst.toml` + 一行 shim 入口 `#import <name>: *`),让磁盘
   世界把它当**普通包** import——配合 2 的注入点,宿主体验
   ="装个包 + 一行注册"。shim 是纯 typst 的桥,包解析走宿主
   自己的包管理。
4. **动态加载形态**(dylib + 注册协议,非 Rust 宿主):长期项,
   暂不做——Value/Engine 语义跨动态边界的安全边界需要单独设���。
