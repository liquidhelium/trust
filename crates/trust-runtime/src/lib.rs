//! TRUST 运行时支撑层:机器翻译生成的 Rust 代码在运行期依赖本 crate。
//!
//! 设计原则(见 docs/research-route.md §3):用户可见的值保持 typst-library
//! 的 `Value`,零私有值模型;所有语义函数与 `typst-eval` 的解释实现逐条
//! 对照(报错文案与 span 行为一致),差分测试以此为验收。

use comemo::Tracked;
use typst::diag::{At, SourceResult, bail};
use typst::engine::Engine;
use typst::foundations::{
    Arg, Args, Array, Closure, ClosureNode, Context, Func, IntoValue, Scope, Str, Value, ops,
};
use typst::syntax::{Span, Spanned, ast, SyntaxKind};

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

/// 调用一个函数值(解释器对非函数调用的报错同款)。
pub fn call(
    func: &Value,
    engine: &mut Engine,
    context: Tracked<Context>,
    span: Span,
    args: Args,
) -> SourceResult<Value> {
    if let Value::Func(func) = func {
        func.call(engine, context, args)
    } else {
        bail!(span, "expected function or content, found {}", func.ty())
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
    let node = root
        .children()
        .find(|child| child.kind() == SyntaxKind::Closure)
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
