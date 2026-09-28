//! `dform-direct`: the command line (`dform::cli`) with the mock linked in
//! instead of spawned: the direct backend, or with `DFORM_BACKEND=wire`
//! the wire backend (every message encoded and decoded through prost).
//! Calls answer on the simulated clock, or with `DFORM_SEED=N` in an
//! order that seed picks. A plugin executable is out of its reach. For
//! tests and benches.

use dform_core::plugin::queue::Order;
use dform_mock::Linked;
use std::process::ExitCode;

fn main() -> ExitCode {
    let wire = match std::env::var("DFORM_BACKEND").as_deref() {
        Ok("wire") => true,
        Ok("direct") | Err(_) => false,
        Ok(other) => {
            eprintln!("dform-direct: DFORM_BACKEND={other}: expected direct or wire");
            return ExitCode::FAILURE;
        }
    };
    let order = match std::env::var("DFORM_SEED").map(|s| s.parse::<u64>()) {
        Err(_) => Order::Clock,
        Ok(Ok(seed)) => Order::Seed(seed),
        Ok(Err(e)) => {
            eprintln!("dform-direct: DFORM_SEED: {e}");
            return ExitCode::FAILURE;
        }
    };
    let linked: &'static Linked = Box::leak(Box::new(Linked { order, wire }));
    dform::cli::main(linked, std::env::args_os())
}
