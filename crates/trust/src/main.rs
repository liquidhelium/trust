//! `trust` CLI:转译器命令行入口(骨架)。

use std::env;

fn main() {
    let arg = env::args().nth(1);
    match arg.as_deref() {
        Some("-V" | "--version") => println!("trust {}", env!("CARGO_PKG_VERSION")),
        _ => {
            eprintln!(
                "trust {} — typst → rust 转译器(骨架;子命令按 docs/research-route.md 的 M0 落地)",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}
