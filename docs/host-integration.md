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

### 3. FileId 预 intern 顺序约定(唯一"静默错"项)

生成物里 `S{n}` span 常量是**翻译进程** FileId interner 的**绝对
序号**(span 原始 u64 直接 `Span::from_raw` 进产物)。宿主进程必须
在**创建任何 World / intern 任何 FileId 之前**,按翻译期同序
(entry 所在源树、路径字典序)预 intern:

```rust
InjectWorld::preintern_package_files(&pkg_root.join("src"));
```

错了**不报错**:span 静默解码到错误文件——plugin 的相对路径解析
(按实参值 span 的 FileId 定位)、错误诊断定位全部指错地方
(KNOWN-ISSUES #29,render-diff 曾因缺它报 "would escape the
project root")。

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

1. **span 自包含,删除预 intern 约定**(优先级最高,唯一静默错项)。
   方案:生成物内嵌 `(路径, 原文)` 与 `(file 序号, byte range)`
   表;首次使用时自 intern + `Source::new(path, text)` 重解析物化
   span(同文本解析确定性地产出同 range 的 span)。降级选项:span
   常数改为**文件级锚 span**(保文件身份、丢行级精度,KNOWN #4
   部分回退)——plugin/路径类用途只需要文件身份。完成即可把 §3
   从契约里删掉。
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
