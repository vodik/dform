//! A long-running controller does not grow per event: the program is
//! parsed once (a file again only when it changed) and the diagnostics
//! registry holds one evaluation's sources. Measured as the registry's
//! length after every event (`DFORM_TEST_SOURCES`), not RSS: exact, and
//! not at the allocator's mercy.

mod common;
use common::Scratch;
use std::io::BufRead;
use std::process::Command;

const WORKLOAD: &str = include_str!("../examples/bootstrap/stacks/workload.df");
const EVENTS: usize = 200;

fn release(s: &Scratch, image: &str) {
    s.write(
        "data/release.facts",
        &format!("edition 2026\n\nrelease(\"{image}\")\n"),
    );
}

#[test]
fn two_hundred_events_keep_the_source_registry_bounded() {
    let s = Scratch::project("ctl-memory");
    s.write("stacks/workload.df", WORKLOAD);
    release(&s, "gcr.io/renfry/web:0");
    s.write("data/approvals.facts", "edition 2026\n");
    let counts = s.path("sources.txt");
    let mut child = Command::new(env!("CARGO_BIN_EXE_dform"))
        .args([
            "controller",
            "run",
            "--poll",
            "5",
            "--max-events",
            &EVENTS.to_string(),
            "stacks/workload.df",
        ])
        .env("DFORM_TEST_SOURCES", &counts)
        .current_dir(&s.dir)
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let stdout = child.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    // Every event is a new release: a source whose text is new each time.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(240);
    let mut done = 0;
    let mut lines = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left) {
            Ok(l) => {
                if l.ends_with("is undeformed") {
                    done += 1;
                    if done < EVENTS {
                        release(&s, &format!("gcr.io/renfry/web:{done}"));
                    }
                }
                lines.push(l);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            Err(e) => {
                let _ = child.kill();
                panic!(
                    "controller did not finish: {e}; {done} events; last lines: {:#?}",
                    &lines[lines.len().saturating_sub(10)..]
                );
            }
        }
    }
    assert!(child.wait().unwrap().success());
    assert_eq!(
        done,
        EVENTS,
        "{:#?}",
        &lines[lines.len().saturating_sub(10)..]
    );
    let counts: Vec<usize> = s
        .read("sources.txt")
        .lines()
        .map(|l| l.parse().unwrap())
        .collect();
    assert_eq!(counts.len(), EVENTS);
    // After the first event the registry holds the program's files; every
    // later event adds nothing that outlives it.
    assert!(
        counts.iter().all(|&n| n == counts[0]),
        "sources per event: first {:?}, last {:?}",
        &counts[..5],
        &counts[EVENTS - 5..]
    );
}
