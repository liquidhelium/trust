//! TRUST 运行时支撑层:机器翻译生成的 Rust 代码在运行期依赖本 crate。
//!
//! 设计原则(见 docs/research-route.md §3):用户可见的值保持 typst-library
//! 的 `Value`,零私有值模型;所有语义函数与 `typst-eval` 的解释实现逐条
//! 对照(报错文案与 span 行为一致),差分测试以此为验收。

use comemo::Tracked;
use typst::World;
use typst::diag::{At, SourceResult, bail};
use typst::engine::Engine;
use typst::foundations::{
    Arg, Args, Array, Closure, ClosureNode, Content, Context, Func, IntoValue, Module, Scope,
    Selector, Str, Transformation, Value, ops,
};
use typst::syntax::ast::{self, AstNode};
use typst::syntax::{Span, Spanned, SyntaxKind};

use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use typst::diag::{Hint, SourceDiagnostic, Trace};

/// 每构建状态(per-build 槽板)的存储与槽原语。
///
/// 见 `_things/DESIGN-per-build.md`:typst 运行时 = 有限命名槽 + 每次
/// 求值写一次 + 按值快照;这里提供对应的 Rust 形态——写一次单元
/// (`OnceLock<Value>`)与随函数值传递的状态句柄([`StateSlot`],
/// 原生闭包 C 方案):`module_init` 每次求值构造新实例,无全局注册
/// 表,生命周期随函数值(对照 typst 闭包的数据布局)。
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
    engine: &mut Engine,
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

/// 在标准库全局作用域解析一个名字(对照解释器的 `Scopes::get`:
/// 局部之外查 `library.global`,miss 且名字为 `std` 时特判指向
/// `library.std`;未命中报同款 `unknown variable`)。读取走
/// `read_checked`,与原生一致地发出弃用警告。
pub fn global(engine: &mut Engine, name: &str, span: Span) -> SourceResult<Value> {
    match engine.library.global.scope().get(name) {
        Some(binding) => Ok(binding.read_checked((engine, span)).clone()),
        None if name == "std" => Ok(engine.library.std.read().clone()),
        None => bail!(span, "unknown variable: {name}"),
    }
}

/// 全局查找,不存在返回 `None`(对照 typst 捕获语义:创建期不可见的
/// 名字不捕获,由闭包体内自己的 import 在调用期绑定;`std` 特判
/// 与 [`global`] 同款)。
pub fn global_opt(engine: &mut Engine, name: &str) -> SourceResult<Option<Value>> {
    Ok(match engine.library.global.scope().get(name) {
        Some(binding) => Some(binding.read_checked((engine, Span::detached())).clone()),
        None if name == "std" => Some(engine.library.std.read().clone()),
        None => None,
    })
}

/// 字段访问 `target.name`(对照 `access_field`:`Value::field` +
/// get-rule 样式链回退——元素函数的字段(如 `text.size`)在静态字段
/// 未命中时按 context 样式链取值;sink 透传 engine 以发出弃用警告)。
pub fn field(
    target: &Value,
    name: &str,
    engine: &mut Engine,
    context: Tracked<Context>,
    span: Span,
) -> SourceResult<Value> {
    let err = match target.field(name, (engine, span)).at(span) {
        Ok(value) => return Ok(value),
        Err(err) => err,
    };
    // Check whether this is a get rule field access(对照 access_field:
    // styles 校验延迟到确认确为样式链字段之后)。
    if let Value::Func(func) = target
        && let Some(element) = func.to_element()
        && let Some(id) = element.field_id(name)
    {
        let styles = context.styles();
        let chain = styles.as_ref().map(|&s| s).unwrap_or_default();
        if let Ok(value) = element.field_from_styles(id, chain) {
            let _ = styles.at(span)?;
            return Ok(value);
        }
    }
    Err(err)
}

