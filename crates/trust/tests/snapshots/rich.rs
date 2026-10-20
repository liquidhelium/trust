// 由 trust 从 `rich` 生成,勿手改。

use std::sync::LazyLock;
use comemo::Tracked;
use typst::diag::{At, SourceResult, bail};
use typst::engine::Engine;
use typst::foundations::{
    Args, CastInfo, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,
    NativeParamInfo, Scope, Value, ops,
};
use typst::syntax::Span;
use typst::syntax::ast::Unit;
use trust_runtime as rt;

/// 生成的错误 span:detached(差分对拍只比较值)。
const SPAN: Span = Span::detached();

/// 构建该模块(名字与导出面照抄源文件)。
pub fn module() -> Module {
    let mut scope = Scope::new();
    scope.define("scale", rt::slot(&V0_scale));
    scope.define("base", rt::slot(&V1_base));
    scope.define("wrap", rt::slot(&V2_wrap));
    Module::new("rich", scope)
}
/// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。
#[allow(non_snake_case, unused_variables)]
pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {
    let _ = engine;
    Ok(())
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_scale(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut x = rt::param_pos(__args, "x", span)?;
    let mut k = match __args.named::<Value>("k")? {
        Some(value) => value,
        None => Value::Int(2),
    };
    rt::finish(__args, span)?;
    Ok(ops::mul(x.clone(), k.clone()).at(SPAN)?)
}

#[allow(non_snake_case, dead_code)]
fn f_v0_scale_params() -> Vec<NativeParamInfo> {
    vec![
        NativeParamInfo { name: "x", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
        NativeParamInfo { name: "k", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: true, variadic: false, required: true, settable: false },
    ]
}
#[allow(non_snake_case, dead_code)]
static D_V0_scale: NativeFuncData = NativeFuncData {
    function: NativeFuncPtr(&f_v0_scale),
    name: "scale",
    title: "scale",
    docs: "",
    def_site: None,
    keywords: &[],
    contextual: false,
    scope: LazyLock::new(&rt::empty_scope),
    params: LazyLock::new(&f_v0_scale_params),
    returns: LazyLock::new(&rt::any_cast_info),
};

#[allow(non_snake_case, dead_code)]
fn V0_scale_init() -> Value {
    Value::Func(Func::from(&D_V0_scale))
}
#[allow(non_snake_case, dead_code)]
static V0_scale: rt::Slot = LazyLock::new(&V0_scale_init);

#[allow(non_snake_case, dead_code)]
fn V1_base_init() -> Value {
    Value::Int(10)
}
#[allow(non_snake_case, dead_code)]
static V1_base: rt::Slot = LazyLock::new(&V1_base_init);

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v2_wrap(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut f = rt::param_pos(__args, "f", span)?;
    let mut rest = Value::Args(__args.take());
    rt::finish(__args, span)?;
    Ok({ let __callee = f.clone(); let __call_args = { let mut __call_args_v = Args::new(SPAN, std::iter::empty::<Value>()); rt::spread_args(&mut __call_args_v, SPAN, rest.clone())?; __call_args_v }; rt::call(&__callee, engine, context, SPAN, __call_args)? })
}

#[allow(non_snake_case, dead_code)]
fn f_v2_wrap_params() -> Vec<NativeParamInfo> {
    vec![
        NativeParamInfo { name: "f", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
        NativeParamInfo { name: "args", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: true, required: false, settable: false },
    ]
}
#[allow(non_snake_case, dead_code)]
static D_V2_wrap: NativeFuncData = NativeFuncData {
    function: NativeFuncPtr(&f_v2_wrap),
    name: "wrap",
    title: "wrap",
    docs: "",
    def_site: None,
    keywords: &[],
    contextual: false,
    scope: LazyLock::new(&rt::empty_scope),
    params: LazyLock::new(&f_v2_wrap_params),
    returns: LazyLock::new(&rt::any_cast_info),
};

#[allow(non_snake_case, dead_code)]
fn V2_wrap_init() -> Value {
    Value::Func(Func::from(&D_V2_wrap))
}
#[allow(non_snake_case, dead_code)]
static V2_wrap: rt::Slot = LazyLock::new(&V2_wrap_init);
