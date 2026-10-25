# typst 架构笔记与可借鉴点

日期:2026-10-25 · 来源:typst 0.15.1 源码通读(eval/binding/access/scope/
func/comemo 集成)、typst-video fork(cetz-native 分支)、cetz-diagnose
诊断报告、trust 项目自身的对拍与画像实测。

## 1. crate 分层(0.15.1)

```
typst-syntax   纯 AST/lex/parse,零依赖 typst 值层 —— trust 前端直接吃
typst-library  值模型(Value/Func/Closure/Scope/Binding)+ 全部 #[func]
typst-eval     解释器:Eval for ast::*;closure 调用、import、赋值
typst-realize  样式作用域展开(show/set 规则→content 树定型)
typst-layout   布局(需要 introspector 两轮收敛)
typst(-cli)    组装层:compile<T> = eval→realize→layout
comemo         横切:所有跨相位函数的 memo 基建
```

要点:**求值与排版严格相位分离**。eval 阶段产 Content 树,realize 才
应用样式,layout 才有位置。`context expr` 是两个相位之间的桥——闭包
延期到有 styles/location 时求值(ClosureNode::Context)。

## 2. 值模型(trust 已对齐的部分)

- 一切用户可见值是 `typst_library::Value`;Func 有四种内在
  (Native/Element/Closure/With),closure 携带 `captured: Scope`。
- `Binding` 的 `Captured(Capturer)` 标记实现"函数外变量只读"——
  写路径 `Binding::write` 报错,读路径无感。trust 的 bail_captured
  即镜像此处(scope.rs:318)。
- `Scope` 是 IndexMap + 层级;`Scopes::get_mut` 自顶向下第一命中,
  **内层 Normal 绑定遮蔽外层 Captured**——trust 捕获帧只查当前层的
  修复即对齐此语义。
- `LazyHash` 让大值(数组/字典/模块)哈希一次终身缓存——comemo 键
  的成本因此是"每实参个数"线性而非按内容深算(画像实测:两侧各
  ~33% 时间在哈希,是架构地板而非实现退化)。

## 3. comemo 记账(最值得理解的一层)

- 每个跨相位函数 `#[comemo::memoize]`,键 = 全部输入(含 Tracked
  世界状态)的 128 位稳定哈希;`TrackedMut`(如 Sink)把副作用记进
  constraint,命中时回放。
- `eval_closure` 本身就是 memoize 的(call.rs:599)——解释器对重复
  闭包调用也走缓存。trust 的 eval_stub_native 键集与之间构;实测
  去掉记账直呼反而慢 21%(2.01→2.44s):**记账是净收益,不是税**。
- Route(递归深度/包含集)参与键,天然防跨位置串味。

## 4. 测试组织(已仿照,见 crates/trust-suite)

- `tests/suite/**.typ`:`--- name flags ---` 分段 + `// Error:/Warning:`
  注记(范围前缀 `2:3-4:5`/`"path" …`);`skip.txt` 显式跳过;本地
  `cargo test --test suite <filter>` 按名过滤。
- harness 世界(tests/src)注入 `test`/`test-repr` 全局、固定日期、
  120pt 页宽——InjectWorld::enable_suite 镜像了全部细节。
- 我们的差异:产物需要编译,故 refresh(切分+翻译)/test 两步 + 214
  挂载 crate;typst 无编译步故单命令。

## 5. 求值密集负载的结构性结论(cetz 系实测)

- 解释器每调用常数 2~3.3µs,一张图 ~10^5 次调用(cetz-diagnose);
- 值模型/comemo 地板占端到端 ~90%(trust 画像:生成代码本体 0.6%);
- fork 式原生 crate(机器几何进 Rust 结构体、仅闭包边界出 Value)
  实测 10-150×——但那是绕开值模型,不是"编译替代解释"的一层差;
  值保真模型内(trust 的定位)天花板 ~2-2.5×。

## 6. 可借鉴清单(按可落地排序)

1. **相位化思维用于 trust 分层**:`eval→realize→layout` 的严格相位
   分界值得在 trust 生成物组织上延续(目前生成代码只覆盖 eval 相位,
   样式/布局全走 typst 原生——保持这个边界,不要越相位内联)。
2. **Binding::capture 的 capturer 区分**:Function vs Context 两种
   只读来源,错误文案不同——trust 的 bail_captured 已区分。
3. **注记驱动测试 + 显式 skip 名单**:known-divergence.txt 即
   skip.txt 的严格化(双向断言,防名单腐化)。
4. **LazyHash 式"哈希一次"设计原则**:任何进 comemo 键的自定义值
   都应预哈希缓存(StateSlot 按 id 即此精神)。
5. **Route/深度守卫**:trust 的 check_call_depth 对应 typst 的
   MAX_CALL_DEPTH;生成代码不必自造调用栈抽象。
6. **暂不借鉴**:typst 的 element/set-show 宏体系(生成侧只用其
   运行时面);bytecode VM(issue #3307 关闭)——社区同样未落地,
   与 trust 编译路线互补而非竞争。
