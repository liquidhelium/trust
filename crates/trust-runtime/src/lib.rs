//! TRUST 运行时支撑层:机器翻译生成的 Rust 代码在运行期依赖本 crate。
//!
//! 设计原则(见 docs/research-route.md §3):用户可见的值保持 typst-library
//! 的 `Value`,零私有值模型;所有语义函数与 `typst-eval` 的解释实现逐条
//! 对照(报错文案与 span 行为一致),差分测试以此为验收。

use comemo::Track;
use comemo::Tracked;
use typst::World;
use typst::diag::{At, SourceResult, bail};
use typst::engine::Engine;
use typst::foundations::{
    Arg, Args, Array, Closure, ClosureNode, Content, Context, Func, IntoValue, Module, Scope,
    Selector, Str, Transformation, Value, ops,
};
use typst::syntax::{Span, Spanned, SyntaxKind, ast};

use ecow::{EcoVec, eco_format, eco_vec};
use typst::diag::SourceDiagnostic;

/// 每构建状态(per-build 槽板)的存储与槽原语。
///
/// 见 `_things/DESIGN-per-build.md`:typst 运行时 = 有限命名槽 + 每次
/// 求值写一次 + 按值快照;这里提供对应的 Rust 形态——写一次单元
/// (`OnceLock<Value>`)与按构建键控的状态注册表。
///
/// **构建键 = `engine.library` 的内容哈希**(`LazyHash` 缓存,便宜,
/// 与 comemo 同一失效纪律)。宿主注入模块会重建 Library(哈希变化),
/// 因此协议是:`module_init` 在注入前的哈希下注册状态,宿主注入后
/// 调 [`state_rebind_last_to`] 把最近创建的状态搬到注入后的哈希下
/// (`init_native_module` 已内置)。
///
/// 义务与限制:一个 Library 内容 = 一次构建;注册表条目不自动回收。
pub fn build_key(engine: &Engine) -> u128 {
    typst::utils::hash128(engine.library)
}

fn state_error(message: &str) -> EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(
        Span::detached(),
        typst::foundations::Str::from(message),
    )]
}

/// 读一个顶层绑定槽(写一次单元)。
pub fn read(slot: &std::sync::OnceLock<Value>) -> SourceResult<Value> {
    slot.get()
        .cloned()
        .ok_or_else(|| state_error("模块槽未初始化(求值序错误)"))
}

/// 读创建期物化的全局名槽:有值读槽(对照 typst 创建期捕获),
/// 无值回退调用期全局查找(对照创建期不可见名字的调用期解析)。
pub fn read_or(
    slot: &std::sync::OnceLock<Value>,
    engine: &Engine,
    name: &str,
) -> SourceResult<Value> {
    match slot.get() {
        Some(value) => Ok(value.clone()),
        None => global(engine, name, Span::detached()),
    }
}

/// 读存为内容(body)的槽。
pub fn read_content(slot: &std::sync::OnceLock<Value>) -> SourceResult<Content> {
    match read(slot)? {
        Value::Content(content) => Ok(content),
        _ => Err(state_error("模块 body 槽不是内容(生成器 bug)")),
    }
}

/// 填一个顶层绑定槽。首填成功即定;重入(如 `module_init` 因模块缓存
/// 驱逐而重新求值)时同值幂等跳过,不同值仍是生成器 bug,明确报错。
pub fn fill(slot: &std::sync::OnceLock<Value>, value: Value) -> SourceResult<()> {
    match slot.get() {
        Some(existing) => {
            if *existing == value {
                Ok(())
            } else {
                Err(state_error("模块槽重复填充为不同值(生成器 bug)"))
            }
        }
        None => slot
            .set(value)
            .map_err(|_| state_error("模块槽并发填充冲突")),
    }
}

type AnyState = std::sync::Arc<dyn std::any::Any + Send + Sync>;

/// 每构建状态的全局注册表:(library 哈希, 状态类型) → 状态实例。
static STATES: std::sync::LazyLock<
    std::sync::RwLock<std::collections::HashMap<(u128, std::any::TypeId), AnyState>>,
> = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));

thread_local! {
    /// 最近一次 `state_get_or_insert` 创建/复用的状态(供宿主注入后
    /// 重绑定;同线程同步协议,见 `state_rebind_last_to`)。
    static LAST_STATE: std::cell::RefCell<Option<(u128, std::any::TypeId, AnyState)>> =
        const { std::cell::RefCell::new(None) };
}

