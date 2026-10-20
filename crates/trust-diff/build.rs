//! 差分测试的构建脚本:把 fixtures/*.typ 翻译为 Rust,写入 OUT_DIR。
//!
//! fixtures 只描述语言特性(与任何目标包无关);每个 fixture 导出一组
//! `#let` 绑定,测试端以同一组调用在解释器与生成物两侧对拍。

use std::collections::BTreeMap;
use std::path::PathBuf;

fn main() {
    let fixtures = PathBuf::from("fixtures");
    println!("cargo:rerun-if-changed={}", fixtures.display());

    let out_dir = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR set"));
    let gen_dir = out_dir.join("gen");
    std::fs::create_dir_all(&gen_dir).expect("create gen dir");

    let mut names = Vec::new();
    // 单文件 fixture:fixtures/*.typ。
    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("typ") {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).expect("stem").to_string();
        let src = std::fs::read_to_string(&path).expect("read fixture");
        let code = trust::translate(&src, &name).unwrap_or_else(|diags| {
            panic!(
                "fixture {name} 翻译失败:\n{}",
                diags.iter().map(|d| format!("  {:?}: {}", d.span, d.message)).collect::<Vec<_>>().join("\n")
            )
        });
        std::fs::write(gen_dir.join(format!("{name}.rs")), code).expect("write generated");
        names.push(name);
    }
    // 目录 fixture(多文件包):fixtures/pkg_*/,入口固定 entry.typ。
    for entry in std::fs::read_dir(&fixtures).expect("read fixtures dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = path.file_stem().and_then(|s| s.to_str()).expect("stem").to_string();
        let mut files = BTreeMap::new();
        collect_typ_files(&path, "", &mut files);
        let input = trust::PackageInput { files, entry: "entry.typ".into() };
        let code = trust::translate_package(&input, &name).unwrap_or_else(|diags| {
            panic!(
                "package fixture {name} 翻译失败:\n{}",
                diags.iter().map(|d| format!("  {:?}: {}", d.span, d.message)).collect::<Vec<_>>().join("\n")
            )
        });
        std::fs::write(gen_dir.join(format!("{name}.rs")), code).expect("write generated");
        names.push(name);
    }
    names.sort();

    let mut mod_rs = String::from("// 由 build.rs 生成\n");
    for name in &names {
        mod_rs.push_str(&format!(
            "#[allow(unused)]\npub mod {name} {{\n    include!(concat!(env!(\"OUT_DIR\"), \"/gen/{name}.rs\"));\n}}\n\n"
        ));
    }
    std::fs::write(gen_dir.join("mod.rs"), mod_rs).expect("write mod.rs");
}

/// 递归收集目录下 .typ 文件,键为相对包根的 POSIX 路径。
fn collect_typ_files(dir: &PathBuf, prefix: &str, out: &mut BTreeMap<String, String>) {
    for entry in std::fs::read_dir(dir).expect("read dir") {
        let entry = entry.expect("entry");
        let path = entry.path();
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        if path.is_dir() {
            collect_typ_files(&path, &format!("{prefix}{file_name}/"), out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("typ") {
            let rel = format!("{prefix}{file_name}");
            let src = std::fs::read_to_string(&path).expect("read file");
            out.insert(rel, src);
        }
    }
}
