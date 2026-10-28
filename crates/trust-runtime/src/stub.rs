//! The `stub` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, destruct::*, eval::*, host::*, lookup::*,
    macros::*, slots::*, styles::*,
};

use comemo::{Track, Tracked, TrackedMut};
use ecow::{EcoString, EcoVec, eco_format, eco_vec};
use typst::Library;
use typst::World;
use typst::diag::{At, SourceResult, bail};
use typst::diag::{Hint, SourceDiagnostic, Trace};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{
    Arg, Args, Array, Binding, Capturer, Closure, ClosureNode, Content, Context, Func, IntoValue,
    Module, Scope, Scopes, Selector, Str, Transformation, Value, ops,
};
use typst::introspection::{EmptyIntrospector, Introspector};
use typst::syntax::ast::{self, AstNode};
use typst::syntax::{Span, Spanned, SyntaxKind};
use typst::utils::{LazyHash, Protected};

/// `Routines.eval_closure` dispatch with a stub direct call. When the
/// closure is one of our stubs (captured scope carries `__trust_routine`),
/// it goes through [`eval_stub_native`] — **comemo accounting is kept**
/// (the memo key is isomorphic to typst-eval's `eval_closure`: same types,
/// same order, same dedup granularity); only "interpreting the stub body"
/// is replaced by a direct call into the native body. Dropping memoization
/// entirely (a bare direct call) would also lose comemo's parallel
/// deduplication (concurrent identical calls would each re-execute) and
/// drops cold-compile throughput to 0.43× — the accounting is load-bearing,
/// not overhead. Non-stub closures fall through unchanged to typst-eval's
/// default routine.
// The engine parts arrive as separate tracked parameters, mirroring
// typst-eval's own closure-eval entry signature.
#[allow(clippy::too_many_arguments)]
pub fn trust_eval_closure(
    func: &Func,
    closure: &LazyHash<Closure>,
    world: Tracked<'_, dyn World + '_>,
    library: &LazyHash<Library>,
    introspector: Tracked<'_, dyn Introspector + '_>,
    traced: Tracked<'_, Traced>,
    sink: TrackedMut<'_, Sink>,
    route: Tracked<'_, Route<'_>>,
    context: Tracked<'_, Context>,
    args: Args,
) -> SourceResult<Value> {
    if stub_routine(closure).is_some() {
        return eval_stub_native(
            func,
            closure,
            world,
            library,
            introspector,
            traced,
            sink,
            route,
            context,
            args,
        );
    }
    typst_eval::eval_closure(
        func,
        closure,
        world,
        library,
        introspector,
        traced,
        sink,
        route,
        context,
        args,
    )
}

/// Fetches the direct-call routine from a stub's captured scope (the hit
/// test; non-stub closures pay a single scope lookup). **Borrows
/// throughout** — cloning the TrustRoutine (with its defaults Vec) would be
/// pure overhead on the hot path: the check only needs a reference, and so
/// does the direct-call body (the state handle is cloned once as an Arc
/// only when inserting the argument, as the protocol requires).
fn stub_routine(closure: &LazyHash<Closure>) -> Option<(&TrustRoutine, Option<&StateSlot>)> {
    let routine = closure.captured.get("__trust_routine")?;
    let state = closure.captured.get("__trust_state");
    let Value::Dyn(routine) = routine.read() else {
        return None;
    };
    let cell = routine.downcast::<TrustRoutine>()?;
    let slot = match state {
        Some(state) => {
            let Value::Dyn(state) = state.read() else {
                return None;
            };
            Some(state.downcast::<StateSlot>()?)
        }
        None => None,
    };
    Some((cell, slot))
}

