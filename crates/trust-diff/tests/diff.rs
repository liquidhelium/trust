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
            "m.pt-of(3)",
            "m.deg-sum(10deg)",
            "m.half()",
            "m.em-plus(1em)",
        ],
        "control" => vec![
            "m.fact(1)",
            "m.fact(5)",
            "m.fact(10)",
            "m.fib(1)",
            "m.fib(10)",
            "m.sum-to(100)",
            "m.greet(\"Ada\")",
            "m.greet(\"Ada\", formal: true)",
            "m.shadow(41)",
            "m.join-words((\"a\", \"b\", \"c\"))",
            "m.classify(-3)",
            "m.classify(0)",
            "m.classify(9)",
            "m.first-over((1, 2, 9, 1), 5)",
            "m.sum-odds(range(10))",
            "m.skip-to((1, 2, 3, 2, 1), 3)",
            "m.find-first((4, 9, 2), x => x > 5)",
            "m.find-first((4, 1, 2), x => x > 5)",
            "m.accumulate(range(10))",
        ],
        "collections" => vec![
            "m.make-range(5)",
            "m.total((1, 2, 3))",
            "m.doubled((1, 2, 3))",
            "m.evens(range(10))",
            "m.sorted-desc((3, 1, 2))",
            "m.at-or((1, 2), 1)",
            "m.at-or((1, 2), 9)",
            "m.count-in(2, (1, 2, 3))",
            "m.count-in(9, (1, 2, 3))",
            "m.dict-make(1, 2)",
            "m.dict-get((a: 10, b: 32))",
            "m.dict-keys((x: 1, y: 2))",
            "m.dict-spread((a: 1))",
            "m.pair-up((\"a\", \"b\"), (1, 2))",
            "m.merge-all(((a: 1), (b: 2), (a: 3)))",
        ],
        "closure" => vec![
            "m.adder(10)(5)",
            "m.compose(calc.sqrt, x => x * 2)(16)",
            "m.apply-twice(x => x * 3, 2)",
            "m.greet2(\"Bob\")",
            "m.greet2(\"Bob\", greeting: \"Yo\")",
            "m.variadic(1, 2, 3, k: 4)",
            "m.kwargs(1, a: 2).pos()",
            "m.kwargs(1, a: 2).at(\"a\")",
            "m.fold-nums((1, 2, 3), 10, (acc, x) => acc - x)",
            "m.zip-with((a, b) => a * b, (1, 2), (3, 4))",
        ],
        "stdlib" => vec![
            "m.hypot(3, 4)",
            "m.clamp01(-1)",
            "m.clamp01(0.5)",
            "m.mins((3, 1, 2))",
            "m.pow-of-two(10)",
            "m.shout(\"abc\")",
            "m.str-len(\"héllo\")",
            "m.to-int(\"42\")",
            "m.repeat-str(\"ab\", 3)",
            "m.abs-sum((-1, 2, -3))",
        ],
        "content" => vec![
            "m.label(\"beta\")",
            "m.badge(\"gold\", 3)",
            "m.plain()",
            "m.multiline()",
            "m.mixed(21)",
            "m.joined(\"x\", 5)",
        ],
        "destructuring" => vec![
            "m.fst-snd((3, 4))",
            "m.swap-xy((1, 2))",
            "m.head-tail((1, 2, 3))",
            "m.dict-pick((a: 3, b: 4))",
            "m.dict-rest((a: 1, b: 2, c: 3))",
            "m.sum-pairs(((1, 2), (3, 4)))",
            "m.reassign((10, 4))",
            "m.nested(((1, 2), 3))",
            "m.set-deep((a: (b: 1)))",
            "m.swap-at((1, 2, 3))",
        ],
        "toplevel" => vec!["m.squares5", "m.golden-name", "m.mean-of", "m.mean(3, 5)"],
        "pkg_shapes" => vec![
            "m.area(3, 4)",
            "m.describe(\"rect\", 2.5, 3.25)",
            "m.scale-area(2, 3)",
            "m.scale-area(2, 3, k: 10)",
            "m.golden-ratio()",
            "m.utils.clamp(5, 1, 3)",
            "m.utils.lerp(0, 10, 0.25)",
            "m.pad(\"x\")",
            "m.dec(3.14159)",
        ],
        "sysinputs" => vec!["m.who", "m.shout(\"hi\")"],
        "set_show" => vec!["m.styled", "m.cond(true)", "m.cond(false)", "m.shown"],
        "context" => vec!["m.ctx-size", "m.ctx-call"],
        "pkg_include" => vec!["m.greeting", "m.after"],
        "regress" => vec![
            // A1 状态函数解构参数(stub 转发)。
            "m.pat((1, 2))",
            "m.pat((1, 2), k: 5)",
            "m.pat((1, 2), k: 100)",
            // A3 多段赋值。
            "m.deep()",
            // A4 while 值。
            "m.count-to(0)",
            "m.count-to(3)",
            // A5 代码块 join。
            "m.block-join()",
            "m.block-none()",
            "m.block-tail-none()",
            // A6 默认值。
            "m.def-ok()",
            "m.def-ok(x: 1)",
            // B10 深度护栏内的自递归。
            "m.down(0)",
            "m.down(40)",
            // C2 自递归闭包(命名,体引用自身)。
            "m.self-rec(0)",
            "m.self-rec(1)",
            "m.self-rec(5)",
            "m.self-rec(30)",
            // C2 值位置闭包(多捕获 + 复合算术)。
            "m.big-closure(3, 4)",
            "m.big-closure(1, -2)",
            // Equation 经源码片段解释求值。
            "m.eq-simple()",
            "m.eq-sym()",
            "m.eq-captured(2)",
        ],
        _ => vec![],
    }
}

