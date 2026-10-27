//! Suite refresh: vendored typst tests/suite (+ `reg/` regressions) →
//! per-category synthetic packages, translated trees, manifest, mount
//! crates and the driver copy — all under `tests/build/` (gitignored).
//!
//! Run via `cargo suite`, then `cargo testit` (aliases in
//! `.cargo/config.toml`). The tracked `driver/` template's tcat
//! dependency block is the one machine-maintained tracked artifact
//! (category list, lockfile-like); everything else lives only in
//! `build/`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[path = "../split.rs"]
mod split;

fn main() {
    let root = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let build = root.join("build");
    let (manifest, crates) = split::split_and_write(&root, &build);
    write_manifest(&build, &manifest);
    write_scaffolding(&root, &build, &crates);
    let total: usize = manifest.len();
    println!("refresh: {total} 段 / {} crate", crates.len());
}

fn write_manifest(build: &Path, manifest: &[split::Case]) {
    let json = serde_json::to_string_pretty(manifest).expect("manifest json");
    std::fs::write(build.join("manifest.json"), json).expect("write manifest.json");
}

/// Idempotent mount-crate scaffolding and the driver copy (all inside
/// `build/`, untracked).
fn write_scaffolding(root: &Path, build: &Path, crates: &BTreeMap<String, usize>) {
    for key in crates.keys() {
        let dir = build.join(format!("gencrate-{key}"));
        std::fs::create_dir_all(&dir).expect("create gencrate dir");
        std::fs::write(
            dir.join("Cargo.toml"),
            // `build/gencrate-x` sits three levels below the repo root,
            // same depth the old `crates/trust-suite/gencrate-x` had.
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
    // The tracked driver template's tcat block = the category list
    // (machine-maintained, reviewed in diffs, lockfile-like).
    let names: Vec<&String> = crates.keys().collect();
    let deps = names
        .iter()
        .map(|k| format!("tcat-{k} = {{ path = \"../gencrate-{k}\" }}"))
        .collect::<Vec<_>>()
        .join("\n");
    let template_path = root.join("driver/Cargo.toml");
    let template = std::fs::read_to_string(&template_path).expect("read driver template");
    let template = replace_between(
        &template,
        "# BEGIN tcat deps",
        "# END tcat deps",
        &format!("\n{deps}\n"),
    )
    .expect("tcat deps markers");
    std::fs::write(&template_path, &template).expect("write driver template");

    // The driver copy under build/ is the only one ever built; its
    // `../gencrate-*` paths resolve against build/.
    let driver = build.join("driver");
    std::fs::create_dir_all(driver.join("src")).expect("create driver src");
    std::fs::create_dir_all(driver.join("tests")).expect("create driver tests");
    std::fs::write(driver.join("Cargo.toml"), &template).expect("write driver Cargo.toml");
    std::fs::copy(root.join("driver/lib.rs"), driver.join("lib.rs"))
        .expect("copy driver lib.rs");
    std::fs::write(
        driver.join("tests/suite.rs"),
        "// refresh 生成:#[path] 挂载被追踪的对拍 harness(仅 build/ 副本被构建)。\n\
         #[path = \"../../../suite_src/suite.rs\"]\nmod suite;\n",
    )
    .expect("write driver suite shim");

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
        driver.join("src/dispatch.rs"),
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
    let tail = &text[start..];
    let stop = start + tail.find(end)?;
    Some(format!("{}{}{}", &text[..start], with, &text[stop..]))
}
