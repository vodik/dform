//! The mock provider (`src/fakecloud.rs`) behind the plugin protocol. dform
//! starts it for every mock schema; `dform provider check` checks it.

fn main() -> std::process::ExitCode {
    match dform::fakecloud::serve() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("dform-provider-fake: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
