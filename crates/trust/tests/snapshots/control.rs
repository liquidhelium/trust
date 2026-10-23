// 由 trust 从 `control` 生成,勿手改。

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
static S0: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288344138459499));
static S1: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288375944433528));
static S2: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288378614278059));
static S3: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288333923402147));
static S4: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288235487394932));
static S5: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288287491323272));
static S6: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 288027471681572));
static S7: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span("main.typ", 290479085446172));
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
    V0_pick: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = std::sync::Arc::new(__State::default());
    let _ = context;
    { let v = Value::Func(Func::from(&D_V0_pick)); rt::fill(&__st.V0_pick, v.clone())?; scope.define("pick", v); }
    Ok(Module::new("control", scope).with_content({ let __items = vec![(rt::embed(Value::None)).into_value()]; rt::attach_labels(engine, __items) }).with_file_id(file_span().id().expect("file span id")))
}

/// 一行接入:驱动 module_init 求值并返回模块(注入宿主 library 用
/// `rt::bind_module`;测试世界 InjectWorld::init_native_module 已内置)。
pub fn install(world: comemo::Tracked<dyn typst::World + '_>, library: &typst::utils::LazyHash<typst::Library>) -> SourceResult<Module> {
    rt::init_module(world, library, module_init)
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_pick(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut items = rt::param_pos(__args, "items", span)?;
    rt::finish(__args, span)?;
    Ok({ let mut __out = Value::None;
    let mut out = { let mut __a: Vec<Value> = Vec::new(); rt::array(__a).into_value() };
        __out = rt::join(__out, { let mut __out = Value::None;
  for __it1 in rt::iter_values(&(items.clone()), *S4)? {
      let mut x = __it1;

      let __v = { let mut __out = Value::None;
    __out = rt::join(__out, if rt::boolean(Value::Bool(matches!(ops::compare(&(x.clone()), &(Value::Int(0))).at(*S0)?, std::cmp::Ordering::Greater)), *S3)? { { let mut __out = Value::None;
    __out = rt::join(__out, { rt::check_call_depth(engine, *S1)?; let __mut_args = { let mut __call_args_v = Args::new(*S1, std::iter::empty::<Value>()); __call_args_v.push(*S2, x.clone()); __call_args_v }; let __place = rt::place_mut(&mut out, &[], *S1)?; rt::method_mut(engine, __place, "push", __mut_args, *S1)? }, *S1)?;
    let _ = &__out;
    __out
} } else { Value::None }, *S3)?;
    let _ = &__out;
    __out
};
      __out = rt::join(__out, __v, *S5)?;
  }
  __out }, *S6)?;
    __out = rt::join(__out, out.clone(), *S7)?;
    let _ = &__out;
    __out
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
