//! The binary plans a program with the mock linked in. (Building it for
//! this test also builds it for the CLI's tests, which run it from beside
//! `dform`.)

use std::process::Command;

#[test]
fn plans_with_the_mock_linked_in() {
    let dir = std::env::temp_dir().join(format!("dform-direct-plan-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("p.df"),
        "edition 2026\n\nresource net.vpc a { cidr = \"10.0.0.0/16\" }\n",
    )
    .unwrap();
    for backend in ["direct", "wire"] {
        let out = Command::new(env!("CARGO_BIN_EXE_dform-direct"))
            .args(["plan", "p.df"])
            .env("DFORM_BACKEND", backend)
            .current_dir(&dir)
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{backend}: {out:?}");
        assert!(stdout.contains("+ net.vpc.a"), "{backend}: {stdout}");
    }
    let _ = std::fs::remove_dir_all(&dir);
}
