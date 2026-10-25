//! Injectable Typst World — TRUST's host integration layer, on **stock
//! typst, zero fork**.
//!
//! Two injection paths, both through public APIs (verified by this crate's
//! tests):
//!
//! - **Global module injection**: after `Library::builder().build()`, do
//!   `scope_mut().define(name, module)` on `library.global` and refresh the
//!   `std` binding (build() gives std a copy of global's old Arc).
//!   Afterwards `#import cetz: ...` / `cetz.util.add(...)` are visible
//!   globally.
//! - **Package virtualization**: when typst-eval resolves `@ns/name:ver`,
//!   it only asks the World for the `typst.toml` and entry file under
//!   `VirtualRoot::Package(spec)` (see
//!   `typst-eval/src/import.rs::resolve_package`). This World returns a
//!   virtual manifest and a shim entry (e.g. `#import cetz: *`) for the
//!   given spec, so the native module is served as that version of the
//!   package; non-virtualized specs keep going through the other sources
//!   of `file()`/`source()` as usual.
//!
//! Pure-Rust closure construction is shown in this crate's tests: the
//! `Closure` fields are all public (syntax node + defaults + captured
//! scope), `Func::from(Closure)` is a first-class typst function, and
//! `Func::with` composes functions carrying preset arguments.

use std::collections::HashMap;
use std::path::PathBuf;

use comemo::Track;
use typst::diag::{FileError, FileResult, SourceResult};
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{
    Binding, Bytes, Context, Datetime, Dict, Duration, IntoValue, Module, Scope, StyleChain, Value,
};
use typst::introspection::EmptyIntrospector;
use typst::syntax::{
    FileId, RootedPath, Source, Span, SyntaxMode, VirtualPath, VirtualRoot, package::PackageSpec,
};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

/// Signature of the module evaluation entry: the host calls it once per
/// build and gets that build's module.
///
/// This mirrors typst's module evaluation: the generated code stores
/// top-level bindings in per-build slots, and the build identity is the
/// World's identity, so one World instance = one build.
pub type ModuleInit = trust_runtime::ModuleInit;

/// A [`World`] running on in-memory files with an injected library.
///
/// Evaluation-only (`font()` is always empty) — enough for tests that never
/// reach layout; mount a FontBook later if render comparisons are needed.
pub struct InjectWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    fonts: Vec<Font>,
    main: FileId,
    sources: HashMap<FileId, Source>,
    files: HashMap<FileId, Bytes>,
    /// Disk root for external packages (e.g. ~/.cache/typst/packages):
    /// unresolved package paths fall back here.
    packages_root: Option<PathBuf>,
    /// Project-root disk directory: unresolved Project-relative paths fall
    /// back here to read files (e.g. wasm plugins and other non-source
    /// files).
    fs_root: Option<PathBuf>,
    /// Fixed date in suite mode (typst's official tests world: 1970-01-01
    /// 12:00; today returns the same instant for any offset). None = the
    /// system's current time.
    suite_today: Option<Datetime>,
    /// **Parse cache** for package fallback sources (FileId → Source):
    /// without it, every `source()` fallback re-parses from scratch via
    /// `Source::new` — even when comemo replays reads to validate cache
    /// hits — so steady-state compilation would re-parse every external
    /// package file on each round.
    package_sources: std::sync::RwLock<HashMap<FileId, Source>>,
}

/// Cheap clone of the world (all fields are Arc/value semantics; a shared
/// base for test batches: fonts, the source table, and the file table are
/// cloned once and each test only changes main and the injected modules).
static SOURCE_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FONT_CALLS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Profiling counters (see InjectWorld::source/font).
pub fn world_call_counts() -> (u64, u64) {
    (
        SOURCE_CALLS.load(std::sync::atomic::Ordering::Relaxed),
        FONT_CALLS.load(std::sync::atomic::Ordering::Relaxed),
    )
}

impl Clone for InjectWorld {
    fn clone(&self) -> Self {
        Self {
            library: self.library.clone(),
            book: self.book.clone(),
            fonts: self.fonts.clone(),
            main: self.main,
            sources: self.sources.clone(),
            files: self.files.clone(),
            packages_root: self.packages_root.clone(),
            fs_root: self.fs_root.clone(),
            suite_today: self.suite_today,
            package_sources: std::sync::RwLock::new(self.package_sources.read().unwrap().clone()),
        }
    }
}

