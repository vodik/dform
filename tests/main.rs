//! The root crate's integration tests, one binary: each file under
//! `tests/` is a module of it, so `cargo test -p dform --test main --
//! golden::` runs what `tests/golden.rs` holds. The few that change
//! process-wide state (the environment, a signal's flag, the executor's
//! hooks, the progress route) are binaries of their own (Cargo.toml).

mod common;
mod inspection_common;
mod lsp_client;
mod tables_common;

mod addresses;
mod adopt;
mod aggregates;
mod apply_block;
mod apply_builtin_only;
mod apply_confirm;
mod apply_events;
mod apply_order;
mod apply_project;
mod apply_retries;
mod apply_tick2;
mod apply_unattended;
