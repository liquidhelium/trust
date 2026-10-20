//! 差分对拍驱动:golden(解释器)↔ 生成物,同一调用两侧比较。

use std::collections::BTreeMap;

use trust_world::InjectWorld;
use typst::foundations::{Module, Scope};

/// 一个 fixture 的对拍用例:(调用表达式) 列表,以 `m.` 前缀在模块上执行。
fn cases(fixture: &str) -> Vec<&'static str> {
    match fixture {
        "arith" => vec![
            "m.add(1, 2)",
            "m.add(1.5, 2.25)",
            "m.mul(3, 4)",
            "m.fdiv(7, 2)",
            "m.fdiv(3.0, 2)",
            "m.neg(5)",
            "m.concat(\"foo\", \"bar\")",
            "m.max2(3, 9)",
            "m.max2(9, 3)",
            "m.max2(2.5, 2.5)",
            "m.both(true, true)",
            "m.both(true, false)",
            "m.either(false, true)",
            "m.either(false, false)",
            "m.clamp(5, 1, 10)",
            "m.clamp(0, 1, 10)",
            "m.clamp(11, 1, 10)",
            "m.contains-needle(\"foobar\", \"bar\")",
            "m.contains-needle((1, 2, 3), 2)",
        ],
        _ => vec![],
    }
}

/// fixture 源码(构建期同一份文件)。
fn fixture_src(name: &str) -> String {
    let path = format!("fixtures/{name}.typ");
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read fixture {name}"))
}

/// 双路执行并比较:golden = 解释器求值 fixture 模块;rust = 生成物模块。
fn differential(fixture: &str) {
    let calls = cases(fixture);
    if calls.is_empty() {
        panic!("fixture {fixture} 没有对拍用例");
    }

    // golden:解释器。
    let golden_world = InjectWorld::new(&fixture_src(fixture), vec![]);
    let golden: Module = golden_world.eval_main().expect("golden 模块求值");
    let mut golden_scope = Scope::new();
    golden_scope.define("m", golden.clone());

    // rust:生成物。
    let rust_module = gen_module(fixture);
    let rust_world = InjectWorld::new("", vec![]);
    let mut rust_scope = Scope::new();
    rust_scope.define("m", rust_module.clone());

    let mut failures = Vec::new();
    for call in calls {
        let g = golden_world
            .eval_code(&format!("{call}"), golden_scope_clone(&golden_scope))
            .unwrap_or_else(|e| panic!("golden 执行失败 {call}: {e:?}"));
        let r = rust_world
            .eval_code(&format!("{call}"), golden_scope_clone(&rust_scope))
            .unwrap_or_else(|e| panic!("rust 执行失败 {call}: {e:?}"));
        let (g, r) = (format!("{g:?}"), format!("{r:?}"));
        if g != r {
            failures.push(format!("  {call}\n    golden: {g}\n    rust:   {r}"));
        }
    }
    assert!(
        failures.is_empty(),
        "fixture {fixture} 差分失败({} 例):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn golden_scope_clone(scope: &Scope) -> Scope {
    // eval_code 会消耗 scope;逐用例重建。
    scope.clone()
}

fn gen_module(fixture: &str) -> Module {
    match fixture {
        "arith" => trust_diff::gen_modules::arith::module(),
        _ => panic!("未登记的 fixture {fixture}"),
    }
}

macro_rules! diff_test {
    ($($name:ident),* $(,)?) => {
        $(#[test]
        fn $name() {
            differential(stringify!($name));
        })*
    };
}

diff_test!(arith);

#[test]
fn all_fixtures_covered() {
    // fixtures 目录里的每个文件都必须有用例与生成物登记。
    let mut declared: BTreeMap<String, bool> = BTreeMap::new();
    for entry in std::fs::read_dir("fixtures").unwrap() {
        let name = entry
            .unwrap()
            .path()
            .file_stem()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        declared.insert(name, true);
    }
    let covered: Vec<&str> = vec!["arith"];
    for name in declared.keys() {
        assert!(
            covered.contains(&name.as_str()),
            "fixture {name} 未登记用例(cases/gen_module)"
        );
    }
}
