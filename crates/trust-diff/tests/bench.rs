//! 性能对照:解释器(golden)与生成物执行同一段纯计算,
//! 证明 L0 直译层已消除解释器每调用常数(nuitka 路线的核心收益)。
//!
//! comemo 会按参数 memoize,因此每轮计时使用递增参数避开缓存;
//! 模块装配(module_init / eval_main)在计时区外各做一次:
//!
//! `cargo test -p trust-diff --release --test bench -- --nocapture --ignored`

use std::time::Instant;

use trust_world::InjectWorld;
use typst::foundations::{Module, Scope};

const BODY: &str = include_str!("../fixtures/bench.typ");

/// 一轮负载(带偏移避免缓存命中):返回调用表达式。
fn round(calls: &[&str], offset: i64) -> Vec<String> {
    calls
        .iter()
        .map(|c| c.replace("N", &offset.to_string()))
        .collect()
}

/// 装配好的执行侧:World + `m` 模块作用域。
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
#[ignore = "性能对照,显式运行"]
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
    assert_eq!(g, r, "基准负载差分失败:\n{g:?}\n{r:?}");

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
    println!("interpreter: {interp_total} ms (5 轮合计)");
    println!("compiled:    {comp_total} ms (5 轮合计)");
    println!(
        "speedup:     {:.1}x",
        interp_total as f64 / comp_total as f64
    );
}
