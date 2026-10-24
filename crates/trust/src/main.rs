//! The `trust` CLI: command line for the typst → rust transpiler.

use std::io::Write;

/// Download a package from the registry and unpack it into the local cache
/// (mirrors the typst CLI's automatic download of missing packages).
/// Returns true on success (cache directory in place).
/// Diagnostic excerpt: span → file:line:col + source line (mirrors the
/// typst CLI's diagnostic presentation). The span's FileId,
/// re-interned by path, is the same origin.
fn diag_snippet(
    diag: &trust::Diag,
    files: &std::collections::BTreeMap<String, String>,
) -> String {
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

fn package_cache_root() -> std::path::PathBuf {
    std::env::var_os("TYPST_PACKAGE_CACHE_PATH")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            let home = std::env::var_os("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".cache/typst/packages")
        })
}

fn fetch_package(ns: &str, name: &str, version: &str) -> bool {
    if ns != "preview" {
        return false; // Only the official preview namespace is recognized
    }
    let url = format!("https://packages.typst.org/preview/{name}-{version}.tar.gz");
    let Ok(resp) = ureq::get(&url).call() else {
        return false;
    };
    let reader = std::io::BufReader::new(resp.into_reader());
    let gz = flate2::read::GzDecoder::new(reader);
    let cache = package_cache_root();
    let dir = cache.join(ns).join(name).join(version);
    let parent = dir.parent().map(std::path::Path::to_path_buf);
    if let Some(p) = parent
        && std::fs::create_dir_all(&p).is_err()
    {
        return false;
    }
    let mut archive = tar::Archive::new(gz);
    archive.unpack(&dir).is_ok()
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("-V" | "--version") => println!("trust {}", env!("CARGO_PKG_VERSION")),
        Some("translate") => {
            let mut input: Option<&str> = None;
            let mut output: Option<&str> = None;
            let mut name = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" | "--output" => {
                        output = args.get(i + 1).map(String::as_str);
                        i += 2;
                    }
                    "--name" => {
                        name = args.get(i + 1).map(String::as_str);
                        i += 2;
                    }
                    other => {
                        input = Some(other);
                        i += 1;
                    }
                }
            }
            let Some(input) = input else {
                eprintln!("usage: trust translate <input.typ> [--name MODULE] [-o OUTPUT.rs]");
                std::process::exit(2);
            };
            let src = match std::fs::read_to_string(input) {
                Ok(src) => src,
                Err(err) => {
                    eprintln!("failed to read {input}: {err}");
                    std::process::exit(1);
                }
            };
            let name = name.unwrap_or_else(|| {
                std::path::Path::new(input)
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("module")
            });
            match trust::translate(&src, name) {
                Ok(code) => match output {
                    Some(path) => {
                        if let Err(err) = std::fs::write(path, code) {
                            eprintln!("failed to write {path}: {err}");
                            std::process::exit(1);
                        }
                    }
                    None => {
                        let mut out = std::io::stdout();
                        let _ = writeln!(out, "{code}");
                    }
                },
                Err(diags) => {
                    for diag in diags {
                        let mut one = std::collections::BTreeMap::new();
                        // In single-file form the span's vpath is
                        // /main.typ — the double-key insert guarantees a
                        // hit.
                        one.insert("main.typ".to_string(), src.clone());
                        one.insert(input.to_string(), src.clone());
                        eprintln!(
                            "{input}: {:?}{}: {}",
                            diag.span,
                            diag_snippet(&diag, &one),
                            diag.message
                        );
                    }
                    std::process::exit(1);
                }
            }
        }
        Some("translate-pkg") => {
            // Exploration entry: translate a local package directory (entry
            // auto-detected), writing the generated source to
            // <out>/gen.rs with a trust-runtime dependency note.
            let mut input: Option<&str> = None;
            let mut output: Option<&str> = None;
            let mut i = 2;
            while i < args.len() {
                match args[i].as_str() {
                    "-o" | "--output" => {
                        output = args.get(i + 1).map(String::as_str);
                        i += 2;
                    }
                    "--lenient" => i += 1,
                    "--with-deps" => i += 1,
                    other => {
                        input = Some(other);
                        i += 1;
                    }
                }
            }
            let (Some(input), Some(output)) = (input, output) else {
                eprintln!("usage: trust translate-pkg <PKG_ROOT> -o <OUTPUT.rs|DIR/> [--lenient]");
                std::process::exit(2);
            };
            let lenient = args.iter().any(|a| a == "--lenient");
            let with_deps = args.iter().any(|a| a == "--with-deps");
            let root = std::path::Path::new(input);
            let mut files = std::collections::BTreeMap::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir).expect("read dir") {
                    let entry = entry.expect("entry");
                    let path = entry.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|e| e.to_str()) == Some("typ") {
                        let rel = path
                            .strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/");
                        files.insert(rel, std::fs::read_to_string(&path).expect("read"));
                    }
                }
            }
            // Entry: read typst.toml's entrypoint per the package spec
            // (relative to the package root, mandatory for published
            // packages; no particular entry name is assumed).
            let entry = std::fs::read_to_string(root.join("typst.toml"))
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
                });
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
            // version numbers).
            let name = std::fs::read_to_string(root.join("typst.toml"))
                .ok()
                .and_then(|text| {
                    let manifest: toml::Table = toml::from_str(&text).ok()?;
                    manifest.get("package")?.get("name")?.as_str().map(str::to_string)
                })
                .or_else(|| root.file_name().and_then(|s| s.to_str()).map(str::to_string))
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
                            std::fs::write(&path, content).expect("write");
                            bytes += content.len();
                        }
                        println!("generated tree of {} files ({} bytes) at {output}", tree.len(), bytes);
                    }
                    Err(diags) => {
                        for d in diags {
                            eprintln!("  {:?}{}: {}", d.span, diag_snippet(&d, &pkg.files), d.message);
                        }
                        std::process::exit(1);
                    }
                }
                return;
            }
            if lenient {
                let (code, diags) = trust::translate_package_lenient(&pkg, &name);
                for d in &diags {
                    eprintln!("degraded {:?}{}: {}", d.span, diag_snippet(&d, &pkg.files), d.message);
                }
                std::fs::write(output, &code).expect("write");
                println!(
                    "generated {output} ({} bytes, {} degradations; list above and in the file header)",
                    code.len(),
                    diags.len()
                );
            } else {
                match trust::translate_package(&pkg, &name) {
                    Ok(code) => {
                        std::fs::write(output, &code).expect("write");
                        println!("generated {output} ({} bytes)", code.len());
                    }
                    Err(diags) => {
                        for d in diags {
                            eprintln!("  {:?}{}: {}", d.span, diag_snippet(&d, &pkg.files), d.message);
                        }
                        std::process::exit(1);
                    }
                }
            }
        }
        Some("coverage") => {
            let Some(root) = args.get(2).map(String::as_str) else {
                eprintln!("usage: trust coverage <DIR|FILE.typ>");
                std::process::exit(2);
            };
            std::process::exit(coverage(root));
        }
        _ => {
            eprintln!(
                "trust {} — typst → rust transpiler\n\nusage:\n  trust translate <input.typ> [--name MODULE] [-o OUTPUT.rs]\n  trust coverage <DIR> (coverage measurement)",
                env!("CARGO_PKG_VERSION")
            );
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
                            format!("top-level: {}", d.message.split("encountered ").nth(1).unwrap_or("?"))
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
        return format!("top-level: {}", m.split("encountered ").nth(1).unwrap_or("?"));
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
            std::env::var_os("HOME").map(|h| {
                std::path::PathBuf::from(h).join(".cache/typst/packages")
            })
        })
        .unwrap_or_else(|| std::path::PathBuf::from(".cache/typst/packages"));
    let mut dep_entries = std::collections::BTreeMap::new();
    loop {
        // Scan all text for "@..." string literals (specs contain no
        // escapes).
        let mut specs: Vec<String> = Vec::new();
        for text in files.values() {
            let bytes = text.as_bytes();
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
            let Some(rest) = spec.strip_prefix('@') else { continue };
            let Some((ns_name, _ver)) = rest.split_once(':') else {
                eprintln!("unsupported dependency spec format: {spec}");
                continue;
            };
            let Some((ns, name)) = ns_name.split_once('/') else {
                continue;
            };
            let dir = cache_root.join(ns).join(name).join(
                rest.split_once(':').map(|(_, v)| v).unwrap_or(""),
            );
            // Automatic fetch of missing packages (mirrors the typst CLI's
            // registry download): registry form
            // {name}-{version}.tar.gz; TRUST_NO_FETCH=1 disables it. Only
            // on failure does it fall back to runtime interpretation.
            if !dir.join("typst.toml").exists() {
                let ok = std::env::var_os("TRUST_NO_FETCH").is_none()
                    && fetch_package(ns, name, rest.split_once(':').map(|(_, v)| v).unwrap_or(""));
                if !ok {
                    eprintln!("dependency not found in local package cache (falling back to runtime interpretation): {spec}");
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
            let src_root = entry.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
            let mut stack = vec![dir.join(&src_root)];
            let base = src_root;
            while let Some(d) = stack.pop() {
                let Ok(entries) = std::fs::read_dir(&d) else { continue };
                for e in entries.flatten() {
                    let path = e.path();
                    if path.is_dir() {
                        stack.push(path);
                    } else if path.extension().and_then(|x| x.to_str()) == Some("typ") {
                        let Ok(text) = std::fs::read_to_string(&path) else { continue };
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
