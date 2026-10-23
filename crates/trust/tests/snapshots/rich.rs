// 由 trust 从 `rich` 生成,勿手改。

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
static S0: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 289662109446693));
static S1: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 314764805686989));
static S2: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 329176865792586));
static S3: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 329312190300619));
static S4: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 329532092626172));
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
    V0_scale: std::sync::OnceLock<Value>,
    DEF_V0_scale_k: std::sync::OnceLock<Value>,
    V1_base: std::sync::OnceLock<Value>,
    V2_wrap: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = std::sync::Arc::new(__State::default());
    let _ = context;
    { let v = Value::Func(rt::native_closure(&D_V0_scale, "(x, k: 2)", vec![{ let v = Value::Int(2); rt::fill(&__st.DEF_V0_scale_k, v.clone())?; v }], &rt::StateSlot::new(__st.clone()))); rt::fill(&__st.V0_scale, v.clone())?; scope.define("scale", v); }
    { let v = Value::Int(10);
        let mut V1_base = Value::None;
{ let __p = rt::place_mut(&mut V1_base, &[], *S1)?; *__p = v.clone(); }
        rt::fill(&__st.V1_base, V1_base.clone())?; scope.define("base", V1_base.clone());
    }
    { let v = Value::Func(Func::from(&D_V2_wrap)); rt::fill(&__st.V2_wrap, v.clone())?; scope.define("wrap", v); }
    Ok(Module::new("rich", scope).with_content({ let __items = vec![(rt::embed(Value::None)).into_value(), (rt::embed(Value::None)).into_value(), (rt::embed(Value::None)).into_value()]; rt::attach_labels(engine, __items) }).with_file_id(file_span().id().expect("file span id")))
}

/// 一行接入:驱动 module_init 求值并返回模块(注入宿主 library 用
/// `rt::bind_module`;测试世界 InjectWorld::init_native_module 已内置)。
pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {
    rt::init_module(world, library, module_init)
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_scale(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let __st = rt::take_state::<__State>(__args)?;
    let mut x = rt::param_pos(__args, "x", span)?;
    let mut k = match __args.named::<Value>("k")? {
        Some(value) => value,
        None => rt::read(&__st.DEF_V0_scale_k)?,
    };
    rt::finish(__args, span)?;
    Ok(ops::mul(x.clone(), k.clone()).at(*S0)?)
}

#[allow(non_snake_case, dead_code)]
fn f_v0_scale_params() -> Vec<NativeParamInfo> {
    vec![
        NativeParamInfo { name: "x", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: true, named: false, variadic: false, required: true, settable: false },
        NativeParamInfo { name: "k", docs: "", def_site: None, input: CastInfo::Any, default: None, positional: false, named: true, variadic: false, required: false, settable: false },
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

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v2_wrap(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let __n_pos_args = __args.to_pos().len();
    let mut f = rt::param_pos(__args, "f", span)?;
    let __sink_pos_0 = __args.consume(__n_pos_args.saturating_sub(1))?;
    let mut rest = Value::Args(__args.take());
    if let Value::Args(__sink_inner) = &mut rest { __sink_inner.items.extend(__sink_pos_0.into_iter()); }
    rt::finish(__args, span)?;
    Ok({ rt::check_call_depth(engine, *S2)?; let __callee = rt::callee(f.clone(), *S3)?; let __call_args = { let mut __call_args_v = Args::new(*S2, std::iter::empty::<Value>()); rt::spread_args(&mut __call_args_v, *S4, rest.clone())?; __call_args_v }; rt::call_with(__callee, None, *S2, __call_args, engine, context)? })
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
