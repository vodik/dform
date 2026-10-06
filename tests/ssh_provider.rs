//! The built-in `ssh` provider (`use ssh`): `ssh.read` over SFTP and
//! `ssh.run` over exec, against a real sshd each test starts on a free
//! port with a throwaway host key, and a client key in a scratch `HOME`
//! (no agent). A host that does not answer yet, and a file that is not
//! there yet, are "not yet": the apply waits (R-81). A host key is
//! recorded by the first apply and checked after; a changed one is an
//! error until `dform state forget-host`.
//!
//! The one shell-out allowed in tests: the `sshd` binary. With none
//! installed, each test says so and passes.

mod common;
use common::{Run, Scratch, dform, repo, yes};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// The sshd binary, if one is installed.
fn sshd_binary() -> Option<PathBuf> {
    let found = ["/usr/sbin/sshd", "/usr/bin/sshd", "/usr/local/sbin/sshd"]
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists());
    if found.is_none() {
        eprintln!("skipped: no sshd binary installed");
    }
    found
}

/// A throwaway ed25519 key from `seed`.
fn key(seed: u8) -> PrivateKey {
    PrivateKey::from(Ed25519Keypair::from_seed(&[seed; 32]))
}

fn fingerprint(k: &PrivateKey) -> String {
    k.public_key().fingerprint(HashAlg::Sha256).to_string()
}

fn write_private(path: &Path, k: &PrivateKey) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, k.to_openssh(LineEnding::LF).unwrap().as_bytes()).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

/// The user the tests run as: the one sshd lets in.
fn user() -> String {
    // SAFETY: getpwuid's record is read before any other call to it.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        assert!(!pw.is_null());
        std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned()
    }
}

/// A port nothing listens on (as of now).
fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

/// An sshd on `port` with the host key `host_seed`, letting in the key
/// `client` holds; killed on drop.
struct Sshd {
    child: Child,
}

impl Sshd {
    fn start(bin: &Path, s: &Scratch, port: u16, host_seed: u8) -> Sshd {
        let dir = s.path("sshd");
        std::fs::create_dir_all(&dir).unwrap();
        let host_key = dir.join(format!("host_key_{host_seed}"));
        write_private(&host_key, &key(host_seed));
        std::fs::write(
            dir.join("authorized_keys"),
            key(CLIENT).public_key().to_openssh().unwrap() + "\n",
        )
        .unwrap();
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile none\n\
                 AuthorizedKeysFile {}\nStrictModes no\nUsePAM no\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\nSubsystem sftp internal-sftp\nLogLevel ERROR\n",
                host_key.display(),
                dir.join("authorized_keys").display(),
            ),
        )
        .unwrap();
        let child = Command::new(bin)
            .args(["-D", "-e", "-f"])
            .arg(&config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut sshd = Sshd { child };
        let start = Instant::now();
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            if let Ok(Some(status)) = sshd.child.try_wait() {
                panic!("sshd exited: {status}");
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "sshd did not listen"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        sshd
    }
}

