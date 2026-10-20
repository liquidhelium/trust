//! 差分测试:同一 fixture,typst-eval 解释执行(golden)与 trust 生成物
//! 执行,逐调用比较值(§5.1 过程验证)。

pub mod gen_modules {
    include!(concat!(env!("OUT_DIR"), "/gen/mod.rs"));
}
