//! Rust evaluator for the Nix expression language, reached from cppnix
//! through the C ABI in `capi`. Pipeline: rnix CST -> `compile` -> `ir`
//! module -> `vm` -> `print`. See ENG-12068.

pub mod builtins;
pub mod capi;
pub mod compile;
pub mod eval;
pub mod host;
pub mod ir;
pub mod print;
pub mod task;
pub mod value2;
pub mod vm;
pub mod builtins2;
pub mod builtins3;
pub mod builtins_gen;
