//! Differential test driver: golden (interpreter) ↔ generated code — the
//! same call runs on both sides and the results are compared.

use std::collections::BTreeMap;

use trust_world::InjectWorld;
use typst::foundations::{Module, Scope};

/// Comparison cases for one fixture: a list of call expressions, executed
/// on the module via the `m.` prefix.
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
        "pkg_nested_item" => vec!["m.renamed()", "m.plain()"],
        "sysinputs" => vec!["m.who", "m.shout(\"hi\")"],
        "set_show" => vec!["m.styled", "m.cond(true)", "m.cond(false)", "m.shown"],
        "context" => vec!["m.ctx-size", "m.ctx-call"],
        "pkg_include" => vec!["m.greeting", "m.after"],
        "regress" => vec![
            // Stateful function with a destructuring parameter (stub
            // forwarding).
            "m.pat((1, 2))",
            "m.pat((1, 2), k: 5)",
            "m.pat((1, 2), k: 100)",
            // Multi-segment assignment.
            "m.deep()",
            // Value of a while loop.
            "m.count-to(0)",
            "m.count-to(3)",
            // Code-block join.
            "m.block-join()",
            "m.block-none()",
            "m.block-tail-none()",
            // Default values.
            "m.def-ok()",
            "m.def-ok(x: 1)",
            // Self-recursion within the depth guardrail.
            "m.down(0)",
            "m.down(40)",
            // Self-recursive closure (named; the body references itself).
            "m.self-rec(0)",
            "m.self-rec(1)",
            "m.self-rec(5)",
            "m.self-rec(30)",
            // Value-position closure (multiple captures + compound
            // arithmetic).
            "m.big-closure(3, 4)",
            "m.big-closure(1, -2)",
            // Equations interpreted from source snippets.
            "m.eq-simple()",
            "m.eq-sym()",
            "m.eq-captured(2)",
        ],
        _ => vec![],
    }
}

/// Fixture source (the same file the build consumed).
fn fixture_src(name: &str) -> String {
    let path = format!("fixtures/{name}.typ");
    std::fs::read_to_string(path).unwrap_or_else(|_| panic!("read fixture {name}"))
}

/// A single file inside a package fixture.
fn fixture_file(name: &str, rel: &str) -> String {
    let path = format!("fixtures/{name}/{rel}");
    std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {path}"))
}

/// All (relative path, source) pairs of a package fixture.
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

/// Runs both sides and compares: golden = the interpreter evaluating the
/// fixture module; rust = the generated module.
fn differential(fixture: &str) {
    let calls = cases(fixture);
    if calls.is_empty() {
        panic!("fixture {fixture} has no comparison cases");
    }

    // Golden: the interpreter (package fixtures need all source files
    // assembled).
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
    let golden: Module = golden_world.eval_main().expect("golden module evaluation");
    let mut golden_scope = Scope::new();
    golden_scope.define("m", golden.clone());

    // Rust: the generated code (module_init = the mirror of module
    // evaluation, per-build slots).
    let mut rust_world = InjectWorld::new("", vec![]);
    let rust_module = rust_world.init_native_module("m", gen_module_init(fixture));
    // Compare module bodies (top-level content and includes).
    let (gb, rb) = (
        format!("{:?}", golden.content()),
        format!("{:?}", rust_module.clone().content()),
    );
    assert_eq!(gb, rb, "module body mismatch (input {fixture})");

    let mut rust_scope = Scope::new();
    rust_scope.define("m", rust_module.clone());

    // Compare error paths: both sides must error with verbatim-identical
    // messages (locations are not compared: the two sides' file ids differ).
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
            Ok(value) => format!("(no error, returned {value:?})"),
        };
        let (gm, rm) = (render(g), render(r));
        assert_eq!(gm, rm, "error case {call}: messages differ between sides");
    }

    let mut failures = Vec::new();
    for call in calls {
        let g = golden_world
            .eval_code(call, golden_scope_clone(&golden_scope))
            .unwrap_or_else(|e| panic!("golden execution failed for {call}: {e:?}"));
        let r = rust_world
            .eval_code(call, golden_scope_clone(&rust_scope))
            .unwrap_or_else(|e| panic!("rust execution failed for {call}: {e:?}"));
        let (g, r) = (format!("{g:?}"), format!("{r:?}"));
        if g != r {
            failures.push(format!("  {call}\n    golden: {g}\n    rust:   {r}"));
        }
    }
    assert!(
        failures.is_empty(),
        "fixture {fixture} differential failure ({} cases):\n{}",
        failures.len(),
        failures.join("\n")
    );
}

fn error_cases(fixture: &str) -> Vec<&'static str> {
    // Generic operator errors (both sides share typst ops; this checks the
    // wiring and that spans do not leak into messages).
    let mut cases = vec!["1 / 0", "2 + \"x\"", "\"a\" - \"b\"", "none + 1"];
    match fixture {
        // Messages reproduced by the generated runtime
        // (rt::boolean/iter_values/d_array_finish).
        "control" => cases.push("m.bool-fail()"),
        "collections" => cases.push("m.iter-fail()"),
        "destructuring" => cases.push("m.destr-fail()"),
        // Error paths of fixed regressions (message + hint verbatim).
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
    // eval_code consumes the scope; rebuild it per case.
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
        "pkg_nested_item" => trust_diff::gen_modules::pkg_nested_item::module_init,
        "regress" => trust_diff::gen_modules::regress::module_init,
        _ => panic!("unregistered fixture {fixture}"),
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
    pkg_nested_item,
    set_show,
    context,
    pkg_include,
    regress
);

#[test]
fn all_fixtures_covered() {
    // Every file in the fixtures directory must be registered with cases
    // and a generated module.
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
        "pkg_nested_item",
        "regress",
    ];
    for name in declared.keys() {
        assert!(
            covered.contains(&name.as_str()),
            "fixture {name} has no registered cases (cases/gen_module)"
        );
    }
}