impl InjectWorld {
    /// Creates a new world: main-file text + the list of modules to inject
    /// into the global scope `(name, module)`; `sys.inputs` is empty.
    pub fn new(main_text: &str, modules: impl IntoIterator<Item = (&'static str, Module)>) -> Self {
        Self::with_inputs(main_text, modules, Dict::new())
    }

    /// Creates a new world with `sys.inputs` set (baked into the Library at
    /// construction; inputs are Library contents).
    pub fn with_inputs(
        main_text: &str,
        modules: impl IntoIterator<Item = (&'static str, Module)>,
        inputs: Dict,
    ) -> Self {
        let main = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("main.typ").expect("valid path"),
        )
        .intern();

        let mut library = Library::builder().with_inputs(inputs).build();
        for (name, module) in modules {
            library.global.scope_mut().define(name, module);
        }
        // In build(), the std binding holds another Arc copy of global; the
        // scope_mut writes above are copy-on-write and would fork the two —
        // refresh here to keep them consistent.
        library.std = Binding::detached(library.global.clone().into_value());

        let packages_root = std::env::var_os("USER_HOME")
            .map(|h| PathBuf::from(h).join(".cache/typst/packages"))
            .or_else(|| {
                std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/typst/packages"))
            })
            .filter(|p| p.exists());
        let fs_root = std::env::var("TRUST_WORLD_ROOT")
            .ok()
            .map(PathBuf::from)
            .filter(|p| p.exists());
        Self {
            library: LazyHash::new(library),
            book: LazyHash::new(FontBook::new()),
            fonts: Vec::new(),
            main,
            sources: HashMap::from([(main, Source::new(main, main_text.to_string()))]),
            files: HashMap::new(),
            packages_root,
            fs_root,
            suite_today: None,
            package_sources: std::sync::RwLock::new(HashMap::new()),
        }
    }

    /// Suite mode: mirrors typst's official tests world (tests/src/world.rs)
    /// — Features::all, the test globals (`test`/`test-repr`/`print`/
    /// `lines`/`bounds`, colors `conifer`/`forest`), default styles (page
    /// width 120pt, margins 10pt, page height auto, font size 10pt — the
    /// inner page is exactly 100pt wide) and the fixed date. When driving
    /// the official tests/suite, call this on both the golden and gen sides
    /// **before** injecting modules.
    pub fn enable_suite(&mut self) {
        let mut lib = typst::Library::builder()
            .with_features(typst::Features::all())
            .build();
        lib.global.scope_mut().define_func::<suite::test>();
        lib.global.scope_mut().define_func::<suite::test_repr>();
        lib.global.scope_mut().define_func::<suite::print>();
        lib.global.scope_mut().define_func::<suite::lines>();
        lib.global.scope_mut().define_func::<suite::bounds>();
        lib.global.scope_mut().define("conifer", suite::conifer());
        lib.global.scope_mut().define("forest", suite::forest());
        {
            use typst::foundations::Smart;
            use typst::layout::{Abs, Margin, PageElem};
            use typst::text::{TextElem, TextSize};
            lib.styles.set(PageElem::width, Smart::Custom(Abs::pt(120.0).into()));
            lib.styles.set(PageElem::height, Smart::Auto);
            lib.styles.set(
                PageElem::margin,
                Smart::Custom(Margin::splat(Some(Smart::Custom(Abs::pt(10.0).into())))),
            );
            lib.styles.set(TextElem::size, TextSize(Abs::pt(10.0).into()));
        }
        lib.std = Binding::detached(lib.global.clone().into_value());
        self.library = LazyHash::new(lib);
        self.suite_today = Some(Datetime::from_ymd(1970, 1, 1).expect("valid fixed date"));
    }

    /// Injects a native module: drives `module_init` (the mirror of module
    /// evaluation; per-build slots register by library contents), injects
    /// the returned module into the global scope, and rebuilds the Library
    /// (rewrapping the LazyHash keeps comemo's hashing correct). Module
    /// evaluation is cached via comemo memoize (the same mechanism as
    /// `typst_eval::eval`): the same (world read constraints, library
    /// contents, init fn) is not evaluated twice, and dependency changes
    /// invalidate automatically.
    pub fn init_native_module(&mut self, name: &'static str, init: ModuleInit) -> Module {
        // Thin wrapper: the protocol itself lives in trust_runtime (the
        // host integration protocol, see rt::init_module / rt::bind_module);
        // InjectWorld is only a test world.
        let world: &dyn World = self;
        let module =
            trust_runtime::init_module(world.track(), &self.library, init).unwrap_or_else(
                |diags| {
                    panic!(
                        "module_init evaluation failed: {:?}",
                        diags.iter().map(|d| d.message.clone()).collect::<Vec<_>>()
                    )
                },
            );
        self.library = trust_runtime::bind_module(&self.library, name, module.clone());
        module
    }

    /// Replaces the same-named definition in the global scope with the
    /// module and rebuilds the Library.
    /// Package path → disk path in the local package repository layout
    /// (external package fallback).
    fn package_path(&self, id: FileId) -> Option<PathBuf> {
        let root = self.packages_root.as_ref()?;
        let VirtualRoot::Package(spec) = id.root() else {
            return None;
        };
        // Layout: {root}/{namespace}/{name}/{version}/{vpath}.
        let mut path = root
            .join(spec.namespace.as_str())
            .join(spec.name.as_str())
            .join(spec.version.to_string());
        for part in id.vpath().get_with_slash().split('/') {
            if !part.is_empty() {
                path = path.join(part);
            }
        }
        path.exists().then_some(path)
    }

    /// Swaps the main file (for test batches: clone the base, then set main
    /// per test; `path` is a project-root-relative path such as
    /// `tests/suite/model/x.typ` — relative imports resolve against the
    /// real directory, matching the official tests world's vpaths).
    pub fn set_main(&mut self, path: &str, text: &str) {
        let id = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(path).unwrap_or_else(|_| panic!("invalid path {path}")),
        )
        .intern();
        self.main = id;
        self.sources.insert(id, Source::new(id, text.to_string()));
    }

