//! The mock provider (`dform-mock`) behind the plugin protocol, written
//! with the SDK: natively the executable dform starts for every mock
//! schema and `dform provider check` checks (`src/main.rs`), as a
//! component the same provider in dform's wasm host. Its world, inventory
//! and schemas are files, so it uses the filesystem (`wasi:filesystem`,
//! which the host grants the mock).

dform_sdk::provider!(dform_mock::Mock::process(), uses = ["wasi:filesystem"]);