impl Drop for Sshd {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The client's key seed, and the host keys'.
const CLIENT: u8 = 7;
const HOST_A: u8 = 1;
const HOST_B: u8 = 2;

/// A project whose vpc's cidr is what the host prints, and whose secret's
/// password (sensitive) is the file the host holds, as a cluster's
/// kubeconfig configures its provider: the client key in its `home/.ssh`.
fn project(name: &str, port: u16) -> Scratch {
    let s = Scratch::project(name);
    std::fs::create_dir_all(s.path("home/.ssh")).unwrap();
    write_private(&s.path("home/.ssh/id_ed25519"), &key(CLIENT));
    std::fs::create_dir_all(s.path("remote")).unwrap();
    s.write(
        "p.df",
        &format!(
            "\nuse fake\nuse ssh\n\n\
             let host = \"127.0.0.1:{port}\"\n\
             let cidr = ssh.run(host, \"{user}\", \"cat {dir}/cidr\")\n\
             let raw = ssh.read(host, \"{user}\", \"{dir}/k3s.yaml\")\n\
             resource net.vpc v {{ cidr }}\n\
             resource db.secret kube {{ password = raw }}\n",
            user = user(),
            dir = s.path("remote").display(),
        ),
    );
    s.write(
        "providers/fake/schema.df",
        &(std::fs::read_to_string(repo().join("crates/dform-mock/schemas/fake.df")).unwrap()
            + "type_provider(db.secret, \"fakecloud\")\n\
               type_attr(db.secret, \"password\", \"string\", [\"sensitive\"])\n"),
    );
    s.write("remote/cidr", "10.7.0.0/16");
    s.write("remote/k3s.yaml", "token: KUBE-SECRET-ONE\n");
    s
}

/// `dform ARGS` in `s`, its `home` as HOME and no agent.
fn run(s: &Scratch, args: &[&str]) -> Run {
    let out = dform()
        .args(yes(args))
        .current_dir(&s.dir)
        .env("HOME", s.path("home"))
        .env_remove("SSH_AUTH_SOCK")
        .env("DFORM_WAIT_POLL_MS", "50")
        .output()
        .unwrap();
    Run::from(out)
}

/// The objects state maps.
fn identities(s: &Scratch) -> Vec<String> {
    state(s)["resources"]
        .as_object()
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default()
}

fn state(s: &Scratch) -> serde_json::Value {
    s.json("dform.state/p/state.json")
}

/// `ssh.run`'s stdout is the vpc's cidr; `ssh.read`'s content never
/// prints and the plan file holds only its digest. An apply of the plan
/// file reads it again, and refuses the plan when it changed.
#[test]
fn reads_a_file_and_runs_a_command() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-read-run", port);
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    assert!(r.stdout.contains("10.7.0.0/16"), "{}", r.stdout);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("KUBE-SECRET"), "{out}");
    }
    // A secret from the first answer, where no sensitive attribute
    // labels it: by its call.
    s.write(
        "q.df",
        &format!(
            "\nuse ssh\nkube(r) where r = ssh.read(\"127.0.0.1:{port}\", \"{}\", \"{}\")\n",
            user(),
            s.path("remote/k3s.yaml").display()
        ),
    );
    for args in [
        &["query", "kube", "q.df"][..],
        &["query", "kube", "--json", "q.df"],
    ] {
        let r = run(&s, args).success();
        assert!(!r.stdout.contains("KUBE-SECRET"), "{args:?}: {}", r.stdout);
        assert!(
            r.stdout.contains("secret(23 B)") || r.stdout.contains("ssh.read[\\\"127.0.0.1:"),
            "{args:?}: {}",
            r.stdout
        );
    }
    let file = s.read("plan.json");
    assert!(!file.contains("KUBE-SECRET"), "{file}");
    let plan: serde_json::Value = serde_json::from_str(&file).unwrap();
    let answers = plan["inputs"]["answers"].as_array().unwrap();
    assert_eq!(answers.len(), 1, "{file}");
    assert!(
        answers[0]["sensitive"]
            .as_str()
            .unwrap()
            .starts_with("ssh.read/127.0.0.1:"),
        "{file}"
    );
    assert!(answers[0]["digest"].is_string(), "{file}");

    // The file moved since the plan: the plan is stale.
    s.write("remote/k3s.yaml", "token: KUBE-SECRET-TWO\n");
    let r = run(&s, &["apply", "plan.json"]).failure();
    assert!(r.stderr.contains("changed since the plan"), "{}", r.stderr);
    assert!(!r.stderr.contains("KUBE-SECRET"), "{}", r.stderr);

    run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    let r = run(&s, &["apply", "plan.json"]).success();
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("KUBE-SECRET"), "{out}");
    }
    let st = state(&s);
    let known = &st["known_hosts"][format!("127.0.0.1:{port}")];
    assert_eq!(known["key_type"], "ssh-ed25519", "{st}");
    assert_eq!(known["fingerprint"], fingerprint(&key(HOST_A)), "{st}");
    assert!(!st.to_string().contains("KUBE-SECRET"), "{st}");
}

