//! The `ssh://` transport (R-153): `io.read("ssh://USER@HOST/PATH")` over
//! SFTP, and `git+ssh://` repositories through `git-upload-pack`, against a
//! real sshd each test starts on a free port with a throwaway host key,
//! and a client key in a scratch `HOME` (no agent). It only reads: a
//! program that calls `ssh.run` is told a command is a provider's apply
//! (R-151); `use ssh` and `ssh.read` are gone, each an error naming the
//! location. A host that does not answer yet, and a file that is not
//! there yet, are "not yet": the apply waits (R-81). A host key is
//! recorded by the first apply and checked after; a changed one is an
//! error until `dform state forget-host`.
//!
//! Authentication never prompts (R-125): the key dform.toml names for the
//! location (`[io] credentials`), the agent's keys, then the
//! unencrypted `~/.ssh/id_*`; a refusal says what was offered and what to
//! do. The agent is russh's, served by the test on a unix socket.
//!
//! The one shell-out allowed in tests: the `sshd` binary. With none
//! installed, each test says so and passes.

mod common;
use common::{Run, Scratch, dform, repo, yes};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey, PublicKey};
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
        Sshd::letting_in(
            bin,
            s,
            port,
            host_seed,
            &[key(CLIENT).public_key().clone()],
            "",
        )
    }

    /// An sshd that lets in the keys `authorized`, `extra` more of its
    /// configuration.
    fn letting_in(
        bin: &Path,
        s: &Scratch,
        port: u16,
        host_seed: u8,
        authorized: &[PublicKey],
        extra: &str,
    ) -> Sshd {
        let dir = s.path("sshd");
        std::fs::create_dir_all(&dir).unwrap();
        let host_key = dir.join(format!("host_key_{host_seed}"));
        write_private(&host_key, &key(host_seed));
        let lines: Vec<String> = authorized
            .iter()
            .map(|k| k.to_openssh().unwrap() + "\n")
            .collect();
        std::fs::write(dir.join("authorized_keys"), lines.concat()).unwrap();
        let config = dir.join("sshd_config");
        std::fs::write(
            &config,
            format!(
                "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nPidFile none\n\
                 AuthorizedKeysFile {}\nStrictModes no\nUsePAM no\n\
                 PasswordAuthentication no\nKbdInteractiveAuthentication no\n\
                 PubkeyAuthentication yes\nSubsystem sftp internal-sftp\nLogLevel ERROR\n{extra}",
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

/// A project whose secret's password (sensitive) is the file the host
/// holds, as a cluster's kubeconfig configures its provider: the client
/// key in its `home/.ssh`.
fn project(name: &str, port: u16) -> Scratch {
    let s = Scratch::project(name);
    std::fs::create_dir_all(s.path("home/.ssh")).unwrap();
    write_private(&s.path("home/.ssh/id_ed25519"), &key(CLIENT));
    std::fs::create_dir_all(s.path("remote")).unwrap();
    s.write(
        "p.df",
        &format!(
            "\nuse fake\n\n\
             let host = \"127.0.0.1:{port}\"\n\
             let raw: secret(string) = io.read(\"ssh://{user}@${{host}}{dir}/k3s.yaml\")\n\
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
    s.write("remote/k3s.yaml", "token: KUBE-SECRET-ONE\n");
    s
}

/// `dform ARGS` in `s`, its `home` as HOME and no agent.
fn run(s: &Scratch, args: &[&str]) -> Run {
    run_with(s, None, args)
}

/// `dform ARGS` in `s`, its `home` as HOME, `creds` its credentials, and
/// the agent at `agent`, if any.
fn run_with(s: &Scratch, agent: Option<&Path>, args: &[&str]) -> Run {
    let mut cmd = dform();
    cmd.args(yes(args))
        .current_dir(&s.dir)
        .env("HOME", s.path("home"))
        .env("XDG_CACHE_HOME", s.path("home/.cache"))
        .env("DFORM_CREDENTIALS", s.path("creds"))
        .env("DFORM_WAIT_POLL_MS", "50");
    match agent {
        Some(a) => cmd.env("SSH_AUTH_SOCK", a),
        None => cmd.env_remove("SSH_AUTH_SOCK"),
    };
    Run::from(cmd.output().unwrap())
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

/// What a host's file read into a secret holds never prints and the plan
/// file holds only its digest. An apply of the plan file reads it again,
/// and refuses the plan when it changed.
#[test]
fn reads_a_file() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-read", port);
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    answered(&r);
    for out in [&r.stdout, &r.stderr] {
        assert!(!out.contains("KUBE-SECRET"), "{out}");
    }
    // A secret from the first answer, where no sensitive attribute
    // labels it: by its call.
    s.write(
        "q.df",
        &format!(
            "\nuse fake\nlet raw: secret(string) = io.read(\"ssh://{}@127.0.0.1:{port}{}\")\n\
             kube(r) where r = raw\n",
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
            r.stdout.contains("secret(23 B)") || r.stdout.contains("sensitive"),
            "{args:?}: {}",
            r.stdout
        );
    }
    let file = s.read("plan.json");
    assert!(!file.contains("KUBE-SECRET"), "{file}");
    let plan: serde_json::Value = serde_json::from_str(&file).unwrap();
    let answers = plan["deployments"][0]["inputs"]["answers"]
        .as_array()
        .unwrap();
    assert_eq!(answers.len(), 1, "{file}");
    assert!(
        answers[0]["sensitive"]
            .as_str()
            .unwrap()
            .starts_with(&format!(
                "table.text.@document/ssh://{}@127.0.0.1:{port}/",
                user()
            )),
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
/// it, and past the `ssh` provider's `timeout` stops saying what it
/// waited on (R-122). A file not there yet is the same: once it appears,
/// the wait resolves.
#[test]
fn a_host_or_a_file_not_there_yet_is_waited_on() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-not-yet", port);
    let timeout = |t: &str| {
        s.write(
            "dform.toml",
            &format!("[project]\nedition = \"2026\"\n\n[io]\nwait = \"{t}\"\n"),
        )
    };
    timeout("1s");
    let r = run(&s, &["apply", "p.df"]).failure();
    assert!(r.stderr.contains("waiting on "), "{}", r.stderr);
    // The location as the program reads it (R-153).
    assert!(r.stderr.contains("ssh://"), "{}", r.stderr);
    assert!(
        r.stderr
            .contains(" not reached in 1s (`[io] wait` in dform.toml)"),
        "{}",
        r.stderr
    );

    // The host answers; the kubeconfig is not written yet.
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    std::fs::remove_file(s.path("remote/k3s.yaml")).unwrap();
    let r = run(&s, &["plan", "p.df"]).success();
    assert!(
        r.stdout
            .contains(&format!("\n  waits on  ssh://{}@127.0.0.1:{port}/", user())),
        "{}",
        r.stdout
    );
    timeout("30s");
    // The file is written once the apply says it waits on it.
    let mut child = dform()
        .args(["apply", "--yes", "p.df"])
        .current_dir(&s.dir)
        .env("HOME", s.path("home"))
        .env_remove("SSH_AUTH_SOCK")
        .env("DFORM_WAIT_POLL_MS", "50")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut err = std::io::BufReader::new(child.stderr.take().unwrap());
    let mut said = String::new();
    while !said.contains("waiting on ssh://") {
        let mut line = String::new();
        let n = std::io::BufRead::read_line(&mut err, &mut line).unwrap();
        assert!(n > 0, "the apply ended before it waited:\n{said}");
        said.push_str(&line);
    }
    s.write("remote/k3s.yaml", "token: KUBE-SECRET-LATE\n");
    std::io::Read::read_to_string(&mut err, &mut said).unwrap();
    let out = child.wait_with_output().unwrap();
    let r = Run::from(std::process::Output {
        status: out.status,
        stdout: out.stdout,
        stderr: said.into_bytes(),
    })
    .success();
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

    let r = run(&s, &["state", "forget-host", "10.9.9.9", "p.df"]).failure();
    assert!(
        r.stderr.contains("records no key for 10.9.9.9"),
        "{}",
        r.stderr
    );
    run(&s, &["state", "forget-host", &host, "p.df"]).success();
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
    refused(
        &r,
        &format!(
            "127.0.0.1:{port} refused {u}: no agent at SSH_AUTH_SOCK, and the host did not \
             accept ~/.ssh/id_ed25519",
            u = user()
        ),
        &format!(
            "add one of these keys to {u}'s authorized_keys on 127.0.0.1:{port}, or `ssh-add` \
             the key it holds",
            u = user()
        ),
    );
}

/// The plan `r` printed read the file: the secret is planned, waiting on
/// nothing.
fn answered(r: &Run) {
    assert!(r.stdout.contains("+ db.secret kube"), "{}", r.stdout);
    assert!(!r.stdout.contains("waits on"), "{}", r.stdout);
}

/// A read runs no command (R-151). A program that calls `ssh.run` is
/// told where a command belongs, at the call, with no host contacted; the
/// ssh provider (`use ssh`, `ssh.read`) and its `[providers] ssh` table
/// are gone, each an error naming the location (R-153).
#[test]
fn ssh_run_is_not_a_function_and_the_provider_is_gone() {
    let s = project("ssh-run", free_port());
    let p = s.read("p.df").replace(
        "let raw: ",
        "let out = ssh.run(host, \"root\", \"apt-get install -y k3s\")\nlet raw: ",
    );
    s.write("p.df", &p);
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr
            .contains("p.df:5:11: `ssh.run` is not a function: a command is a provider's apply"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("io.read(\"ssh://USER@HOST/PATH\")"),
        "{}",
        r.stderr
    );

    s.write(
        "p.df",
        "\nuse ssh\nlet raw = ssh.read(\"h\", \"u\", \"/etc/k3s.yaml\")\n",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("the ssh provider is gone (R-153)"),
        "{}",
        r.stderr
    );
    s.write(
        "p.df",
        "\nlet raw = ssh.read(\"h\", \"u\", \"/etc/k3s.yaml\")\n",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(
        r.stderr.contains("`ssh.read` is gone (R-153)"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("io.read(\"ssh://USER@HOST/PATH\")"),
        "{}",
        r.stderr
    );
    s.write(
        "dform.toml",
        "[project]\nedition = \"2026\"\n\n[providers]\nssh = { timeout = \"10m\" }\n",
    );
    let r = run(&s, &["plan", "p.df"]).failure();
    assert!(r.stderr.contains("[io] wait"), "{}", r.stderr);
}

/// An OpenSSH ed25519 key with the passphrase `blabla` (russh's own test
/// key): what an operator's `~/.ssh/id_ed25519` usually is.
const LOCKED: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAACmFlczI1Ni1jYmMAAAAGYmNyeXB0AAAAGAAAABDLGyfA39
J2FcJygtYqi5ISAAAAEAAAAAEAAAAzAAAAC3NzaC1lZDI1NTE5AAAAIN+Wjn4+4Fcvl2Jl
KpggT+wCRxpSvtqqpVrQrKN1/A22AAAAkOHDLnYZvYS6H9Q3S3Nk4ri3R2jAZlQlBbUos5
FkHpYgNw65KCWCTXtP7ye2czMC3zjn2r98pJLobsLYQgRiHIv/CUdAdsqbvMPECB+wl/UQ
e+JpiSq66Z6GIt0801skPh20jxOO3F52SoX1IeO5D5PXfZrfSZlw6S8c7bwyp2FHxDewRx
7/wNsnDM0T7nLv/Q==
-----END OPENSSH PRIVATE KEY-----
";

/// The key [`LOCKED`] holds, as `ssh-add` gives it to the agent.
fn unlocked() -> PrivateKey {
    russh::keys::decode_secret_key(LOCKED, Some("blabla")).unwrap()
}

/// `r` failed with the refusal `line` and, on the next line, `todo`.
fn refused(r: &Run, line: &str, todo: &str) {
    let mut lines = r.stderr.lines().map(str::trim);
    assert!(
        lines.any(|l| l.ends_with(line)) && lines.next() == Some(todo),
        "{line}\n{todo}\n---\n{}",
        r.stderr
    );
}

/// An agent (russh's) on a unix socket in `s`, holding `keys`; it serves
/// until the test process ends.
fn agent(s: &Scratch, keys: &[PrivateKey]) -> PathBuf {
    let sock = s.path("agent.sock");
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let listener = {
        let _g = rt.enter();
        tokio::net::UnixListener::bind(&sock).unwrap()
    };
    std::thread::spawn(move || {
        rt.block_on(russh::keys::agent::server::serve(
            tokio_stream::wrappers::UnixListenerStream::new(listener),
            (),
        ))
    });
    let keys = keys.to_vec();
    let path = sock.clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async move {
            let mut c = russh::keys::agent::client::AgentClient::connect_uds(&path)
                .await
                .unwrap();
            for k in &keys {
                c.add_identity(k, &[]).await.unwrap();
            }
        });
    sock
}

/// The operator's key has a passphrase and is in their agent: the agent
/// signs, the file is never decrypted. With no agent, the refusal says
/// so and what to do, in two lines.
#[test]
fn a_key_with_a_passphrase_is_used_through_the_agent() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-agent", port);
    s.write("home/.ssh/id_ed25519", LOCKED);
    let _sshd = Sshd::letting_in(
        &bin,
        &s,
        port,
        HOST_A,
        &[unlocked().public_key().clone()],
        "",
    );

    let r = run(&s, &["plan", "p.df"]).failure();
    let host = format!("127.0.0.1:{port}");
    refused(
        &r,
        &format!(
            "{host} refused {u}: no agent at SSH_AUTH_SOCK, and ~/.ssh/id_ed25519 has a passphrase",
            u = user()
        ),
        "start an agent and `ssh-add`, or name an unencrypted deploy key in dform.toml: [io] \
         credentials = { \"ssh://HOST/*\" = \"ssh:k3s-admin\" }",
    );

    let sock = agent(&s, &[key(30), unlocked()]);
    let r = run_with(&s, Some(&sock), &["plan", "p.df"]).success();
    answered(&r);
}

/// An agent whose keys the host does not hold: the refusal names each it
/// offered, and the key file not read.
#[test]
fn an_agent_whose_keys_the_host_refuses_says_which() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-agent-refused", port);
    s.write("home/.ssh/id_ed25519", LOCKED);
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let sock = agent(&s, &[key(30), key(31)]);
    let r = run_with(&s, Some(&sock), &["plan", "p.df"]).failure();
    // The agent's order is its own; a key with no comment is shown by its
    // fingerprint.
    let (a, b) = (fingerprint(&key(30)), fingerprint(&key(31)));
    let offered = match r.stderr.find(&a) < r.stderr.find(&b) {
        true => format!("{a}, {b}"),
        false => format!("{b}, {a}"),
    };
    refused(
        &r,
        &format!(
            "127.0.0.1:{port} refused {u}: the agent offered 2 keys ({offered}) and the host \
             accepted none, and ~/.ssh/id_ed25519 has a passphrase",
            u = user(),
        ),
        &format!(
            "`ssh-add ~/.ssh/id_ed25519`, or add one of these keys to {u}'s authorized_keys on \
             127.0.0.1:{port}",
            u = user(),
        ),
    );
}

/// `[io] credentials = { "ssh://HOST/*" = "ssh:NAME" }`: the
/// credential `ssh:NAME`'s file, or the agent's key of that fingerprint
/// (or comment), offered first (a host stops listening after
/// `MaxAuthTries` refusals); neither is a refusal saying where it looked.
#[test]
fn a_named_key_is_the_agents_or_the_credentials() {
    let Some(bin) = sshd_binary() else { return };
    let port = free_port();
    let s = project("ssh-named", port);
    std::fs::remove_file(s.path("home/.ssh/id_ed25519")).unwrap();
    let unnamed = s.read("dform.toml");
    let named = |k: &str| {
        format!("{unnamed}\n[io]\ncredentials = {{ \"ssh://127.0.0.1:*\" = \"ssh:{k}\" }}\n")
    };
    s.write("dform.toml", &named("k3s-admin"));
    let _sshd = Sshd::letting_in(
        &bin,
        &s,
        port,
        HOST_A,
        &[key(CLIENT).public_key().clone()],
        "MaxAuthTries 1\n",
    );

    // Neither in the agent nor on disk.
    let sock = agent(&s, &[key(30)]);
    let r = run_with(&s, Some(&sock), &["plan", "p.df"]).failure();
    let creds = s.path("creds/ssh/k3s-admin");
    refused(
        &r,
        &format!(
            "127.0.0.1:{port} refused {u}: the agent offered 1 key ({fp}) and the host accepted \
             none, and the key \"k3s-admin\" is not in the agent and there is no {creds}",
            u = user(),
            fp = fingerprint(&key(30)),
            creds = creds.display(),
        ),
        &format!(
            "`ssh-add` it (its comment or SHA256 fingerprint names it), or put the unencrypted \
             key at {}",
            creds.display()
        ),
    );

    // The credential's file.
    std::fs::create_dir_all(s.path("creds/ssh")).unwrap();
    write_private(&creds, &key(CLIENT));
    run_with(&s, Some(&sock), &["plan", "p.df"]).success();
    std::fs::remove_file(&creds).unwrap();

    // The agent's, among fifteen it holds that the host refuses, and the
    // host hangs up at the first refused.
    s.write("dform.toml", &named(&fingerprint(&key(CLIENT))));
    std::fs::remove_file(&sock).unwrap();
    let mut keys: Vec<PrivateKey> = (40..55).map(key).collect();
    keys.push(key(CLIENT));
    let sock = agent(&s, &keys);
    run_with(&s, Some(&sock), &["plan", "p.df"]).success();

    // None named: the host hangs up before `~/.ssh/id_ed25519`, and the
    // refusal says so.
    s.write("dform.toml", &unnamed);
    write_private(&s.path("home/.ssh/id_ed25519"), &key(CLIENT));
    std::fs::remove_file(&sock).unwrap();
    let sock = agent(&s, &keys[..2]);
    let r = run_with(&s, Some(&sock), &["plan", "p.df"]).failure();
    let line = r
        .stderr
        .lines()
        .find(|l| l.contains("refused"))
        .unwrap_or("");
    assert!(
        line.ends_with("and the host hung up after those (its MaxAuthTries)"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "name the key the host holds in dform.toml, and it is offered first: [io] \
             credentials = { \"ssh://HOST/*\" = \"ssh:k3s-admin\" }"
        ),
        "{}",
        r.stderr
    );
}

/// A repository over ssh (`git+ssh://`, R-153): the host runs
/// `git-upload-pack` (the test's sshd, the host's git), dform fetches its
/// branches and tags into the mirror over the SSH client in process, and
/// reads the file at the tag; the rows name the commit.
#[test]
fn a_repository_is_read_over_ssh() {
    let Some(bin) = sshd_binary() else { return };
    let git = |dir: &Path, args: &[&str]| common::try_git(dir, args);
    let port = free_port();
    let s = project("ssh-git", port);
    std::fs::create_dir_all(s.path("served")).unwrap();
    if git(&s.path("served"), &["init", "-q", "--bare", "ops.git"]).is_none() {
        eprintln!("skipped: no git to make the fixture with");
        return;
    }
    git(&s.path("served"), &["clone", "-q", "ops.git", "work"]).unwrap();
    let w = s.path("served/work");
    std::fs::create_dir_all(w.join("docs")).unwrap();
    std::fs::write(w.join("docs/vpcs.yml"), "name: a\n---\nname: b\n").unwrap();
    git(&w, &["add", "."]).unwrap();
    git(&w, &["commit", "-q", "-m", "vpcs"]).unwrap();
    git(&w, &["tag", "v1.0.0"]).unwrap();
    git(
        &w,
        &["push", "-q", "origin", "HEAD:refs/heads/main", "v1.0.0"],
    )
    .unwrap();
    let commit = git(&w, &["rev-parse", "HEAD"]).unwrap();
    s.write(
        "p.df",
        &format!(
            "\nuse fake\n\
             resource net.vpc \"${{d.name}}\" {{ cidr_block = \"10.0.0.0/16\" }} \
             where d in yaml.decode(io.read(\"git+ssh://{u}@127.0.0.1:{port}{repo}/docs/vpcs.yml?ref=v1.0.0\"))\n",
            u = user(),
            repo = s.path("served/ops.git").display(),
        ),
    );
    let _sshd = Sshd::start(&bin, &s, port, HOST_A);
    let r = run(&s, &["plan", "--out", "plan.json", "p.df"]).success();
    assert!(
        r.stdout.contains("+ net.vpc a") && r.stdout.contains("+ net.vpc b"),
        "{}",
        r.stdout
    );
    // The plan file holds the commit read; the mirror is the cache's.
    assert!(
        s.read("plan.json").contains(&commit),
        "{}",
        s.read("plan.json")
    );
    let mirror = s.path("home/.cache/dform/git");
    assert!(
        std::fs::read_dir(&mirror).map(|d| d.count()).unwrap_or(0) == 1,
        "{}",
        mirror.display()
    );
}
