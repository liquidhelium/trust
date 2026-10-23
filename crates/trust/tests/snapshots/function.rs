// 由 trust 从 `function` 生成,勿手改。

use std::sync::LazyLock;
use comemo::Tracked;
use typst::diag::{At, SourceResult, bail};
use ecow::eco_vec;
use typst::engine::Engine;
use typst::foundations::{
    Args, CastInfo, Content, Context, Dict, Func, IntoValue, Module, NativeFuncData, NativeFuncPtr,
    NativeParamInfo, Scope, Value, ops,
};
use typst::syntax::Span;
use typst::syntax::ast::Unit;
use trust_runtime as rt;

/// 兜底错误 span(未被源站点覆盖处;差分对拍只比较值)。
const SPAN: Span = Span::detached();
static S0: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 300716430196727));
/// 当前文件的锚点 span(带 FileId;plugin 等按 span 定位的全局函数依赖它)。
#[allow(non_snake_case, dead_code)]
pub(crate) fn file_span() -> Span {
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
    let __st = std::sync::Arc::new(__State::default());
    let _ = context;
    { let v = Value::Func(Func::from(&D_V0_add)); rt::fill(&__st.V0_add, v.clone())?; scope.define("add", v); }
    Ok(Module::new("function", scope).with_content({ let __items = vec![(rt::embed(Value::None)).into_value()]; rt::attach_labels(engine, __items) }).with_file_id(file_span().id().expect("file span id")))
}

/// 一行接入:驱动 module_init 求值并返回模块(注入宿主 library 用
/// `rt::bind_module`;测试世界 InjectWorld::init_native_module 已内置)。
pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {
    rt::init_module(world, library, module_init)
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_add(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut a = rt::param_pos(__args, "a", span)?;
    let mut b = rt::param_pos(__args, "b", span)?;
    rt::finish(__args, span)?;
    Ok(ops::add(a.clone(), b.clone()).at(*S0)?)
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
