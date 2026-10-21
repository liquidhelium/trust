//! 翻译上下文与名字解析环境。
//!
//! `Ctx` 贯穿翻译全程:诊断累积、宽松模式开关、临时 id 计数,以及包
//! 模式的文件上下文。`Env` 是静态作用域的名字解析表(局部栈 + 可见
//! 模块槽);`Ref` 是模块槽的引用形态;`Mode` 区分函数体(引擎在手)
//! 与顶层值初始化(免引擎)。

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
}

/// 模块绑定的引用形态:普通槽(LazyLock)或引擎槽(OnceLock + getter)。
#[derive(Clone)]
pub(super) enum Ref {
    Slot(String),
    Engine(String),
    /// 来自模块值字段的绑定(`import deps.oxifmt: strfmt`)。
    ModuleField {
        expr: String,
        name: String,
    },
    /// 直接以表达式求值的绑定(模块值本身或其字段)。
    Value(String),
}

/// 名字解析环境:局部栈 + 可见模块槽(静态作用域,按声明序累积)。
#[derive(Clone)]
pub(super) struct Env {
    pub(super) locals: Vec<(String, String)>,
    pub(super) module: Vec<(String, Ref)>,
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum Mode {
    /// 函数体:引擎在手,全部构造可用。
    FnBody,
    /// 顶层值初始化:免引擎,涉及调用/标准库引用即报错。
    ModuleInit,
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
}

/// 在局部栈压入新绑定,返回未冲突的 rust 名。
pub(super) fn bind_local(env: &mut Env, name: &str) -> String {
    let rust = fresh_rust_ident(name, &env.locals);
    env.locals.push((name.to_string(), rust.clone()));
    rust
}