/// 取本构建的类型化状态;不存在则以 `init` 创建(幂等:同构建复用,
/// 对照 typst 同编译内模块只求值一次)。
pub fn state_get_or_insert<T: std::any::Any + Send + Sync>(
    engine: &Engine,
    init: impl FnOnce() -> T,
) -> std::sync::Arc<T> {
    let key = build_key(engine);
    let type_id = std::any::TypeId::of::<T>();
    let found = STATES.read().unwrap().get(&(key, type_id)).cloned();
    let arc: AnyState = match found {
        Some(found) => found,
        None => {
            let fresh: AnyState = std::sync::Arc::new(init());
            STATES
                .write()
                .unwrap()
                .entry((key, type_id))
                .or_insert_with(|| fresh.clone())
                .clone()
        }
    };
    LAST_STATE.with(|slot| *slot.borrow_mut() = Some((key, type_id, arc.clone())));
    std::sync::Arc::downcast::<T>(arc).expect("state entry type mismatch")
}

/// 取本构建的类型化状态;未注册(`module_init` 从未在此构建运行)明确报错。
pub fn state_require<T: std::any::Any + Send + Sync>(
    engine: &Engine,
) -> SourceResult<std::sync::Arc<T>> {
    let key = build_key(engine);
    let type_id = std::any::TypeId::of::<T>();
    let found = STATES.read().unwrap().get(&(key, type_id)).cloned();
    match found {
        Some(arc) => Ok(std::sync::Arc::downcast::<T>(arc).expect("state entry type mismatch")),
        None => Err(state_error(
            "模块状态未注册:宿主未调用 module_init(装配期求值)",
        )),
    }
}

/// 宿主在把模块注入 Library 之后调用:把最近创建的状态搬到注入后的
/// library 哈希下(注入前的哈希条目保留,供装配期内的嵌套调用)。
///
/// `from` 为本次驱动的注入前 library 哈希(守卫):模块求值缓存命中时
/// 不重新执行 `module_init`,最近状态可能残留自其他构建,不匹配即跳过。
pub fn state_rebind_last_to(library: &typst::utils::LazyHash<typst::Library>, from: u128) {
    let to = typst::utils::hash128(library);
    let Some((from_seen, type_id, arc)) = LAST_STATE.with(|slot| slot.borrow_mut().take()) else {
        return;
    };
    if from_seen != from {
        return;
    }
    STATES.write().unwrap().insert((to, type_id), arc);
}

/// 在标准库全局作用域解析一个名字(对照解释器的 `Scopes::get`:
/// 局部之外查 `library.global`,miss 且名字为 `std` 时特判指向
/// `library.std`;未命中报同款 `unknown variable`)。
pub fn global(engine: &Engine, name: &str, span: Span) -> SourceResult<Value> {
    match engine.library.global.scope().get(name) {
        Some(binding) => Ok(binding.read().clone()),
        None if name == "std" => Ok(engine.library.std.read().clone()),
        None => bail!(span, "unknown variable: {name}"),
    }
}

/// 全局查找,不存在返回 `None`(对照 typst 捕获语义:创建期不可见的
/// 名字不捕获,由闭包体内自己的 import 在调用期绑定;`std` 特判
/// 与 [`global`] 同款)。
pub fn global_opt(engine: &Engine, name: &str) -> SourceResult<Option<Value>> {
    Ok(match engine.library.global.scope().get(name) {
        Some(binding) => Some(binding.read().clone()),
        None if name == "std" => Some(engine.library.std.read().clone()),
        None => None,
    })
}

/// 字段访问 `target.name`(对照 `ast::FieldAccess` 求值:`Value::field`)。
pub fn field(target: &Value, name: &str, span: Span) -> SourceResult<Value> {
    target.field(name, ()).at(span)
}

/// 调用一个函数值;类型值调用其构造器(对照解释器:`str(1)`、`int("5")`)。
pub fn call(
    func: &Value,
    engine: &mut Engine,
    context: Tracked<Context>,
    span: Span,
    args: Args,
) -> SourceResult<Value> {
    match func {
        Value::Func(func) => func.call(engine, context, args),
        Value::Type(ty) => {
            let ctor = ty.constructor().at(span)?;
            ctor.call(engine, context, args)
        }
        other => bail!(span, "expected function or content, found {}", other.ty()),
    }
}

