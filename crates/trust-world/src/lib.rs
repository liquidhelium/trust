//! 注入式 Typst World——TRUST 的宿主接入层,**原版 typst,零 fork**。
//!
//! 两条注入路径,全部使用公开 API(已在本 crate 的测试中验证):
//!
//! - **全局模块注入**:`Library::builder().build()` 之后对
//!   `library.global` 做 `scope_mut().define(name, module)`,并刷新
//!   `std` 绑定(build() 给 std 的是 global 旧 Arc 的副本)。此后
//!   `#import cetz: ...` / `cetz.util.add(...)` 全局可见。
//! - **包虚拟化**:typst-eval 解析 `@ns/name:ver` 时只向 World 索取
//!   `VirtualRoot::Package(spec)` 下的 `typst.toml` 与入口文件
//!   (见 `typst-eval/src/import.rs::resolve_package`)。本 World 对
//!   指定 spec 返回虚拟 manifest 与 shim 入口(如 `#import cetz: *`),
//!   原生模块即被当作该版本的包提供;未虚拟化的 spec 照常走
//!   `file()`/`source()` 的其他来源。
//!
//! 闭包的纯 Rust 构造见本 crate 测试:`Closure` 字段全公开
//! (语法节点 + defaults + captured scope),`Func::from(Closure)` 即
//! typst 一等函数;`Func::with` 可组合出携带预置参数的函数。

use std::collections::HashMap;
use std::path::PathBuf;

use comemo::Track;
use typst::diag::{FileError, FileResult, SourceResult};
use typst::engine::{Route, Sink, Traced};
use typst::foundations::{
    Binding, Bytes, Context, Datetime, Duration, IntoValue, Module, Scope, StyleChain, Value,
};
use typst::introspection::EmptyIntrospector;
use typst::syntax::{
    FileId, RootedPath, Source, Span, SyntaxMode, VirtualPath, VirtualRoot,
    package::PackageSpec,
};
use typst::text::{Font, FontBook};
use typst::utils::LazyHash;
use typst::{Library, LibraryExt, World};

/// 以内存文件 + 注入库运行的 [`World`]。
///
/// 评估专用(`font()` 恒空),足够支撑不落排版的求值测试;后续需要
/// 排版对拍时再挂 FontBook。
/// 模块引擎值初始化函数的签名。
pub type ModuleInit =
    fn(&mut typst::engine::Engine, comemo::Tracked<Context>, &mut Scope) -> SourceResult<()>;

pub struct InjectWorld {
    library: LazyHash<Library>,
    book: LazyHash<FontBook>,
    main: FileId,
    sources: HashMap<FileId, Source>,
    files: HashMap<FileId, Bytes>,
    /// 外部包磁盘根(如 ~/.cache/typst/packages):未命中的包路径回退到这里。
    packages_root: Option<PathBuf>,
    /// 项目根磁盘目录:未命中的 Project 相对路径回退到这里读文件
    /// (如 wasm 插件等非源文件)。
    fs_root: Option<PathBuf>,
}

impl InjectWorld {
    /// 新建:主文件文本 + 要注入全局作用域的模块列表 `(名字, 模块)`。
    pub fn new(
        main_text: &str,
        modules: impl IntoIterator<Item = (&'static str, Module)>,
    ) -> Self {
        let main = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new("main.typ").expect("valid path"),
        )
        .intern();

        let mut library = Library::builder().build();
        for (name, module) in modules {
            library.global.scope_mut().define(name, module);
        }
        // build() 中 std 绑定持有 global 的另一个 Arc 副本;上面
        // scope_mut 的写时复制会让两者分叉,这里刷新保持一致。
        library.std = Binding::detached(library.global.clone().into_value());

        let packages_root = std::env::var_os("USER_HOME")
            .map(|h| PathBuf::from(h).join(".cache/typst/packages"))
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache/typst/packages")))
            .filter(|p| p.exists());
        let fs_root = std::env::var("TRUST_WORLD_ROOT").ok().map(PathBuf::from).filter(|p| p.exists());
        Self {
            library: LazyHash::new(library),
            book: LazyHash::new(FontBook::new()),
            main,
            sources: HashMap::from([(main, Source::new(main, main_text.to_string()))]),
            files: HashMap::new(),
            packages_root,
            fs_root,
        }
    }

    /// 注入原生模块并完成引擎值初始化:先注入模块(引擎值可引用模块内
    /// 函数),再用临时 Engine 驱动 `module_init` 合并顶层引擎值,最后以
    /// 更新后的模块重建 Library(重包 LazyHash 保持 comemo 哈希正确)。
    pub fn init_native_module(
        &mut self,
        name: &'static str,
        mut module: Module,
        init: Option<ModuleInit>,
    ) -> Module {
        self.inject_module(name, module.clone());
        if let Some(init) = init {
            let mut sink = Sink::new();
            let world: &dyn World = self;
            let traced = Traced::default();
            let mut engine = typst::engine::Engine {
                library: &self.library,
                world: world.track(),
                introspector: typst::utils::Protected::new(EmptyIntrospector.track()),
                traced: traced.track(),
                sink: sink.track_mut(),
                route: Route::default(),
            };
            let context = Context::none();
            init(&mut engine, context.track(), module.scope_mut())
                .expect("module_init 求值顶层引擎值失败");
            self.inject_module(name, module.clone());
            module
        } else {
            module
        }
    }

