//! Suite differential harness: per case, golden (interpreter) vs gen
//! (translated assembly), judged by the former probe runner's rules and
//! gated against `known-divergence.txt` (both directions: no new failure,
//! no silent fix without updating the list).

use comemo::Track;
use std::collections::BTreeMap;
use std::collections::BTreeSet;

#[path = "../src/dispatch.rs"]
mod dispatch;

use trust_suite::split::Case;
use trust_world::InjectWorld;
use typst::World;
use typst::engine::{Engine, Route, Sink, Traced};
use typst::foundations::{Context, Module};
use typst::introspection::EmptyIntrospector;
use typst::text::Font;
use typst_layout::PagedDocument;

struct Compiled {
    pages: Result<Vec<String>, Vec<String>>,
    warnings: Vec<String>,
}

fn compile(w: &InjectWorld) -> Compiled {
    let warned = typst::compile::<PagedDocument>(w as &dyn World);
    let warnings = warned
        .warnings
        .iter()
        .map(|d| d.message.to_string())
        .filter(|m| !m.contains("trust 降级"))
        .collect();
    match warned.output {
        Ok(doc) => {
            let opts = typst_svg::SvgOptions::default();
            Compiled {
                pages: Ok(doc.pages().iter().map(|p| typst_svg::svg(p, &opts)).collect()),
                warnings,
            }
        }
        Err(errs) => Compiled {
            pages: Err(errs.iter().map(|e| e.message.to_string()).collect()),
            warnings,
        },
    }
}

/// 单文件按需装配(直连求值,不经 memo)后注入全局 `tm`。
fn assemble(w: &mut InjectWorld, key: &str, file: &str) -> Result<(Module, Vec<String>), Vec<String>> {
    let mut sink = Sink::new();
    let library = w.library().clone();
    let world: &dyn World = w;
    let traced = Traced::default();
    let mut engine = Engine {
        library: &library,
        world: world.track(),
        introspector: typst::utils::Protected::new(EmptyIntrospector.track()),
        traced: traced.track(),
        sink: sink.track_mut(),
        route: Route::default(),
    };
    let context = Context::none();
    let result = dispatch::dispatch_by_path(&key.to_string(), file, &mut engine, context.track());
    let mut warnings: Vec<String> = sink
        .warnings()
        .iter()
        .map(|d| d.message.to_string())
        .filter(|m| !m.contains("trust 降级"))
        .collect();
    match result {
        Some(Ok(module)) => {
            w.inject_module("tm", module.clone());
            Ok((module, std::mem::take(&mut warnings)))
        }
        Some(Err(diags)) => Err(diags.iter().map(|d| d.message.to_string()).collect()),
        None => Err(vec![format!("module_by_path 未登记: {file}")]),
    }
}

fn judge(case: &Case, golden: &Compiled, gen_c: &Compiled, paged: bool) -> (String, String) {
    let expected_error = case.notes.iter().any(|n| n.kind == "error");
    let golden_ok = golden.pages.is_ok();
    if expected_error != !golden_ok {
        return (
            "golden-anomaly".into(),
            format!(
                "注记与基准不符:期望错误={expected_error}, 基准通过={golden_ok}, notes={:?}, 基准诊断={:?}",
                case.notes.iter().filter(|n| n.kind == "error").map(|n| n.text.clone()).collect::<Vec<_>>(),
                golden.pages.as_ref().err()
            ),
        );
    }
    let norm = |v: &[String]| -> Vec<String> {
        v.iter()
            .map(|m| m.replace("VERSION", "0.15.1"))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    };
    if expected_error {
        let gm = golden.pages.as_ref().err().cloned().unwrap_or_default();
        let nm = gen_c.pages.as_ref().err().cloned().unwrap_or_default();
        if nm.is_empty() {
            return ("gen-missed-error".into(), format!("期望错误 {gm:?},gen 通过"));
        }
        let (a, b) = (norm(&gm), norm(&nm));
        if a == b {
            return ("pass".into(), String::new());
        }
        return ("diag-mismatch".into(), format!("golden={a:?} gen={b:?}"));
    }
    if let Err(e) = &gen_c.pages {
        return ("gen-error".into(), format!("gen 意外错误: {e:?}"));
    }
    let warn_notes: Vec<String> = case
        .notes
        .iter()
        .filter(|n| n.kind == "warning")
        .map(|n| n.text.clone())
        .collect();
    if !warn_notes.is_empty() && norm(&warn_notes) != norm(&gen_c.warnings) {
        return (
            "warning-mismatch".into(),
            format!("期望 {:?};gen {:?}", norm(&warn_notes), norm(&gen_c.warnings)),
        );
    }
    if !paged {
        return ("pass".into(), String::new());
    }
    let gp = golden.pages.as_ref().ok().unwrap();
    let np = gen_c.pages.as_ref().ok().unwrap();
    if gp.len() != np.len() {
        return ("page-count".into(), format!("页数 {} vs {}", gp.len(), np.len()));
    }
    for (i, (a, b)) in gp.iter().zip(np.iter()).enumerate() {
        if a != b {
            if let Ok(dump) = std::env::var("TRUST_SUITE_DUMP") {
                if dump == case.name {
                    let _ = std::fs::write(format!("/tmp/suite-{}-g.svg", case.name), a);
                    let _ = std::fs::write(format!("/tmp/suite-{}-n.svg", case.name), b);
                }
            }
            return ("svg-diff".into(), format!("第 {} 页 SVG 不一致", i + 1));
        }
    }
    ("pass".into(), String::new())
}