/// 方法调用 `target.name(args..)`(对照 `eval_field_callee`:类型作用域
/// 的方法把 target 作为第一个位置参数;Symbol/Func/Type/Module 允许字段
/// 调用语法;其余情况按原版文案报错)。
pub fn method(
    target: &Value,
    name: &str,
    engine: &mut Engine,
    context: Tracked<Context>,
    span: Span,
    args: Args,
) -> SourceResult<Value> {
    let mut args = args;
    let callee = if let Some(method) = target.ty().scope().get(name) {
        // 方法调用:target 作为第一个位置参数。
        args.insert(0, args.span, target.clone());
        method.read().clone()
    } else if matches!(
        target,
        Value::Symbol(_) | Value::Func(_) | Value::Type(_) | Value::Module(_)
    ) {
        target.field(name, ()).at(span)?
    } else {
        // 与原版一致的两段式报错:字段存在但不可调用 / 字段不存在。
        match target.field(name, ()) {
            Ok(_) => {
                bail!(
                    span,
                    "`{name}` is not a valid method for type `{}`",
                    target.ty().short_name()
                )
            }
            Err(_) => {
                bail!(
                    span,
                    "type {} has no method `{name}`",
                    target.ty().short_name()
                )
            }
        }
    };
    call(&callee, engine, context, span, args)
}

/// 以源码片段构造闭包值:节点经官方解析器解析,defaults 由生成代码在
/// 创建期求值后传入,captured 携带外层绑定的当前值。
///
/// 闭包体由 typst-eval 解释执行——这是 v1 的保真路径;闭包参数的默认值
/// 求值次数与原版一致(创建期)。
/// 解释闭包的虚拟源登记:FileId → 片段源码(诊断定位用;宿主在
/// 错误打印时经 [`closure_source`] 反查)。
static CLOSURE_SOURCES: std::sync::RwLock<Vec<(typst::syntax::FileId, String)>> =
    std::sync::RwLock::new(Vec::new());

/// 查一个虚拟闭包文件的源码(不存在返回 `None`)。
pub fn closure_source(id: typst::syntax::FileId) -> Option<String> {
    CLOSURE_SOURCES
        .read()
        .unwrap()
        .iter()
        .find(|(fid, _)| *fid == id)
        .map(|(_, src)| src.clone())
}

/// 解释片段(`closure`/`context_closure`)的虚拟源:登记进
/// [`CLOSURE_SOURCES`] 并包装成 markup 合法形态(`#let … = …`),
/// 片段节点的 span 从此携带 FileId,解释执行的错误可定位到片段行。
fn virtual_closure_source(src: &str) -> typst::syntax::Source {
    let n = CLOSURE_SOURCES.read().unwrap().len();
    let id = typst::syntax::RootedPath::new(
        typst::syntax::VirtualRoot::Project,
        typst::syntax::VirtualPath::new(format!("__trust_closure_{n}.typ"))
            .expect("valid closure path"),
    )
    .intern();
    // 命名绑定形态直接嵌入(init 是 block 或单行,markup 下合法);
    // 其余(裸闭包、context 体)包一层括号再赋给合成名——markup 的
    // `#let x = expr` init 在**行尾**截止,裸闭包体跨行会被截断成空体,
    // 括号强制 code 上下文才能跨行(匿名形态本就无自引用,名字不可达)。
    let trimmed = src.trim_start();
    let text = if trimmed.starts_with("let ") {
        format!("#{trimmed}")
    } else {
        format!("#let __trust_closure = ({trimmed})")
    };
    let source = typst::syntax::Source::new(id, text.clone());
    CLOSURE_SOURCES.write().unwrap().push((id, text));
    source
}