    /// 以模块替换全局作用域中的同名定义并重建 Library。
    fn inject_module(&mut self, name: &'static str, module: Module) {
        let mut library = Clone::clone(&*self.library);
        // bind 而非 define:允许替换同名定义(module_init 后的更新)。
        library
            .global
            .scope_mut()
            .bind(name.into(), Binding::detached(module));
        library.std = Binding::detached(library.global.clone().into_value());
        self.library = typst::utils::LazyHash::new(library);
    }

    /// 包 FileId 到磁盘仓库路径(仅 VirtualRoot::Package)。
    fn package_path(&self, id: FileId) -> Option<PathBuf> {
        let root = self.packages_root.as_ref()?;
        let VirtualRoot::Package(spec) = id.root() else { return None; };
        // 布局:{root}/{namespace}/{name}/{version}/{vpath}。
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

    /// 追加一个相对主文件的源文件(多文件包形态)。
    pub fn add_source(&mut self, path: &str, text: &str) {
        let id = RootedPath::new(
            VirtualRoot::Project,
            VirtualPath::new(path).unwrap_or_else(|_| panic!("invalid path {path}")),
        )
        .intern();
        self.sources.insert(id, Source::new(id, text.to_string()));
    }

    /// 虚拟化一个包:`spec` 的包解析到 `entry` 入口文件,内容为
    /// `entry_text`(shim,通常一行 `#import <全局模块>: *`)。
    ///
    /// 由此,`#import "<spec>": ...` 不经磁盘与网络,直接落到注入的
    /// 原生模块;其他包不受影响。
    pub fn add_virtual_package(&mut self, spec: &str, entry: &str, entry_text: &str) -> Result<(), String> {
        let spec: PackageSpec =
            spec.parse().map_err(|err| format!("invalid package spec: {err}"))?;
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

    /// 以 typst-eval 求值主文件,得到模块(与 typst 编译器的模块求值
    /// 同一路径,含包解析)。
    pub fn eval_main(&self) -> SourceResult<Module> {
        let mut sink = Sink::new();
        let source = self
            .sources
            .get(&self.main)
            .cloned()
            .expect("main source present");
        // Track 只实现于 `dyn World`,先升格再 track(与 typst::compile 同法)。
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

    /// 以代码模式求值一段字符串;`scope` 成为 VM 的顶层作用域,
    /// 可直接注入额外绑定(差分测试的执行口)。
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
        if let Some(source) = self.sources.get(&id) {
            return Ok(source.clone());
        }
        // 包路径回退:磁盘上的包仓库。
        if let Some(path) = self.package_path(id) {
            let text = std::fs::read_to_string(&path)
                .map_err(|_| FileError::NotFound(path.clone()))?;
            return Ok(Source::new(id, text));
        }
        Err(FileError::NotFound(PathBuf::from("not in this world")))
    }

    fn file(&self, id: FileId) -> FileResult<Bytes> {
        if let Some(bytes) = self.files.get(&id) {
            return Ok(bytes.clone());
        }
        // 对源文件,提供其文本的字节视图(World trait 文档认可的等价物)。
        if let Some(src) = self.sources.get(&id) {
            return Ok(Bytes::from_string(src.text().to_string()));
        }
        if let Some(path) = self.package_path(id) {
            let bytes = std::fs::read(&path).map_err(|_| FileError::NotFound(path.clone()))?;
            return Ok(Bytes::new(bytes));
        }
        // 项目根文件回退(wasm 插件、数据文件等)。vpath 以 `/` 开头,
        // join 前必须剥掉,否则会把根整个替换掉。
        if let Some(root) = &self.fs_root
            && id.root() == &VirtualRoot::Project
        {
            let rel = id.vpath().get_with_slash().trim_start_matches('/');
            let path = root.join(rel);
            if path.exists() {
                let bytes =
                    std::fs::read(&path).map_err(|_| FileError::NotFound(path.clone()))?;
                return Ok(Bytes::new(bytes));
            }
        }
        Err(FileError::NotFound(PathBuf::from("not in this world")))
    }


    fn font(&self, _index: usize) -> Option<Font> {
        None
    }

    fn today(&self, _offset: Option<Duration>) -> Option<Datetime> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use typst::foundations::{Args, Closure, ClosureNode, Func};

    /// 在纯 Rust 里构造 `(a, b) => a + b`:
    /// 语法节点来自官方解析器,defaults 在创建期已求值(这里无命名参数),
    /// captured 为空 Scope——字段与求值语义对照 `typst-eval` 的
    /// `Eval for ast::Closure`。
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

        // 代码模式 + 全局可见性
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

        // 代码模式同样走包解析路径
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
        // Func::with(vanilla 公开组合子)预置参数,得到携带状态的函数;
        // 等价于 fork 里为 palette.new 加的 with_args。
        let add = add_closure();
        let mut args = Args::new(Span::detached(), [Value::Int(2)]);
        let add2 = add.with(&mut args);

        let mut scope = Scope::new();
        scope.define("add2", add2);
        let world = InjectWorld::new("", vec![]);
        let v = world.eval_code("add2(3)", scope).expect("call composed func");
        assert_eq!(v, Value::Int(5));
    }
}
