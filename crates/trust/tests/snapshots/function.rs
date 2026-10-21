// 由 trust 从 `function` 生成,勿手改。

use std::sync::LazyLock;
use comemo::Tracked;
use typst::diag::{At, SourceResult, bail};
use ecow::eco_vec;
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
/// 当前文件的锚点 span(带 FileId;plugin 等按 span 定位的全局函数依赖它)。
#[allow(non_snake_case, dead_code)]
fn file_span() -> Span {
    static CELL: std::sync::OnceLock<Span> = std::sync::OnceLock::new();
    *CELL.get_or_init(|| {
        let id = typst::syntax::RootedPath::new(
            typst::syntax::VirtualRoot::Project,
            typst::syntax::VirtualPath::new("main.typ").expect("path"),
        )
        .intern();
        typst::syntax::Source::new(id, String::new()).root().span()
    })
}

/// 本模块的求值状态:每个顶层绑定一个写一次槽(每构建一份)。
#[derive(Default)]
struct __State {
    V0_add: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = rt::state_get_or_insert::<__State>(engine, __State::default);
    let _ = context;
    { let v = Value::Func(Func::from(&D_V0_add)); rt::fill(&__st.V0_add, v.clone())?; scope.define("add", v); }
    Ok(Module::new("function", scope))
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_add(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut a = rt::param_pos(__args, "a", span)?;
    let mut b = rt::param_pos(__args, "b", span)?;
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