pub fn closure(src: &str, defaults: Vec<Value>, captured: &[(&'static str, Value)]) -> Func {
    let source = virtual_closure_source(src);
    let root = source.root();
    let text = source.text().to_string();
    // 源片段可能是裸闭包 `(x) => ..`(Code 下直接是 Closure)或命名
    // 绑定形态 `f(x) = ..`(LetBinding 包着 Closure),统一取首个
    // Closure 节点;命名形态下闭包名保留,递归自引用可用。
    let node = find_closure_node(root)
        .unwrap_or_else(|| panic!("closure source parses to a closure: {text:?}"))
        .clone();
    if std::env::var_os("TRUST_DEBUG_CAPTURE").is_some() {
        eprintln!(
            "[closure] 闭包节点 span: {:?} (detached={})",
            node.span(),
            node.span().is_detached()
        );
    }
    let num_pos_params = node
        .cast::<ast::Closure>()
        .expect("closure node casts")
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    let closure_span = node.span();
    if std::env::var_os("TRUST_DEBUG_CAPTURE").is_some() {
        eprintln!(
            "[closure+ span={:?}] {}",
            closure_span,
            text[..text.len().min(100)].replace('\n', "\\n")
        );
    }
    let mut scope = Scope::new();
    for (name, value) in captured {
        if std::env::var_os("TRUST_DEBUG_CAPTURE").is_some()
            && (src.contains("ctx => {") || src.contains("key =>"))
        {
            let brief = format!("{value:?}");
            eprintln!("[capture] {name} = {}", &brief[..brief.len().min(120)]);
        }
        scope.define(name, value.clone());
    }
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    // 对照 typst:闭包创建时 spanned(定义点);解释体内错误的传播点
    // (如 array.any 谓词的 cast 用 test.span())从此可定位到虚拟源。
    .spanned(closure_span)
}

/// 先序遍历找第一个 Closure 节点。
fn find_closure_node(node: &typst_syntax::SyntaxNode) -> Option<&typst_syntax::SyntaxNode> {
    if node.kind() == SyntaxKind::Closure {
        return Some(node);
    }
    node.children().find_map(find_closure_node)
}

/// for 循环的迭代源(对照 `Eval for ast::ForLoop`:数组按值、字典按键值
/// 对、字符串按字素、字节串按字节)。
pub fn iterate(value: &Value, span: Span) -> SourceResult<Vec<Value>> {
    use unicode_segmentation::UnicodeSegmentation;
    Ok(match value {
        Value::Array(array) => array.iter().cloned().collect(),
        Value::Dict(dict) => dict
            .iter()
            .map(|(key, value): (&Str, &Value)| {
                Array::from_iter([key.clone().into_value(), value.clone()]).into_value()
            })
            .collect(),
        Value::Str(str) => str
            .as_str()
            .graphemes(true)
            .map(|g| Value::Str(g.into()))
            .collect(),
        Value::Bytes(bytes) => bytes
            .as_slice()
            .iter()
            .map(|b| Value::Int(*b as i64))
            .collect(),
        _ => bail!(span, "cannot loop over {}", value.ty()),
    })
}

/// code block 的 join 语义(对照 `ops::join`)。
pub fn join(lhs: Value, rhs: Value, span: Span) -> SourceResult<Value> {
    ops::join(lhs, rhs).at(span)
}

/// if/while 的条件求值:非 boolean 按原版 cast 报错。
pub fn boolean(value: Value, span: Span) -> SourceResult<bool> {
    value.cast::<bool>().at(span)
}

/// 位置参数绑定(按声明序,对照 eval_closure 的 `args.expect`)。
pub fn param_pos(args: &mut Args, name: &str, _span: Span) -> SourceResult<Value> {
    args.expect::<Value>(name)
}

/// 命名参数绑定(仅按名,对照 eval_closure 的 `args.named`)。
pub fn param_named(args: &mut Args, name: &str, _span: Span) -> SourceResult<Option<Value>> {
    args.named::<Value>(name)
}

/// 有默认值的参数:未提供时求默认值。
pub fn param_or(
    args: &mut Args,
    name: &str,
    _span: Span,
    default: fn(&mut Engine, Tracked<Context>) -> SourceResult<Value>,
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Value> {
    match args.named_or_find::<Value>(name)? {
        Some(value) => Ok(value),
        None => default(engine, context),
    }
}

/// 参数绑定完成后检查剩余参数(对照 `Args::finish`)。
pub fn finish(args: &mut Args, _span: Span) -> SourceResult<()> {
    if let Some(arg) = args.items.first() {
        match &arg.name {
            Some(name) => bail!(arg.span, "unexpected argument: {name}"),
            None => bail!(arg.span, "unexpected argument"),
        }
    }
    Ok(())
}

/// 构造一个命名参数(生成代码用于调用点)。
pub fn named_arg(span: Span, name: &str, value: Value) -> Arg {
    Arg {
        span,
        name: Some(name.into()),
        value: Spanned::new(value, span),
    }
}

/// 展开参数(`f(..args)`,对照 `Eval for ast::Arg::Spread`:数组按位置、
/// 字典按命名、None 跳过)。
pub fn spread_args(args: &mut Args, span: Span, value: Value) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Array(array) => {
            for item in array {
                args.push(span, item);
            }
            Ok(())
        }
        Value::Dict(dict) => {
            for (key, value) in dict {
                args.items.push(named_arg(span, &key, value));
            }
            Ok(())
        }
        Value::Args(inner) => {
            for item in inner.items.iter() {
                args.items.push(item.clone());
            }
            Ok(())
        }
        other => bail!(span, "cannot spread {}", other.ty()),
    }
}

/// 数组便捷构造(生成代码使用 `array!` 宏的等价物)。
pub fn array(values: Vec<Value>) -> Array {
    Array::from_iter(values)
}

/// 字典构造助手(生成代码逐键插入,保源序)。
pub fn dict_put(dict: &mut typst::foundations::Dict, key: &str, value: Value) {
    dict.insert(key.into(), value);
}

/// 字典展开 `(..other)`(对照 `Eval for ast::DictItem::Spread`)。
pub fn dict_spread(
    dict: &mut typst::foundations::Dict,
    value: Value,
    span: Span,
) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Dict(other) => {
            for (key, value) in other {
                dict.insert(key, value);
            }
            Ok(())
        }
        other => bail!(span, "cannot spread {}", other.ty()),
    }
}

