// 由 trust 从 `function` 生成,勿手改。

use std::sync::LazyLock;
use comemo::Tracked;
use typst::diag::{At, SourceResult};
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
    scope.define("add", rt::slot(&V0_add));
    Module::new("function", scope)
}
/// 引擎值补注册:宿主在具备引擎时调用,把顶层引擎值合并进模块作用域。
#[allow(non_snake_case, unused_variables)]
pub fn module_init(engine: &mut Engine, context: Tracked<Context>, scope: &mut Scope) -> SourceResult<()> {
    let _ = engine;
    Ok(())
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_add(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut a = rt::param(__args, "a", span)?;
    let mut b = rt::param(__args, "b", span)?;
    rt::finish(__args, span)?;
    Ok(ops::add(a.clone(), b.clone()).at(SPAN)?)
}

#[allow(non_snake_case, dead_code)]
fn f_v0_add_params() -> Vec<NativeParamInfo> {
    vec![
        NativeParamInfo { name: "a", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
        NativeParamInfo { name: "b", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
    ]
}
#[allow(non_snake_case, dead_code)]
static D_V0_add: NativeFuncData = NativeFuncData {
    function: NativeFuncPtr(&f_v0_add),
    name: "add",
    title: "add",
    docs: "",
    def_site: None,
    keywords: &[],
    contextual: false,
    scope: LazyLock::new(&rt::empty_scope),
    params: LazyLock::new(&f_v0_add_params),
    returns: LazyLock::new(&rt::any_cast_info),
};

#[allow(non_snake_case, dead_code)]
fn V0_add_init() -> Value {
    Value::Func(Func::from(&D_V0_add))
}
#[allow(non_snake_case, dead_code)]
static V0_add: rt::Slot = LazyLock::new(&V0_add_init);
