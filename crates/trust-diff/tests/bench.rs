//! Performance comparison: the interpreter (golden) and the generated code
//! run the same pure computation, measuring whether the direct-translation
//! layer removes the interpreter's per-call constant overhead (the core
//! benefit sought from a Nuitka-style approach).
//!
//! comemo memoizes by arguments, so each timed round uses increasing
//! arguments to stay off the cache; module assembly (module_init /
//! eval_main) runs once per side outside the timed region:
//!
//! `cargo test -p trust-diff --release --test bench -- --nocapture --ignored`

use std::time::Instant;

use trust_world::InjectWorld;
use typst::foundations::{Module, Scope};

const BODY: &str = include_str!("../fixtures/bench.typ");

/// One round of workload (offset avoids cache hits): returns the call
/// expressions.
fn round(calls: &[&str], offset: i64) -> Vec<String> {
    calls
        .iter()
        .map(|c| c.replace("N", &offset.to_string()))
        .collect()
}

/// An assembled execution side: World + the `m` module scope.
struct Side {
    world: InjectWorld,
    scope: Scope,
}

fn setup_interpreted() -> Side {
    let world = InjectWorld::new(BODY, vec![]);
    let module: Module = world.eval_main().expect("golden eval");
    let mut scope = Scope::new();
    scope.define("m", module.clone());
    Side { world, scope }
}

fn setup_compiled() -> Side {
    let mut world = InjectWorld::new("", vec![]);
    let module = world.init_native_module("m", trust_diff::gen_modules::bench::module_init);
    let mut scope = Scope::new();
    scope.define("m", module.clone());
    Side { world, scope }
}

fn run(side: &Side, calls: &[String]) -> Vec<String> {
    calls
        .iter()
        .map(|call| {
            format!(
                "{:?}",
                side.world.eval_code(call, side.scope.clone()).expect("run")
            )
        })
        .collect()
}

#[test]
#[ignore = "performance comparison; run explicitly"]
fn interpreter_vs_compiled() {
    let templates = [
        "m.fib-mod(12000 + N)",
        "m.sum-nums(20000 + N)",
        "m.repeat-join(3000 + N)",
        "m.call-loop(8000 + N)",
        "m.map-heavy(2000 + N)",
    ];

    let golden = setup_interpreted();
    let compiled = setup_compiled();
    let first = round(&templates, 0);
    let (g, r) = (run(&golden, &first), run(&compiled, &first));
    assert_eq!(g, r, "bench workload mismatch:\n{g:?}\n{r:?}");

    let mut interp_total = 0u128;
    let mut comp_total = 0u128;
    for template in &templates {
        let mut interp_one = 0u128;
        let mut comp_one = 0u128;
        for offset in 0..5 {
            let calls = round(std::slice::from_ref(template), offset);

            let start = Instant::now();
            run(&golden, &calls);
            interp_one += start.elapsed().as_millis();

            let start = Instant::now();
            run(&compiled, &calls);
            comp_one += start.elapsed().as_millis();
        }
        interp_total += interp_one;
        comp_total += comp_one;
        println!(
            "  {:16} interp {interp_one:5} ms  compiled {comp_one:5} ms  {:.1}x",
            template.split('(').next().unwrap_or(template),
            interp_one as f64 / comp_one as f64
        );
    }
    println!("interpreter: {interp_total} ms (5 rounds total)");
    println!("compiled:    {comp_total} ms (5 rounds total)");
    println!(
        "speedup:     {:.1}x",
        interp_total as f64 / comp_total as f64
    );
}
