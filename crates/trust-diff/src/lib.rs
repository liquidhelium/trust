//! Differential testing: the same fixture is executed by the typst-eval
//! interpreter (golden) and by the trust-generated code, comparing values
//! call by call.

pub mod gen_modules {
    include!(concat!(env!("OUT_DIR"), "/gen/mod.rs"));
}
