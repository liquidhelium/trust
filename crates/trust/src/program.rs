//! Standalone-program generation: the `translate` subcommand's product —
//! a self-contained cargo bin project that renders the input document
//! (PDF/SVG/PNG) with no typst toolchain at run time. Evaluation is the
//! translated module (`src/typ/`); realization/layout/export are borrowed
//! from the typst crates, exactly as in the differential harness.
//!
//! Design notes live in `_things/DESIGN-standalone-program.md` (UX: this
//! subcommand owns the name `translate`; module-shaped translation moved
//! to `translate-pkg`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The typst version the translator itself builds against; the generated
/// project pins the same line (kept in one place next to the runtime's
/// own dependency).
const TYPST_VERSION: &str = "0.15.1";

/// Public parameters for one generation run.
pub struct ProgramSpec {
    /// The input `.typ` file (entry of the translation closure).
    pub input: PathBuf,
    /// Output project directory.
    pub out_dir: PathBuf,
    /// Extra files/dirs to embed (mounted at their package-root-relative
    /// path when inside the root, else as given relative to the CWD).
    pub embed: Vec<PathBuf>,
    /// Default output formats baked into the program (`pdf`, `svg`,
    /// `png`).
    pub formats: Vec<String>,
    /// Non-.typ files of merged dependency packages: `{spec}/{rel}` vkey →
    /// on-disk path (wasm plugins, data files; embedded like `--embed`).
    pub dep_assets: Vec<(String, PathBuf)>,
    /// Prefer system font discovery over the embedded typst-assets set.
    pub system_fonts: bool,
    pub lenient: bool,
    pub no_fmt: bool,
}

/// Result summary for the CLI to print.
pub struct ProgramResult {
    pub out_dir: PathBuf,
    pub module_files: usize,
    pub embedded_files: usize,
}

