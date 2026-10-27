//! The `trust` CLI: command line for the typst → rust transpiler.

use clap::Parser;

mod program;

/// Collects every `.typ` file under `root` (recursively) as
/// root-relative POSIX vpath → source text. `skip` (a canonicalized dir)
/// is never descended into.
fn collect_typ_sources(
    root: &std::path::Path,
    skip: Option<&std::path::Path>,
) -> std::collections::BTreeMap<String, String> {
    let mut files = std::collections::BTreeMap::new();
    for entry in walkdir::WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("typ") {
            continue;
        }
        if let Some(skip) = skip
            && std::fs::canonicalize(path).is_ok_and(|p| p.starts_with(skip))
        {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        files.insert(rel, std::fs::read_to_string(path).expect("read .typ"));
    }
    files
}

/// Collects the **import closure** of one entry file: every `.typ` file
/// reachable through `import`/`include` string sources (anywhere in the
/// tree, including inside function bodies — typst resolves those at call
/// time, but the files are statically known), keyed by root-relative
/// POSIX vpath. Files outside the closure are never translated.
///
/// `@spec` imports are not followed here (they are package-space, not
/// project files; `--with-deps` handles them separately). Fully dynamic
/// sources (computed paths) are unknowable statically — at run time they
/// read from the world, like typst.
fn collect_import_closure(
    root: &std::path::Path,
    entry: &str,
    dynamic: &std::collections::BTreeSet<String>,
) -> std::collections::BTreeMap<String, String> {
    fn scan(node: &typst_syntax::SyntaxNode, dir: &str, out: &mut Vec<String>) {
        for source in [
            node.cast::<typst_syntax::ast::ModuleImport>()
                .map(|i| i.source()),
            node.cast::<typst_syntax::ast::ModuleInclude>()
                .map(|i| i.source()),
        ]
        .into_iter()
        .flatten()
        {
            if let typst_syntax::ast::Expr::Str(s) = source {
                let raw = s.get().to_string();
                if !raw.starts_with('@') && !raw.starts_with('/') {
                    let joined = if dir.is_empty() {
                        raw.clone()
                    } else {
                        format!("{dir}/{raw}")
                    };
                    if let Some(norm) = normalize_vpath(&joined) {
                        out.push(norm);
                    }
                }
            }
        }
        for child in node.children() {
            scan(child, dir, out);
        }
    }

    let mut files = std::collections::BTreeMap::new();
    let mut queue = vec![entry.to_string()];
    let mut seen = std::collections::BTreeSet::new();
    while let Some(vpath) = queue.pop() {
        if dynamic.contains(&vpath) || !seen.insert(vpath.clone()) {
            continue;
        }
        let on_disk = root.join(&vpath);
        let Ok(text) = std::fs::read_to_string(&on_disk) else {
            continue;
        };
        let root_node = typst_syntax::parse(&text);
        let dir = vpath.rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        let mut found = Vec::new();
        scan(&root_node, &dir, &mut found);
        for f in found {
            if !seen.contains(&f) {
                queue.push(f);
            }
        }
        files.insert(vpath, text);
    }
    files
}

/// Normalizes a joined vpath lexically (`a/../b` → `b`); `None` when it
/// escapes the root.
fn normalize_vpath(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return None;
                }
            }
            other => parts.push(other),
        }
    }
    Some(parts.join("/"))
}

