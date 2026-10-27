//! Suite splitting + translation (the Rust port of the former
//! `_things/typst-tests-probe/split.py`): section headers, note lines,
//! fixture mirroring, and the per-category package translation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// One `--- name attrs ---` section of a suite file.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Case {
    pub name: String,
    /// Category key (`layout-page.typ` → `layout-page`).
    pub key: String,
    /// Package-relative path of the section file (`dir/__t__name.typ`).
    pub file: String,
    /// Original suite-relative source path.
    pub src: String,
    pub attrs: Vec<String>,
    pub notes: Vec<Note>,
}

#[derive(serde::Serialize, serde::Deserialize)]
pub struct Note {
    pub kind: String,
    pub text: String,
}

struct Section {
    name: String,
    attrs: Vec<String>,
    notes: Vec<Note>,
    body: Vec<String>,
}

/// `// Error: [range] message` — the range (a line span, a `2:3-4:5`, or a
/// `"path" 2:3` prefix) is stripped; hints keep their text verbatim.
fn parse_note(line: &str) -> Option<Note> {
    let rest = line.trim_start();
    let (kind, text) = ["Error", "Warning", "Hint"].iter().find_map(|k| {
        let prefix = format!("// {k}: ");
        rest.strip_prefix(&prefix)
            .map(|t| (k.to_lowercase(), t.to_string()))
    })?;
    let text = if kind == "hint" {
        text
    } else {
        strip_range(&text)
    };
    Some(Note { kind, text })
}

/// Mirrors official notes.rs's range-prefix shapes: `18-25`, `2:3`,
/// `2:3-4:5`, `"path" 2:3-4:5`.
fn strip_range(text: &str) -> String {
    let b = text.as_bytes();
    let mut i = 0;
    if b.first() == Some(&b'"') {
        i = 1;
        while i < b.len() && b[i] != b'"' {
            i += 1;
        }
        if i >= b.len() {
            return text.to_string();
        }
        i += 1;
    }
    let start = i;
    let digit = |c: u8| c.is_ascii_digit() || c == b':' || c == b'-';
    while i < b.len() && (b[i].is_ascii_digit() || b[i] == b':') {
        i += 1;
    }
    if i > start && i < b.len() && b[i] == b'-' {
        i += 1;
        while i < b.len() && digit(b[i]) {
            i += 1;
        }
    }
    // A bare number/digit-prefix without a trailing span separator is not
    // a range (official shapes always end the range with whitespace).
    if i > start && i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
        return text[i..].trim_start().to_string();
    }
    text.trim_start().to_string()
}

fn sections_of(text: &str) -> Vec<Section> {
    let mut out: Vec<Section> = Vec::new();
    let mut cur: Option<Section> = None;
    for line in text.split('\n') {
        let t = line.trim_end();
        if let Some(inner) = t.strip_prefix("--- ")
            && let Some(header) = inner.strip_suffix(" ---")
        {
            if let Some(sec) = cur.take() {
                out.push(sec);
            }
            let mut parts = header.split_whitespace();
            let name = parts.next().unwrap_or_default().to_string();
            let attrs = parts.map(str::to_string).collect();
            cur = Some(Section {
                name,
                attrs,
                notes: Vec::new(),
                body: Vec::new(),
            });
        } else if let Some(sec) = cur.as_mut() {
            if t.trim_start().starts_with("// ")
                && let Some(note) = parse_note(t)
            {
                sec.notes.push(note);
            } else {
                sec.body.push(line.to_string());
            }
        }
    }
    if let Some(sec) = cur.take() {
        out.push(sec);
    }
    out
}