/// A host that refuses the connection is "not yet": the apply waits on
/// it, and past its budget stops saying what it waited on. A file not
/// there yet is the same: once it appears, the wait resolves.
#[test]
fn a_host_or_a_file_not_there_yet_is_waited_on() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-not-yet", port);
    let r = run(&s, &["apply", "--wait", "1s", "p.df"]).failure();
    assert!(r.stderr.contains("waiting on "), "{}", r.stderr);
    assert!(r.stderr.contains("ssh.run[\"127.0.0.1:"), "{}", r.stderr);
    assert!(r.stderr.contains("still unknown"), "{}", r.stderr);

    // The host answers; the kubeconfig is not written yet.
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    std::fs::remove_file(s.path("remote/k3s.yaml")).unwrap();
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout.contains("\n  waits on  127.0.0.1:"),
        "{}",
        r.stdout
    );
    let apply = {
        let s_dir = s.dir.clone();
        let home = s.path("home");
        std::thread::spawn(move || {
            Run::from(
                dform()
                    .args(["apply", "--yes", "--wait", "30s", "p.df"])
                    .current_dir(&s_dir)
                    .env("HOME", home)
                    .env_remove("SSH_AUTH_SOCK")
                    .env("DFORM_WAIT_POLL_MS", "50")
                    .output()
                    .unwrap(),
            )
        })
    };
    std::thread::sleep(Duration::from_millis(1500));
    s.write("remote/k3s.yaml", "token: KUBE-SECRET-LATE\n");
    let r = apply.join().unwrap().success();
    assert!(r.stderr.contains("waiting on ssh.read["), "{}", r.stderr);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("KUBE-SECRET"), "{out}");
    }
    assert!(
        identities(&s).iter().any(|i| i.contains("kube")),
        "{:?}",
        identities(&s)
    );
}

/// The host key the first apply met is checked after: a host with
/// another key is an error naming both, until `state forget-host`.
#[test]
fn a_changed_host_key_is_an_error_until_forget_host() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-host-key", port);
    let host = format!("127.0.0.1:{port}");
    {
        let _a = Sshd::start(&bin, &s, port, HOST_A);
        run(&s, &["apply", "p.df"]).success();
    }
    let _b = Sshd::start(&bin, &s, port, HOST_B);
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains(&fingerprint(&key(HOST_A)))
            && r.stderr.contains(&fingerprint(&key(HOST_B))),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains(&format!("dform state forget-host {host}")),
        "{}",
        r.stderr
    );

    let r = run(&s, &["state", "forget-host", "10.9.9.9", "p"]).failure();
    assert!(
        r.stderr.contains("records no key for 10.9.9.9"),
        "{}",
        r.stderr
    );
    run(&s, &["state", "forget-host", &host, "p"]).success();
    assert!(state(&s)["known_hosts"].get(&host).is_none());
    run(&s, &["apply", "p.df"]).success();
    assert_eq!(
        state(&s)["known_hosts"][&host]["fingerprint"],
        fingerprint(&key(HOST_B))
    );
}

/// A key the host does not let in is an error, not "not yet".
#[test]
fn an_authentication_failure_is_an_error() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-auth", port);
    write_private(&s.path("home/.ssh/id_ed25519"), &key(CLIENT + 1));
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("authentication failed with ~/.ssh/id_ed25519"),
        "{}",
        r.stderr
    );
}

/// A command that fails is an error naming its status and its stderr.
#[test]
fn a_failing_command_is_an_error() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-run-fails", port);
    std::fs::remove_file(s.path("remote/cidr")).unwrap();
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(r.stderr.contains("exited with status 1"), "{}", r.stderr);
    assert!(r.stderr.contains("No such file"), "{}", r.stderr);
}