/// Download a package from the registry and unpack it into the local cache
/// (mirrors the typst CLI's automatic download of missing packages).
/// Returns true on success (cache directory in place).
/// Diagnostic excerpt: span → file:line:col + source line (mirrors the
/// typst CLI's diagnostic presentation). The span's FileId,
/// re-interned by path, is the same origin.
fn diag_snippet(diag: &trust::Diag, files: &std::collections::BTreeMap<String, String>) -> String {
    use std::fmt::Write as _;
    let Some(id) = diag.span.id() else {
        return String::new();
    };
    let vpath = id.vpath().get_with_slash().to_string();
    let Some(src) = files.get(&vpath).or_else(|| {
        // Dependency merge-key form (@spec/rel): strip the spec prefix and
        // retry.
        vpath
            .find('/')
            .and_then(|i| files.get(&vpath[i + 1..]))
            .or_else(|| files.get(vpath.trim_start_matches('/')))
    }) else {
        return format!(" @ {vpath}");
    };
    let source = typst_syntax::Source::new(id, src.clone());
    let Some(node) = source.find(diag.span) else {
        return format!(" @ {vpath}");
    };
    let range = node.range();
    let (line, col) = source
        .lines()
        .byte_to_line_column(range.start)
        .unwrap_or((0, 0));
    // byte_to_line is 0-based: line L = lines()[L].
    let line_text = src.lines().nth(line).unwrap_or("");
    let mut out = String::new();
    let _ = write!(out, " @ {vpath}:{}:{}", line + 1, col + 1);
    if !line_text.trim().is_empty() {
        let _ = write!(
            out,
            "\n    {}",
            if line_text.len() > 110 {
                format!("{}…", &line_text[..110])
            } else {
                line_text.to_string()
            }
        );
    }
    out
}