/// Memoized stub direct-call body (input set identical to typst-eval's
/// `eval_closure`, so the comemo key is isomorphic — same types, same
/// order; internally performs default injection + state-handle insertion +
/// the native body call in one step, skipping the stub body's
/// interpretation and inner dispatch).
#[allow(clippy::too_many_arguments)]
#[comemo::memoize]
fn eval_stub_native(
    func: &Func,
    closure: &LazyHash<Closure>,
    world: Tracked<'_, dyn World + '_>,
    library: &LazyHash<Library>,
    introspector: Tracked<'_, dyn Introspector + '_>,
    traced: Tracked<'_, Traced>,
    sink: TrackedMut<'_, Sink>,
    route: Tracked<'_, Route<'_>>,
    context: Tracked<'_, Context>,
    mut args: Args,
) -> SourceResult<Value> {
    let _ = func;
    let Some((cell, slot)) = stub_routine(closure) else {
        return Err(eco_vec![SourceDiagnostic::error(
            Span::detached(),
            Str::from("stub direct call missing routine marker (generator bug)"),
        )]);
    };
    let mut engine = Engine {
        library,
        world,
        introspector: Protected::from_raw(introspector),
        traced,
        sink,
        route: Route::extend(route),
    };
    // Named-argument default injection (mirrors eval_closure's
    // default.clone() at binding time): only fills in items the caller did
    // not provide, and inserts them **before** the caller's arguments —
    // `Args::named`'s "last one wins for the same name" semantics keeps the
    // caller's value in effect. Positional binding (eat skips named items)
    // is unaffected by the insertion position.
    for (name, value) in &cell.defaults {
        if !args
            .items
            .iter()
            .any(|arg| arg.name.as_deref() == Some(name.as_str()))
        {
            args.items.insert(
                0,
                Arg {
                    span: args.span,
                    name: Some(name.clone()),
                    value: Spanned::new(value.clone(), args.span),
                },
            );
        }
    }
    if let Some(slot) = slot {
        args.insert(
            0,
            args.span,
            Value::Dyn(typst::foundations::Dynamic::new(slot.clone())),
        );
    }
    (cell.f)(&mut engine, context, &mut args)
}

/// Swaps `Library.routines.eval_closure` for [`trust_eval_closure`].
/// The routines table is a `&'static` shared reference and cannot be edited
/// in place (that would affect every other Library in the process —
/// including the comparison side): copy it, replace just that pointer, and
/// `Box::leak` it as this crate's static. `Routines`'s Hash is an empty
/// impl, so the swap does not affect the LazyHash key. Only replaces when
/// the current routine is typst-eval's default (a host's custom routine is
/// never overridden; repeated installs are idempotent).
pub(crate) fn install_stub_fast_path(library: &mut Library) {
    static FAST: std::sync::OnceLock<&'static typst::routines::Routines> =
        std::sync::OnceLock::new();
    let current = library.routines;
    if std::ptr::eq(
        current.eval_closure as *const (),
        trust_eval_closure as *const (),
    ) {
        return; // already installed (repeated install)
    }
    if !std::ptr::eq(
        current.eval_closure as *const (),
        typst_eval::eval_closure as *const (),
    ) {
        return; // host-defined routine: respect it, don't override
    }
    let fast = FAST.get_or_init(|| {
        Box::leak(Box::new(typst::routines::Routines {
            eval_closure: trust_eval_closure,
            ..*current
        }))
    });
    library.routines = fast;
}

/// Builds a closure value from a source snippet: the node is parsed by the
/// official parser, defaults are passed in after the generated code
/// evaluates them at creation time, and captured carries the current values
/// of the outer bindings.
///
/// The closure body is interpreted by typst-eval — the fidelity path for
/// v1; closure parameter defaults are evaluated the same number of times as
/// in the original (at creation time).
/// Registry of virtual sources for interpreted closures: FileId → snippet
/// source (for diagnostic locations; the host looks it up via
/// [`closure_source`] when printing errors).
pub(crate) static CLOSURE_SOURCES: std::sync::RwLock<Vec<(typst::syntax::FileId, String)>> =
    std::sync::RwLock::new(Vec::new());

/// Looks up the source of a virtual closure file (`None` if absent).
pub fn closure_source(id: typst::syntax::FileId) -> Option<String> {
    CLOSURE_SOURCES
        .read()
        .unwrap()
        .iter()
        .find(|(fid, _)| *fid == id)
        .map(|(_, src)| src.clone())
}

