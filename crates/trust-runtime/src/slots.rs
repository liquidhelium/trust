//! The `slots` section of `trust_runtime` (a mechanical split out of lib.rs).

#![allow(unused_imports)]
use crate::{
    assign::*, calls::*, content::*, context::*, destruct::*, eval::*, host::*, lookup::*,
    macros::*, stub::*, styles::*,
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

/// Storage and slot primitives for per-build state (the per-build slot
/// scheme).
///
/// The typst runtime model is: a finite set of named slots + one write per
/// evaluation + snapshots by value; this module provides the matching Rust
/// shapes — write-once cells (`OnceLock<Value>`) and a state handle that
/// travels with the function value ([`StateSlot`], the native-closure stub
/// approach): each evaluation of `module_init` constructs fresh instances,
/// there is no global registry, and the lifetime follows the function value
/// (mirroring the data layout of typst closures).
fn state_error(message: &str) -> EcoVec<SourceDiagnostic> {
    eco_vec![SourceDiagnostic::error(
        Span::detached(),
        typst::foundations::Str::from(message),
    )]
}

// --- Single gate for slot access (mirrors Binding::read / Binding::write) ---
//
// Write-once slots (OnceLock) and mutable slots (Mutex) share the same
// `read`/`fill` entries: the translation side does not need to (and cannot,
// at every emission site) dispatch on the field type — a slot's mutability
// is decided at plan time via mutable_slots, which picks the field type;
// the behavioral differences are gathered into these two entries.

/// Uniform access to slot fields (OnceLock writes once; Mutex writes any
/// number of times).
pub trait SlotField {
    fn slot_read(&self) -> SourceResult<Value>;
    fn slot_fill(&self, value: Value) -> SourceResult<()>;
}

/// Whether slot tracing is on. Probed once and cached: the slot-read path
/// runs for every top-level variable reference in generated code, and each
/// `var_os` takes the environ lock (the same reasoning as stub.rs's
/// `DEBUG_CAPTURE`).
fn trace_slots() -> bool {
    static TRACE: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var_os("TRUST_TRACE_SLOTS").is_some());
    *TRACE
}

/// Reads a top-level binding slot (write-once cell).
fn read_lock(slot: &std::sync::OnceLock<Value>) -> SourceResult<Value> {
    if let Some(v) = slot.get() {
        if trace_slots() {
            eprintln!("[slot] read-ok  @{:?}", std::ptr::from_ref(slot));
        }
        return Ok(v.clone());
    }
    if std::env::var_os("TRUST_SLOT_PANIC").is_some() {
        panic!("slot-unfilled @{:?}", std::ptr::from_ref(slot));
    }
    Err(state_error(&format!(
        "module slot not initialized (evaluation-order error, slot @{:?})",
        std::ptr::from_ref(slot)
    )))
}

/// Fills a top-level binding slot (write-once). The first successful fill
/// wins; on re-entry (e.g. `module_init` re-evaluated after a module-cache
/// eviction) the same value is an idempotent no-op, while a different value
/// is still a generator bug and is reported as an error.
fn fill_lock(slot: &std::sync::OnceLock<Value>, value: Value) -> SourceResult<()> {
    if trace_slots() {
        eprintln!("[slot] fill    @{:?}", std::ptr::from_ref(slot));
    }
    match slot.get() {
        Some(existing) => {
            if *existing == value {
                Ok(())
            } else {
                if std::env::var_os("TRUST_TRACE_REFILL").is_some() {
                    eprintln!(
                        "[refill] existing={existing:?}\n[value]   new={value:?}\n{}",
                        std::backtrace::Backtrace::force_capture()
                    );
                }
                Err(state_error(
                    "module slot refilled with a different value (generator bug)",
                ))
            }
        }
        None => slot
            .set(value)
            .map_err(|_| state_error("concurrent conflicting fill of module slot")),
    }
}

impl SlotField for std::sync::OnceLock<Value> {
    fn slot_read(&self) -> SourceResult<Value> {
        read_lock(self)
    }
    fn slot_fill(&self, value: Value) -> SourceResult<()> {
        fill_lock(self, value)
    }
}

impl SlotField for std::sync::Mutex<Value> {
    fn slot_read(&self) -> SourceResult<Value> {
        read_ms(self)
    }
    fn slot_fill(&self, value: Value) -> SourceResult<()> {
        write_ms(self, value)
    }
}

/// Reads a top-level binding slot (single gate: the field type decides
/// write-once vs mutable semantics).
pub fn read(slot: &impl SlotField) -> SourceResult<Value> {
    slot.slot_read()
}

/// Fills a top-level binding slot (single gate).
pub fn fill(slot: &impl SlotField, value: Value) -> SourceResult<()> {
    slot.slot_fill(value)
}

/// Reads a global-name slot materialized at creation time: with a value,
/// reads the slot (mirroring typst's creation-time capture); without one,
/// falls back to a call-time global lookup (mirroring call-time resolution
/// of names not visible at creation time).
pub fn read_or(
    slot: &std::sync::OnceLock<Value>,
    engine: &mut Engine,
    name: &str,
) -> SourceResult<Value> {
    match slot.get() {
        Some(value) => Ok(value.clone()),
        None => global(engine, name, Span::detached()),
    }
}