/// Command-line interface (clap derive).
#[derive(clap::Parser)]
#[command(name = "trust", version, about = "typst → rust transpiler")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Translate a single .typ file into a standalone program project
    /// (cargo bin crate; renders PDF/SVG/PNG with no typst toolchain)
    Translate {
        /// Input .typ source file (the document to turn into a program)
        input: String,
        /// Output project directory; defaults to ./<stem>-program/
        dir: Option<String>,
        /// Recursively resolve and merge `@spec` dependencies from the
        /// local package cache into the translation
        #[arg(long)]
        with_deps: bool,
        /// Extra file or directory to embed (repeatable; mounted at its
        /// package-root-relative path when inside the root)
        #[arg(long = "embed")]
        embeds: Vec<String>,
        /// Mark a file as dynamic (repeatable): it leaves the import
        /// closure — not translated, not embedded — and is imported at
        /// run time through the interpreter, read live from --root (the
        /// opt-out dual of --embed's pinning)
        #[arg(long = "dynamic")]
        dynamics: Vec<String>,
        /// Default output formats baked into the program (pdf,svg,png;
        /// the program's --format flag overrides per invocation)
        #[arg(long, value_delimiter = ',')]
        format: Option<String>,
        /// Prefer system font discovery over the embedded typst-assets
        /// set
        #[arg(long)]
        system_fonts: bool,
        /// Also run `cargo build --release` on the generated project
        #[arg(long)]
        compile: bool,
        /// Degrade unsupported constructs to runtime warnings instead of
        /// failing
        #[arg(long)]
        lenient: bool,
        /// Emit the translator's raw output without pretty-printing
        #[arg(long)]
        no_fmt: bool,
    },
    /// Translate a package directory — or a single .typ file — into a
    /// library module (the host-integration artifact)
    TranslatePkg {
        /// Package root directory, or a single .typ file (translated as a
        /// one-file package with that file as the entry)
        pkg_root: String,
        /// Output: a .rs file, or a directory with trailing `/` for a
        /// generated source tree
        #[arg(short, long)]
        output: String,
        /// Degrade unsupported constructs to runtime warnings instead of
        /// failing
        #[arg(long)]
        lenient: bool,
        /// Recursively resolve and merge `@spec` dependencies from the
        /// local package cache into the translation
        #[arg(long)]
        with_deps: bool,
        /// Emit the translator's raw output without pretty-printing
        #[arg(long)]
        no_fmt: bool,
    },
    /// Measure language coverage of a file or directory
    Coverage {
        /// Input file or directory
        root: String,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Translate {
            input,
            dir,
            with_deps,
            embeds,
            dynamics,
            format,
            system_fonts,
            compile,
            lenient,
            no_fmt,
        } => {
            let input_path = std::path::PathBuf::from(&input);
            if !input_path.is_file() {
                eprintln!("translate: input {input} is not a file");
                std::process::exit(2);
            }
            let stem = input_path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("program")
                .to_string();
            let out_dir = dir
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(format!("./{stem}-program")));

            // File closure: the import closure of the entry file —
            // sibling files outside the closure are never translated (and
            // cannot break the build). The output project never enters
            // the closure (only import/include-reachable files do).
            let root = input_path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .map(std::path::Path::to_path_buf)
                .unwrap_or_else(|| std::path::PathBuf::from("."));
            let entry = input_path
                .file_name()
                .and_then(|s| s.to_str())
                .expect("file name")
                .to_string();
            // Dynamic files leave the closure: an import edge to them
            // falls back to run-time interpretation (mixed execution),
            // their source read live from disk — edits apply without
            // retranslation.
            let mut dynamic = std::collections::BTreeSet::new();
            for d in &dynamics {
                let path = std::path::Path::new(d);
                let rel = if path.is_absolute() {
                    path.strip_prefix(&root).ok().map(|r| r.to_path_buf())
                } else {
                    Some(path.to_path_buf())
                };
                let Some(rel) = rel else {
                    eprintln!("--dynamic {d} is not under the package root");
                    std::process::exit(2);
                };
                dynamic.insert(rel.to_string_lossy().replace('\\', "/"));
            }
            for d in &dynamic {
                if embeds.iter().any(|e| e.trim_start_matches("./") == *d) {
                    eprintln!("--dynamic {d} is also --embed'd: pick one (pinned or live)");
                    std::process::exit(2);
                }
            }
            let mut files = collect_import_closure(&root, &entry, &dynamic);
            if !files.contains_key(&entry) {
                eprintln!("internal: entry {entry} missing from the closure");
                std::process::exit(2);
            }
            let dep_entries = if with_deps {
                collect_translatable_deps(&mut files)
            } else {
                Default::default()
            };
            // Non-.typ package files (wasm plugins, data) of merged
            // dependencies: embedded under their {spec}/{rel} key so the
            // program resolves them like the package cache would.
            let mut dep_assets: Vec<(String, std::path::PathBuf)> = Vec::new();
            if !dep_entries.is_empty() {
                let cache_root = std::env::var_os("TYPST_PACKAGE_CACHE_PATH")
                    .map(std::path::PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(|h| std::path::PathBuf::from(h).join(".cache/typst/packages"))
                    })
                    .unwrap_or_else(|| std::path::PathBuf::from(".cache/typst/packages"));
                for spec in dep_entries.keys() {
                    // "@ns/name:ver" → cache/<ns>/<name>/<ver>
                    let Some(rest) = spec.strip_prefix('@') else {
                        continue;
                    };
                    let Some((ns, name_ver)) = rest.split_once('/') else {
                        continue;
                    };
                    let Some((name, ver)) = name_ver.split_once(':') else {
                        continue;
                    };
                    let base = cache_root.join(ns).join(name).join(ver);
                    let mut stack = vec![base.clone()];
                    while let Some(d) = stack.pop() {
                        let Ok(entries) = std::fs::read_dir(&d) else {
                            continue;
                        };
                        for e in entries.flatten() {
                            let p = e.path();
                            if p.is_dir() {
                                stack.push(p);
                            } else if p.extension().and_then(|x| x.to_str()) != Some("typ") {
                                let Ok(rel) = p.strip_prefix(&base) else {
                                    continue;
                                };
                                let vkey =
                                    format!("{spec}/{}", rel.to_string_lossy().replace('\\', "/"));
                                if !files.contains_key(&vkey) {
                                    dep_assets.push((vkey, p));
                                }
                            }
                        }
                    }
                }
            }
            let spec = program::ProgramSpec {
                input: input_path,
                out_dir: out_dir.clone(),
                dep_assets,
                embed: embeds.iter().map(std::path::PathBuf::from).collect(),
                formats: format
                    .as_deref()
                    .map(|f| {
                        f.split(',')
                            .map(str::trim)
                            .filter(|s| !s.is_empty())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default(),
                system_fonts,
                lenient,
                no_fmt,
            };
            match program::generate(&spec, &files, &entry, &dep_entries, &stem) {
                Ok(result) => {
                    println!(
                        "generated {} ({} module files, {} embedded files)",
                        result.out_dir.display(),
                        result.module_files,
                        result.embedded_files
                    );
                    if compile {
                        let status = std::process::Command::new("cargo")
                            .args(["build", "--release"])
                            .current_dir(&out_dir)
                            .status()
                            .unwrap_or_else(|e| {
                                eprintln!("failed to run cargo: {e}");
                                std::process::exit(1);
                            });
                        if !status.success() {
                            std::process::exit(1);
                        }
                        let bin = program::sanitize_bin_name(&stem);
                        println!("binary: {}/target/release/{bin}", out_dir.display());
                    }
                }
                Err(msg) => {
                    eprintln!("{msg}");
                    std::process::exit(1);
                }
            }
        }
        Command::TranslatePkg {
            pkg_root,
            output,
            lenient,
            with_deps,
            no_fmt,
        } => {
            let input = pkg_root.as_str();
            let output = output.as_str();
            let root_path = std::path::Path::new(input);
            // Single .typ file: translate as a one-file package (root =
            // the parent directory, entry = the file itself; sibling
            // .typ files join the set — assembly is on-demand).
            let (root, single_entry) = if root_path.is_file() {
                (
                    root_path
                        .parent()
                        .unwrap_or(std::path::Path::new("."))
                        .to_path_buf(),
                    root_path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_string),
                )
            } else {
                (root_path.to_path_buf(), None)
            };
            let root = root.as_path();
            let mut files = collect_typ_sources(root_path, None);
            // Entry: for a single .typ input, the file itself; otherwise
            // read typst.toml's entrypoint per the package spec (relative
            // to the package root, mandatory for published packages; no
            // particular entry name is assumed).
            let entry = match single_entry {
                Some(entry) if files.contains_key(&entry) => Ok(entry),
                Some(entry) => Err(format!("entry file `{entry}` is not in the file set")),
                None => std::fs::read_to_string(root.join("typst.toml"))
                    .map_err(|e| format!("failed to read typst.toml: {e}"))
                    .and_then(|text| {
                        let manifest: toml::Table =
                            toml::from_str(&text).map_err(|e| e.to_string())?;
                        let entry = manifest
                            .get("package")
                            .and_then(|p| p.get("entrypoint"))
                            .and_then(|v| v.as_str())
                            .ok_or_else(|| "typst.toml is missing package.entrypoint".to_string())?
                            .trim_start_matches('/')
                            .to_string();
                        if files.contains_key(&entry) {
                            Ok(entry)
                        } else {
                            Err(format!("entrypoint `{entry}` is not in the file set"))
                        }
                    }),
            };
            let entry = match entry {
                Ok(entry) => entry,
                Err(msg) => {
                    eprintln!("failed to resolve entrypoint: {msg}");
                    std::process::exit(2);
                }
            };
            // Only collect the package source tree containing the entry
            // (docs/tests etc. are not part of the package).
            let src_root = entry
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/"))
                .unwrap_or_default();
            if !src_root.is_empty() {
                files.retain(|k, _| k.starts_with(&src_root));
            }
            // Recursive dependencies (--with-deps): resolve @spec from the
            // local package cache, merging translatable dependency packages
            // into the file set ({spec}/{rel} keys) with import edges
            // staticized; cache misses keep the runtime interpretation
            // fallback (with a warning).
            let dep_entries = if with_deps {
                collect_translatable_deps(&mut files)
            } else {
                Default::default()
            };
            let pkg = trust::PackageInput {
                files,
                entry,
                dep_entries,
            };
            // The module name comes from the manifest's package.name (not
            // the directory name — package cache directory names are
            // version numbers); a single .typ input names by its stem.
            let name = root_path
                .extension()
                .and_then(|e| e.to_str())
                .filter(|e| *e == "typ")
                .and_then(|_| {
                    root_path
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .or_else(|| {
                    std::fs::read_to_string(root.join("typst.toml"))
                        .ok()
                        .and_then(|text| {
                            let manifest: toml::Table = toml::from_str(&text).ok()?;
                            manifest
                                .get("package")?
                                .get("name")?
                                .as_str()
                                .map(str::to_string)
                        })
                })
                .or_else(|| {
                    root.file_name()
                        .and_then(|s| s.to_str())
                        .map(str::to_string)
                })
                .unwrap_or_else(|| "pkg".into());
            // `-o DIR/` (trailing slash): directory form, a multi-file
            // output mirroring the source tree.
            if let Some(dir) = output.strip_suffix('/') {
                let tree_result = if lenient {
                    let (tree, diags) = trust::translate_package_tree_lenient(&pkg, &name);
                    if tree.is_empty() {
                        Err(diags)
                    } else {
                        Ok(tree)
                    }
                } else {
                    trust::translate_package_tree(&pkg, &name)
                };
                match tree_result {
                    Ok(tree) => {
                        let out_root = std::path::Path::new(dir);
                        std::fs::create_dir_all(out_root).expect("create output dir");
                        let mut bytes = 0usize;
                        for (rel, content) in &tree {
                            let path = out_root.join(rel);
                            std::fs::create_dir_all(path.parent().expect("parent"))
                                .expect("create dirs");
                            // Pretty-print unless opted out (see `fmt`).
                            let content = if no_fmt {
                                content.clone()
                            } else {
                                trust::format_generated(content)
                            };
                            std::fs::write(&path, &content).expect("write");
                            bytes += content.len();
                        }
                        println!(
                            "generated tree of {} files ({} bytes) at {output}",
                            tree.len(),
                            bytes
                        );
                    }
                    Err(diags) => {
                        for d in diags {
                            eprintln!(
                                "  {:?}{}: {}",
                                d.span,
                                diag_snippet(&d, &pkg.files),
                                d.message
                            );
                        }
                        std::process::exit(1);
                    }
                }
                return;
            }
            if lenient {
                let (code, diags) = trust::translate_package_lenient(&pkg, &name);
                for d in &diags {
                    eprintln!(
                        "degraded {:?}{}: {}",
                        d.span,
                        diag_snippet(d, &pkg.files),
                        d.message
                    );
                }
                let code = if no_fmt {
                    code
                } else {
                    trust::format_generated(&code)
                };
                std::fs::write(output, &code).expect("write");
                println!(
                    "generated {output} ({} bytes, {} degradations; list above and in the file header)",
                    code.len(),
                    diags.len()
                );
            } else {
                match trust::translate_package(&pkg, &name) {
                    Ok(code) => {
                        let code = if no_fmt {
                            code
                        } else {
                            trust::format_generated(&code)
                        };
                        std::fs::write(output, &code).expect("write");
                        println!("generated {output} ({} bytes)", code.len());
                    }
                    Err(diags) => {
                        for d in diags {
                            eprintln!(
                                "  {:?}{}: {}",
                                d.span,
                                diag_snippet(&d, &pkg.files),
                                d.message
                            );
                        }
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Coverage { root } => {
            std::process::exit(coverage(&root));
        }
    }
}

/// Coverage measurement: translate every .typ in a directory (or a single
/// file) one by one, clustering gaps by diagnostic message (to prioritize
/// work by data).
fn coverage(root: &str) -> i32 {
    fn walk(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, files);
            } else if path.extension().and_then(|e| e.to_str()) == Some("typ") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    let path = std::path::Path::new(root);
    if path.is_dir() {
        walk(path, &mut files);
    } else {
        files.push(path.to_path_buf());
    }
    files.sort();

    // Package mode: when the directory contains lib.typ, translate the
    // multi-file package as a whole (top-level imports resolvable).
    if path.is_dir() {
        let has_lib = files
            .iter()
            .any(|f| f.file_name().and_then(|n| n.to_str()) == Some("lib.typ"));
        // Entry: the shallowest lib.typ (standard package layout
        // src/lib.typ).
        let entry = files
            .iter()
            .filter(|f| f.file_name().and_then(|n| n.to_str()) == Some("lib.typ"))
            .min_by_key(|f| f.components().count())
            .map(|f| {
                f.strip_prefix(path)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/")
            });
        if let (true, Some(entry)) = (has_lib, entry) {
            let src_root = entry
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/"))
                .unwrap_or_default();
            let mut map = std::collections::BTreeMap::new();
            for file in &files {
                let rel = file
                    .strip_prefix(path)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                // Only collect the package source tree containing the entry
                // (docs/tests/gallery are not part of the package).
                if !src_root.is_empty() && !rel.starts_with(&src_root) {
                    continue;
                }
                if let Ok(src) = std::fs::read_to_string(file) {
                    map.insert(rel, src);
                }
            }
            let input = trust::PackageInput {
                files: map,
                entry,
                dep_entries: Default::default(),
            };
            match trust::translate_package(&input, "cetz") {
                Ok(_) => println!("package mode: whole-package translation passed"),
                Err(diags) => {
                    let mut kinds: std::collections::BTreeMap<String, usize> =
                        std::collections::BTreeMap::new();
                    for d in diags {
                        let key = if d.message.contains("encountered ") {
                            format!(
                                "top-level: {}",
                                d.message.split("encountered ").nth(1).unwrap_or("?")
                            )
                        } else {
                            d.message.clone()
                        };
                        *kinds.entry(key).or_default() += 1;
                    }
                    println!("package mode: whole-package gaps (by frequency):");
                    let mut es: Vec<_> = kinds.into_iter().collect();
                    es.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
                    for (k, n) in es.iter().take(20) {
                        println!("  {n:4}  {k}");
                    }
                }
            }
        }
    }

    use std::collections::BTreeMap;
    let mut kind_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut per_file: Vec<(String, usize)> = Vec::new();
    let mut total = 0usize;
    let mut ok_files = 0usize;
    for file in &files {
        let Ok(src) = std::fs::read_to_string(file) else {
            continue;
        };
        total += 1;
        let diags = match trust::translate(&src, "m") {
            Ok(_) => Vec::new(),
            Err(diags) => diags,
        };
        if diags.is_empty() {
            ok_files += 1;
        }
        per_file.push((file.display().to_string(), diags.len()));
        for diag in diags {
            // Normalization: strip concrete names/types and cluster by gap
            // category.
            let key = normalize_diag(&diag.message);
            *kind_counts.entry(key).or_default() += 1;
        }
    }

    println!("files: {ok_files}/{total} fully translatable");
    println!("\ngap clusters (by frequency):");
    let mut entries: Vec<_> = kind_counts.into_iter().collect();
    entries.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (kind, n) in entries.iter().take(24) {
        println!("  {n:4}  {kind}");
    }
    println!("\nhardest files (by diagnostic count):");
    per_file.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (file, n) in per_file.iter().take(10) {
        println!("  {n:4}  {file}");
    }
    0
}

/// Normalize a diagnostic message into a gap category.
fn normalize_diag(message: &str) -> String {
    let m = message;
    let pick = |prefix: &str| -> Option<String> {
        m.find(prefix)
            .map(|i| m[i..].split(')').next().unwrap_or(&m[i..]).to_string())
    };
    if m.contains("encountered ") {
        return format!(
            "top-level: {}",
            m.split("encountered ").nth(1).unwrap_or("?")
        );
    }
    if let Some(e) = pick("expression ") {
        return format!("expression: {e}");
    }
    if m.contains("not supported") {
        return m.to_string();
    }
    m.to_string()
}

/// Recursive dependency closure: scan the text for `"@spec"` references,
/// resolve them from the local package cache (`TYPST_PACKAGE_CACHE_PATH`
/// or `~/.cache/typst/packages`), merge the dependency package's sources
/// into `files` under `{spec}/{path-inside-package}` keys, and return the
/// spec → entry merge keys. Iterated to a fixed point (the dependencies'
/// own dependencies are merged the same way). Specs missing from the cache
/// are not merged (the output falls back to runtime interpretation), with
/// a warning.
fn collect_translatable_deps(
    files: &mut std::collections::BTreeMap<String, String>,
) -> std::collections::BTreeMap<String, String> {
    let cache_root = std::env::var_os("TYPST_PACKAGE_CACHE_PATH")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|h| std::path::PathBuf::from(h).join(".cache/typst/packages"))
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".cache/typst/packages"));
    let mut dep_entries = std::collections::BTreeMap::new();
    loop {
        // Scan all text for "@..." string literals (specs contain no
        // escapes).
        let mut specs: Vec<String> = Vec::new();
        for text in files.values() {
            // Skip doc-comment lines (///) — a README example's @spec would
            // be a false dependency hit (scan noise).
            let text = text
                .lines()
                .filter(|l| !l.trim_start().starts_with("///"))
                .collect::<Vec<_>>()
                .join("\n");
            let bytes = text.as_bytes();
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i] == b'"' && i + 1 < bytes.len() && bytes[i + 1] == b'@' {
                    let start = i + 1;
                    let mut j = start + 1;
                    while j < bytes.len() && bytes[j] != b'"' && bytes[j] != b'\n' {
                        j += 1;
                    }
                    if j < bytes.len() && bytes[j] == b'"' {
                        let spec = text[start..j].to_string();
                        if !specs.contains(&spec) {
                            specs.push(spec);
                        }
                    }
                    i = j;
                }
                i += 1;
            }
        }
        let mut added = false;
        for spec in specs {
            if dep_entries.contains_key(&spec) {
                continue;
            }
            // A spec looks like @ns/name:ver.
            let Some(rest) = spec.strip_prefix('@') else {
                continue;
            };
            let Some((ns_name, _ver)) = rest.split_once(':') else {
                eprintln!("unsupported dependency spec format: {spec}");
                continue;
            };
            let Some((ns, name)) = ns_name.split_once('/') else {
                continue;
            };
            let dir = cache_root
                .join(ns)
                .join(name)
                .join(rest.split_once(':').map(|(_, v)| v).unwrap_or(""));
            // Automatic fetch of missing packages (mirrors the typst CLI's
            // registry download): registry form
            // {name}-{version}.tar.gz; TRUST_NO_FETCH=1 disables it. Only
            // on failure does it fall back to runtime interpretation.
            if !dir.join("typst.toml").exists() {
                // typst-kit's SystemPackages: same cache layout the CLI
                // uses (atomic unpack into the cache, data dir first).
                let ok = std::env::var_os("TRUST_NO_FETCH").is_none()
                    && rest
                        .split_once(':')
                        .and_then(|(_, v)| v.parse().ok())
                        .map(|version| {
                            let spec = typst_syntax::package::PackageSpec {
                                namespace: ns.into(),
                                name: name.into(),
                                version,
                            };
                            let packages = typst_kit::packages::SystemPackages::new(
                                typst_kit::downloader::SystemDownloader::new("trust"),
                            );
                            packages.obtain(&spec).is_ok()
                        })
                        .unwrap_or(false);
                if !ok {
                    eprintln!(
                        "dependency not found in local package cache (falling back to runtime interpretation): {spec}"
                    );
                    continue;
                }
                eprintln!("missing dependency fetched automatically: {spec}");
            }
            let manifest = match std::fs::read_to_string(dir.join("typst.toml")) {
                Ok(m) => m,
                Err(_) => {
                    eprintln!("failed to read dependency manifest: {spec}");
                    continue;
                }
            };
            let Ok(table) = manifest.parse::<toml::Table>() else {
                eprintln!("failed to parse dependency manifest: {spec}");
                continue;
            };
            let Some(entry) = table
                .get("package")
                .and_then(|p| p.get("entrypoint"))
                .and_then(|v| v.as_str())
                .map(str::to_string)
            else {
                continue;
            };
            let src_root = entry
                .rsplit_once('/')
                .map(|(d, _)| format!("{d}/"))
                .unwrap_or_default();
            let mut stack = vec![dir.join(&src_root)];
            while let Some(d) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&d) else {
                    continue;
                };
                for e in entries.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|x| x.to_str()) == Some("typ") {
                        let Ok(text) = std::fs::read_to_string(&path) else {
                            continue;
                        };
                        let rel = path
                            .strip_prefix(&dir)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/");
                        files.insert(format!("{spec}/{rel}"), text);
                    }
                }
            }
            dep_entries.insert(spec.clone(), format!("{spec}/{entry}"));
            added = true;
            eprintln!("dependency merged into translation: {spec} (entry {entry})");
        }
        if !added {
            break;
        }
    }
    dep_entries
}
