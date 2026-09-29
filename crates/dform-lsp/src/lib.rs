//! `dform lsp`: the language server (README "Language server"). Three
//! features over the evaluator: a contributors hover (`explain`),
//! diagnostics of the selected environment (`analysis`) and schema
//! completion (`complete`); besides them parse and compile diagnostics,
//! formatting by `dform fmt`'s formatter and go-to-definition of
//! predicates, modules and policies (`nav`). Synchronous over stdio
//! (lsp-server), one evaluation at a time.

pub mod analysis;
pub mod complete;
pub mod explain;
pub mod nav;
pub mod server;
pub mod text;

pub use server::{Options, serve, serve_stdio};