fn walk(root: &std::path::Path) -> Vec<(std::path::PathBuf, Vec<u8>)> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if let Ok(data) = std::fs::read(&p) {
                out.push((p, data));
            }
        }
    }
    out
}

#[test]
fn suite() {
    std::thread::Builder::new()
        .stack_size(512 * 1024 * 1024)
        .spawn(run)
        .unwrap()
        .join()
        .unwrap();
}

fn run() {
    let root = std::path::PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest: Vec<Case> =
        serde_json::from_str(&std::fs::read_to_string(root.join("manifest.json")).expect(
            "manifest.json 缺失:先跑 cargo run -p trust-suite --bin refresh",
        ))
        .expect("manifest 解析");

    let filters: Vec<String> = std::env::var("TRUST_SUITE_FILTER")
        .map(|f| f.split(',').map(str::to_string).collect())
        .unwrap_or_default();
    let max: usize = std::env::var("TRUST_SUITE_MAX")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(usize::MAX);

    let known: BTreeSet<String> = std::fs::read_to_string(root.join("tests/known-divergence.txt"))
        .expect("known-divergence.txt")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| l.trim().to_string())
        .collect();

    // 共享基座:字体 + 套件源(cats 树按套件路径与包内路径双键挂载)
    // + 可选开发资产(typst-dev-assets 的字体与 files/,存在才挂——
    // 资产类用例(image/svg/bib)依赖它,缺失时它们落入 golden-anomaly)。
    let mut fonts: Vec<Font> = typst_assets::fonts()
        .flat_map(|data| Font::iter(typst::foundations::Bytes::new(data)))
        .collect();
    let dev = std::path::PathBuf::from(
        std::env::var("TRUST_DEV_ASSETS").unwrap_or_else(|_| {
            format!("{}/code/typst-dev-assets/files", std::env::var("HOME").unwrap_or_default())
        }),
    );
    let dev_present = dev.exists();
    if dev_present {
        for (p, data) in walk(&dev) {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if ext == "ttf" || ext == "otf" {
                fonts.extend(Font::iter(typst::foundations::Bytes::new(data)));
            }
        }
    }
    let mut base = InjectWorld::new("// unset", vec![]);
    base.enable_suite();
    base.set_fonts(fonts);
    if dev_present {
        for (p, data) in walk(&dev) {
            let rel = p.strip_prefix(&dev).unwrap();
            // 双键:根相对 `assets/...` 与 suite 前缀 `suite/assets/...`
            // (用例的 `../../../assets` 相对解析落在后者)。
            base.add_file(&format!("assets/{}", rel.to_string_lossy()), data.clone());
            base.add_file(&format!("suite/assets/{}", rel.to_string_lossy()), data);
        }
    }
    // 本地 @test 包(官方 tests/packages/**,镜像官方 harness 的包根解析)。
    let tpkgs = std::path::PathBuf::from(
        std::env::var("TRUST_TEST_PACKAGES").unwrap_or_else(|_| {
            format!(
                "{}/code/typst-0.15.1-tests/tests/packages",
                std::env::var("HOME").unwrap_or_default()
            )
        }),
    );
    if tpkgs.exists() {
        for (p, data) in walk(&tpkgs) {
            let Ok(rel) = p.strip_prefix(&tpkgs) else { continue };
            let mut comps = rel.components();
            let Some(first) = comps.next() else { continue };
            let dir = first.as_os_str().to_string_lossy();
            // <name>-<version> 目录 → @test/<name>:<version> 包根。
            let Some((pkg, version)) = dir.rsplit_once('-') else { continue };
            let inner: std::path::PathBuf = comps.collect();
            base.add_package_file(
                &format!("@test/{pkg}:{version}"),
                &inner.to_string_lossy(),
                data,
            );
        }
    }
    for (p, data) in walk(&root.join("cats")) {
        if p.extension().and_then(|e| e.to_str()) == Some("typ")
            && p.file_name().and_then(|n| n.to_str()) != Some("lib.typ")
        {
            let rel = p.strip_prefix(root.join("cats")).unwrap();
            let text = String::from_utf8_lossy(&data).into_owned();
            base.add_source(&format!("suite/cats/{}", rel.to_string_lossy()), &text);
            let mut comps = rel.components();
            let _key = comps.next();
            let unprefixed: std::path::PathBuf = comps.collect();
            let unprefixed = unprefixed.to_string_lossy().to_string();
            if !unprefixed.is_empty() {
                base.add_source(&unprefixed, &text);
            }
        }
    }

    let mut cases = manifest;
    // html/pdf/pdftags/bundle 超出当前管线(paged/SVG),整段跳过(与旧探针
    // 口径一致;known-divergence 名单按此过滤后的集合维护)。
    cases.retain(|c| {
        !c.attrs
            .iter()
            .any(|a| a.starts_with("html") || a.starts_with("pdf") || a.starts_with("bundle"))
    });
    if !filters.is_empty() {
        cases.retain(|c| filters.iter().any(|f| c.name.contains(f.as_str())));
    }
    cases.truncate(max);

    let mut by_status: BTreeMap<String, usize> = BTreeMap::new();
    let mut new_failures = Vec::new();
    let mut flaky = Vec::new();
    let mut fixed = Vec::new();
    let mut unexpected_pass = Vec::new();
    let mut fail_details = Vec::new();

    let run_case = |case: &Case, base: &InjectWorld, root: &std::path::Path| -> (String, String) {
        comemo::evict(0);
        let body = std::fs::read_to_string(root.join("cats").join(&case.key).join(&case.file))
            .expect("case body");
        let path = format!("suite/cats/{}/{}", case.key, case.file);
        let paged = case
            .attrs
            .iter()
            .any(|a| a == "paged" || a == "empty" || a == "large");
        let mut golden = base.clone();
        golden.set_main(&path, &body);
        let g = compile(&golden);
        comemo::evict(0);
        let gen_compiled2 = {
            let mut w = base.clone();
            match assemble(&mut w, &case.key, &case.file) {
                Ok((_, mut asm_warnings)) => {
                    // main 与 golden 用完全相同的 vpath(两世界各自
                    // clone,不冲突):用例内相对路径的解析基准一致。
                    w.set_main(&path, "#include tm\n");
                    let mut c = compile(&w);
                    c.warnings.append(&mut asm_warnings);
                    c
                }
                Err(msgs) => Compiled { pages: Err(msgs), warnings: Vec::new() },
            }
        };
        judge(case, &g, &gen_compiled2, paged)
    };

    for case in &cases {
        let (status, detail) = run_case(case, &base, &root);
        *by_status.entry(status.clone()).or_default() += 1;
        if status != "pass" {
            if !known.contains(&case.name) {
                // 单例复跑闸:同轮内立刻重跑一次,复跑通过 = 闪烁(告警
                // 不判死);真回归两轮皆挂才进新增名单。
                let (retry_status, retry_detail) = run_case(case, &base, &root);
                if retry_status == "pass" {
                    flaky.push(case.name.clone());
                    continue;
                }
                fail_details.push(format!("{}  [{}] {}", case.name, retry_status, retry_detail));
                new_failures.push(case.name.clone());
            } else {
                fail_details.push(format!("{}  [{}] {}", case.name, status, detail));
            }
        } else if known.contains(&case.name) {
            unexpected_pass.push(case.name.clone());
        }
    }
    if !flaky.is_empty() {
        println!("  ⚠ 闪烁(复跑通过,已豁免): {}", flaky.join(", "));
    }
    for k in &known {
        if !cases.iter().any(|c| c.name == *k) && filters.is_empty() {
            fixed.push(k.clone()); // 已不在名单中产生失败(名单过期)
        }
    }

    let total: usize = by_status.values().sum();
    println!("══ trust-suite:{total} 测试(known-divergence {} 项)══", known.len());
    for (k, v) in &by_status {
        println!("  {k:<24} {v}");
    }
    for f in fail_details.iter().take(20) {
        println!("  ✗ {f}");
    }
    if fail_details.len() > 20 {
        println!("  … 另 {} 个失败", fail_details.len() - 20);
    }
    assert!(
        new_failures.is_empty(),
        "新增失败(不在 known-divergence.txt):\n  {}",
        new_failures.join("\n  ")
    );
    assert!(
        unexpected_pass.is_empty(),
        "known-divergence 中的用例现在通过(更新名单并审视):\n  {}",
        unexpected_pass.join("\n  ")
    );
    assert!(fixed.is_empty(), "名单含未运行/已消失的用例(修剪名单):\n  {}", fixed.join("\n  "));
}