/// Generates the standalone program project.
pub fn generate(
    spec: &ProgramSpec,
    files: &BTreeMap<String, String>,
    entry: &str,
    dep_entries: &BTreeMap<String, String>,
    name: &str,
) -> Result<ProgramResult, String> {
    // 1) Translate the closure (package pipeline; the module tree is the
    // unchanged `translate-pkg` product).
    let pkg = trust::PackageInput {
        files: files.clone(),
        entry: entry.to_string(),
        dep_entries: dep_entries.clone(),
    };
    let tree = if spec.lenient {
        let (tree, diags) = trust::translate_package_tree_lenient(&pkg, name);
        if tree.is_empty() {
            return Err(format!("translation failed: {diags:?}"));
        }
        tree
    } else {
        trust::translate_package_tree(&pkg, name)
            .map_err(|diags| format!("translation failed: {diags:?}"))?
    };

    // 2) Layout: src/typ/ (module tree), src/lib.rs (module wrapper),
    // src/main.rs (the standalone host), assets/ (embedded bytes),
    // vendor/trust-runtime/, Cargo.toml.
    let src = spec.out_dir.join("src");
    let typ_dir = src.join("typ");
    std::fs::create_dir_all(&typ_dir).map_err(|e| format!("create {}: {e}", typ_dir.display()))?;
    let mut module_files = 0usize;
    for (rel, content) in &tree {
        let path = typ_dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("create dirs: {e}"))?;
        }
        let content = if spec.no_fmt {
            content.clone()
        } else {
            trust::format_generated(content)
        };
        std::fs::write(&path, content).map_err(|e| format!("write {}: {e}", path.display()))?;
        module_files += 1;
    }

    // 3) Embedded assets: every disk-backed source file of the closure
    // plus the `--embed` paths. Sources that live only in the file map
    // (merged dependencies) are inlined as text literals instead.
    let package_root = spec
        .input
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut embedded: BTreeMap<String, PathBuf> = BTreeMap::new();
    for (vkey, on_disk) in &spec.dep_assets {
        embedded.insert(vkey.clone(), on_disk.clone());
    }
    for vpath in files.keys() {
        let on_disk = package_root.join(vpath);
        if on_disk.is_file() {
            embedded.insert(vpath.clone(), on_disk);
        }
    }
    for path in &spec.embed {
        let canonical =
            std::fs::canonicalize(path).map_err(|e| format!("--embed {}: {e}", path.display()))?;
        let root_canonical =
            std::fs::canonicalize(&package_root).unwrap_or_else(|_| package_root.clone());
        if let Ok(rel) = canonical.strip_prefix(&root_canonical) {
            walk_embed(
                &canonical,
                &rel.to_string_lossy().replace('\\', "/"),
                &mut embedded,
            );
        } else {
            let rel = path.to_string_lossy().replace('\\', "/");
            walk_embed(&canonical, rel.trim_start_matches("./"), &mut embedded);
        }
    }

    let assets_dir = spec.out_dir.join("assets");
    std::fs::create_dir_all(&assets_dir).map_err(|e| format!("create assets: {e}"))?;
    let mut table = String::new();
    let mut inlined = String::new();
    let mut embedded_files = 0usize;
    for (idx, (vpath, on_disk)) in embedded.iter().enumerate() {
        let ext = on_disk
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("bin");
        let asset_name = format!("{idx}.{ext}");
        let dest = assets_dir.join(&asset_name);
        std::fs::copy(on_disk, &dest).map_err(|e| format!("copy asset {on_disk:?}: {e}"))?;
        table.push_str(&format!(
            "    ({vpath:?}, include_bytes!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/assets/{asset_name}\"))),\n"
        ));
        embedded_files += 1;
    }
    for (vpath, text) in files {
        if !embedded.contains_key(vpath) {
            inlined.push_str(&format!("    ({vpath:?}, {:?}),\n", text));
            embedded_files += 1;
        }
    }
    std::fs::write(
        src.join("assets.rs"),
        format!(
            "//! Embedded file table (generated).\n\n\
             /// Disk-backed assets: vpath → bytes.\n\
             pub static FILES: &[(&str, &[u8])] = &[\n{table}];\n\n\
             /// Inlined sources (merged dependencies): vpath → text.\n\
             pub static TEXTS: &[(&str, &str)] = &[\n{inlined}];\n"
        ),
    )
    .map_err(|e| format!("write assets.rs: {e}"))?;

    // 4) The module wrapper (same shape as the suite's mount crates).
    std::fs::write(
        src.join("lib.rs"),
        format!(
            "//! Translated module of `{name}` (generated; do not edit).\n\n\
             #[allow(unknown_lints, clippy::all)]\n\
             #[allow(unused, unused_mut, non_snake_case, non_upper_case_globals, dead_code)]\n\
             #[path = \"typ/mod.rs\"]\npub mod pkg;\n"
        ),
    )
    .map_err(|e| format!("write lib.rs: {e}"))?;

    // 5) Cargo.toml, then the host (it references the lib crate name).
    let bin_name = sanitize_bin_name(name);
    let lib_crate = format!("{bin_name}_program");
    std::fs::write(
        spec.out_dir.join("Cargo.toml"),
        cargo_toml(&lib_crate, &bin_name),
    )
    .map_err(|e| format!("write Cargo.toml: {e}"))?;
    let formats = if spec.formats.is_empty() {
        vec!["pdf".to_string()]
    } else {
        spec.formats.clone()
    };
    std::fs::write(
        src.join("main.rs"),
        host_source(entry, &formats, spec.system_fonts, &lib_crate),
    )
    .map_err(|e| format!("write main.rs: {e}"))?;

    // 6) The vendored runtime (the artifact must build without a trust
    // checkout; the runtime is version-locked to the translator anyway).
    vendor_runtime(&spec.out_dir)?;

    Ok(ProgramResult {
        out_dir: spec.out_dir.clone(),
        module_files,
        embedded_files,
    })
}

/// Rust-identifier-safe name for the generated bin (`-` → `_`, leading
/// digits prefixed).
pub(crate) fn sanitize_bin_name(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() || out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn walk_embed(root: &Path, prefix: &str, out: &mut BTreeMap<String, PathBuf>) {
    if root.is_file() {
        let key = if prefix.is_empty() {
            root.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default()
        } else {
            prefix.to_string()
        };
        if !key.is_empty() {
            out.insert(key, root.to_path_buf());
        }
        return;
    }
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        let file = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let rel = if prefix.is_empty() {
            file
        } else {
            format!("{prefix}/{file}")
        };
        if p.is_dir() {
            walk_embed(&p, &rel, out);
        } else {
            out.insert(rel, p);
        }
    }
}

