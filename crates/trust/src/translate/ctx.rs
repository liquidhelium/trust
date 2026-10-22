//! 翻译上下文与名字解析环境。
//!
//! `Ctx` 贯穿翻译全程:诊断累积、宽松模式开关、临时 id 计数,以及包
//! 模式的文件上下文。`Env` 是静态作用域的名字解析表(局部栈 + 可见
//! 模块槽);`Ref` 是模块槽的引用形态。
//!
//! 生成物里模块级绑定编译为 per-build 槽板字段(见
//! `_things/DESIGN-per-build.md`),所有引用都形如
//! `rt::read(&__st.槽名)?`;`used_state` 追踪当前函数是否发出过此类
//! 引用,决定函数体是否需要 `__st` 序言。

use super::Diag;
use super::naming::{escape, fresh_rust_ident};

pub(super) struct Ctx {
    pub(super) diags: Vec<Diag>,
    /// 宽松模式(探索):缺口降级为占位,不作为整体失败。
    pub(super) lenient: bool,
    pub(super) next_id: usize,
    /// 包模式下当前处理的文件路径(体内字符串 import 解析用)。
    pub(super) current_file: Option<String>,
    /// 包模式的文件集(字符串 import 解析)。
    pub(super) package_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 包模式:文件路径 �� 生成 mod 名(体内字符串 import 取模块值)。
    pub(super) package_mods: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 槽名→所属文件(模块值链通配解析:体内 import cetz.draw: *)。
    pub(super) slot_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 包模式:导出表(路径 → 名字 → 槽;体内**通配** import 的静态
    /// 展开用——目标为已翻译文件时其导出面在翻译期已知)。
    pub(super) export_table: Option<
        std::rc::Rc<std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>>,
    >,
    /// 当前函数体是否引用了模块槽(决定是否生成 `__st` 序言)。
    pub(super) used_state: bool,
    /// 值位置闭包(C2:闭包表达式也编译为原生体 + 签名保真 stub)的
    /// 去重表:键 = (闭包原文, 规范化捕获名集) → (id, 是否携带 __st)。
    /// 体代码是 (AST, 捕获名集) 的纯函数,同键必然同码;去重防同定义
    /// 闭包在多调用点膨胀。
    pub(super) closure_dedup:
        std::collections::HashMap<String, (String, bool)>,
    /// 每调用点节点静态的发射区(`NODE_cN`,与闭包项同区发射到根)。
    pub(super) closure_nodes: String,
    pub(super) next_node_id: usize,
    pub(super) next_closure_id: usize,
    /// C2 闭包 f_/D_ 汇集(发射到产物根部——跨文件去重后同一定义只
    /// 此一份,根级可见 __State 与全部导入)。
    pub(super) closure_items: String,
    /// 函数体嵌套深度(>0 表示当前在生成的 f_ 函数体内;`return`
    /// 仅在该上下文合法——外层 Rust 函数返回类型不同)。
    pub(super) in_fn: usize,
    /// 错误站点 span 表:(源文件路径, AST span 原始 u64)→ 站点索引
    /// (生成物以 `*S{idx}` 引用;运行期经 `rt::source_span` 以当前
    /// 进程的 FileId 编号物化——span 自包含,宿主无预 intern 约定)。
    pub(super) span_table: Vec<(String, u64)>,
    span_index: std::collections::HashMap<u64, usize>,
}

/// 模块绑定的引用形态(统一指向 per-build 槽板字段)。
#[derive(Clone)]
pub(super) enum Ref {
    /// 顶层绑定槽(每构建写一次)。
    Slot(String),
    /// 来自模块值字段的绑定(`import deps.oxifmt: strfmt`)。
    ModuleField { expr: String, name: String },
    /// 直接以表达式求值的绑定(模块值本身或其字段)。
    Value(String),
    /// 创建期物化的全局名:槽有值读槽(typst 创建期捕获),无值
    /// 回退调用期全局查找(typst 创建期不可见名字的语义)。
    GlobalFallback { slot: String, name: String },
}

/// 名字解析环境:局部栈 + 可见模块槽(静态作用域,按声明序累积)。
#[derive(Clone)]
pub(super) struct Env {
    pub(super) locals: Vec<(String, String)>,
    pub(super) module: Vec<(String, Ref)>,
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

    /// 为一个**编译期常量片段源**发射每调用点静态
    /// `pub(crate) static NODE_cN: LazyLock<(SyntaxNode, usize)>`,
    /// 返回静态名——热路径创建零锁零查找(静态即缓存)。
    pub(super) fn closure_node_static(&mut self, src: &str) -> String {
        let name = format!("NODE_c{}", self.next_node_id);
        self.next_node_id += 1;
        self.closure_nodes.push_str(&format!(
            "pub(crate) static {name}: std::sync::LazyLock<(typst_syntax::SyntaxNode, usize)> = std::sync::LazyLock::new(|| rt::parse_closure_node({src:?}));\n"
        ));
        name
    }

    /// 登记一个源 span,返回生成物里的引用 `*S{idx}`(错误站点用,
    /// 对照原版:错误的定位指向源表达式;生成物里是首次使用时按
    /// 当前进程 FileId 物化的 LazyLock——见 `rt::source_span`)。
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

    /// 登记源 span 并以其常量名填充格式串中的 `{at}` 占位。
    pub(super) fn span_fmt(&mut self, span: typst_syntax::Span, fmt: String) -> String {
        let at = self.span_of(span);
        fmt.replace("{at}", &at)
    }

    /// 生成错误站点 span 块(首次使用时以当前进程 FileId 物化——
    /// 自包含,无宿主预 intern 约定;无站点则空)。
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

    /// 把模块槽引用编译为表达式(标记当前函数用到 `__st`)。
    pub(super) fn ref_value(&mut self, module_ref: &Ref) -> String {
        self.used_state = true;
        match module_ref {
            Ref::Slot(id) => format!("rt::read(&__st.{id})?"),
            Ref::ModuleField { expr, name } => {
                format!("rt::field(&({expr}), \"{name}\", engine, context, SPAN)?")
            }
            Ref::Value(expr) => expr.clone(),
            Ref::GlobalFallback { slot, name } => {
                let name = escape(name);
                format!("rt::read_or(&__st.{slot}, engine, \"{name}\")?")
            }
        }
    }
}

/// 在局部栈压入新绑定,返回未冲突的 rust 名。
pub(super) fn bind_local(env: &mut Env, name: &str) -> String {
    let rust = fresh_rust_ident(name, &env.locals);
    env.locals.push((name.to_string(), rust.clone()));
    rust
}