/// 空作用域(生成代码 NativeFuncData::scope 的初值)。
pub fn empty_scope() -> Scope {
    Scope::new()
}

/// 任意返回类型描述(生成代码 NativeFuncData::returns 的初值)。
pub fn any_cast_info() -> typst::foundations::CastInfo {
    typst::foundations::CastInfo::Any
}

/// 数组展开 `(..other)`(生成代码的数组字面量 spread)。
pub fn array_spread(values: &mut Vec<Value>, value: Value, span: Span) -> SourceResult<()> {
    match value {
        Value::None => Ok(()),
        Value::Array(array) => {
            values.extend(array);
            Ok(())
        }
        Value::Args(args) => {
            values.extend(args.items.into_iter().map(|arg| arg.value.v));
            Ok(())
        }
        other => bail!(span, "cannot spread {}", other.ty()),
    }
}

/// 可变方法(push/pop/insert/remove)的就地调用,对照
/// `typst-eval/src/methods.rs::call_method_mut`。
pub fn method_mut(
    value: &mut Value,
    method: &str,
    mut args: Args,
    span: Span,
) -> SourceResult<Value> {
    let ty = value.ty();
    let mut output = Value::None;
    match value {
        Value::Array(array) if matches!(method, "push" | "pop" | "insert" | "remove") => {
            match method {
                "push" => array.push(args.expect("value")?),
                "pop" => output = array.pop().at(span)?,
                "insert" => array
                    .insert(args.expect("index")?, args.expect("value")?)
                    .at(span)?,
                _ => {
                    output = array
                        .remove(args.expect("index")?, args.named("default")?)
                        .at(span)?
                }
            }
        }
        Value::Dict(dict) if matches!(method, "insert" | "remove") => match method {
            "insert" => dict.insert(args.expect("key")?, args.expect("value")?),
            _ => {
                output = dict
                    .remove(args.expect("key")?, args.named("default")?)
                    .at(span)?
            }
        },
        _ => bail!(span, "type {ty} has no method `{method}`"),
    }
    args.finish()?;
    Ok(output)
}

/// 消费(数组 into_iter 无逐元素克隆),每轮体值按 join 累积。
pub fn foreach(
    value: &Value,
    span: Span,
    mut f: impl FnMut(Value) -> SourceResult<Value>,
) -> SourceResult<Value> {
    use unicode_segmentation::UnicodeSegmentation;
    let mut out = Value::None;
    match value {
        Value::Array(array) => {
            for item in array.clone() {
                out = ops::join(out, f(item)?).at(span)?;
            }
        }
        Value::Dict(dict) => {
            for (key, item) in dict.iter() {
                let pair = Array::from_iter([key.clone().into_value(), item.clone()]).into_value();
                out = ops::join(out, f(pair)?).at(span)?;
            }
        }
        Value::Str(string) => {
            for grapheme in string.as_str().graphemes(true) {
                let item = Value::Str(grapheme.into());
                out = ops::join(out, f(item)?).at(span)?;
            }
        }
        Value::Bytes(bytes) => {
            for byte in bytes.as_slice() {
                let item = Value::Int(*byte as i64);
                out = ops::join(out, f(item)?).at(span)?;
            }
        }
        other => bail!(span, "cannot loop over {}", other.ty()),
    }
    Ok(out)
}

// --- content 直译助手(对照 typst-eval/src/markup.rs 的构造方式) ---

/// 文本片段。
pub fn text(s: &str) -> typst::foundations::Content {
    typst::text::TextElem::packed(typst::foundations::Str::from(s))
}

/// 空格。
pub fn space() -> typst::foundations::Content {
    typst::text::SpaceElem::shared().clone()
}

/// 软换行 `\`。
pub fn linebreak() -> typst::foundations::Content {
    typst::text::LinebreakElem::shared().clone()
}

/// 段落间隔。
pub fn parbreak() -> typst::foundations::Content {
    typst::model::ParbreakElem::shared().clone()
}

/// 标记序列组合(对照 `Content::sequence`)。
pub fn content_sequence(parts: Vec<typst::foundations::Content>) -> typst::foundations::Content {
    typst::foundations::Content::sequence(parts)
}

/// 嵌入表达式的值转为 content(对照 markup 中 `#expr` 的 display 语义)。
pub fn embed(value: Value) -> typst::foundations::Content {
    value.display()
}

