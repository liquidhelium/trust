//! Suite refresh: vendored typst tests/suite (+ `reg/` regressions) →
//! per-category synthetic packages, translated trees, manifest, and mount
//! crates. The Rust port of the former `_things/typst-tests-probe` python
//! pipeline; run `cargo run -p trust-suite --bin refresh` whenever
//! `suite/` or `reg/` changes, then `cargo test -p trust-suite --release`.
//!
//! Shape (one package per suite **source file**, ≈220 categories):
//! each package = that file's per-case sections (`__t__<name>.typ`,
//! mirroring the original relative path so relative/cross-dir imports
//! resolve) + **all** fixture files (section-less files, mirrored the same
//! way). Sections only import fixtures; packages have no cross-deps.
//! Many small crates instead of one big one: rustc's front end is
//! single-threaded per crate, one 10MB crate can burn an hour, while
//! ~200 ≤1MB crates build in parallel in minutes.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "../split.rs"]
mod split;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let (manifest, crates) = split::split_and_write(&root);
    write_manifest(&root, &manifest);
    write_scaffolding(&root, &crates);
    let total: usize = manifest.len();
    println!("refresh: {total} 段 / {} crate", crates.len());
}

fn write_manifest(root: &Path, manifest: &[split::Case]) {
    let json = serde_json::to_string_pretty(manifest).expect("manifest json");
    std::fs::write(root.join("manifest.json"), json).expect("write manifest.json");
}

/// Idempotent mount-crate scaffolding (committed once; refresh only adds
/// missing ones and reports stale ones instead of deleting, so workspace
/// membership changes stay visible in git).
fn write_scaffolding(root: &Path, crates: &BTreeMap<String, usize>) {
    for key in crates.keys() {
        let dir = root.join(format!("gencrate-{key}"));
        std::fs::create_dir_all(&dir).expect("create gencrate dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            format!(
                "[package]\nname = \"tcat-{key}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\
                 publish = false\n\n[lib]\npath = \"lib.rs\"\n\n[dependencies]\n\
                 typst = \"0.15.1\"\ntypst-syntax = \"0.15.1\"\n\
                 trust-runtime = {{ path = \"../../../crates/trust-runtime\" }}\n\
                 ecow = \"0.2\"\ncomemo = \"0.5.1\"\n"
            ),
        )
        .expect("write gencrate Cargo.toml");
        std::fs::write(
            dir.join("lib.rs"),
            format!(
                "//! 套件 `{key}` 的翻译产物挂载(refresh 生成,勿手改)。\n\n\
                 #[allow(unknown_lints, clippy::all)]\n\
                 #[allow(unused, unused_mut, non_snake_case, non_upper_case_globals, dead_code)]\n\
                 #[path = \"../gen/{key}/mod.rs\"]\npub mod pkg;\n"
            ),
        )
        .expect("write gencrate lib.rs");
    }
    // 工作区成员/依赖/分发表:全量重写(幂等)。
    let names: Vec<&String> = crates.keys().collect();
    let deps = names
        .iter()
        .map(|k| format!("tcat-{k} = {{ path = \"gencrate-{k}\" }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let cargo = std::fs::read_to_string(root.join("Cargo.toml")).expect("read Cargo.toml");
    let cargo = replace_between(
        &cargo,
        "# BEGIN tcat deps",
        "# END tcat deps",
        &format!("{deps}\n"),
    )
    .expect("tcat deps markers");
    std::fs::write(root.join("Cargo.toml"), cargo).expect("write Cargo.toml");

    let ident = |k: &str| -> String {
        k.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    };
    let arms = names
        .iter()
        .map(|k| {
            format!(
                "        \"{k}\" => tcat_{i}::pkg::module_by_path(file, &tcat_{i}::pkg::new_state(), engine, context),",
                i = ident(k)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(
        root.join("src/dispatch.rs"),
        format!(
            "// refresh 生成,勿手改:套件类别 → 生成 crate 的按文件装配分发。\n\
             #[allow(unused)]\npub fn dispatch_by_path(\n    key: &str,\n    file: &str,\n    \
             engine: &mut typst::engine::Engine,\n    context: comemo::Tracked<typst::foundations::Context>,\n\
             ) -> Option<typst::diag::SourceResult<typst::foundations::Module>> {{\n    match key {{\n\
             {arms}\n        _ => None,\n    }}\n}}\n"
        ),
    )
    .expect("write dispatch.rs");
}

fn replace_between(
    text: &str,
    begin: &str,
    end: &str,
    with: &str,
) -> Option<String> {
    let start = text.find(begin)? + begin.len();
    let stop = text[start..].find(end)? + start;
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..start]);
    out.push('\n');
    out.push_str(with);
    out.push_str(&text[stop..]);
    Some(out)
}