/// One-shot parse of an interpreted snippet (`closure`/`native_closure`),
/// **without a global cache**: meant as the initializer of a per-call-site
/// `static LazyLock` in generated code — the static itself is the cache, so
/// the hot path takes no lock and does no lookup. Mirrors native closure
/// creation — `Eval for ast::Closure`'s node is a reference into the source
/// file's AST (an Arc inside SyntaxNode), with zero parse and zero lookup
/// at creation time; here the snippet is located once (parse + find the
/// Closure node) and later creations just clone the node reference — the
/// same "locate the AST once, rebuild values cheaply" shape as native. The
/// snippet is also registered in [`CLOSURE_SOURCES`] (diagnostic lookup,
/// cold path; the virtual file lets spans resolve to snippet lines).
pub fn parse_closure_node(src: &str) -> (typst_syntax::SyntaxNode, usize) {
    let text = wrap_closure_source(src);
    let id = register_closure_source(text.clone());
    let source = typst::syntax::Source::new(id, text.clone());
    locate_closure(&source, &text)
}

fn cached_closure_node(src: &str) -> (typst_syntax::SyntaxNode, usize) {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, (typst_syntax::SyntaxNode, usize)>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(hit) = CACHE.read().unwrap().get(src) {
        return hit.clone();
    }
    let text = wrap_closure_source(src);
    let id = register_closure_source(text.clone());
    let source = typst::syntax::Source::new(id, text.clone());
    let pair = locate_closure(&source, &text);
    CACHE
        .write()
        .unwrap()
        .insert(src.to_string(), pair.clone());
    pair
}

/// Wraps a raw snippet into parseable markup source. Valid markup forms: a
/// named binding (`let f(x) = ..`) embeds directly; anything else (a bare
/// closure / context body) is wrapped in parentheses — markup's
/// `#let x = expr` init ends **at the end of the line**, so a multi-line
/// bare closure body would be truncated to an empty body; the parentheses
/// force a code context that spans lines.
fn wrap_closure_source(src: &str) -> String {
    let trimmed = src.trim_start();
    if trimmed.starts_with("let ") {
        format!("#{trimmed}")
    } else {
        format!("#let __trust_closure = ({trimmed})")
    }
}

/// Allocates a virtual FileId for a closure snippet and registers the
/// snippet text for diagnostic lookup. One write-lock section covers the
/// id allocation and the push, so concurrent initialization cannot mint
/// two ids for the same virtual path.
fn register_closure_source(text: String) -> typst::syntax::FileId {
    let mut guard = CLOSURE_SOURCES.write().unwrap();
    let id = typst::syntax::RootedPath::new(
        typst::syntax::VirtualRoot::Project,
        typst::syntax::VirtualPath::new(format!("__trust_closure_{}.typ", guard.len()))
            .expect("valid closure path"),
    )
    .intern();
    guard.push((id, text));
    id
}

/// Parses the wrapped snippet and locates its Closure node — a bare
/// closure `(x) => ..` or a named binding `f(x) = ..` (a LetBinding
/// wrapping the Closure; the named form keeps its name, so recursive
/// self-reference works) — returning the node and its positional-parameter
/// count.
fn locate_closure(
    source: &typst::syntax::Source,
    text: &str,
) -> (typst_syntax::SyntaxNode, usize) {
    let node = find_closure_node(source.root())
        .unwrap_or_else(|| panic!("closure source parses to a closure: {text:?}"))
        .clone();
    let num_pos_params = node
        .cast::<ast::Closure>()
        .expect("closure node casts")
        .params()
        .children()
        .filter(|p| matches!(p, ast::Param::Pos(_)))
        .count();
    (node, num_pos_params)
}

