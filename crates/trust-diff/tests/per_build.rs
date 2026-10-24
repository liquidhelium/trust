//! Boundary acceptance for the per-build slot scheme: two builds in the same
//! process with different `sys.inputs` values → each correct, with no stale
//! values leaking from one build into the other. The old implementation
//! (process-wide static slots) returned the first build's value on the
//! second build here.

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
        .expect("who is defined")
        .read()
        .clone();
    format!("{value:?}")
}

#[test]
fn inputs_isolated_per_build() {
    let a = build_who("alice");
    let b = build_who("bob");
    assert!(a.contains("alice"), "first build should see alice: {a}");
    assert!(b.contains("bob"), "second build should see bob: {b}");
}