/// 迭代器形态的循环源(数组零物化,值逐个移出;字典/字符串按原版
/// 逐对/字素),供生成代码内联 `for` 使用(break/continue 原生可用)。
pub fn iter_values(value: &Value, span: Span) -> SourceResult<Box<dyn Iterator<Item = Value>>> {
    use unicode_segmentation::UnicodeSegmentation;
    Ok(match value {
        Value::Array(array) => Box::new(array.clone().into_iter()),
        Value::Dict(dict) => Box::new(
            dict.iter()
                .map(|(key, item): (&Str, &Value)| {
                    Array::from_iter([key.clone().into_value(), item.clone()]).into_value()
                })
                .collect::<Vec<_>>()
                .into_iter(),
        ),
        Value::Str(string) => Box::new(
            string
                .as_str()
                .graphemes(true)
                .map(|g| Value::Str(g.into()))
                .collect::<Vec<_>>()
                .into_iter(),
        ),
        Value::Bytes(bytes) => Box::new(
            bytes
                .as_slice()
                .iter()
                .map(|b| Value::Int(*b as i64))
                .collect::<Vec<_>>()
                .into_iter(),
        ),
        other => bail!(span, "cannot loop over {}", other.ty()),
    })
}

// --- 解构助手(对照 typst-eval/src/binding.rs 的语义与报错文案) ---

/// 数组解构取第 i 个元素(不足时报错)。
pub fn d_at(array: &typst::foundations::Array, i: usize, span: Span) -> SourceResult<Value> {
    match array.at(i as i64, None) {
        Ok(value) => Ok(value),
        Err(_) => Err(elements_mismatch_diagnostic(
            array.len(),
            i + 1,
            false,
            span,
        )),
    }
}

/// 数组解构的剩余收集(`..rest`);`total` 为模式消费的总位数。
pub fn d_rest(
    array: &typst::foundations::Array,
    i: usize,
    total: usize,
    span: Span,
) -> SourceResult<typst::foundations::Array> {
    let sink_size = (1 + array.len()).checked_sub(total);
    match sink_size {
        Some(sink_size) if i + sink_size <= array.len() => {
            Ok(array.as_slice()[i..i + sink_size].iter().cloned().collect())
        }
        _ => Err(elements_mismatch_diagnostic(
            array.len(),
            total - 1,
            true,
            span,
        )),
    }
}

/// 数组解构结束后校验(还有剩余元素则报错)。
pub fn d_array_finish(
    array: &typst::foundations::Array,
    consumed: usize,
    span: Span,
) -> SourceResult<()> {
    if consumed < array.len() {
        return Err(elements_mismatch_diagnostic(
            array.len(),
            consumed,
            false,
            span,
        ));
    }
    Ok(())
}

/// 字典解构按键取值。
pub fn d_get(dict: &typst::foundations::Dict, key: &str, span: Span) -> SourceResult<Value> {
    match dict.get(key) {
        Ok(value) => Ok(value.clone()),
        Err(_) => bail!(span, "dictionary does not contain key \"{key}\""),
    }
}

/// 字典解构的剩余收集(`..rest`,排除已用键)。
pub fn d_dict_rest(dict: &typst::foundations::Dict, used: &[&str]) -> typst::foundations::Dict {
    let mut rest = typst::foundations::Dict::new();
    for (key, value) in dict.iter() {
        if !used.iter().any(|u| *u == key.as_str()) {
            rest.insert(key.clone(), value.clone());
        }
    }
    rest
}

/// 对照原版 `wrong_number_of_elements`:主消息 + hint(数量语义进
/// hint,与 typst-eval 逐字一致)。
fn elements_mismatch_diagnostic(
    len: usize,
    expected_count: usize,
    spread: bool,
    span: Span,
) -> EcoVec<SourceDiagnostic> {
    let quantifier = if len > expected_count {
        "too many"
    } else {
        "not enough"
    };
    let expected = match (spread, expected_count) {
        (true, 1) => "at least 1 element".to_string(),
        (true, c) => format!("at least {c} elements"),
        (false, 0) => "an empty array".to_string(),
        (false, 1) => "a single element".to_string(),
        (false, c) => format!("{c} elements"),
    };
    eco_vec![
        SourceDiagnostic::error(
            span,
            typst::foundations::Str::from(format!("{quantifier} elements to destructure"))
        )
        .with_hint(format!(
            "the provided array has a length of {len}, but the pattern expects {expected}"
        ))
    ]
}

/// 字段赋值 `d.k = v`(就地修改局部字典;对照解释器 `Access` 语义)。
pub fn field_assign(target: &mut Value, key: &str, value: Value, span: Span) -> SourceResult<()> {
    match target {
        Value::Dict(dict) => {
            dict.insert(key.into(), value);
            Ok(())
        }
        other => bail!(span, "cannot mutate field of {}", other.ty()),
    }
}

