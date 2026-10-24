//! Translation context and name-resolution environment.
//!
//! `Ctx` runs through the whole translation: diagnostic accumulation, the
//! lenient-mode switch, temporary id counters, and the package-mode file
//! context. `Env` is the static-scope name-resolution table (local stack +
//! visible module slots); `Ref` is the reference form of a module slot.
//!
//! In the generated output, module-level bindings compile to per-build
//! slot-board fields (see `_things/DESIGN-per-build.md`) and every
//! reference takes the form `rt::read(&__st.slot_name)?`; `used_state`
//! tracks whether the current function has emitted any such reference,
//! which decides whether its body needs the `__st` prologue.

use super::Diag;
use super::naming::{escape, fresh_rust_ident};

pub(super) struct Ctx {
    pub(super) diags: Vec<Diag>,
    /// Lenient mode (exploration): gaps degrade to placeholders instead of
    /// failing the whole translation.
    pub(super) lenient: bool,
    pub(super) next_id: usize,
    /// Path of the file currently being translated in package mode (for
    /// resolving in-body string imports).
    pub(super) current_file: Option<String>,
    /// The package-mode file set (string-import resolution).
    pub(super) package_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// Package mode: file path → generated mod name (in-body string imports
    /// take their module value from here).
    pub(super) package_mods: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// Slot name → owning file (for resolving module-value-chain wildcards:
    /// in-body `import cetz.draw: *`).
    pub(super) slot_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// Package mode: export table (path → name → slot); used to statically
    /// expand in-body **wildcard** imports — when the target is a translated
    /// file, its export surface is known at translation time.
    pub(super) export_table: Option<
        std::rc::Rc<std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>>,
    >,
    /// Whether the current function body references a module slot (decides
    /// whether the `__st` prologue is generated).
    pub(super) used_state: bool,
    /// Dedup table for value-position closures (C2: closure expressions
    /// also compile to a native body + signature-faithful stub):
    /// key = (closure source text, normalized captured-name set) →
    /// (id, whether it carries __st). Body code is a pure function of
    /// (AST, captured-name set), so equal keys yield equal code; dedup
    /// keeps one definition shared across call sites from ballooning.
    pub(super) closure_dedup:
        std::collections::HashMap<String, (String, bool)>,
    /// Emission area for per-call-site node statics (`NODE_cN`, emitted to
    /// the root alongside closure items).
    pub(super) closure_nodes: String,
    pub(super) next_node_id: usize,
    pub(super) next_closure_id: usize,
    /// Collection of C2 closure f_/D_ items (emitted at the root of the
    /// output — after cross-file dedup there is exactly one copy per
    /// definition, with root-level visibility of __State and all imports).
    pub(super) closure_items: String,
    /// Function-body nesting depth (>0 means we are inside a generated f_
    /// function body; `return` is only legal there — the enclosing Rust
    /// functions have different return types).
    pub(super) in_fn: usize,
    /// Loop nesting depth (translate-time check for break/continue outside
    /// a loop, driven by the scripting-loop error tests).
    pub(super) in_loop: usize,
    /// Mutable-slot set (slot names of top-level bindings mutated in
    /// place): reads go through read_ms, fills through fill_ms, mutations
    /// do read-modify-write (write_ms).
    pub(super) mutable_slots: std::collections::HashSet<String>,
    /// Currently translating a closure body (closures carry __st on
    /// demand; direct module-slot references are unavailable — mirroring
    /// typst: writing a captured name is already an error there, left to
    /// diagnostics).
    pub(super) in_closure: usize,
    /// Captured-name stack (one frame per closure level; elements are the
    /// **captured-name Rust locals** bound into the closure environment;
    /// mirrors typst's read-only `Binding::capture` marker in the closure's
    /// captured scope): assignments / mutating methods landing on a
    /// captured local report "variables from outside the function are
    /// read-only" at runtime (mirroring `Binding::write`'s rejection of
    /// Captured).
    pub(super) captured_locals: Vec<std::collections::HashSet<String>>,
    /// Stack of hoisting-slot frames for expression-position `let` (one
    /// frame per scannable scope: block / bare-expression function body /
    /// content item; key = binding name, value = the hoisted Rust name of
    /// the block-top `let mut`). Mirrors typst: `let` is an expression that
    /// evaluates to none and binds/shadows the name in the **current
    /// lexical scope**, immediately visible to later evaluations of the
    /// same scope — in the two-pass translation, the binding name is pushed
    /// into env at its position (references before the position still
    /// resolve to the outer scope) while the value is written to a hoisted
    /// slot (discarded when the block exits, so scoping stays exact).
    pub(super) let_hoists: Vec<std::collections::HashMap<String, String>>,
    /// Error-site span table: (source file path, raw AST span u64) → site
    /// index (the generated code references `*S{idx}`; at runtime it is
    /// materialized via `rt::source_span` with the current process's
    /// FileId numbering — spans are self-contained, no host pre-intern
    /// convention).
    pub(super) span_table: Vec<(String, u64)>,
    span_index: std::collections::HashMap<u64, usize>,
}