/// 调用点护栏:函数调用深度(对照 `vm.engine.route.check_call_depth`,
/// 互递归触发报错而非段错误)。
pub fn check_call_depth(engine: &Engine, span: Span) -> SourceResult<()> {
    engine.route.check_call_depth().at(span)
}

/// 解析被调值(对照原生 `callee.eval()?.cast::<Func>()`:函数值、
/// 类型值(取构造器)、符号(取关联函数)均可调;其余按 cast 文案
/// 报错——实参在此之前不会被求值,与原生求值序一致)。按值收
/// (调用点持有临时值,常见路径零克隆)。
pub fn callee(value: Value, span: Span) -> SourceResult<Func> {
    if let Value::Func(func) = value {
        return Ok(func);
    }
    value.cast::<Func>().at(span)
}

/// 统一调用路径(对照 `call_func`):func 以调用点 span 标记、错误挂
/// tracepoint(`error occurred in this call of function ...`)、栈上
/// `stacker::maybe_grow` 扩栈;方法调用经 `insert` 把目标作为首个
/// 位置参数(对照 `args.insert(0, target_expr.span(), target)`)。
pub fn call_with(
    func: Func,
    insert: Option<(Value, Span)>,
    span: Span,
    mut args: Args,
    engine: &mut Engine,
    context: Tracked<Context>,
) -> SourceResult<Value> {
    let func = func.spanned(span);
    if let Some((target, target_span)) = insert {
        args.insert(0, target_span, target);
    }
    // tracepoint 名字仅在错误路径物化(trace 闭包惰性调用)。不包
    // stacker::maybe_grow:调用深度由 check_call_depth 护栏(80 层)
    // 截停,不会走到栈溢出;每调用一次的栈探测是纯税(原生在解释
    // 路径上需要它,编译路径不需要)。
    func.call(engine, context, args).trace(
        engine.world,
        || typst::diag::Tracepoint::Call(func.name().map(EcoString::from)),
        span,
    )
}

/// 方法调用被调值解析(对照 `eval_field_callee`:类型作用域的方法
/// 优先,其次 content 元素作用域;Symbol/Func/Type/Module 允许字段
/// 调用语法;报错按原版两段式 + hint 体系)。
///
/// 返回 `(函数值, 方法调用时的目标值)`;方法调用时调用方须把目标
/// 作为首个位置参数传入(目标按值带回,避免一次克隆)。
pub fn method_callee(
    target: Value,
    name: &str,
    access_text: &str,
    engine: &mut Engine,
    span: Span,
) -> SourceResult<(Func, Option<Value>)> {
    let sink = (&mut *engine, span);
    let mut is_method_call = false;
    let callee_value = if let Some(method) = target.ty().scope().get(name) {
        is_method_call = true;
        method.read_checked(sink).clone()
    } else if let Value::Content(content) = &target
        && let Some(method) = content.elem().scope().get(name)
    {
        is_method_call = true;
        method.read_checked(sink).clone()
    } else if matches!(
        &target,
        Value::Symbol(_) | Value::Func(_) | Value::Type(_) | Value::Module(_)
    ) {
        target.field(name, sink).at(span)?
    } else {
        // Otherwise we cannot call this field and produce an error.
        match target.field(name, sink) {
            Ok(callee_value) => {
                let is_dict = matches!(target, Value::Dict(_));
                let is_named = matches!(target, Value::Args(_));
                let (kind, ty_name) = element_or_type_with_name(&target);
                let message = if is_dict {
                    EcoString::from("cannot directly call dictionary keys as functions")
                } else if is_named {
                    EcoString::from("cannot directly call named argument fields as functions")
                } else {
                    eco_format!("`{name}` is not a valid method for {kind} `{ty_name}`")
                };
                let mut err =
                    SourceDiagnostic::error(span, typst::foundations::Str::from(message));
                if callee_value.clone().cast::<Func>().is_ok() {
                    err = err.with_hint(eco_format!(
                        "to call the stored function, wrap the field access in parentheses: `({access_text})(..)`"
                    ));
                } else {
                    err = err.with_hint(eco_format!(
                        "to access the `{name}` {}, remove the function arguments: `{access_text}`",
                        if is_dict { "key" } else if is_named { "argument" } else { "field" },
                    ));
                }
                if is_dict {
                    err = err.with_hint(
                        "dictionary keys cannot be used with method syntax as keys \
                         could conflict with built-in method names",
                    );
                } else if is_named {
                    err = err.with_hint(
                        "named arguments cannot be used with method syntax as argument \
                         names could conflict with built-in method names",
                    );
                }
                return Err(eco_vec![err]);
            }
            Err(_) => {
                let (kind, ty_name) = element_or_type_with_name(&target);
                bail!(span, "{kind} {ty_name} has no method `{name}`")
            }
        }
    };
    match callee_value {
        Value::Func(func) if is_method_call => Ok((func, Some(target))),
        Value::Func(func) => Ok((func, None)),
        callee_value => callee_value
            .cast::<Func>()
            .map(|func| (func, None))
            .at(span),
    }
}

