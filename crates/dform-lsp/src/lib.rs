//! `dform lsp`: the language server (README "Language server"). Three
//! features over the evaluator: a contributors hover with the docs of
//! what is under point (`explain`), diagnostics of the selected
//! environment (`analysis`) and schema completion (`complete`); besides
//! them parse and compile diagnostics, formatting by `dform fmt`'s
//! formatter, go-to-definition of predicates, modules and policies
//! (`nav`), quick fixes (`actions`), references (`refs`), rename
//! (`rename`) and signature help (`signature`). Synchronous over stdio
//! (lsp-server), one evaluation at a time.

pub mod actions;
pub mod analysis;
pub mod cells;
pub mod complete;
pub mod explain;
pub mod inlay;
pub mod nav;
pub mod refs;
pub mod rename;
pub mod server;
pub mod signature;
pub mod text;

pub use server::{Options, serve, serve_stdio};
