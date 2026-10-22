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

const fn nz(n: u64) -> std::num::NonZeroU64 {
    match std::num::NonZeroU64::new(n) {
        Some(v) => v,
        None => panic!("span 常量非法:0"),
    }
}
const S0: Span = Span::from_raw(nz(300716430196727));
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
    G_V0_add_a: std::sync::OnceLock<Value>,
    G_V0_add_b: std::sync::OnceLock<Value>,
}

/// 模块求值的镜像:宿主每次构建(World)调用一次,按源序求值全部顶层绑定。
pub fn module_init(engine: &mut Engine, context: Tracked<Context>) -> SourceResult<Module> {
    let mut scope = Scope::new();
    let __st = std::sync::Arc::new(__State::default());
    let _ = context;
    if let Some(v) = rt::capture_opt(engine, "a")? { rt::fill(&__st.G_V0_add_a, v)?; }
    if let Some(v) = rt::capture_opt(engine, "b")? { rt::fill(&__st.G_V0_add_b, v)?; }
    { let v = Value::Func(Func::from(&D_V0_add)); rt::fill(&__st.V0_add, v.clone())?; scope.define("add", v); }
    Ok(Module::new("function", scope).with_content(rt::content_sequence(vec![rt::embed(Value::None)])).with_file_id(file_span().id().expect("file span id")))
}

#[allow(non_snake_case, dead_code, unused_variables)]
fn f_v0_add(engine: &mut Engine, context: Tracked<Context>, __args: &mut Args) -> SourceResult<Value> {
    let span = __args.span;
    let mut a = rt::param_pos(__args, "a", span)?;
    let mut b = rt::param_pos(__args, "b", span)?;
    rt::finish(__args, span)?;
    Ok(ops::add(a.clone(), b.clone()).at(S0)?)
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
