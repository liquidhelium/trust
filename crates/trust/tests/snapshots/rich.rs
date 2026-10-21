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

const fn nz(n: u64) -> std::num::NonZeroU64 {
    match std::num::NonZeroU64::new(n) {
        Some(v) => v,
        None => panic!("span 常量非法:0"),
    }
}
const S0: Span = Span::from_raw(nz(289662109446693));
const S1: Span = Span::from_raw(nz(329176865792586));
const S2: Span = Span::from_raw(nz(329312190300619));
const S3: Span = Span::from_raw(nz(329532092626172));
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
    V0_scale: std::sync::OnceLock<Value>,
    G_V0_scale_x: std::sync::OnceLock<Value>,
    G_V0_scale_k: std::sync::OnceLock<Value>,
    DEF_V0_scale_k: std::sync::OnceLock<Value>,
    V1_base: std::sync::OnceLock<Value>,
    V2_wrap: std::sync::OnceLock<Value>,
    G_V2_wrap_f: std::sync::OnceLock<Value>,
    G_V2_wrap_rest: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = std::sync::Arc::new(__State::default());
    let _ = context;
    if let Some(v) = rt::global_opt(engine, "x")? { rt::fill(&__st.G_V0_scale_x, v)?; }
    if let Some(v) = rt::global_opt(engine, "k")? { rt::fill(&__st.G_V0_scale_k, v)?; }
    { let v = Value::Func(rt::native_closure(&D_V0_scale, "(x, k: 2)", vec![{ let v = Value::Int(2); rt::fill(&__st.DEF_V0_scale_k, v.clone())?; v }], &rt::StateSlot::new(__st.clone()))); rt::fill(&__st.V0_scale, v.clone())?; scope.define("scale", v); }
    { let v = Value::Int(10);
        let mut V1_base = Value::None;
V1_base = v.clone();
        rt::fill(&__st.V1_base, V1_base.clone())?; scope.define("base", V1_base.clone());
    }
    if let Some(v) = rt::global_opt(engine, "f")? { rt::fill(&__st.G_V2_wrap_f, v)?; }
    if let Some(v) = rt::global_opt(engine, "rest")? { rt::fill(&__st.G_V2_wrap_rest, v)?; }
    { let v = Value::Func(Func::from(&D_V2_wrap)); rt::fill(&__st.V2_wrap, v.clone())?; scope.define("wrap", v); }
    Ok(Module::new("rich", scope).with_content(rt::content_sequence(vec![rt::embed(Value::None), rt::embed(Value::None), rt::embed(Value::None)])).with_file_id(file_span().id().expect("file span id")))
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
    Ok(ops::mul(x.clone(), k.clone()).at(S0)?)
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
    let mut f = rt::param_pos(__args, "f", span)?;
    let mut rest = Value::Args(__args.take());
    rt::finish(__args, span)?;
    Ok({ rt::check_call_depth(engine, S1)?; let __callee = rt::callee(&(f.clone()), S2)?; let __call_args = { let mut __call_args_v = Args::new(S1, std::iter::empty::<Value>()); rt::spread_args(&mut __call_args_v, S3, rest.clone())?; __call_args_v }; rt::call_with(__callee, None, S1, __call_args, engine, context)? })
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
