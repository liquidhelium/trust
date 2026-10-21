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
    V0_scale: std::sync::OnceLock<Value>,
    V1_base: std::sync::OnceLock<Value>,
    V2_wrap: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = rt::state_get_or_insert::<__State>(engine, __State::default);
    let _ = context;
    { let v = Value::Func(Func::from(&D_V0_scale)); rt::fill(&__st.V0_scale, v.clone())?; scope.define("scale", v); }
    { let v = Value::Int(10);
        let mut V1_base = Value::None;
V1_base = v.clone();
        rt::fill(&__st.V1_base, V1_base.clone())?; scope.define("base", V1_base.clone());
    }
    { let v = Value::Func(Func::from(&D_V2_wrap)); rt::fill(&__st.V2_wrap, v.clone())?; scope.define("wrap", v); }
    Ok(Module::new("rich", scope).with_content(rt::content_sequence(vec![rt::embed(Value::None), rt::embed(Value::None), rt::embed(Value::None)])))
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
    Ok({ let __callee = f.clone(); let __call_args = { let mut __fs = file_span(); let mut __call_args_v = Args::new(__fs, std::iter::empty::<Value>()); rt::spread_args(&mut __call_args_v, __fs, rest.clone())?; __call_args_v }; rt::call(&__callee, engine, context, SPAN, __call_args)? })
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