    /// Injects a module directly (bypassing comemo memo): test batches
    /// assemble a single file per test, and the memo key is unstable for
    /// purely native evaluation (an empty world-read constraint would conflate
    /// distinct builds), so the module is evaluated directly and then bound.
    pub fn inject_module(&mut self, name: &'static str, module: Module) {
        self.library = trust_runtime::bind_module(&self.library, name, module);
    }

    /// Appends a source file relative to the main file (multi-file package
    /// shapes).
    pub fn add_source(&mut self, path: &str, text: &str) {
        let id = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(path).unwrap_or_else(|_| panic!("invalid path {path}")),
        )
        .intern();
        self.sources.insert(id, Source::new(id, text.to_string()));
    }

    /// Virtualizes a package: the package `spec` resolves to the `entry`
    /// entry file with contents `entry_text` (a shim, usually one line
    /// `#import <global module>: *`).
    ///
    /// With this, `#import "<spec>": ...` lands on the injected native
    /// module without touching disk or network; other packages are
    /// unaffected.
    pub fn add_virtual_package(
        &mut self,
        spec: &str,
        entry: &str,
        entry_text: &str,
    ) -> Result<(), String> {
        let spec: PackageSpec = spec
            .parse()
            .map_err(|err| format!("invalid package spec: {err}"))?;
        let manifest_id = RootedPath::new(
            VirtualRoot::Package(spec.clone()),
            VirtualPath::new("typst.toml").expect("valid path"),
        )
        .intern();
        let manifest = format!(
            "[package]\nname = \"{}\"\nversion = \"{}\"\nentrypoint = \"{}\"\n",
            spec.name, spec.version, entry
        );
        self.files.insert(manifest_id, Bytes::from_string(manifest));

        let entry_id = RootedPath::new(
            VirtualRoot::Package(spec),
            VirtualPath::new(entry).expect("valid path"),
        )
        .intern();
        self.sources
            .insert(entry_id, Source::new(entry_id, entry_text.to_string()));
        Ok(())
    }

    /// Appends a data file to an (existing) virtual package (wasm plugins,
    /// images, ...) — in-package relative reads (`plugin("plugin.wasm")` /
    /// `read("data.csv")`) resolve against the spec root; the golden side
    /// reads the real file via the package-cache fallback, and the gen side
    /// supplies it here.
    pub fn add_virtual_package_file(
        &mut self,
        spec: &str,
        rel: &str,
        data: Vec<u8>,
    ) -> Result<(), String> {
        let spec: PackageSpec = spec
            .parse()
            .map_err(|err| format!("invalid package spec: {err}"))?;
        let id = RootedPath::new(
            VirtualRoot::Package(spec),
            VirtualPath::new(rel).expect("valid path"),
        )
        .intern();
        self.files.insert(id, Bytes::new(data));
        Ok(())
    }

    /// Mounts a project-root data file (the generated top-level package's
    /// spans use the Project root, and in-package relative reads resolve to
    /// the project root — probes mount data on both sides this way).
    pub fn add_file(&mut self, path: &str, data: Vec<u8>) {
        let id = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(path).expect("valid path"),
        )
        .intern();
        self.files.insert(id, Bytes::new(data));
    }

    /// Mounts fonts (for layout/rendering): `font()` and `book()` serve
    /// them from here on. Evaluation-only scenarios need not call this;
    /// mount `typst-assets` or system fonts for render comparisons.
    pub fn set_fonts(&mut self, fonts: Vec<Font>) {
        let mut book = FontBook::new();
        for font in &fonts {
            book.push(font.info().clone());
        }
        self.fonts = fonts;
        self.book = LazyHash::new(book);
    }

    /// Evaluates the main file with typst-eval and returns the module (the
    /// same module-evaluation path as the typst compiler, package
    /// resolution included).
    pub fn eval_main(&self) -> SourceResult<Module> {
        let mut sink = Sink::new();
        let source = self
            .sources
            .get(&self.main)
            .cloned()
            .expect("main source present");
        // Track is only implemented for `dyn World`; upcast first, then
        // track (same as typst::compile).
        let world: &dyn World = self;
        typst_eval::eval(
            world.track(),
            &self.library,
            Traced::default().track(),
            sink.track_mut(),
            Route::default().track(),
            &source,
        )
    }

    /// Evaluates a string in code mode; `scope` becomes the VM's top-level
    /// scope, so extra bindings can be injected directly (the execution
    /// entry for differential tests).
    pub fn with_engine<R>(&self, f: impl FnOnce(&mut Engine) -> R) -> R {
        let mut sink = Sink::new();
        let world: &dyn World = self;
        let traced = Traced::default();
        let mut engine = Engine {
            world: world.track(),
            library: &self.library,
            introspector: typst::utils::Protected::from_raw(
                typst::introspection::EmptyIntrospector.track(),
            ),
            traced: traced.track(),
            sink: sink.track_mut(),
            route: Route::default(),
        };
        f(&mut engine)
    }

    pub fn eval_code(&self, code: &str, scope: Scope) -> SourceResult<Value> {
        let mut sink = Sink::new();
        let world: &dyn World = self;
        typst_eval::eval_string(
            world.track(),
            &self.library,
            sink.track_mut(),
            EmptyIntrospector.track(),
            Context::new(None, Some(StyleChain::new(&self.library.styles))).track(),
            code,
            typst::routines::SpanMode::Uniform(Span::detached()),
            SyntaxMode::Code,
            scope,
        )
    }

    pub fn library(&self) -> &LazyHash<Library> {
        &self.library
    }
}

