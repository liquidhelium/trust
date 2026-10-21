//! per-build 槽板的边界验收(见 `_things/DESIGN-per-build.md` §4):
//! 同进程两次构建、`sys.inputs` 取不同值 → 各自正确、无旧值残留。
//! 旧实现(进程级 static 槽)在此用例下第二次构建拿到旧值
//! (KNOWN-ISSUES #5)。

use trust_world::InjectWorld;
use typst::foundations::{Dict, IntoValue};

fn build_who(who: &str) -> String {
    let mut inputs = Dict::new();
    inputs.insert("who".into(), who.into_value());
    let mut world = InjectWorld::with_inputs("", vec![], inputs);
    let module = world.init_native_module("m", trust_diff::gen_modules::sysinputs::module_init);
    let value = module
        .scope()
        .get("who")
        .expect("who 已定义")
        .read()
        .clone();
    format!("{value:?}")
}

#[test]
fn inputs_isolated_per_build() {
    let a = build_who("alice");
    let b = build_who("bob");
    assert!(a.contains("alice"), "第一构建应见 alice:{a}");
    assert!(
        b.contains("bob"),
        "第二构建应见 bob(旧实现此处是 alice,KNOWN-ISSUES #5):{b}"
    );
}
