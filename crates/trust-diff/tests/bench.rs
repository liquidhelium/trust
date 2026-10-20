//! 性能对照:解释器(golden)与生成物执行同一段纯计算,
//! 证明 L0 直译层已消除解释器每调用常数(nuitka 路线的核心收益)。
//!
//! comemo 会按参数 memoize,因此每轮计时使用递增参数避开缓存:
//!
//! `cargo test -p trust-diff --release --test bench -- --nocapture --ignored`

use std::time::Instant;

use trust_world::InjectWorld;
use typst::foundations::{Module, Scope};

const BODY: &str = include_str!("../fixtures/bench.typ");

/// 一轮负载(带偏移避免缓存命中):返回 (调用表达式, 期望值)。
fn round(calls: &[&str], offset: i64) -> Vec<String> {
    calls
        .iter()
        .map(|c| c.replace("N", &offset.to_string()))
        .collect()
}

fn run_interpreted(calls: &[String]) -> Vec<String> {
    let world = InjectWorld::new(BODY, vec![]);
    let module: Module = world.eval_main().expect("golden eval");
    let mut scope = Scope::new();
    scope.define("m", module.clone());
    calls
        .iter()
        .map(|call| {
            format!("{:?}", world.eval_code(call, scope.clone()).expect("golden run"))
        })
        .collect()
}

fn run_compiled(calls: &[String]) -> Vec<String> {
    let module = trust_diff::gen_modules::bench::module();
    let world = InjectWorld::new("", vec![]);
    let mut scope = Scope::new();
    scope.define("m", module.clone());
    calls
        .iter()
        .map(|call| {
            format!("{:?}", world.eval_code(call, scope.clone()).expect("rust run"))
        })
        .collect()
}

#[test]
#[ignore = "基准:显式运行"]
fn interpreter_vs_compiled() {
    // 模板里的 N 由每轮偏移替换;先对拍正确性。
    // make-fib(N)(..):工厂每轮新参数 → 新闭包实例 → 两侧都无 memoize 红利。
    let templates = [
        "m.fib-mod(12000 + N)",
        "m.sum-nums(20000 + N)",
        "m.repeat-join(3000 + N)",
        "m.make-fib(N)(21)",
    ];

    let first = round(&templates, 0);
    let (g, r) = (run_interpreted(&first), run_compiled(&first));
    assert_eq!(g, r, "基准负载差分失败:\n{g:?}\n{r:?}");

    let mut interp_total = 0u128;
    let mut comp_total = 0u128;
    for offset in 0..5 {
        let calls = round(&templates, offset);

        let start = Instant::now();
        run_interpreted(&calls);
        interp_total += start.elapsed().as_millis();

        let start = Instant::now();
        run_compiled(&calls);
        comp_total += start.elapsed().as_millis();
    }
    println!("interpreter: {interp_total} ms (5 轮合计)");
    println!("compiled:    {comp_total} ms (5 轮合计)");
    println!("speedup:     {:.1}x", interp_total as f64 / comp_total as f64);
}