impl World for InjectWorld {
    fn library(&self) -> &LazyHash<Library> {
        &self.library
    }

    fn book(&self) -> &LazyHash<FontBook> {
        &self.book
    }

    fn main(&self) -> FileId {
        self.main
    }

    fn source(&self, id: FileId) -> FileResult<Source> {
        SOURCE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if let Some(source) = self.sources.get(&id) {
            return Ok(source.clone());
        }
        // Package path fallback: the on-disk package repository; parse once
        // into the cache (see the field comment).
        if let Some(cached) = self.package_sources.read().unwrap().get(&id) {
            return Ok(cached.clone());
        }
        if let Some(path) = self.package_path(id) {
            let text =
                std::fs::read_to_string(&path).map_err(|_| FileError::NotFound(path.clone()))?;
            let source = Source::new(id, text);
            self.package_sources
                .write()
                .unwrap()
                .insert(id, source.clone());
            return Ok(source);
        }
        Err(FileError::NotFound(PathBuf::from("not in this world")))
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if let Some(bytes) = self.files.get(&id) {
            return Ok(bytes.clone());
        }
        // For source files, serve a byte view of their text (an equivalent
        // the World trait docs sanction).
        if let Some(src) = self.sources.get(&id) {
            return Ok(Bytes::from_string(src.text().to_string()));
        }
        if let Some(path) = self.package_path(id) {
            let bytes = std::fs::read(&path).map_err(|_| FileError::NotFound(path.clone()))?;
            return Ok(Bytes::new(bytes));
        }
        // Project-root file fallback (wasm plugins, data files, ...). The
        // vpath starts with `/`, which must be stripped before join or it
        // would replace the root entirely.
        if let Some(root) = &self.fs_root
            && id.root() == &VirtualRoot::Project
        {
            let rel = id.vpath().get_with_slash().trim_start_matches('/');
            let path = root.join(rel);
            if path.exists() {
                let bytes = std::fs::read(&path).map_err(|_| FileError::NotFound(path.clone()))?;
                return Ok(Bytes::new(bytes));
            }
        }
        Err(FileError::NotFound(PathBuf::from("not in this world")))
    }

