// 由 trust 从 `control` 生成,勿手改。

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
    scope.define("pick", rt::slot(&V0_pick));
    Module::new("control", scope)
}
/// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。
#[allow(non_snake_case, unused_variables)]
pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {
    let _ = engine;
    Ok(())
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_pick(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut items = rt::param(__args, "items", span)?;
    rt::finish(__args, span)?;
    Ok({ let mut __out = Value::None;
    let mut out = { let mut __a: Vec<Value> = Vec::new(); rt::array(__a).into_value() };
        __out = rt::join(__out, { let mut __out = Value::None;
  for __it1 in rt::iter_values(&(items.clone()), SPAN)? {
      let mut x = { __it1.clone() };

      let __v = { let mut __out = Value::None;
    let _ = __out;
    if rt::boolean(Value::Bool(matches!(ops::compare(&(x.clone()), &(Value::Int(0))).at(SPAN)?, std::cmp::Ordering::Greater)), SPAN)? { { let mut __out = Value::None;
    let _ = __out;
    { let __mut_args = { let mut __call_args_v = Args::new(SPAN, std::iter::empty::<Value>()); __call_args_v.push(SPAN, x.clone()); __call_args_v }; rt::method_mut(&mut out, "push", __mut_args, SPAN)? }
} } else { Value::None }
};
      __out = rt::join(__out, __v, SPAN)?;
  }
  __out }, SPAN)?;
    let _ = __out;
    out.clone()
})
}

#[allow(non_snake_case, dead_code)]
fn f_v0_pick_params() -> Vec<NativeParamInfo> {
    vec![
        NativeParamInfo { name: "items", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
    ]
}
#[allow(non_snake_case, dead_code)]
static D_V0_pick: NativeFuncData = NativeFuncData {
    function: NativeFuncPtr(&f_v0_pick),
    name: "pick",
    title: "pick",
    docs: "",
    def_site: None,
    keywords: &[],
    contextual: false,
    scope: LazyLock::new(&rt::empty_scope),
    params: LazyLock::new(&f_v0_pick_params),
    returns: LazyLock::new(&rt::any_cast_info),
};

#[allow(non_snake_case, dead_code)]
fn V0_pick_init() -> Value {
    Value::Func(Func::from(&D_V0_pick))
}
#[allow(non_snake_case, dead_code)]
static V0_pick: rt::Slot = LazyLock::new(&V0_pick_init);