/// Reads a slot whose value is stored as content (the body).
pub fn read_content(slot: &std::sync::OnceLock<Value>) -> SourceResult<Content> {
    match read_lock(slot)? {
        Value::Content(content) => Ok(content),
        _ => Err(state_error(
            "module body slot does not hold content (generator bug)",
        )),
    }
}

/// Self-function cache for self-recursive closures (the stub is rebuilt
/// once per **instance**): creation carries the defaults; on first
/// self-reference a signature-faithful stub is rebuilt with the same
/// StateSlot and cached — N levels of recursion rebuild it only once, and
/// its identity (the slot id) matches the original instance.
pub struct SelfFuncCell {
    defaults: Vec<Value>,
    func: std::sync::OnceLock<Func>,
}

pub fn self_func_cell(defaults: Vec<Value>) -> SelfFuncCell {
    SelfFuncCell {
        defaults,
        func: std::sync::OnceLock::new(),
    }
}

pub fn self_func(
    cell: &SelfFuncCell,
    data: &'static typst::foundations::NativeFuncData,
    params_src: &str,
    slot: &StateSlot,
) -> Func {
    cell.func
        .get_or_init(|| native_closure(data, params_src, cell.defaults.clone(), Some(slot)))
        .clone()
}

/// Static-node version (the node comes from the call site's static;
/// rebuilding takes no lookup and no lock).
pub fn self_func_node(
    cell: &SelfFuncCell,
    data: &'static typst::foundations::NativeFuncData,
    node: &(typst_syntax::SyntaxNode, usize),
    slot: &StateSlot,
) -> Func {
    cell.func
        .get_or_init(|| native_closure_node(data, node, cell.defaults.clone(), Some(slot)))
        .clone()
}

/// Unpacking with the slot (for self-recursive closure prologues): returns
/// (capture tuple, slot clone).
pub fn take_caps_self<T: std::any::Any + Send + Sync + Clone>(
    args: &mut Args,
) -> SourceResult<(T, StateSlot)> {
    let value: Value = args.expect("__trust_state")?;
    match value {
        Value::Dyn(dynamic) => match dynamic.downcast::<StateSlot>() {
            Some(slot) => {
                let caps = slot
                    .inner
                    .clone()
                    .downcast::<T>()
                    .map(|arc| (*arc).clone())
                    .map_err(|_| {
                        state_error(&format!(
                            "state handle type mismatch (generator bug): expected {}",
                            std::any::type_name::<T>()
                        ))
                    })?;
                Ok((caps, slot.clone()))
            }
            None => Err(state_error(
                "first positional argument is not a state handle (generator bug)",
            )),
        },
        _ => Err(state_error(
            "first positional argument is not a state handle (generator bug)",
        )),
    }
}

/// Capture-tuple unpacking for stub closures (the StateSlot carries the
/// capture tuple rather than module state; take_state returns an Arc, and
/// destructuring the tuple by value costs one clone — the tuple is small,
/// the cost is negligible).
pub fn take_caps<T: std::any::Any + Send + Sync + Clone>(args: &mut Args) -> SourceResult<T> {
    let arc: std::sync::Arc<T> = take_state(args)?;
    Ok((*arc).clone())
}

/// Stub-closure capture unpacking: names not visible at creation time (the
/// Option is None) report unknown variable at call time (matching native
/// capture semantics).
pub fn cap(value: Option<Value>, name: &str, span: Span) -> SourceResult<Value> {
    match value {
        Some(value) => Ok(value),
        None => bail!(span, "unknown variable: {name}"),
    }
}

/// Native body prologue: unpack the per-build state from the first
/// positional argument and remove it from the arguments (mirroring the
/// stub forwarding: `__trust_impl(__trust_state, ..user args)`).
pub fn take_state<T: std::any::Any + Send + Sync>(
    args: &mut Args,
) -> SourceResult<std::sync::Arc<T>> {
    let value: Value = args.expect("__trust_state")?;
    match value {
        Value::Dyn(dynamic) => match dynamic.downcast::<StateSlot>() {
            Some(slot) => match slot.inner.clone().downcast::<T>() {
                Ok(arc) => Ok(arc),
                Err(_) => Err(state_error(&format!(
                    "state handle type mismatch (generator bug): expected {}",
                    std::any::type_name::<T>()
                ))),
            },
            None => Err(state_error(
                "first positional argument is not a state handle (generator bug)",
            )),
        },
        _ => Err(state_error(
            "first positional argument is not a state handle (generator bug)",
        )),
    }
}

// Style rule primitives (mirroring typst-eval/src/rules.rs): evaluation
// **only constructs**, never applies — a set rule builds `Styles`, a show
// rule builds `Recipe`; the actual transformation happens in typst's
// realize/layout (shared by generated code and interpreter alike).

/// Mutable module-slot helpers (top-level bindings modified in place; the
/// slot is a Mutex<Value>).
pub fn read_ms(slot: &std::sync::Mutex<Value>) -> SourceResult<Value> {
    Ok(slot.lock().unwrap().clone())
}

pub fn write_ms(slot: &std::sync::Mutex<Value>, v: Value) -> SourceResult<()> {
    *slot.lock().unwrap() = v;
    Ok(())
}