    fn font(&self, index: usize) -> Option<Font> {
        FONT_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.fonts.get(index).cloned()
    }

    fn today(&self, offset: Option<Duration>) -> Option<Datetime> {
        // Suite mode: mirrors the official tests world's
        // Time::fixed(1970-01-01 12:00).today — the result **carries the
        // date only** (the time-of-day is truncated); offset seconds must
        // fit in i32 (out of range → None → "unable to get the current
        // date"), and shifting by the offset may cross a day boundary.
        if self.suite_today.is_some() {
            let secs = match offset {
                None => 0.0f64,
                Some(d) => {
                    let [w, dd, h, m, s] = d.decompose();
                    let total: f64 = w as f64 * 604_800.0
                        + dd as f64 * 86_400.0
                        + h as f64 * 3_600.0
                        + m as f64 * 60.0
                        + s as f64;
                    if !total.is_finite()
                        || total < f64::from(i32::MIN)
                        || total > f64::from(i32::MAX)
                    {
                        return None;
                    }
                    total
                }
            };
            let shifted = 43_200.0 + secs; // counted from 12:00
            let days = shifted.div_euclid(86_400.0) as i64;
            let (y, m, d) = civil_from_days(days);
            return Datetime::from_ymd(y as i32, m as u8, d as u8);
        }
        // The system's current time (with the timezone offset; both sides
        // of a test world share this source — returning None here would
        // make date accesses like `sys.datetime` fail with "unable to get
        // the current date").
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?;
        let offset_secs = offset
            .map(|d| {
                let [w, dd, h, m, s] = d.decompose();
                w * 604_800 + dd * 86_400 + h * 3_600 + m * 60 + s
            })
            .unwrap_or(0);
        let secs = now.as_secs() as i64 + offset_secs;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        let (y, m, d) = civil_from_days(days);
        Datetime::from_ymd_hms(
            y as i32,
            m as u8,
            d as u8,
            (rem / 3600) as u8,
            ((rem % 3600) / 60) as u8,
            (rem % 60) as u8,
        )
    }
}

/// Days since the epoch → Gregorian date (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use typst::foundations::{Args, Closure, ClosureNode, Func};

    /// Builds `(a, b) => a + b` in pure Rust: the syntax node comes from
    /// the official parser, defaults are evaluated at creation time (none
    /// here — no named parameters), and captured is an empty Scope — the
    /// fields and evaluation semantics mirror `typst-eval`'s
    /// `Eval for ast::Closure`.
    fn add_closure() -> Func {
        let root = typst_syntax::parse_code("(a, b) => a + b");
        let node = root.children().next().expect("one child").clone();
        assert_eq!(node.kind(), typst_syntax::SyntaxKind::Closure);
        let closure = Closure {
            node: ClosureNode::Closure(node),
            defaults: vec![],
            captured: Scope::new(),
            num_pos_params: 2,
        };
        Func::from(closure)
    }

    fn cetz_module(add: Func) -> Module {
        let mut util = Scope::new();
        util.define("add", add.clone());
        let util = Module::new("util", util);
        let mut scope = Scope::new();
        scope.define("util", util);
        scope.define("add", add);
        Module::new("cetz", scope)
    }

    #[test]
    fn global_module_injection_and_closure_from_rust() {
        let cetz = cetz_module(add_closure());
        let world = InjectWorld::new(
            "#import cetz: util\n#assert(util.add(1, 2) == 3)\n#assert(cetz.add(40, 2) == 42)",
            vec![("cetz", cetz)],
        );
        world.eval_main().expect("markup eval with injected module");

        // Code mode + global visibility
        let v = world
            .eval_code("cetz.util.add(1, 2)", Scope::new())
            .expect("code eval");
        assert_eq!(v, Value::Int(3));
    }

    #[test]
    fn virtual_package_shim() {
        let cetz = cetz_module(add_closure());
        let mut world = InjectWorld::new(
            "#import \"@preview/cetz:0.5.2\": add\n#assert(add(2, 3) == 5)",
            vec![("cetz", cetz)],
        );
        world
            .add_virtual_package("@preview/cetz:0.5.2", "lib.typ", "#import cetz: add")
            .expect("virtualize");
        world.eval_main().expect("package import through shim");

        // Code mode goes through package resolution too
        let v = world
            .eval_code(
                "import \"@preview/cetz:0.5.2\": add; add(20, 22)",
                Scope::new(),
            )
            .expect("code eval with package import");
        assert_eq!(v, Value::Int(42));
    }

    #[test]
    fn with_composition_carries_state() {
        // Func::with (a stock public combinator) presets arguments,
        // producing a function that carries state.
        let add = add_closure();
        let mut args = Args::new(Span::detached(), [Value::Int(2)]);
        let add2 = add.with(&mut args);

        let mut scope = Scope::new();
        scope.define("add2", add2);
        let world = InjectWorld::new("", vec![]);
        let v = world
            .eval_code("add2(3)", scope)
            .expect("call composed func");
        assert_eq!(v, Value::Int(5));
    }
}