/// Materializes not-yet-captured names at creation time: global scope
/// first, then the math scope (mirrors native closure creation —
/// CapturesVisitor resolves against the scopes at the creation point, where
/// both `get` and `get_in_math` carry the library base; an interpreted
/// snippet's captured scope has no base, which this restores), with the
/// `std` special case as in [`global_opt`]. A name still not found is
/// simply not captured (call time then reports unknown variable, same as
/// native).
pub(crate) fn resolve_pending(
    engine: &mut Engine,
    captured: &mut Vec<(&'static str, Value)>,
    pending: &[(&'static str, bool)],
) {
    for (name, in_math) in pending {
        if captured.iter().any(|(n, _)| n == name) {
            continue;
        }
        let found = if *in_math {
            capture_math_opt(engine, name)
        } else {
            capture_code_opt(engine, name)
        };
        if let Ok(Some(value)) = found {
            captured.push((name, value));
        }
    }
}

pub fn closure(
    src: &str,
    defaults: Vec<Value>,
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
) -> Func {
    let (node, num_pos_params) = cached_closure_node(src);
    let mut captured: Vec<(&'static str, Value)> = captured.to_vec();
    resolve_pending(engine, &mut captured, pending);
    // Probe the debug switch once (previously this ran getenv on **every
    // closure creation** — glibc's getenv also takes the environ lock, pure
    // overhead on the generated side's cold compiles).
    static DEBUG_CAPTURE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let debug = *DEBUG_CAPTURE.get_or_init(|| std::env::var_os("TRUST_DEBUG_CAPTURE").is_some());
    let mut scope = Scope::new();
    for (name, value) in &captured {
        if debug && src.contains("ctx => {") {
            let brief = format!("{value:?}");
            eprintln!("[capture] {name} = {}", &brief[..brief.len().min(120)]);
        }
        // Captured bindings get the read-only mark (mirrors
        // `Binding::capture(Capturer::Function)` on typst closures'
        // captured scope): writes to captures are rejected by typst's
        // evaluator via `Binding::write` with its read-only error.
        scope.bind(
            (*name).into(),
            Binding::detached(value.clone()).capture(Capturer::Function),
        );
    }
    let closure_span = node.span();
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    // Mirrors typst: the closure is spanned at creation (the definition
    // site); error propagation points inside the interpreted body (e.g. the
    // cast in array.any's predicate using test.span()) can now resolve to
    // the virtual source.
    .spanned(closure_span)
}

/// Finds the first Closure node in pre-order.
fn find_closure_node(node: &typst_syntax::SyntaxNode) -> Option<&typst_syntax::SyntaxNode> {
    if node.kind() == SyntaxKind::Closure {
        return Some(node);
    }
    node.children().find_map(find_closure_node)
}

/// Per-call-site static version of interpreted closure creation (the node
/// comes from the call site's static; the pending materialization protocol
/// is as in [`closure`]).
pub fn closure_node_at(
    node: &(typst_syntax::SyntaxNode, usize),
    span: Span,
    base: usize,
    defaults: Vec<Value>,
    captured: &[(&'static str, Value)],
    pending: &[(&'static str, bool)],
    engine: &mut Engine,
) -> Func {
    let (node, num_pos_params) = node.clone();
    // Synthesize real, per-node spans onto the parsed nodes: the bare
    // static has detached spans, and FileId-carrying spans are
    // load-bearing inside typst-eval-interpreted bodies — a dynamic
    // `import "…"` resolves relative to span.id(), and measure() resolves
    // introspections by finding the closest matching element BY SPAN, so
    // collapsing the body onto one span makes every element "match"
    // itself (a measure-loop that must not converge then converges).
    // RangeMapper keeps each node at its own offset in the original file
    // (for named closures the emitted "let " prefix skews positions by
    // its length — harmless, the FileId is what carries semantics).
    let mut node = node;
    match span.id() {
        Some(id) => {
            let len = node.len();
            match typst_syntax::RangeMapper::new(std::iter::once(base..base + len))
                .and_then(|mapper| node.synthesize_mapped(id, &mapper))
            {
                Ok(()) => {}
                Err(_) => node.synthesize(span),
            }
        }
        None => node.synthesize(span),
    }
    let mut captured: Vec<(&'static str, Value)> = captured.to_vec();
    resolve_pending(engine, &mut captured, pending);
    let mut scope = Scope::new();
    for (name, value) in &captured {
        // Same as `closure`: captured bindings get the read-only mark.
        scope.bind(
            (*name).into(),
            Binding::detached(value.clone()).capture(Capturer::Function),
        );
    }
    let closure_span = node.span();
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    .spanned(closure_span)
}

/// Opaque handle for per-build state: wraps an `Arc<dyn Any + Send +
/// Sync>` into typst's value world (`Value::Dyn`) and travels with the
/// function value (mirroring the captured data layout of typst closures);
/// the native body unpacks it from the first positional argument via
/// [`take_state`].
#[derive(Clone)]
pub struct StateSlot {
    /// Monotonic instance id (a global counter): identity semantics.
    /// **Pointer hashing must not be used** — after a slot is freed the
    /// allocator reuses the address (ABA), two distinct instances would
    /// compare equal, their comemo memo keys would collide, and answers
    /// would leak across instances (one instance's cached result returned
    /// for another's call).
    pub(crate) id: u64,
    pub(crate) inner: std::sync::Arc<dyn std::any::Any + Send + Sync>,
}

impl StateSlot {
    /// Wraps a piece of state / a capture tuple.
    pub fn new<T: std::any::Any + Send + Sync>(state: std::sync::Arc<T>) -> Self {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            id: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            inner: state,
        }
    }
}

impl std::fmt::Debug for StateSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("state slot")
    }
}

impl typst::foundations::Repr for StateSlot {
    fn repr(&self) -> EcoString {
        "state slot".into()
    }
}

impl typst::foundations::NativeType for StateSlot {
    const NAME: &'static str = "state";
    fn data() -> &'static typst::foundations::NativeTypeData {
        static DATA: std::sync::OnceLock<typst::foundations::NativeTypeData> =
            std::sync::OnceLock::new();
        DATA.get_or_init(|| typst::foundations::NativeTypeData {
            name: "state",
            long_name: "state",
            title: "State",
            docs: "internal per-build state slot",
            def_site: typst::utils::DefSite {
                path: "trust-runtime",
                key: "StateSlot",
            },
            keywords: &[],
            constructor: std::sync::LazyLock::new(|| None),
            scope: std::sync::LazyLock::new(typst::foundations::Scope::new),
        })
    }
}

impl PartialEq for StateSlot {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl std::hash::Hash for StateSlot {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// Stub direct-call routine: the native-body entry (same type as
/// `NativeFuncData.function`: `&'static dyn Fn`) carried by the third
/// binding `__trust_routine` of a stub's captured scope, plus the
/// named-argument defaults table (parameter name × creation-time
/// materialized value; mirrors typst closures' `closure.defaults`).
/// [`trust_eval_closure`]'s fast path uses this to recognize our stubs,
/// inject defaults for missing named arguments (mirroring `eval_closure`'s
/// `default.clone()` at binding time), and call the native body directly
/// instead of the memoized interpretation.
/// The native-body signature driven by translated closures.
pub type TrustRoutineFn =
    dyn Fn(&mut Engine, Tracked<'_, Context>, &mut Args) -> SourceResult<Value> + Send + Sync;

#[derive(Clone)]
pub struct TrustRoutine {
    pub f: &'static TrustRoutineFn,
    /// (named-argument name, creation-time default), in declaration order.
    /// The fast path injects only items the caller did not provide;
    /// provided ones are skipped (injected items go before the caller's
    /// arguments, and `Args::named`'s "last one wins for the same name"
    /// semantics keeps the caller's value in effect).
    pub defaults: Vec<(Str, Value)>,
}

impl std::fmt::Debug for TrustRoutine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("trust routine")
    }
}

impl typst::foundations::Repr for TrustRoutine {
    fn repr(&self) -> EcoString {
        "trust routine".into()
    }
}

impl typst::foundations::NativeType for TrustRoutine {
    const NAME: &'static str = "routine";
    fn data() -> &'static typst::foundations::NativeTypeData {
        static DATA: std::sync::OnceLock<typst::foundations::NativeTypeData> =
            std::sync::OnceLock::new();
        DATA.get_or_init(|| typst::foundations::NativeTypeData {
            name: "routine",
            long_name: "routine",
            title: "Routine",
            docs: "internal stub direct-call routine",
            def_site: typst::utils::DefSite {
                path: "trust-runtime",
                key: "TrustRoutine",
            },
            keywords: &[],
            constructor: std::sync::LazyLock::new(|| None),
            scope: std::sync::LazyLock::new(typst::foundations::Scope::new),
        })
    }
}

impl PartialEq for TrustRoutine {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(
            self.f as *const _ as *const (),
            other.f as *const _ as *const (),
        ) && self.defaults == other.defaults
    }
}

impl std::hash::Hash for TrustRoutine {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (self.f as *const _ as *const () as usize).hash(state);
        self.defaults.hash(state);
    }
}

/// Extracts named-argument names from the stub node in declaration order
/// and pairs them with the creation-time materialized defaults table
/// (mirrors typst closures' `closure.defaults`); a count mismatch (a change
/// in the generator's output shape) yields an empty table — the fast path
/// then injects nothing and semantics fall back to the stub's own binding.
fn stub_named_defaults(node: &typst_syntax::SyntaxNode, defaults: &[Value]) -> Vec<(Str, Value)> {
    let Some(closure_node) = find_closure_node(node) else {
        return Vec::new();
    };
    let Some(closure) = closure_node.cast::<ast::Closure>() else {
        return Vec::new();
    };
    let names: Vec<&str> = closure
        .params()
        .children()
        .filter_map(|param| match param {
            ast::Param::Named(named) => Some(named.name().as_str()),
            _ => None,
        })
        .collect();
    if names.len() != defaults.len() {
        return Vec::new();
    }
    names
        .into_iter()
        .zip(defaults.iter().cloned())
        .map(|(name, value)| (Str::from(name), value))
        .collect()
}

/// Stub captured scope (three protocol bindings: the native-body function
/// value + the direct-call routine + the state handle).
fn stub_scope(
    data: &'static typst::foundations::NativeFuncData,
    node: &typst_syntax::SyntaxNode,
    defaults: &[Value],
    // None = a stateless function (the body has no take_state prologue):
    // no state handle is carried, and neither the fast nor slow path
    // inserts a state argument. **Whether a function is native is an
    // externally invisible implementation detail** — all translated
    // functions use a uniform Closure-shaped stub (mirroring original user
    // closures where `Func::scope()` is None; a bare Native would leak the
    // identity, since import dispatch distinguishes user closures by
    // scope()).
    state: Option<&StateSlot>,
) -> Scope {
    let mut scope = Scope::new();
    scope.define("__trust_impl", Func::from(data));
    scope.define(
        "__trust_routine",
        Value::Dyn(typst::foundations::Dynamic::new(TrustRoutine {
            f: data.function.0,
            defaults: stub_named_defaults(node, defaults),
        })),
    );
    if let Some(state) = state {
        scope.define(
            "__trust_state",
            Value::Dyn(typst::foundations::Dynamic::new(state.clone())),
        );
    }
    scope
}

/// Signature-faithful stub closure (the native-closure stub approach): the
/// original `{name}{params}` signature is preserved (parameter checks /
/// `params()` / repr / default introspection all faithful); the forwarding
/// body passes the state handle to the native implementation as the first
/// positional argument. `defaults` holds creation-time materialized values
/// (mirroring typst closures evaluating defaults at creation time; a
/// missing argument at call time is taken from defaults — the AST's default
/// expressions are never evaluated). State travels with the function value,
/// with the same lifetime — global-registry addressing is thereby retired.
/// Per-call-site static stub creation (the node is already parsed and
/// cached by the call site's `static LazyLock`, so this takes no lookup and
/// no lock: Scope construction + define + Func::from).
pub fn native_closure_node(
    data: &'static typst::foundations::NativeFuncData,
    node: &(typst_syntax::SyntaxNode, usize),
    defaults: Vec<Value>,
    state: Option<&StateSlot>,
) -> Func {
    let (node, num_pos_params) = node.clone();
    let scope = stub_scope(data, &node, &defaults, state);
    let closure_span = node.span();
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    .spanned(closure_span)
}

pub fn native_closure(
    data: &'static typst::foundations::NativeFuncData,
    params_src: &str,
    defaults: Vec<Value>,
    // None = a stateless function: the stub is still constructed (Closure
    // shape preserves identity) but carries/forwards no state.
    state: Option<&StateSlot>,
) -> Func {
    let (params, arg_list) = stub_signature(params_src);
    let src = stub_source(data.name, &params, &arg_list, state.is_some());
    let (node, num_pos_params) = cached_closure_node(&src);
    let scope = stub_scope(data, &node, &defaults, state);
    let closure_span = node.span();
    Func::from(Closure {
        node: ClosureNode::Closure(node),
        defaults,
        captured: scope,
        num_pos_params,
    })
    .spanned(closure_span)
}

/// Stub source composition (a pure function, uncached): `stub_signature`
/// (pattern parameters → synthesized names, unnamed sinks get a name) +
/// `stub_source` merged — called at **translation time**; the generated
/// code embeds the result as a literal in each call site's static.
pub fn compose_stub(name: &str, params_src: &str) -> String {
    let (params, args) = stub_signature(params_src);
    stub_source(name, &params, &args, true)
}

/// Stub signature and forwarding argument list (cached by `params_src`
/// text — stub creation is frequent, so parsing happens only once).
///
/// Pattern parameters (destructuring parameters) are replaced in the
/// signature by the synthesized name `__trust_pN` (the forwarding signature
/// must bind the names the forwarding body references; the destructuring
/// itself is done by the native body); an unnamed sink `..` gets the
/// synthesized name `__trust_sink`.
fn stub_signature(params_src: &str) -> (String, String) {
    static CACHE: std::sync::LazyLock<
        std::sync::RwLock<std::collections::HashMap<String, (String, String)>>,
    > = std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    if let Some(hit) = CACHE.read().unwrap().get(params_src) {
        return hit.clone();
    }
    let root = typst_syntax::parse_code(&format!("let f{params_src} = none"));
    let (mut params, mut args): (Vec<String>, Vec<String>) = (Vec::new(), Vec::new());
    if let Some(closure_node) = find_closure_node(&root)
        && let Some(closure) = closure_node.cast::<ast::Closure>()
    {
        let mut nth_pattern = 0usize;
        for param in closure.params().children() {
            let text = param.to_untyped().full_text().to_string();
            match param {
                ast::Param::Pos(ast::Pattern::Normal(ast::Expr::Ident(_))) => {
                    args.push(text.clone());
                    params.push(text);
                }
                ast::Param::Pos(ast::Pattern::Placeholder(_)) => {
                    args.push(text.clone());
                    params.push(text);
                }
                ast::Param::Pos(_) => {
                    // Destructuring parameter: the signature binds a
                    // synthesized name; the native body destructures
                    // itself.
                    let name = format!("__trust_p{nth_pattern}");
                    nth_pattern += 1;
                    args.push(name.clone());
                    params.push(name);
                }
                ast::Param::Named(named) => {
                    let name = named.name().as_str();
                    args.push(format!("{name}: {name}"));
                    params.push(text);
                }
                ast::Param::Spread(spread) => match spread.sink_ident() {
                    Some(ident) => {
                        args.push(format!("..{}", ident.as_str()));
                        params.push(text);
                    }
                    None => {
                        args.push("..__trust_sink".to_string());
                        // Unnamed sink: the text looks like `..`; give it
                        // a synthesized name.
                        let named = if text.trim() == ".." {
                            "..__trust_sink".to_string()
                        } else {
                            text.replace("..", "..__trust_sink")
                        };
                        params.push(named);
                    }
                },
            }
        }
    }
    let out = (params.join(", "), args.join(", "));
    CACHE
        .write()
        .unwrap()
        .insert(params_src.to_string(), out.clone());
    out
}

/// Stub source `let {name}({params}) = __trust_impl(__trust_state, {args})`
/// cached by (function name, signature) (zero formatting on the frequent
/// creation path).
fn stub_source(name: &str, params: &str, args: &str, stateful: bool) -> String {
    /// function name → (params, stateful) → source text
    type StubCache =
        std::collections::HashMap<String, std::collections::HashMap<(String, bool), String>>;
    static CACHE: std::sync::LazyLock<std::sync::RwLock<StubCache>> =
        std::sync::LazyLock::new(|| std::sync::RwLock::new(std::collections::HashMap::new()));
    let key = (params.to_string(), stateful);
    if let Some(inner) = CACHE.read().unwrap().get(name)
        && let Some(hit) = inner.get(&key)
    {
        return hit.clone();
    }
    // Stateful: forward the state handle as the first argument; stateless:
    // pass arguments straight through (the body has no take_state).
    let src = if stateful {
        format!("let {name}({params}) = __trust_impl(__trust_state, {args})")
    } else {
        format!("let {name}({params}) = __trust_impl({args})")
    };
    CACHE
        .write()
        .unwrap()
        .entry(name.to_string())
        .or_default()
        .insert(key, src.clone());
    src
}
