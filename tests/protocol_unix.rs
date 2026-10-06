//! A provider on a unix socket: `DFORM_PROVIDER_TRANSPORT=unix` makes the
//! mock listen on a socket in the temporary directory and name it in its
//! handshake; dform dials it, and the socket is gone after the run.

mod common;
use common::Scratch;
use std::io::BufRead;
use std::process::{Command, Stdio};

fn fake() -> String {
    common::exe("dform-provider-fake")
}

const PROG: &str = r#"

resource net.vpc main { cidr = "10.0.0.0/16" }
resource net.subnet a { vpc_id = ref(net.vpc, "main", "id"), cidr = "10.0.1.0/24" }
use fake
"#;

fn sockets(dir: &std::path::Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".sock"))
        .collect()
}

#[test]
fn the_mock_serves_a_unix_socket_and_dform_dials_it() {
    let s = Scratch::new("unix");
    let tmp = s.path("t");
    std::fs::create_dir_all(&tmp).unwrap();

    // The handshake names the socket.
    let mut child = Command::new(fake())
        .env("DFORM_PROVIDER_TRANSPORT", "unix")
        .env("TMPDIR", &tmp)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    let sock = tmp.join(format!("dform-provider-{}.sock", child.id()));
    assert_eq!(
        line,
        format!("dform-provider|1|unix://{}\n", sock.display())
    );
    assert!(sock.exists());
    // Closing stdin ends it, and it removes its socket.
    drop(child.stdin.take());
    child.wait().unwrap();
    assert!(!sock.exists());

    // dform dials it: apply, then an undeformed plan.
    s.write("p.df", PROG);
    let run = |args: &[&str]| -> common::Run {
        common::dform()
            .args(common::on("p.df", &["--world", "w.json"], args))
            .current_dir(&s.dir)
            .env("DFORM_PROVIDER_TRANSPORT", "unix")
            .env("TMPDIR", &tmp)
            .output()
            .unwrap()
            .into()
    };
    let r = run(&["apply"]).success();
    assert!(r.stdout.ends_with("apply: complete\n"), "{}", r.stdout);
    let r = run(&["plan"]).success();
    assert_eq!(r.summary(), "stack p is up to date", "{}", r.stdout);
    assert_eq!(sockets(&tmp), Vec::<String>::new());

    // Anything else is refused by the provider, naming the variable.
    let out = Command::new(fake())
        .env("DFORM_PROVIDER_TRANSPORT", "carrier-pigeon")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("DFORM_PROVIDER_TRANSPORT=carrier-pigeon: expected tcp or unix"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
