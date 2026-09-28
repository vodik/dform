//! The binary starts, prints its handshake line, and exits when its stdin
//! closes. (Building it for this test also builds it for the CLI's tests,
//! which spawn it from beside `dform`.)

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

#[test]
fn prints_its_handshake_and_exits_when_stdin_closes() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_dform-provider-fake"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert!(line.starts_with("dform-provider|1|"), "{line:?}");
    drop(child.stdin.take());
    assert!(child.wait().unwrap().success());
}