/// Field chain for a dotted name (`process.aabb` → two rt::field calls; a
/// single name is accessed directly). Mirrors typst's field-access
/// semantics: dots in a name walk the fields segment by segment.
pub(super) fn field_access_chain(expr: &str, dotted: &str) -> String {
    let mut segs = dotted.split('.');
    let first = segs.next().unwrap_or(dotted);
    let mut out = format!(
        "rt::field(&({expr}), \"{first}\", engine, context, SPAN)?"
    );
    for seg in segs {
        out = format!(
            "rt::field(&({out}), \"{seg}\", engine, context, SPAN)?"
        );
    }
    out
}

/// Reference form of a module binding (uniformly points at a per-build
/// slot-board field).
#[derive(Clone)]

pub(super) enum Ref {
    /// Top-level binding slot (written once per build).
    Slot(String),
    /// Binding taken from a module-value field (`import deps.oxifmt:
    /// strfmt`): `file` is the **target-file identity** of the item
    /// (derived by chaining entry export table → slot → file; present only
    /// when statically reachable) — in-body `import item: *` expands
    /// statically from it (as in `import draw: *`); None leaves the
    /// wildcard to the runtime layer.
    ModuleField {
        expr: String,
        name: String,
        file: Option<String>,
    },
    /// Binding evaluated directly as an expression (the module value itself
    /// or one of its fields).
    Value(String),
    /// Global name materialized at creation time: read the slot when it
    /// holds a value (typst's creation-time capture), otherwise fall back
    /// to a call-time global lookup (the semantics of names invisible at
    /// creation time).
    GlobalFallback { slot: String, name: String },
}

/// Name-resolution environment: local stack + visible module slots
/// (static scoping, accumulated in declaration order).
#[derive(Clone)]
pub(super) struct Env {
    pub(super) locals: Vec<(String, String)>,
    pub(super) module: Vec<(String, Ref)>,
    /// Runtime wildcard layer (introduced in-scope by `import <runtime
    /// module value>: *` in a body; effective only in that scope; elements
    /// are the layer's Rust local names, innermost last). Name lookup:
    /// locals → layers (runtime; shadows outer module names and globals,
    /// mirroring typst-eval's wildcard-import scopes.top.bind) → module →
    /// global.
    pub(super) wildcards: Vec<String>,
}

impl Ctx {
    pub(super) fn new(lenient: bool) -> Self {
        Self {
            diags: Vec::new(),
            lenient,
            next_id: 0,
            current_file: None,
            package_files: None,
            package_mods: None,
            slot_files: None,
            export_table: None,
            used_state: false,
            in_fn: 0,
            in_loop: 0,
            mutable_slots: std::collections::HashSet::new(),
            in_closure: 0,
            captured_locals: Vec::new(),
            let_hoists: Vec::new(),
            closure_dedup: std::collections::HashMap::new(),
            next_closure_id: 0,
            closure_items: String::new(),
            closure_nodes: String::new(),
            next_node_id: 0,
            span_table: Vec::new(),
            span_index: std::collections::HashMap::new(),
        }
    }

