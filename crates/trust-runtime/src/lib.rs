//! TRUST 运行时支撑层:机器翻译生成的 Rust 代码在运行期依赖本 crate。
//!
//! 设计原则(见 docs/research-route.md §3):用户可见的值保持 typst-library
//! 的 `Value`,零私有值模型;所有语义函数与 `typst-eval` 的解释实现逐条
//! 对照(报错文案与 span 行为一致),差分测试以此为验收。

use comemo::Tracked;
use typst::diag::{At, SourceResult, bail};
use typst::engine::Engine;
use typst::foundations::{
    Arg, Args, Array, Closure, ClosureNode, Context, Func, IntoValue, Module, Scope, Str, Value, ops,
};
use typst::syntax::{Span, Spanned, ast, SyntaxKind};
use typst::World;
use comemo::{Track, TrackedMut};

/// 模块级值槽:由生成代码以 `LazyLock::new(&init_fn)` 构造。
pub type Slot = std::sync::LazyLock<Value, &'static (dyn Fn() -> Value + Send + Sync)>;

/// 读取(必要时强制初始化)一个值槽。
pub fn slot(slot: &Slot) -> Value {
    slot.clone_inner()
}

// 让 Clone 语义显式一点:LazyLock 解引用到 Value。
trait SlotExt {
    fn clone_inner(&self) -> Value;
}
impl SlotExt for Slot {
    fn clone_inner(&self) -> Value {
        (**self).clone()
    }
}

/// 在标准库全局作用域解析一个名字(对照解释器的 `Scopes::get`,
/// 未命中时报同款 `unknown variable`)。
pub fn global(engine: &Engine, name: &str, span: Span) -> SourceResult<Value> {
    match engine.library.global.scope().get(name) {
        Some(binding) => Ok(binding.read().clone()),
        None => bail!(span, "unknown variable: {name}"),
    }
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
                bail!(span, "type {} has no method `{name}`", target.ty().short_name())
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
pub fn closure(src: &str, defaults: Vec<Value>, captured: &[(&'static str, Value)]) -> Func {
    let root = typst_syntax::parse_code(src);
    // 源片段可能是裸闭包 `(x) => ..`(Code 下直接是 Closure)或命名
    // 绑定形态 `f(x) = ..`(LetBinding 包着 Closure),统一取首个
    // Closure 节点;命名形态下闭包名保留,递归自引用可用。
    let node = find_closure_node(&root)
        .expect("closure source parses to a closure")
        .clone();
    let num_pos_params = node
        .cast::<ast::Closure>()
        .expect("closure node casts")
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    let mut scope = Scope::new();
    for (name, value) in captured {
        scope.define(name, value.clone());
    }
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
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
        Value::Bytes(bytes) => {
            bytes.as_slice().iter().map(|b| Value::Int(*b as i64)).collect()
        }
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

/// 按名或按位绑定一个参数(闭包参数两种给法都接受)。
pub fn param(args: &mut Args, name: &str, span: Span) -> SourceResult<Value> {
    match args.named_or_find::<Value>(name)? {
        Some(value) => Ok(value),
        None => {
            // 复刻 Args::missing_argument 的文案与提示。
            for item in &args.items {
                if item.name.as_deref() == Some(name) {
                    bail!(
                        item.span,
                        "the argument `{name}` is positional";
                        hint: "try removing `{name}:`";
                    )
                }
            }
            bail!(span, "missing argument: {name}")
        }
    }
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
    Arg { span, name: Some(name.into()), value: Spanned::new(value, span) }
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
pub fn method_mut(value: &mut Value, method: &str, mut args: Args, span: Span) -> SourceResult<Value> {
    let ty = value.ty();
    let mut output = Value::None;
    match value {
        Value::Array(array) if matches!(method, "push" | "pop" | "insert" | "remove") => {
            match method {
                "push" => array.push(args.expect("value")?),
                "pop" => output = array.pop().at(span)?,
                "insert" => {
                    array.insert(args.expect("index")?, args.expect("value")?).at(span)?
                }
                _ => {
                    output = array
                        .remove(args.expect("index")?, args.named("default")?)
                        .at(span)?
                }
            }
        }
        Value::Dict(dict) if matches!(method, "insert" | "remove") => {
            match method {
                "insert" => dict.insert(args.expect("key")?, args.expect("value")?),
                _ => {
                    output =
                        dict.remove(args.expect("key")?, args.named("default")?).at(span)?
                }
            }
        }
        _ => bail!(span, "type {ty} has no method `{method}`"),
    }
    args.finish()?;
    Ok(output)
}

/// for 循环的零物化执行(对照 `Eval for ast::ForLoop`):迭代源直接
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
                let pair =
                    Array::from_iter([key.clone().into_value(), item.clone()]).into_value();
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
pub fn iter_values(
    value: &Value,
    span: Span,
) -> SourceResult<Box<dyn Iterator<Item = Value>>> {
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
        Err(_) => bail!(span, "{}", elements_mismatch(array.len(), i + 1, false)),
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
        Some(sink_size) if i + sink_size <= array.len() => Ok(
            array.as_slice()[i..i + sink_size].iter().cloned().collect(),
        ),
        _ => bail!(span, "{}", elements_mismatch(array.len(), total - 1, true)),
    }
}

/// 数组解构结束后校验(还有剩余元素则报错)。
pub fn d_array_finish(
    array: &typst::foundations::Array,
    consumed: usize,
    span: Span,
) -> SourceResult<()> {
    if consumed < array.len() {
        bail!(span, "{}", elements_mismatch(array.len(), consumed, false));
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

/// 对照原版 wrong_number_of_elements 的文案形态。
fn elements_mismatch(len: usize, expected: usize, spread: bool) -> String {
    let quantifier = if len > expected { "too many" } else { "not enough" };
    let what = if spread {
        format!("at least {expected} elements")
    } else if expected == 1 {
        "a single element".to_string()
    } else {
        format!("{expected} elements")
    };
    format!("{quantifier} elements to destructure, expected {what}")
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
    let text = bytes.as_str().map_err(typst::diag::FileError::from).at(SPAN0)?;
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

/// accessor 方法赋值(`a.at(i) = v` / `first` / `last`,对照
/// typst-eval/src/methods.rs::call_method_access)。
pub fn accessor_assign(
    target: &mut Value,
    method: &str,
    index: Option<Value>,
    value: Value,
    span: Span,
) -> SourceResult<()> {
    match target {
        Value::Array(array) if matches!(method, "first" | "last" | "at") => {
            let slot: &mut Value = match method {
                "first" => array.first_mut().at(span)?,
                "last" => array.last_mut().at(span)?,
                _ => {
                    let i = index.unwrap_or_else(|| Value::None).cast::<i64>().at(span)?;
                    array.at_mut(i).at(span)?
                }
            };
            *slot = value;
            Ok(())
        }
        Value::Dict(dict) if method == "at" => {
            let key = index.unwrap_or_else(|| Value::None).cast::<Str>().at(span)?;
            { let slot = dict.at_mut(&key).at(span)?; *slot = value; }
            Ok(())
        }
        other => bail!(span, "type {} has no method `{}`", other.ty(), method),
    }
}