/// 运行时导入外部包(未被翻译的依赖走解释器,混合执行——对照
/// typst-eval 的 `import_package` + `resolve_package`)。
pub fn import_external(engine: &mut Engine, spec: &str) -> SourceResult<Module> {
    use typst::syntax::{RootedPath, VirtualPath, VirtualRoot, package::PackageSpec};
    let spec: PackageSpec = match spec.parse() {
        Ok(spec) => spec,
        Err(_) => bail!(Span::detached(), "invalid package spec"),
    };
    let manifest_id = RootedPath::new(
        VirtualRoot::Package(spec.clone()),
        VirtualPath::new("typst.toml").expect("path"),
    )
    .intern();
    let bytes = engine.world.file(manifest_id).at(SPAN0)?;
    let text = bytes
        .as_str()
        .map_err(typst::diag::FileError::from)
        .at(SPAN0)?;
    let manifest: typst::syntax::package::PackageManifest = match toml::from_str(text) {
        Ok(manifest) => manifest,
        Err(err) => bail!(SPAN0, "package manifest is malformed ({})", err.message()),
    };
    manifest.validate(&spec).at(SPAN0)?;
    let entry_id = RootedPath::new(
        VirtualRoot::Package(spec),
        VirtualPath::new(&manifest.package.entrypoint).expect("entry"),
    )
    .intern();
    let source = engine.world.source(entry_id).at(SPAN0)?;
    let mut sink = typst::engine::Sink::new();
    let route = typst::engine::Route::default();
    typst_eval::eval(
        engine.world,
        engine.library,
        typst::engine::Traced::default().track(),
        sink.track_mut(),
        route.track(),
        &source,
    )
}

const SPAN0: Span = Span::detached();

/// 显式降级占位:发出 warning 并返回 none(不静默降级原则——
/// 生成物在此处功能缺失,必须让使用者看见)。
pub fn degraded(engine: &mut Engine, message: &str) -> SourceResult<Value> {
    let diagnostic = typst::diag::SourceDiagnostic::warning(
        Span::detached(),
        typst::foundations::Str::from(format!("trust 降级: {message}")),
    );
    engine.sink.warn(diagnostic);
    Ok(Value::None)
}

/// 可变位置(赋值 / 可变方法的目标)的路径段,对照 typst-eval 的
/// `access.rs`:字段访问与 accessor 方法(`first`/`last`/`at`)。
///
/// typst 语义(`call_method_access` / `access_dict`):
/// - 字段段要求当前值是字典(`Dict::at_mut`);
/// - `at` 段在数组上收整数下标、在字典上收字符串键;
/// - `first`/`last` 仅数组。
pub enum PlaceSeg {
    /// 字典字段名(生成期转义后的字面量)。
    Field(String),
    /// `at` 的下标/键(生成代码求值出的 `Value`)。
    At(Value),
    First,
    Last,
}

/// 赋值目标(`x.f = v` / `x.f OP= v` / 解构写回)的可变位置,对照
/// typst-eval 的 `FieldAssignment` 语义:最后一段字段在字典上**缺失时
/// 插入**;中间段与 [`place_mut`] 同(at_mut 缺失报错,服务可变方法)。
pub fn place_assign<'a>(
    root: &'a mut Value,
    path: &[PlaceSeg],
    span: Span,
) -> SourceResult<&'a mut Value> {
    if let Some((PlaceSeg::Field(name), [])) = path.split_last()
        && let Value::Dict(dict) = root
    {
        if dict.get(name).is_err() {
            dict.insert(Str::from(name.as_str()), Value::None);
        }
        return dict.at_mut(name).at(span);
    }
    place_mut(root, path, span)
}

/// 沿路径段取可变引用(对照 `Expr::access`)。
pub fn place_mut<'a>(
    root: &'a mut Value,
    path: &[PlaceSeg],
    span: Span,
) -> SourceResult<&'a mut Value> {
    let mut current = root;
    for seg in path {
        current = match seg {
            PlaceSeg::Field(name) => match current {
                Value::Dict(dict) => dict.at_mut(name).at(span)?,
                value => {
                    let ty = value.ty();
                    bail!(span, "cannot mutate fields on type {ty}")
                }
            },
            PlaceSeg::At(key) => match current {
                Value::Array(array) => {
                    let index: i64 = key.clone().cast().at(span)?;
                    array.at_mut(index).at(span)?
                }
                Value::Dict(dict) => {
                    let key: typst::foundations::Str = key.clone().cast().at(span)?;
                    dict.at_mut(&key).at(span)?
                }
                value => {
                    let ty = value.ty();
                    if ty.scope().get("at").is_some() {
                        bail!(span, "cannot mutate a temporary value")
                    }
                    bail!(span, "type {ty} has no method `at`")
                }
            },
            PlaceSeg::First => match current {
                Value::Array(array) => array.first_mut().at(span)?,
                value => {
                    let ty = value.ty();
                    if ty.scope().get("first").is_some() {
                        bail!(span, "cannot mutate a temporary value")
                    }
                    bail!(span, "type {ty} has no method `first`")
                }
            },
            PlaceSeg::Last => match current {
                Value::Array(array) => array.last_mut().at(span)?,
                value => {
                    let ty = value.ty();
                    if ty.scope().get("last").is_some() {
                        bail!(span, "cannot mutate a temporary value")
                    }
                    bail!(span, "type {ty} has no method `last`")
                }
            },
        };
    }
    Ok(current)
}

