//! trust-suite:官方 typst tests/suite(+ `reg/` 真实回归)的仓库内
//! 双世界(解释 vs 翻译装配)对拍。先跑 `cargo run -p trust-suite --bin
//! refresh` 生成 cats/gen/manifest/挂载 crate,再 `cargo test -p
//! trust-suite --release`。

pub mod split;
