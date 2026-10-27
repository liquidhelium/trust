//! Official typst tests/suite (+ `reg/` regressions) differential harness
//! host package. `cargo suite` regenerates the translated corpus under
//! `build/` (gitignored); `cargo testit` runs the comparison through the
//! generated driver copy. Aliases live in `.cargo/config.toml`.

pub mod split;
