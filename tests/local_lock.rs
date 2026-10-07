//! The local backend's lock (R-139) is the kernel's (`flock`), which dies
//! with its holder: the pid in the file is for messages only. Two takers
//! of a lock a killed apply left never both hold it; a file naming a live
//! process that holds no lock is taken, and `stack unlock` breaks it; a
//! held one is refused whatever pid it names.

use dform::store::{LOCK, LocalStore, Store};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::time::Duration;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dform-local-lock-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Two takers race over a lock file a killed apply left (a pid that is
/// gone), 200 times: exactly one holds it each time. With the pid file,
/// both could read the dead pid, and the second remove the first's live
/// lock.
#[test]
fn two_takers_of_a_stale_lock_never_both_hold_it() {
    let dir = scratch("race");
    let state = dir.join("state.json");
    let mut dead = std::process::Command::new("true").spawn().unwrap();
    let pid = dead.id();
    dead.wait().unwrap();
    for round in 0..200 {
        std::fs::write(dir.join("state.lock"), format!("{pid}\n")).unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let holding = Arc::new(AtomicUsize::new(0));
        let takers: Vec<_> = (0..2)
            .map(|_| {
                let (state, barrier, holding) = (state.clone(), barrier.clone(), holding.clone());
                std::thread::spawn(move || {
                    let s = LocalStore::beside(&state);
                    barrier.wait();
                    let Ok(l) = s.acquire(LOCK, "app", "x", Duration::ZERO) else {
                        return (false, 0);
                    };
                    let at_once = holding.fetch_add(1, Ordering::SeqCst) + 1;
                    // Held across the other's attempt.
                    std::thread::sleep(Duration::from_millis(2));
                    holding.fetch_sub(1, Ordering::SeqCst);
                    s.release(&l).unwrap();
                    (true, at_once)
                })
            })
            .collect();
        let got: Vec<(bool, usize)> = takers.into_iter().map(|t| t.join().unwrap()).collect();
        assert!(got.iter().any(|g| g.0), "round {round}: no one took it");
        assert!(
            got.iter().all(|g| g.1 <= 1),
            "round {round}: both held the lock at once"
        );
        assert!(
            !dir.join("state.lock").exists(),
            "round {round}: a released lock is removed"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Exactly one of two takers holds a lock at once: the second is refused
/// while the first holds it, naming the first's pid.
#[test]
fn a_held_lock_refuses_the_second_taker() {
    let dir = scratch("held");
    let a = LocalStore::beside(&dir.join("state.json"));
    let b = LocalStore::beside(&dir.join("state.json"));
    let l = a.acquire(LOCK, "app", "x", Duration::ZERO).unwrap();
    let e = b.acquire(LOCK, "app", "x", Duration::ZERO).unwrap_err();
    assert!(
        e.to_string().contains(&format!(
            "stack app is locked by another apply (pid {})",
            std::process::id()
        )),
        "{e}"
    );
    let e = b.break_lease(LOCK, "app").unwrap_err();
    assert!(
        e.to_string().contains("is locked by a running apply"),
        "{e}"
    );
    a.release(&l).unwrap();
    let l = b.acquire(LOCK, "app", "x", Duration::ZERO).unwrap();
    b.release(&l).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A lock file naming a live process (this test's parent) that holds no
/// lock is taken: a reused pid does not make the lock permanent. And
/// `stack unlock` on such a file succeeds.
#[test]
fn a_lock_file_naming_a_live_pid_that_holds_nothing_is_taken() {
    let dir = scratch("reused");
    let parent = std::os::unix::process::parent_id();
    std::fs::write(dir.join("state.lock"), format!("{parent}\n")).unwrap();
    let s = LocalStore::beside(&dir.join("state.json"));
    let msg = s.break_lease(LOCK, "app").unwrap();
    assert!(msg.contains("stack app unlocked"), "{msg}");
    assert!(!dir.join("state.lock").exists());
    std::fs::write(dir.join("state.lock"), format!("{parent}\n")).unwrap();
    let l = s.acquire(LOCK, "app", "x", Duration::ZERO).unwrap();
    assert_eq!(
        std::fs::read_to_string(dir.join("state.lock")).unwrap(),
        format!("{}\n", std::process::id())
    );
    s.release(&l).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

/// A release that fails says so: the lock file cannot be removed (its
/// directory is read-only), and the error names it.
#[test]
fn a_release_that_fails_says_so() {
    use std::os::unix::fs::PermissionsExt;
    let dir = scratch("release");
    let sub = dir.join("d");
    std::fs::create_dir_all(&sub).unwrap();
    let s = LocalStore::beside(&sub.join("state.json"));
    let l = s.acquire(LOCK, "app", "x", Duration::ZERO).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500)).unwrap();
    let r = s.release(&l);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Root may remove it anyway.
    if let Err(e) = r {
        assert!(format!("{e:#}").contains("remove"), "{e:#}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
