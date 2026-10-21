//! `trust` CLI:typst → rust 转译器命令行。

use std::io::Write;

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
                        eprintln!("{input}: {:?}: {}", diag.span, diag.message);
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
            let entry = files
                .keys()
                .filter(|k| k.ends_with("lib.typ"))
                .min_by_key(|k| k.split('/').count())
                .cloned()
                .expect("entry lib.typ");
            // 只收 entry 所在的包源树(docs/tests 等不入包)。
            let src_root = entry
                .rsplit_once('/')
                .map(|(dir, _)| format!("{dir}/"))
                .unwrap_or_default();
            if !src_root.is_empty() {
                files.retain(|k, _| k.starts_with(&src_root));
            }
            let pkg = trust::PackageInput { files, entry };
            let name = root.file_name().and_then(|s| s.to_str()).unwrap_or("pkg");
            // `-o 目录/`(尾斜杠):目录形态,镜像源码树的多文件产物。
            if let Some(dir) = output.strip_suffix('/') {
                if lenient {
                    eprintln!("目录形态暂不支持 --lenient");
                    std::process::exit(2);
                }
                match trust::translate_package_tree(&pkg, name) {
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
                            eprintln!("  {:?}: {}", d.span, d.message);
                        }
                        std::process::exit(1);
                    }
                }
                return;
            }
            if lenient {
                let (code, diags) = trust::translate_package_lenient(&pkg, name);
                for d in &diags {
                    eprintln!("降级 {:?}: {}", d.span, d.message);
                }
                std::fs::write(output, &code).expect("write");
                println!(
                    "已生成 {output}({} 字节,降级 {} 处,清单见上方与文件头注释)",
                    code.len(),
                    diags.len()
                );
            } else {
                match trust::translate_package(&pkg, name) {
                    Ok(code) => {
                        std::fs::write(output, &code).expect("write");
                        println!("已生成 {output}({} 字节)", code.len());
                    }
                    Err(diags) => {
                        for d in diags {
                            eprintln!("  {:?}: {}", d.span, d.message);
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
            let input = trust::PackageInput { files: map, entry };
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