    pub(super) fn fresh_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Emit a per-call-site static
    /// `pub(crate) static NODE_cN: LazyLock<(SyntaxNode, usize)>` for a
    /// compile-time-constant snippet source and return the static's name —
    /// zero locks and zero lookups on the hot creation path (the static is
    /// the cache).
    pub(super) fn closure_node_static(&mut self, src: &str) -> String {
        let name = format!("NODE_c{}", self.next_node_id);
        self.next_node_id += 1;
        self.closure_nodes.push_str(&format!(
            "pub(crate) static {name}: std::sync::LazyLock<(typst_syntax::SyntaxNode, usize)> = std::sync::LazyLock::new(|| rt::parse_closure_node({src:?}));\n"
        ));
        name
    }

    /// Register a source span and return its generated reference `*S{idx}`
    /// (for error sites; mirrors the original: an error's position points
    /// at the source expression; in the output it is a LazyLock
    /// materialized on first use with the current process's FileId — see
    /// `rt::source_span`).
    pub(super) fn span_of(&mut self, span: typst_syntax::Span) -> String {
        let raw = span.into_raw().get();
        let next = self.span_table.len();
        let idx = *self.span_index.entry(raw).or_insert(next);
        if idx == next {
            let path = span
                .id()
                .map(|id| id.vpath().get_with_slash().trim_start_matches('/').to_string())
                .unwrap_or_default();
            self.span_table.push((path, raw));
        }
        format!("*S{idx}")
    }

    /// Register a source span and use its constant name to fill the `{at}`
    /// placeholder in a format string.
    pub(super) fn span_fmt(&mut self, span: typst_syntax::Span, fmt: String) -> String {
        let at = self.span_of(span);
        fmt.replace("{at}", &at)
    }

    /// Emit the error-site span block (materialized on first use with the
    /// current process's FileId — self-contained, no host pre-intern
    /// convention; empty when there are no sites).
    pub(super) fn span_consts(&self) -> String {
        if self.span_table.is_empty() {
            return String::new();
        }
        let mut out = String::new();
        for (idx, (path, raw)) in self.span_table.iter().enumerate() {
            out.push_str(&format!(
                "static S{idx}: std::sync::LazyLock<Span> = std::sync::LazyLock::new(|| rt::source_span({path:?}, {raw}));\n"
            ));
        }
        out
    }

    pub(super) fn diag(&mut self, span: typst_syntax::Span, message: String) {
        let file = self.current_file.clone();
        self.diags.push(Diag {
            span,
            message,
            file,
        });
    }

    /// Compile a module-slot reference into an expression (marking that the
    /// current function uses `__st`).
    pub(super) fn ref_value(&mut self, module_ref: &Ref) -> String {
        self.used_state = true;
        match module_ref {
            Ref::Slot(id) if self.mutable_slots.contains(id) => {
                format!("rt::read_ms(&__st.{id})?")
            }
            Ref::Slot(id) => format!("rt::read(&__st.{id})?"),
            Ref::ModuleField { expr, name, .. } => {
                field_access_chain(expr, name)
            }
            Ref::Value(expr) => expr.clone(),
            Ref::GlobalFallback { slot, name } => {
                let name = escape(name);
                format!("rt::read_or(&__st.{slot}, engine, \"{name}\")?")
            }
        }
    }
}

/// Push a new binding onto the local stack, returning a clash-free Rust
/// name.
pub(super) fn bind_local(env: &mut Env, name: &str) -> String {
    let rust = fresh_rust_ident(name, &env.locals);
    env.locals.push((name.to_string(), rust.clone()));
    rust
}