// 样式规则原语(对照 typst-eval/src/rules.rs):求值期**只构造**,
// 不应用——set 规则构造 `Styles`,show 规则构造 `Recipe`,真正的
// 变换发生在 typst 的 realize/layout(生成物与解释器共用)。

/// `#set elem(..)`:构造样式表(对照 `Eval for ast::SetRule` 的
/// `target.set(engine, args)`)。
pub fn set_rule(
    engine: &mut Engine,
    target: Value,
    args: Args,
    span: Span,
) -> SourceResult<typst::foundations::Styles> {
    let func = target.cast::<Func>().at(span)?;
    let element = func
        .to_element()
        .ok_or_else(|| eco_format!("only element functions can be used in set rules"))
        .at(span)?;
    Ok(element.set(engine, args)?.spanned(span).liftable())
}

/// 空样式表(set 规则条件为假时,对照 `Ok(Styles::new())`)。
pub fn styles_empty() -> typst::foundations::Styles {
    typst::foundations::Styles::new()
}

/// `#show selector?: transform`:构造规则(对照 `Eval for
/// ast::ShowRule`;transform 为 content/函数值)。
pub fn show_rule(
    selector: Option<Value>,
    transform: Value,
    span: Span,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| value.cast::<Selector>().at(span))
        .transpose()?;
    let transform = transform.cast::<Transformation>().at(span)?;
    Ok(typst::foundations::Recipe::new(selector, transform, span))
}

/// `#show selector?: set ..`:transform 是样式(对照
/// `Transformation::Style(set.eval(..)?)`)。
pub fn show_rule_style(
    selector: Option<Value>,
    styles: typst::foundations::Styles,
    span: Span,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| value.cast::<Selector>().at(span))
        .transpose()?;
    Ok(typst::foundations::Recipe::new(
        selector,
        Transformation::Style(styles),
        span,
    ))
}

/// 把规则之后的尾部内容挂上样式(对照 `eval_markup` 的
/// `tail.styled_with_map(styles)`)。
pub fn styled_with_map(
    tail: typst::foundations::Content,
    styles: typst::foundations::Styles,
) -> typst::foundations::Content {
    tail.styled_with_map(styles)
}

/// 把规则之后的尾部内容挂上 show 规则(对照
/// `tail.styled_with_recipe`;无 selector 的规则立即应用)。
pub fn styled_with_recipe(
    engine: &mut Engine,
    context: Tracked<Context>,
    tail: typst::foundations::Content,
    recipe: typst::foundations::Recipe,
) -> SourceResult<typst::foundations::Content> {
    tail.styled_with_recipe(engine, context, recipe)
}

/// `#context expr`:构造推迟求值的闭包(对照 `Eval for
/// ast::Contextual`,realize/layout 期带 location/styles 求值)。
pub fn context_closure(body_src: &str, captured: &[(&'static str, Value)]) -> Func {
    // 与 `closure` 同款:片段挂虚拟文件(markup 包装成 let 绑定),
    // context 体解释执行的错误 span 可定位到片段行。
    let source = virtual_closure_source(body_src);
    let root = source.root();
    // 取根下的首个表达式节点(闭包节点存的是表达式,非根)。
    let node = root
        .children()
        .find(|child| child.cast::<ast::Expr>().is_some())
        .expect("context body parses to an expression");
    let mut scope = Scope::new();
    for (name, value) in captured {
        scope.define(name, value.clone());
    }
    Func::from(Closure {
        node: ClosureNode::Context(node.clone()),
        defaults: vec![],
        captured: scope,
        num_pos_params: 0,
    })
}

/// context 闭包的载体元素(对照 `ContextElem::new(func).pack()`;
/// ContextElem 经 `Packed` 包装后实现 IntoValue)。
pub fn context_body(func: Func) -> typst::foundations::Content {
    use typst::foundations::IntoValue;
    match typst::foundations::ContextElem::new(func).into_value() {
        typst::foundations::Value::Content(content) => content,
        _ => unreachable!("ContextElem packs to content"),
    }
}
