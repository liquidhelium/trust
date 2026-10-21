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
use super::naming::fresh_rust_ident;

pub(super) struct Ctx {
    pub(super) diags: Vec<Diag>,
    /// 宽松模式(探索):缺口降级为占位,不作为整体失败。
    pub(super) lenient: bool,
    pub(super) next_id: usize,
    /// 包模式下当前处理的文件路径(体内字符串 import 解析用)。
    pub(super) current_file: Option<String>,
    /// 包模式的文件集(字符串 import 解析)。
    pub(super) package_files: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 包模式:文件路径 → 生成 mod 名(体内字符串 import 取模块值)。
    pub(super) package_mods: Option<std::rc::Rc<std::collections::BTreeMap<String, String>>>,
    /// 当前函数体是否引用了模块槽(决定是否生成 `__st` 序言)。
    pub(super) used_state: bool,
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
}

/// 名字解析环境:局部栈 + 可见模块槽(静态作用域,按声明序累积)。
#[derive(Clone)]
pub(super) struct Env {
    pub(super) locals: Vec<(String, String)>,
    pub(super) module: Vec<(String, Ref)>,
}

impl Ctx {
    pub(super) fn fresh_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
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
                format!("rt::field(&({expr}), \"{name}\", SPAN)?")
            }
            Ref::Value(expr) => expr.clone(),
        }
    }
}

/// 在局部栈压入新绑定,返回未冲突的 rust 名。
pub(super) fn bind_local(env: &mut Env, name: &str) -> String {
    let rust = fresh_rust_ident(name, &env.locals);
    env.locals.push((name.to_string(), rust.clone()));
    rust
}
