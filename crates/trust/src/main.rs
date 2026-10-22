//! `trust` CLI:typst → rust 转译器命令行。

use std::io::Write;

/// 从 registry 下载并解包一个包到本地缓存(对照 typst CLI 的缺包
/// 自动下载,KNOWN #49)。成功返回 true(缓存目录就位)。
/// 诊断摘录:span → file:行:列 + 源码行(对照 typst CLI 的诊断
/// 呈现;KNOWN #69)。span 的 FileId 按路径再 intern 即同源。
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
        // 依赖合并键形态(@spec/rel):剥 spec 前缀再试。
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
    // byte_to_line 为 0 基:第 L 行 = lines()[L]。
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
        return false; // 只认官方 preview 命名空间
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
                eprintln!("用法: trust translate <input.typ> [--name 模块名] [-o 输出.rs]");
                std::process::exit(2);
            };
            let src = match std::fs::read_to_string(input) {
                Ok(src) => src,
                Err(err) => {
                    eprintln!("读取 {input} 失败: {err}");
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
                            eprintln!("写入 {path} 失败: {err}");
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
                        // 单文件形态 span 的 vpath 是 /main.typ——
                        // 双键插入保证命中。
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
            // 探索入口:翻译一个本地包目录(entry 自动探测),把生成源码
            // 写到 <out>/gen.rs,附 trust-runtime 依赖说明。
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
                eprintln!("用法: trust translate-pkg <包根目录> -o <输出.rs|目录/> [--lenient]");
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
            // 入口:按包规范读 typst.toml 的 entrypoint(相对包根,
            // 发布包必备;不假设任何特定入口名)。
            let entry = std::fs::read_to_string(root.join("typst.toml"))
                .map_err(|e| format!("读 typst.toml 失败: {e}"))
                .and_then(|text| {
                    let manifest: toml::Table =
                        toml::from_str(&text).map_err(|e| e.to_string())?;
                    let entry = manifest
                        .get("package")
                        .and_then(|p| p.get("entrypoint"))
                        .and_then(|v| v.as_str())
                        .ok_or_else(|| "typst.toml 缺 package.entrypoint".to_string())?
                        .trim_start_matches('/')
                        .to_string();
                    if files.contains_key(&entry) {
                        Ok(entry)
                    } else {
                        Err(format!("entrypoint `{entry}` 不在文件集"))
                    }
                });
            let entry = match entry {
                Ok(entry) => entry,
                Err(msg) => {
                    eprintln!("入口解析失败: {msg}");
                    std::process::exit(2);
                }
            };
            // 只收 entry 所在的包源树(docs/tests 等不入包)。
            let src_root = entry
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/"))
                .unwrap_or_default();
            if !src_root.is_empty() {
                files.retain(|k, _| k.starts_with(&src_root));
            }
            // 递归依赖(--with-deps):从本地包缓存解析 @spec,把可
            // 翻译的依赖包并入文件集({spec}/{rel} 键),import 边
            // 静态化;未命中缓存的保持运行时解释回退(警告)。
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
            // 模块名取 manifest 的 package.name(而非目录名——包缓存
            // 目录名是版本号)。
            let name = std::fs::read_to_string(root.join("typst.toml"))
                .ok()
                .and_then(|text| {
                    let manifest: toml::Table = toml::from_str(&text).ok()?;
                    manifest.get("package")?.get("name")?.as_str().map(str::to_string)
                })
                .or_else(|| root.file_name().and_then(|s| s.to_str()).map(str::to_string))
                .unwrap_or_else(|| "pkg".into());
            // `-o 目录/`(尾斜杠):目录形态,镜像源码树的多文件产物。
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
                        println!("已生成树 {} 个文件({} 字节)于 {output}", tree.len(), bytes);
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
                    eprintln!("降级 {:?}{}: {}", d.span, diag_snippet(&d, &pkg.files), d.message);
                }
                std::fs::write(output, &code).expect("write");
                println!(
                    "已生成 {output}({} 字节,降级 {} 处,清单见上方与文件头注释)",
                    code.len(),
                    diags.len()
                );
            } else {
                match trust::translate_package(&pkg, &name) {
                    Ok(code) => {
                        std::fs::write(output, &code).expect("write");
                        println!("已生成 {output}({} 字节)", code.len());
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
                eprintln!("用法: trust coverage <目录或文件.typ>");
                std::process::exit(2);
            };
            std::process::exit(coverage(root));
        }
        _ => {
            eprintln!(
                "trust {} — typst → rust 转译器\n\n用法:\n  trust translate <input.typ> [--name 模块名] [-o 输出.rs]\n  trust coverage <目录>(覆盖度度量)",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}

/// 覆盖度度量:对一个目录(或文件)的全部 .typ 逐个翻译,
/// 按诊断消息聚类统计缺口(数据驱动开发优先级)。
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

    // 包模式:目录含 lib.typ 时按多文件包整体翻译(顶层 import 可解析)。
    if path.is_dir() {
        let has_lib = files
            .iter()
            .any(|f| f.file_name().and_then(|n| n.to_str()) == Some("lib.typ"));
        // entry:最浅的 lib.typ(标准包布局 src/lib.typ)。
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
                // 只收 entry 所在的包源树(docs/tests/gallery 不入包)。
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
                Ok(_) => println!("包模式:整包翻译通过"),
                Err(diags) => {
                    let mut kinds: std::collections::BTreeMap<String, usize> =
                        std::collections::BTreeMap::new();
                    for d in diags {
                        let key = if d.message.contains("遇到 ") {
                            format!("顶层: {}", d.message.split("遇到 ").nth(1).unwrap_or("?"))
                        } else {
                            d.message.clone()
                        };
                        *kinds.entry(key).or_default() += 1;
                    }
                    println!("包模式:整包翻译缺口(按频次):");
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
            // 归一化:去掉具体名字/类型,按缺口类别聚类。
            let key = normalize_diag(&diag.message);
            *kind_counts.entry(key).or_default() += 1;
        }
    }

    println!("文件: {ok_files}/{total} 可整体翻译");
    println!("\n缺口聚类(按频次):");
    let mut entries: Vec<_> = kind_counts.into_iter().collect();
    entries.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (kind, n) in entries.iter().take(24) {
        println!("  {n:4}  {kind}");
    }
    println!("\n最难文件(诊断数):");
    per_file.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
    for (file, n) in per_file.iter().take(10) {
        println!("  {n:4}  {file}");
    }
    0
}

/// 把诊断消息归一化为缺口类别。
fn normalize_diag(message: &str) -> String {
    let m = message;
    let pick = |prefix: &str| -> Option<String> {
        m.find(prefix)
            .map(|i| m[i..].split(')').next().unwrap_or(&m[i..]).to_string())
    };
    if m.contains("遇到 ") {
        return format!("顶层: {}", m.split("遇到 ").nth(1).unwrap_or("?"));
    }
    if let Some(e) = pick("表达式 ") {
        return format!("表达式: {e}");
    }
    if m.contains("暂不支持") || m.contains("暂只支持") {
        return m.to_string();
    }
    m.to_string()
}


/// 递归依赖闭包:扫描文本中的 `"@spec"` 引用,从本地包缓存
/// (`TYPST_PACKAGE_CACHE_PATH` 或 `~/.cache/typst/packages`)解析,
/// 把依赖包源码以 `{spec}/{包内相对路径}` 并入 `files`,返回
/// spec → 入口合并键。迭代到不动点(依赖自身的依赖同样并入)。
/// 缓存未命中的 spec 不并入(生成物运行时回退解释),打警告。
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
        // 扫描全部文本中的 "@..." 字符串字面量(spec 不含转义)。
        let mut specs: Vec<String> = Vec::new();
        for text in files.values() {
            let bytes = text.as_bytes();
            // 跳过文档注释行(///)——README 示例的 @spec 会当依赖误报
            // (pinit 的 fletcher 依赖扫描噪声驱动)。
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
            // spec 形如 @ns/name:ver。
            let Some(rest) = spec.strip_prefix('@') else { continue };
            let Some((ns_name, _ver)) = rest.split_once(':') else {
                eprintln!("依赖 spec 格式不支持: {spec}");
                continue;
            };
            let Some((ns, name)) = ns_name.split_once('/') else {
                continue;
            };
            let dir = cache_root.join(ns).join(name).join(
                rest.split_once(':').map(|(_, v)| v).unwrap_or(""),
            );
            // 缺包自动拉取(对照 typst CLI 的 registry 下载,
            // KNOWN #49):registry 形态 {name}-{version}.tar.gz;
            // TRUST_NO_FETCH=1 禁用。失败才回退运行时解释。
            if !dir.join("typst.toml").exists() {
                let ok = std::env::var_os("TRUST_NO_FETCH").is_none()
                    && fetch_package(ns, name, rest.split_once(':').map(|(_, v)| v).unwrap_or(""));
                if !ok {
                    eprintln!("依赖未在本地包缓存命中(运行时解释回退): {spec}");
                    continue;
                }
                eprintln!("依赖缺包已自动拉取: {spec}");
            }
            let manifest = match std::fs::read_to_string(dir.join("typst.toml")) {
                Ok(m) => m,
                Err(_) => {
                    eprintln!("依赖 manifest 读取失败: {spec}");
                    continue;
                }
            };
            let Ok(table) = manifest.parse::<toml::Table>() else {
                eprintln!("依赖 manifest 解析失败: {spec}");
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
            eprintln!("依赖已并入翻译: {spec}(入口 {entry})");
        }
        if !added {
            break;
        }
    }
    dep_entries
}