fn crate_key(rel: &Path) -> String {
    let s = rel
        .with_extension("")
        .to_string_lossy()
        .replace(['/', '.'], "-")
        .to_lowercase();
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

pub fn split_and_write(root: &Path, build: &Path) -> (Vec<Case>, BTreeMap<String, usize>) {
    // 1) 分段:section 文件 → 类别包;无段文件 = 夹具(镜像进所有包)。
    let mut manifest: Vec<Case> = Vec::new();
    let mut fixtures: BTreeMap<String, String> = BTreeMap::new();
    let mut crates: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    let suite = root.join("suite");
    let reg = root.join("reg");
    let files = walk_typ(&suite);
    let files = files.into_iter().chain(walk_typ(&reg)).collect::<Vec<_>>();
    // reg 文件的类别键加前缀,与官方类目隔离。
    for f in files {
        let in_reg = f.starts_with(&reg);
        let rel = f
            .strip_prefix(if in_reg { &reg } else { &suite })
            .unwrap()
            .to_path_buf();
        let text = std::fs::read_to_string(&f).expect("read suite file");
        let secs = sections_of(&text);
        let key = if in_reg {
            format!("reg-{}", crate_key(&rel))
        } else {
            crate_key(&rel)
        };
        if secs.is_empty() {
            // 夹具与用例同基(包内相对路径):reg 夹具不加前缀——用例文件
            // 在包根,`import "leaf.typ"` 按用例文件的相对解析去找它。
            fixtures.insert(rel.to_string_lossy().into_owned(), text);
            continue;
        }
        for sec in secs {
            assert!(
                seen.insert(sec.name.clone()),
                "重复测试名 {} ({})",
                sec.name,
                rel.display()
            );
            let file_rel = rel
                .parent()
                .map(|p| p.join(format!("__t__{}.typ", sec.name)))
                .unwrap_or_else(|| PathBuf::from(format!("__t__{}.typ", sec.name)));
            let body = format!("{}\n", sec.body.join("\n"));
            crates
                .entry(key.clone())
                .or_default()
                .push((file_rel.to_string_lossy().into_owned(), body.clone()));
            manifest.push(Case {
                name: sec.name,
                key: key.clone(),
                file: file_rel.to_string_lossy().into_owned(),
                src: rel.to_string_lossy().into_owned(),
                attrs: sec.attrs,
                notes: sec.notes,
            });
        }
    }

    // 2) 写类别包源(suite-pkg 形态,同探针)并并行翻译。
    // 翻译器的 vpath 采用官方布局前缀 tests/suite/(cats 磁盘布局不变):
    // gen 侧 span/FileId 的解析深度与基准世界完全一致 —— path()/read()
    // 的逃逸判定和相对解析对深度敏感(../../../../file.txt 从
    // tests/suite/loading/x.typ 逃逸,从更浅路径不逃逸)。
    let prefix = "tests/suite/";
    let cats_dir = build.join("cats");
    let gen_dir = build.join("gen");
    let _ = std::fs::remove_dir_all(&cats_dir);
    let _ = std::fs::remove_dir_all(&gen_dir);
    std::fs::create_dir_all(&cats_dir).expect("create cats");
    std::fs::create_dir_all(&gen_dir).expect("create gen");

    let counts: BTreeMap<String, usize> =
        crates.iter().map(|(k, v)| (k.clone(), v.len())).collect();
    let errors: Mutex<Vec<String>> = Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for (key, sections) in &crates {
            let errors = &errors;
            let (cats_dir, gen_dir, fixtures) = (&cats_dir, &gen_dir, &fixtures);
            s.spawn(move || {
                let pkg_dir = cats_dir.join(key);
                let mut input_files: BTreeMap<String, String> = BTreeMap::new();
                input_files.insert(
                    format!("{prefix}lib.typ"),
                    "// 批驱动入口(空):module_by_path 按需装配\n".into(),
                );
                for (rel, body) in sections {
                    let p = pkg_dir.join(rel);
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent).expect("create case dir");
                    }
                    std::fs::write(&p, body).expect("write case file");
                    input_files.insert(format!("{prefix}{rel}"), body.clone());
                }
                for (rel, text) in fixtures {
                    let p = pkg_dir.join(rel);
                    if let Some(parent) = p.parent() {
                        std::fs::create_dir_all(parent).expect("create fixture dir");
                    }
                    std::fs::write(&p, text).expect("write fixture file");
                    input_files.insert(format!("{prefix}{rel}"), text.clone());
                }
                let input = trust::PackageInput {
                    files: input_files,
                    entry: format!("{prefix}lib.typ"),
                    dep_entries: BTreeMap::new(),
                };
                let (tree, _diags) =
                    trust::translate_package_tree_lenient(&input, &format!("suite-{key}"));
                for (path, code) in &tree {
                    let out = gen_dir.join(key).join(path);
                    if let Some(parent) = out.parent() {
                        std::fs::create_dir_all(parent).expect("create gen dir");
                    }
                    std::fs::write(&out, code).expect("write gen file");
                }
                if tree.is_empty() {
                    errors
                        .lock()
                        .unwrap()
                        .push(format!("类别 {key} 翻译产物为空"));
                }
            });
        }
    });
    let errors = errors.into_inner().unwrap();
    assert!(errors.is_empty(), "翻译失败: {errors:?}");

    (manifest, counts)
}

fn walk_typ(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        let mut level: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        level.sort();
        for p in level {
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|e| e.to_str()) == Some("typ") {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}
