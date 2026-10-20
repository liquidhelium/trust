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
        _ => {
            eprintln!(
                "trust {} — typst → rust 转译器\n\n用法:\n  trust translate <input.typ> [--name 模块名] [-o 输出.rs]",
                env!("CARGO_PKG_VERSION")
            );
        }
    }
}