/// fixture 源码(构建期同一份文件)。
fn fixture_src(name: &str) -> String {
    let path = format!("fixtures/{name}.typ");
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read fixture {name}"))
}

/// 包 fixture 内单个文件。
fn fixture_file(name: &str, rel: &str) -> String {
    let path = format!("fixtures/{name}/{rel}");
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {path}"))
}

/// 包 fixture 的全部 (相对路径, 源码)。
fn pkg_files(name: &str) -> Vec<(String, String)> {
    fn walk(dir: &std::path::Path, prefix: &str, out: &mut Vec<(String, String)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let file = entry.file_name().to_string_lossy().into_owned();
            if path.is_dir() {
                walk(&path, &format!("{prefix}{file}/"), out);
            } else if path.extension().and_then(|e| e.to_str()) == Some("typ") {
                out.push((
                    format!("{prefix}{file}"),
                    std::fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }
    let mut out = Vec::new();
    walk(
        std::path::Path::new(&format!("fixtures/{name}")),
        "",
        &mut out,
    );
    out
}

/// 双路执行并比较:golden = 解释器求值 fixture 模块;rust = 生成物模块。
fn differential(fixture: &str) {
    let calls = cases(fixture);
    if calls.is_empty() {
        panic!("fixture {fixture} 没有对拍用例");
    }

    // golden:解释器(包 fixture 需装配全部源文件)。
    let golden_world = if fixture.starts_with("pkg_") {
        let mut world = InjectWorld::new(&fixture_file(fixture, "entry.typ"), vec![]);
        for (rel, src) in pkg_files(fixture) {
            if rel != "entry.typ" {
                world.add_source(&rel, &src);
            }
        }
        world
    } else {
        InjectWorld::new(&fixture_src(fixture), vec![])
    };
    let golden: Module = golden_world.eval_main().expect("golden 模块求值");
    let mut golden_scope = Scope::new();
    golden_scope.define("m", golden.clone());

    // rust:生成物(module_init = 模块求值的镜像,per-build 槽板)。
    let mut rust_world = InjectWorld::new("", vec![]);
    let rust_module = rust_world.init_native_module("m", gen_module_init(fixture));
    // 模块体对拍(顶层内容与 include)。
    let (gb, rb) = (
        format!("{:?}", golden.content()),
        format!("{:?}", rust_module.clone().content()),
    );
    assert_eq!(gb, rb, "模块 body 不一致(输入 {fixture})");

    let mut rust_scope = Scope::new();
    rust_scope.define("m", rust_module.clone());

    // 错误路径对拍:双侧都须报错,消息文案逐字一致(定位不比:
    // 两侧文件 id 不同;错误站点 span 映射见 KNOWN-ISSUES #4 的修复)。
    for call in error_cases(fixture) {
        let g = golden_world.eval_code(call, golden_scope_clone(&golden_scope));
        let r = rust_world.eval_code(call, golden_scope_clone(&rust_scope));
        let render = |res: typst::diag::SourceResult<typst::foundations::Value>| match res {
            Err(diags) => diags
                .iter()
                .map(|d| {
                    let mut parts = vec![d.message.to_string()];
                    parts.extend(d.hints.iter().map(|h| format!("hint: {h:?}")));
                    parts.join(" | ")
                })
                .collect::<Vec<_>>()
                .join("; "),
            Ok(value) => format!("(未报错,返回 {value:?})"),
        };
        let (gm, rm) = (render(g), render(r));
        assert_eq!(gm, rm, "错误用例 {call}:两侧消息不一致");
    }

    let mut failures = Vec::new();
    for call in calls {
        let g = golden_world
            .eval_code(call, golden_scope_clone(&golden_scope))
            .unwrap_or_else(|e| panic!("golden 执行失败 {call}: {e:?}"));
        let r = rust_world
            .eval_code(call, golden_scope_clone(&rust_scope))
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

fn error_cases(fixture: &str) -> Vec<&'static str> {
    // 通用运算错误(两侧共用 typst ops,验证接线与 span 不进文案)。
    let mut cases = vec!["1 / 0", "2 + \"x\"", "\"a\" - \"b\"", "none + 1"];
    match fixture {
        // 生成物运行时复刻的文案(rt::boolean/iter_values/d_array_finish)。
        "control" => cases.push("m.bool-fail()"),
        "collections" => cases.push("m.iter-fail()"),
        "destructuring" => cases.push("m.destr-fail()"),
        // A/B 批修复的错误路径(文案+hint 逐字)。
        "regress" => cases.extend([
            "m.mid-missing()",
            "m.compound-missing()",
            "m.always-true()",
            "m.spin()",
            "m.block-join-fail()",
            "m.not-fn(1)",
            "m.mk().a(2)",
            "\"hello\".nope(1)",
            "m.down(10000)",
        ]),
        _ => {}
    }
    cases
}

fn golden_scope_clone(scope: &Scope) -> Scope {
    // eval_code 会消耗 scope;逐用例重建。
    scope.clone()
}

fn gen_module_init(fixture: &str) -> trust_world::ModuleInit {
    match fixture {
        "arith" => trust_diff::gen_modules::arith::module_init,
        "control" => trust_diff::gen_modules::control::module_init,
        "collections" => trust_diff::gen_modules::collections::module_init,
        "closure" => trust_diff::gen_modules::closure::module_init,
        "stdlib" => trust_diff::gen_modules::stdlib::module_init,
        "pkg_shapes" => trust_diff::gen_modules::pkg_shapes::module_init,
        "bench" => trust_diff::gen_modules::bench::module_init,
        "content" => trust_diff::gen_modules::content::module_init,
        "toplevel" => trust_diff::gen_modules::toplevel::module_init,
        "destructuring" => trust_diff::gen_modules::destructuring::module_init,
        "sysinputs" => trust_diff::gen_modules::sysinputs::module_init,
        "set_show" => trust_diff::gen_modules::set_show::module_init,
        "context" => trust_diff::gen_modules::context::module_init,
        "pkg_include" => trust_diff::gen_modules::pkg_include::module_init,
        "regress" => trust_diff::gen_modules::regress::module_init,
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

diff_test!(
    arith,
    control,
    collections,
    closure,
    stdlib,
    pkg_shapes,
    content,
    toplevel,
    destructuring,
    sysinputs,
    set_show,
    context,
    pkg_include,
    regress
);

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
    let covered: Vec<&str> = vec![
        "arith",
        "control",
        "collections",
        "closure",
        "stdlib",
        "pkg_shapes",
        "bench",
        "content",
        "toplevel",
        "destructuring",
        "sysinputs",
        "set_show",
        "context",
        "pkg_include",
        "regress",
    ];
    for name in declared.keys() {
        assert!(
            covered.contains(&name.as_str()),
            "fixture {name} 未登记用例(cases/gen_module)"
        );
    }
}