/// Globals for the official test suite (verbatim mirrors of the #[func]
/// definitions in tests/src/world.rs; `bounds`'s BoundsElem draws its
/// boundary box via mark_box_in_place).
mod suite {
    use typst::diag::{At, SourceResult, StrResult, bail};
    use typst::engine::Engine;
    use typst::foundations::{
        Content, IntoValue, NativeElement, NoneValue, Packed, Repr, Value, elem, func,
    };
    use typst::introspection::Locator;
    use typst::layout::{BlockElem, Fragment, Regions};
    use typst::model::{Numbering, NumberingPattern};
    use typst::syntax::Span;
    use typst::visualize::Color;

    pub fn conifer() -> Color {
        Color::from_u8(0x9f, 0xEB, 0x52, 0xFF)
    }

    pub fn forest() -> Color {
        Color::from_u8(0x43, 0xA1, 0x27, 0xFF)
    }


    #[func]
    pub fn test(lhs: Value, rhs: Value) -> StrResult<NoneValue> {
        if lhs != rhs {
            bail!("Assertion failed: {} != {}", lhs.repr(), rhs.repr());
        }
        Ok(NoneValue)
    }

    #[func]
    pub fn test_repr(lhs: Value, rhs: Value) -> StrResult<NoneValue> {
        if lhs.repr() != rhs.repr() {
            bail!("Assertion failed: {} != {}", lhs.repr(), rhs.repr());
        }
        Ok(NoneValue)
    }

    #[func]
    pub fn print(values: Vec<Value>) -> NoneValue {
        let mut out = std::io::stdout().lock();
        use std::io::Write;
        write!(out, "> ").unwrap();
        for (i, value) in values.into_iter().enumerate() {
            if i > 0 {
                write!(out, ", ").unwrap();
            }
            write!(out, "{value:?}").unwrap();
        }
        writeln!(out).unwrap();
        NoneValue
    }

    /// Generates `count` lines of text based on the numbering.
    #[func]
    pub fn lines(
        engine: &mut Engine,
        context: comemo::Tracked<typst::foundations::Context>,
        span: Span,
        count: u64,
        #[default(Numbering::Pattern("A".parse::<NumberingPattern>().unwrap()))]
        numbering: Numbering,
    ) -> SourceResult<Value> {
        (1..=count)
            .map(|n| numbering.apply(engine, context, span, &[n]))
            .collect::<SourceResult<typst::foundations::Array>>()?
            .join(Some('\n'.into_value()), None, None)
            .at(span)
    }

    /// Display boundaries and the baseline around some content's frames.
    #[func]
    pub fn bounds(content: Content) -> SourceResult<Content> {
        Ok(BlockElem::multi_layouter(
            Packed::new(BoundsElem::new(content)),
            layout_bounds,
        )
        .pack())
    }

    #[elem]
    struct BoundsElem {
        #[positional]
        #[required]
        body: Content,
    }

    fn layout_bounds(
        elem: &Packed<BoundsElem>,
        engine: &mut Engine,
        locator: Locator,
        styles: typst::foundations::StyleChain,
        regions: Regions,
    ) -> SourceResult<Fragment> {
        let mut fragment =
            typst_layout::layout_fragment(engine, &elem.body, locator, styles, regions)?;
        for frame in &mut fragment {
            frame.mark_box_in_place();
        }
        Ok(fragment)
    }
}
