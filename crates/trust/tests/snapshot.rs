//! 生成物快照:锁定代表性输入的生成源码形状(§5.1 过程验证的一项)。
//!
//! 更新快照:`UPDATE_SNAPSHOTS=1 cargo test -p trust --test snapshot`。

use std::path::PathBuf;

const INPUTS: &[(&str, &str)] = &[
    ("function", "#let add(a, b) = a + b\n"),
    (
        "rich",
        "#let scale(x, k: 2) = x * k\n#let base = 10\n#let wrap(f, ..rest) = f(..rest)\n",
    ),
    (
        "control",
        "#let pick(items) = {\n  let out = ()\n  for x in items {\n    if x > 0 { out.push(x) }\n  }\n  out\n}\n",
    ),
];

#[test]
fn snapshots_match() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/snapshots");
    std::fs::create_dir_all(&dir).expect("create snapshot dir");
    let update = std::env::var("UPDATE_SNAPSHOTS").is_ok();

    for (name, src) in INPUTS {
        let out = trust::translate(src, name).unwrap_or_else(|diags| {
            panic!(
                "快照输入 {name} 翻译失败: {:?}",
                diags.iter().map(|d| &d.message).collect::<Vec<_>>()
            )
        });
        let path = dir.join(format!("{name}.rs"));
        if update || !path.exists() {
            std::fs::write(&path, &out).expect("write snapshot");
            continue;
        }
        let expect = std::fs::read_to_string(&path).expect("read snapshot");
        assert_eq!(
            expect, out,
            "生成物与快照不一致(输入 {name});确认变更合理后用 UPDATE_SNAPSHOTS=1 更新"
        );
    }
}