/// If the value is content, the string "element" and the name of its element
/// function, or the string "type" and the long name of the value's type.
fn element_or_type_with_name(value: &Value) -> (&'static str, EcoString) {
    if let Value::Content(content) = value {
        ("element", EcoString::from(content.elem().name()))
    } else {
        ("type", EcoString::from(value.ty().long_name()))
    }
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

/// 解释片段(`closure`/`native_closure`)的定位缓存:**原始片段文本 →
/// (闭包节点, 位置参数数)**。对照原生闭包创建——`Eval for ast::Closure`
/// 的 node 是源文件 AST 子树引用(SyntaxNode 内部 Arc),创建期零 parse
/// 零查找;片段首次定位(parse + 找 Closure 节点)后,后续创建只
/// clone 节点引用,与原生同款"AST 定位一次、值可廉价重建"。
/// 键为原始文本(包装/解析只在未命中时进行,命中路径零格式化)。
/// 片段同时登记进 [`CLOSURE_SOURCES`](虚拟文件,span 可定位到片段行)。
fn cached_closure_node(src: &str) -> (typst_syntax::SyntaxNode, usize) {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, (typst_syntax::SyntaxNode, usize)>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(hit) = CACHE.read().unwrap().get(src) {
        return hit.clone();
    }
    // markup 合法形态:命名绑定直接嵌入;其余(裸闭包/context 体)包
    // 一层括号——markup 的 `#let x = expr` init 在**行尾**截止,裸闭包
    // 体跨行会被截断成空体,括号强制 code 上下文才能跨行。
    let trimmed = src.trim_start();
    let text = if trimmed.starts_with("let ") {
        format!("#{trimmed}")
    } else {
        format!("#let __trust_closure = ({trimmed})")
    };
    let n = CLOSURE_SOURCES.read().unwrap().len();
    let id = typst::syntax::RootedPath::new(
        typst::syntax::VirtualRoot::Project,
        typst::syntax::VirtualPath::new(format!("__trust_closure_{n}.typ"))
            .expect("valid closure path"),
    )
    .intern();
    let source = typst::syntax::Source::new(id, text.clone());
    CLOSURE_SOURCES.write().unwrap().push((id, text.clone()));
    // 源片段可能是裸闭包 `(x) => ..` 或命名绑定 `f(x) = ..`(LetBinding
    // 包着 Closure),统一取首个 Closure 节点;命名形态名字保留,
    // 递归自引用可用。
    let node = find_closure_node(source.root())
        .unwrap_or_else(|| panic!("closure source parses to a closure: {text:?}"))
        .clone();
    let num_pos_params = node
        .cast::<ast::Closure>()
        .expect("closure node casts")
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    CACHE
        .write()
        .unwrap()
        .insert(src.to_string(), (node.clone(), num_pos_params));
    (node, num_pos_params)
}

pub fn closure(src: &str, defaults: Vec<Value>, captured: &[(&'static str, Value)]) -> Func {
    let (node, num_pos_params) = cached_closure_node(src);
    let mut scope = Scope::new();
    for (name, value) in captured {
        if std::env::var_os("TRUST_DEBUG_CAPTURE").is_some() && src.contains("ctx => {") {
            let brief = format!("{value:?}");
            eprintln!("[capture] {name} = {}", &brief[..brief.len().min(120)]);
        }
        scope.define(name, value.clone());
    }
    let closure_span = node.span();
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

/// code block 的 join 语义(对照 `ops::join`)。
pub fn join(lhs: Value, rhs: Value, span: Span) -> SourceResult<Value> {
    ops::join(lhs, rhs).at(span)
}

/// if/while 的条件求值:非 boolean 按原版 cast 报错。
pub fn boolean(value: Value, span: Span) -> SourceResult<bool> {
    value.cast::<bool>().at(span)
}

/// while 循环的迭代护栏上限(对照 `typst-eval/src/flow.rs`:
/// `MAX_ITERATIONS = 10_000`,超出报 "loop seems to be infinite")。
pub const MAX_ITERATIONS: usize = 10_000;

/// 位置参数绑定(按声明序,对照 eval_closure 的 `args.expect`)。
pub fn param_pos(args: &mut Args, name: &str, _span: Span) -> SourceResult<Value> {
    args.expect::<Value>(name)
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

/// 构造一个命名参数(生成代码用于调用点;`span` 为参数项 span,
/// `value_span` 为值表达式 span,对照原生 `Eval for ast::Args`)。
pub fn named_arg(span: Span, value_span: Span, name: &str, value: Value) -> Arg {
    Arg {
        span,
        name: Some(name.into()),
        value: Spanned::new(value, value_span),
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
                args.items.push(named_arg(span, span, &key, value));
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
/// `typst-eval/src/methods.rs::call_method_mut`(错误挂方法调用的
/// tracepoint,与原生一致)。
pub fn method_mut(
    engine: &mut Engine,
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
    let name = EcoString::from(method);
    Ok(output).trace(
        engine.world,
        || typst::diag::Tracepoint::Call(Some(name.clone())),
        span,
    )
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

/// 迭代器形态的循环源(对照 `Eval for ast::ForLoop`:数组按值消费、
/// 字典逐键值对、字符串按字素、字节串按字节;全部流式,零物化——
/// 字典/字符串/字节串惰性产出,供生成代码内联 `for` 使用
/// (break/continue 原生可用)。解构模式与 Str/Bytes 的组合由调用侧
/// 在翻译期保证不出现(原生同样报错)。迭代器借入 `value`
/// (生成代码中循环源临时值存活于整条 for 语句)。
pub fn iter_values<'a>(
    value: &'a Value,
    span: Span,
) -> SourceResult<Box<dyn Iterator<Item = Value> + 'a>> {
    use unicode_segmentation::UnicodeSegmentation;
    Ok(match value {
        Value::Array(array) => Box::new(array.clone().into_iter()),
        Value::Dict(dict) => Box::new(dict.iter().map(|(key, item): (&Str, &Value)| {
            Array::from_iter([key.clone().into_value(), item.clone()]).into_value()
        })),
        Value::Str(string) => Box::new(
            string
                .as_str()
                .graphemes(true)
                .map(|g| Value::Str(g.into())),
        ),
        Value::Bytes(bytes) => Box::new(
            bytes
                .as_slice()
                .iter()
                .map(|b| Value::Int(*b as i64)),
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

/// 字典解构按键取值(经 `Dict::get`,保留原版 hint)。
pub fn d_get(dict: &typst::foundations::Dict, key: &str, span: Span) -> SourceResult<Value> {
    dict.get(key).cloned().at(span)
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

/// 运行时导入外部包(未被翻译的依赖走解释器,混合执行——对照
/// typst-eval 的 `import_package` + `import_file`:sink/route 透传
/// (警告与调用深度进入同一链)、环检测、模块挂包短名)。
pub fn import_external(engine: &mut Engine, spec: &str, span: Span) -> SourceResult<Module> {
    use typst::syntax::{RootedPath, VirtualPath, VirtualRoot, package::PackageSpec};
    let spec: PackageSpec = match spec.parse() {
        Ok(spec) => spec,
        Err(_) => bail!(span, "invalid package spec"),
    };
    let manifest_id = RootedPath::new(
        VirtualRoot::Package(spec.clone()),
        VirtualPath::new("typst.toml").expect("path"),
    )
    .intern();
    let bytes = engine.world.file(manifest_id).at(span)?;
    let text = bytes
        .as_str()
        .map_err(typst::diag::FileError::from)
        .at(span)?;
    let manifest: typst::syntax::package::PackageManifest = match toml::from_str(text) {
        Ok(manifest) => manifest,
        Err(err) => bail!(span, "package manifest is malformed ({})", err.message()),
    };
    manifest.validate(&spec).at(span)?;
    let entry_id = RootedPath::new(
        VirtualRoot::Package(spec),
        VirtualPath::new(&manifest.package.entrypoint).expect("entry"),
    )
    .intern();
    let source = engine.world.source(entry_id).at(span)?;
    // Prevent cyclic importing.
    if engine.route.contains(source.id()) {
        bail!(span, "cyclic import");
    }
    typst_eval::eval(
        engine.world,
        engine.library,
        engine.traced,
        comemo::TrackedMut::reborrow_mut(&mut engine.sink),
        engine.route.track(),
        &source,
    )
    .map(|module| module.with_name(manifest.package.name.clone()))
}

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
/// - 字段段要求当前值是字典(`Dict::at_mut`,缺失报错带 hint);
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

/// 普通赋值 `a.b..k = v` 的求值(对照 `apply_assignment` 的
/// FieldAccess 特例:`access_dict` 走到**末段**后在字典上直接
/// `insert`——缺失即创建,单次哈希写入;中间段与 [`place_mut`] 同,
/// at_mut 报错)。末段非字段(at/first/last)按 [`place_mut`] 语义
/// 取可变引用后写入。
pub fn assign_value(
    root: &mut Value,
    path: &[PlaceSeg],
    value: Value,
    span: Span,
) -> SourceResult<()> {
    let Some((last, init)) = path.split_last() else {
        *root = value;
        return Ok(());
    };
    if let PlaceSeg::Field(name) = last {
        let parent = place_mut(root, init, span)?;
        if let Value::Dict(dict) = parent {
            dict.insert(Str::from(name.as_str()), value);
            return Ok(());
        }
    }
    // 末段非字典字段:走通用可变位置(at/first/last 或非字典宿主)。
    *place_mut(root, path, span)? = value;
    Ok(())
}

/// 沿路径段取可变引用(对照 `Expr::access`;字段段错误文案对照
/// `access_dict` 的三段式分支)。
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
                value => access_dict_error(value, span)?,
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

/// 字段可变访问的非字典分支(对照 `access_dict` 的三段式:专用
/// getter 类型 / 无字段类型 / 静态字段未开放可变)。
fn access_dict_error(value: &mut Value, span: Span) -> SourceResult<&mut Value> {
    let ty = value.ty();
    if matches!(
        value,
        Value::Symbol(_) | Value::Content(_) | Value::Module(_) | Value::Func(_) | Value::Args(_)
    ) {
        bail!(span, "cannot mutate fields on {ty}");
    }
    if typst::foundations::fields_on(ty).is_empty() {
        bail!(span, "{ty} does not have accessible fields");
    }
    Err(eco_format!("fields on {ty} are not yet mutable"))
        .hint(eco_format!(
            "try creating a new {ty} with the updated field value instead"
        ))
        .at(span)
}

/// per-build 状态的不透明句柄:包 `Arc<dyn Any + Send + Sync>` 进
/// typst 值世界(`Value::Dyn`),随函数值传递(对照 typst 闭包的
/// captured 数据布局);native 体经 [`take_state`] 从首个位置参数解包。
#[derive(Clone)]
pub struct StateSlot(std::sync::Arc<dyn std::any::Any + Send + Sync>);

impl StateSlot {
    /// 包装一份 per-build 状态(`Arc<__State>` 自动协变为 `Arc<dyn Any>`)。
    pub fn new(state: std::sync::Arc<dyn std::any::Any + Send + Sync>) -> Self {
        Self(state)
    }
}

impl std::fmt::Debug for StateSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("state slot")
    }
}

impl typst::foundations::Repr for StateSlot {
    fn repr(&self) -> EcoString {
        "state slot".into()
    }
}

impl typst::foundations::NativeType for StateSlot {
    const NAME: &'static str = "state";
    fn data() -> &'static typst::foundations::NativeTypeData {
        static DATA: std::sync::OnceLock<typst::foundations::NativeTypeData> =
            std::sync::OnceLock::new();
        DATA.get_or_init(|| typst::foundations::NativeTypeData {
            name: "state",
            long_name: "state",
            title: "State",
            docs: "internal per-build state slot",
            def_site: typst::utils::DefSite {
                path: "trust-runtime",
                key: "StateSlot",
            },
            keywords: &[],
            constructor: std::sync::LazyLock::new(|| None),
            scope: std::sync::LazyLock::new(typst::foundations::Scope::new),
        })
    }
}

impl PartialEq for StateSlot {
    fn eq(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.0, &other.0)
    }
}

impl std::hash::Hash for StateSlot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (std::sync::Arc::as_ptr(&self.0) as *const u8 as usize).hash(state);
    }
}

/// 签名保真的 stub 闭包(原生闭包 C 方案):`{name}{params}` 原签名
/// 保留(参数检查/`params()`/repr/默认值自省全保真),转发体把状态
/// 句柄作为首个位置参数传给 native 实现。`defaults` 为创建期物化值
/// (对照 typst 闭包创建期求值默认值;调用期缺失参数从 defaults 取,
/// AST 里的默认值表达式不会被求值)。状态随函数值走,生命周期与
/// 函数值一致——全局注册表寻址(STATES 线)由此退场。
pub fn native_closure(
    data: &'static typst::foundations::NativeFuncData,
    params_src: &str,
    defaults: Vec<Value>,
    state: &StateSlot,
) -> Func {
    let (params, arg_list) = stub_signature(params_src);
    let src = stub_source(data.name, &params, &arg_list);
    let (node, num_pos_params) = cached_closure_node(&src);
    let mut scope = Scope::new();
    scope.define("__trust_impl", Func::from(data));
    scope.define(
        "__trust_state",
        Value::Dyn(typst::foundations::Dynamic::new(state.clone())),
    );
    let closure_span = node.span();
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    .spanned(closure_span)
}

/// stub 签名与转发实参表(按 `params_src` 文本缓存——stub 创建高频,
/// parse 只发生一次)。
///
/// 模式参数(解构参数)在签名里替换为合成名 `__trust_pN`(转发的
/// 签名必须绑定转发体引用的名字;解构本身由 native 体完成);
/// 无名 sink `..` 补合成名 `__trust_sink`。
fn stub_signature(params_src: &str) -> (String, String) {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, (String, String)>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(hit) = CACHE.read().unwrap().get(params_src) {
        return hit.clone();
    }
    let root = typst_syntax::parse_code(&format!("let f{params_src} = none"));
    let (mut params, mut args): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    if let Some(closure_node) = find_closure_node(&root)
        && let Some(closure) = closure_node.cast::<ast::Closure>()
    {
        let mut nth_pattern = 0usize;
        for param in closure.params().children() {
            let text = param.to_untyped().full_text().to_string();
            match param {
                ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(_))) => {
                    args.push(text.clone());
                    params.push(text);
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                    args.push(text.clone());
                    params.push(text);
                }
                ast::Param::Pos(_) => {
                    // 解构参数:签名绑定合成名,native 体自行解构。
                    let name = format!("__trust_p{nth_pattern}");
                    nth_pattern += 1;
                    args.push(name.clone());
                    params.push(name);
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str();
                    args.push(format!("{name}: {name}"));
                    params.push(text);
                }
                ast::Param::Spread(spread) => match spread.sink_ident() {
                    Some(ident) => {
                        args.push(format!("..{}", ident.as_str()));
                        params.push(text);
                    }
                    None => {
                        args.push("..__trust_sink".to_string());
                        // 无名 sink:原文形如 `..`,补合成名。
                        let named = if text.trim() == ".." {
                            "..__trust_sink".to_string()
                        } else {
                            text.replace("..", "..__trust_sink")
                        };
                        params.push(named);
                    }
                },
            }
        }
    }
    let out = (params.join(", "), args.join(", "));
    CACHE
        .write()
        .unwrap()
        .insert(params_src.to_string(), out.clone());
    out
}

/// stub 源码 `let {name}({params}) = __trust_impl(__trust_state, {args})`
/// 按 (函数名, 签名) 缓存(高频创建路径零格式化)。
fn stub_source(name: &str, params: &str, args: &str) -> String {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, std::collections::HashMap<String, String>>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(inner) = CACHE.read().unwrap().get(name)
        && let Some(hit) = inner.get(params)
    {
        return hit.clone();
    }
    let src = format!("let {name}({params}) = __trust_impl(__trust_state, {args})");
    CACHE
        .write()
        .unwrap()
        .entry(name.to_string())
        .or_default()
        .insert(params.to_string(), src.clone());
    src
}

/// native 体序言:从首个位置参数解包 per-build 状态并从实参中移除
/// (对照 stub 转发:`__trust_impl(__trust_state, ..用户实参)`)。
pub fn take_state<T: std::any::Any + Send + Sync>(
    args: &mut Args,
) -> SourceResult<std::sync::Arc<T>> {
    let value: Value = args.expect("__trust_state")?;
    match value {
        Value::Dyn(dynamic) => match dynamic.downcast::<StateSlot>() {
            Some(slot) => match slot.0.clone().downcast::<T>() {
                Ok(arc) => Ok(arc),
                Err(_) => Err(state_error("状态句柄类型不符(生成器 bug)")),
            },
            None => Err(state_error("首个位置参数不是状态句柄(生成器 bug)")),
        },
        _ => Err(state_error("首个位置参数不是状态句柄(生成器 bug)")),
    }
}

// 样式规则原语(对照 typst-eval/src/rules.rs):求值期**只构造**,
// 不应用——set 规则构造 `Styles`,show 规则构造 `Recipe`,真正的
// 变换发生在 typst 的 realize/layout(生成物与解释器共用)。

/// `#set elem(..)`:构造样式表(对照 `Eval for ast::SetRule` 的
/// `target.set(engine, args)`)。
pub fn set_rule(
    target: Value,
    args: Args,
    span: Span,
    engine: &mut Engine,
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
/// ast::ShowRule`:selector 强转 `ShowableSelector`(不可 show 的
/// 选择器按原版 cast 文案报错,站点为 selector span);附带原版的
/// `show page` 无效警告与 `show par: set block(spacing: ..)` 迁移警告)。
pub fn show_rule(
    selector: Option<Value>,
    sel_span: Span,
    transform: Value,
    span: Span,
    engine: &mut Engine,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| value.cast::<typst::foundations::ShowableSelector>().at(sel_span))
        .transpose()?
        .map(|selector| selector.0);
    let transform = transform.cast::<Transformation>().at(span)?;
    let recipe = typst::foundations::Recipe::new(selector, transform, span);
    check_show_rules(engine, &recipe);
    Ok(recipe)
}

/// `#show selector?: set ..`:transform 是样式(对照
/// `Transformation::Style(set.eval(..)?)`)。
pub fn show_rule_style(
    selector: Option<Value>,
    sel_span: Span,
    styles: typst::foundations::Styles,
    span: Span,
    engine: &mut Engine,
) -> SourceResult<typst::foundations::Recipe> {
    let selector = selector
        .map(|value| value.cast::<typst::foundations::ShowableSelector>().at(sel_span))
        .transpose()?
        .map(|selector| selector.0);
    let recipe = typst::foundations::Recipe::new(selector, Transformation::Style(styles), span);
    check_show_rules(engine, &recipe);
    Ok(recipe)
}

/// 原版 show 规则的两个警告(对照 `check_show_page_rule` 与
/// `check_show_par_set_block`)。
fn check_show_rules(engine: &mut Engine, recipe: &typst::foundations::Recipe) {
    if let Some(Selector::Elem(elem, _)) = recipe.selector()
        && *elem == typst::foundations::Element::of::<typst::layout::PageElem>()
    {
        engine.sink.warn(
            SourceDiagnostic::warning(
                recipe.span(),
                typst::foundations::Str::from(
                    "`show page` is not supported and has no effect",
                ),
            )
            .with_hint("customize pages with `set page(..)` instead"),
        );
    }
    if let Some(Selector::Elem(elem, _)) = recipe.selector()
        && *elem == typst::foundations::Element::of::<typst::model::ParElem>()
        && let Transformation::Style(styles) = recipe.transform()
    {
        let chain = typst::foundations::StyleChain::new(styles);
        if chain.has(typst::layout::BlockElem::above)
            || chain.has(typst::layout::BlockElem::below)
        {
            engine.sink.warn(
                SourceDiagnostic::warning(
                    recipe.span(),
                    typst::foundations::Str::from(
                        "`show par: set block(spacing: ..)` has no effect anymore",
                    ),
                )
                .with_hint("write `set par(spacing: ..)` instead")
                .with_hint(
                    "this is specific to paragraphs as they are not considered blocks anymore",
                ),
            );
        }
    }
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
    // 定位缓存(**原始片段文本**为键,命中零格式化)。context 体是
    // **表达式**(代码块或单一表达式),用 `parse_code` 取 Code 根下
    // 的首个表达式——不能用 `#let` 包装源(其根下首个可 cast 节点是
    // LetBinding 本身:求值会执行体,但返回值是 let 的 none,
    // ContextElem 显示 none = 空内容,cetz 画布因此整体消失)。
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, typst_syntax::SyntaxNode>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    let node = if let Some(hit) = CACHE.read().unwrap().get(body_src) {
        hit.clone()
    } else {
        let text = body_src.to_string();
        let root = typst_syntax::parse_code(&text);
        let node = root
            .children()
            .find(|child| child.cast::<ast::Expr>().is_some())
            .unwrap_or_else(|| panic!("context body parses to an expression: {text:?}"))
            .clone();
        let n = CLOSURE_SOURCES.read().unwrap().len();
        let id = typst::syntax::RootedPath::new(
            typst::syntax::VirtualRoot::Project,
            typst::syntax::VirtualPath::new(format!("__trust_closure_{n}.typ"))
                .expect("valid closure path"),
        )
        .intern();
        CLOSURE_SOURCES.write().unwrap().push((id, text.clone()));
        CACHE
            .write()
            .unwrap()
            .insert(body_src.to_string(), node.clone());
        node
    };
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