fn cargo_toml(package_name: &str, bin_name: &str) -> String {
    format!(
        r#"[package]
name = "{package_name}"
version = "0.0.0"
edition = "2024"
publish = false
# Embedded fonts come from typst-assets (SIL OFL 1.1) via typst-kit.

[lib]
path = "src/lib.rs"

[[bin]]
name = "{bin_name}"
path = "src/main.rs"

[dependencies]
typst = "{TYPST_VERSION}"
typst-syntax = "{TYPST_VERSION}"
typst-layout = "{TYPST_VERSION}"
typst-pdf = "{TYPST_VERSION}"
typst-svg = "{TYPST_VERSION}"
typst-render = "{TYPST_VERSION}"
tiny-skia = "0.12"
typst-kit = {{ version = "{TYPST_VERSION}", default-features = false, features = ["embedded-fonts", "scan-fonts"] }}
trust-runtime = {{ path = "vendor/trust-runtime" }}
ecow = "0.2"
comemo = "0.5.1"

[profile.release]
opt-level = 3
"#
    )
}

/// Copies the translator's own runtime crate next to the generated
/// project (`vendor/trust-runtime`). The runtime's manifest inherits
/// `version`/`edition` from the trust workspace — the vendored copy
/// stands alone, so those keys are resolved to their concrete values.
fn vendor_runtime(out_dir: &Path) -> Result<(), String> {
    let runtime_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../trust-runtime");
    let dest = out_dir.join("vendor/trust-runtime");
    copy_tree(&runtime_src.join("src"), &dest.join("src"))?;
    let manifest = std::fs::read_to_string(runtime_src.join("Cargo.toml"))
        .map_err(|e| format!("vendor Cargo.toml: {e}"))?;
    let (version, edition) = workspace_version_edition();
    let manifest = manifest
        .replace(
            "version.workspace = true",
            &format!("version = \"{version}\""),
        )
        .replace(
            "edition.workspace = true",
            &format!("edition = \"{edition}\""),
        );
    std::fs::write(dest.join("Cargo.toml"), manifest)
        .map_err(|e| format!("vendor Cargo.toml: {e}"))?;
    Ok(())
}

/// The trust workspace's `[workspace.package]` version/edition (resolved
/// into the vendored manifest; a missing key falls back to the values the
/// runtime actually builds with).
fn workspace_version_edition() -> (String, String) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let Ok(text) = std::fs::read_to_string(root) else {
        return ("0.1.0".into(), "2021".into());
    };
    let in_table = text
        .lines()
        .skip_while(|l| !l.trim_start().starts_with("[workspace.package]"))
        // Skip the section header itself (it starts with '[' and would
        // otherwise terminate the table immediately).
        .skip(1)
        .take_while(|l| !l.trim_start().starts_with('['));
    let mut version = None;
    let mut edition = None;
    for line in in_table {
        if let Some((k, v)) = line.split_once('=') {
            let v = v.trim().trim_matches('"').to_string();
            match k.trim() {
                "version" => version = Some(v),
                "edition" => edition = Some(v),
                _ => {}
            }
        }
    }
    (
        version.unwrap_or_else(|| "0.1.0".into()),
        edition.unwrap_or_else(|| "2021".into()),
    )
}

fn copy_tree(src: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("create {}: {e}", dest.display()))?;
    for entry in std::fs::read_dir(src).map_err(|e| format!("read {}: {e}", src.display()))? {
        let entry = entry.map_err(|e| e.to_string())?;
        let p = entry.path();
        if p.is_dir() {
            copy_tree(&p, &dest.join(entry.file_name()))?;
        } else {
            std::fs::copy(&p, dest.join(entry.file_name()))
                .map_err(|e| format!("copy {}: {e}", p.display()))?;
        }
    }
    Ok(())
}

/// The standalone host: static world over embedded assets → library with
/// runtime `--input`s → module install → `#include tm` shim → compile →
/// export. Mirrors the differential harness world (same FileId minting,
/// same install protocol, same compile entry). Placeholders are spliced
/// with plain string replacement — the template is a literal, so no
/// format-string brace escaping applies.
fn host_source(entry: &str, formats: &[String], system_fonts: bool, lib_crate: &str) -> String {
    let formats_lit = if formats.is_empty() {
        "\"pdf\"".to_string()
    } else {
        formats
            .iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    const TEMPLATE: &str = include_str!("host_main_template.rs");
    TEMPLATE
        .replace("__LIB_CRATE__", lib_crate)
        .replace("__ENTRY__", &format!("{entry:?}"))
        .replace("__FORMATS__", &formats_lit)
        .replace("__SYSTEM_FONTS__", &system_fonts.to_string())
}
