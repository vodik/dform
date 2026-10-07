//! Starting a provider's executable and reading its handshake line. Which
//! sources are executables is `dform_core::plugin::source`; the mock is
//! `dform __provider fake` (`client::Process`).

use anyhow::{Context, Result, anyhow, bail};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::time::Duration;

/// The first stdout line of a provider: `dform-provider|VERSION|ADDRESS`.
pub const MAGIC: &str = dform_core::plugin::source::MAGIC;
pub use dform_core::plugin::backend::VERSION;
/// The mock provider's own executable, for the conformance suite and for
/// use outside dform (`crates/dform-provider-fake`).
pub const FAKE: &str = "dform-provider-fake";
/// Names a mock provider executable to run instead of `dform __provider
/// fake`.
pub const FAKE_ENV: &str = "DFORM_PROVIDER_FAKE";

/// How long a provider may take to print its handshake line, and to
/// answer its `Manifest` (`Conn::manifest`).
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// A provider to start: an executable and its arguments.
#[derive(Debug, Clone)]
pub struct Program {
    pub exe: PathBuf,
    pub args: Vec<OsString>,
}

impl Program {
    /// The executable `exe`, with no arguments.
    pub fn exe(exe: impl Into<PathBuf>) -> Program {
        Program {
            exe: exe.into(),
            args: Vec::new(),
        }
    }

    /// The running executable, run as `EXE __provider NAME`: a provider
    /// always of the same build as the dform that starts it.
    pub fn this(name: &str) -> Result<Program> {
        let exe = std::env::current_exe().context("locate the running executable")?;
        Ok(Program {
            exe,
            args: vec!["__provider".into(), name.into()],
        })
    }

    /// The program, for messages: `dform __provider fake`.
    pub fn display(&self) -> String {
        let mut s = self.exe.display().to_string();
        for a in &self.args {
            s.push(' ');
            s.push_str(&a.to_string_lossy());
        }
        s
    }
}

/// Environment the provider starts with beyond dform's own: variables to
/// set and ones to remove. Tests start a provider configured this way
/// rather than writing a wrapper script to exec (a script written just
/// before its exec races every other thread's fork: a child holding the
/// write handle until its own exec makes the exec fail with ETXTBSY).
#[derive(Debug, Clone, Default)]
pub struct Env {
    set: Vec<(String, String)>,
    unset: Vec<String>,
}

impl Env {
    /// Set `key` to `value` in the provider's environment.
    pub fn set(mut self, key: &str, value: &str) -> Env {
        self.set.push((key.to_string(), value.to_string()));
        self
    }

    /// Remove `key` from the provider's environment.
    pub fn unset(mut self, key: &str) -> Env {
        self.unset.push(key.to_string());
        self
    }

    fn apply(&self, cmd: &mut Command) {
        for k in &self.unset {
            cmd.env_remove(k);
        }
        for (k, v) in &self.set {
            cmd.env(k, v);
        }
    }
}

/// A provider's process, owned: dropping it kills and reaps the process,
/// so no path between its spawn and its end (a failed handshake read, a
/// failed dial, a connection dropped) leaves it running or a zombie.
pub struct ChildGuard(Child);

impl ChildGuard {
    /// Its exit status, if it has exited (reaped: std keeps it, so a
    /// `kill` after it signals no other process that took the pid).
    pub fn try_wait(&mut self) -> Option<ExitStatus> {
        self.0.try_wait().ok().flatten()
    }

    /// Kill the process (unless it has exited) and reap it.
    pub fn kill(&mut self) -> Option<ExitStatus> {
        if let Some(s) = self.try_wait() {
            return Some(s);
        }
        let _ = self.0.kill();
        self.0.wait().ok()
    }

    pub fn id(&self) -> u32 {
        self.0.id()
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.kill();
    }
}

/// A started provider: the process, its stdin (closing it asks it to
/// exit), and the address its handshake named.
pub struct Started {
    pub child: ChildGuard,
    pub stdin: Option<ChildStdin>,
    pub address: String,
}

/// Start `program` with `env` and read its handshake line. Its stderr is
/// dform's; what it prints on stdout after the handshake goes to dform's
/// stderr.
pub fn start(program: &Program, env: &Env) -> Result<Started> {
    let exe = program.display();
    let mut cmd = Command::new(&program.exe);
    cmd.args(&program.args);
    env.apply(&mut cmd);
    let mut child = ChildGuard(
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("start provider {exe}"))?,
    );
    let stdout = child.0.stdout.take().expect("piped stdout");
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut lines = BufReader::new(stdout);
        let mut first = String::new();
        let r = lines.read_line(&mut first).map(|_| first);
        let _ = tx.send(r);
        // The rest of its stdout, so it never blocks on a full pipe.
        for line in lines.lines().map_while(|l| l.ok()) {
            let _ = writeln!(std::io::stderr(), "{line}");
        }
    });
    let line = match rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(r) => r.with_context(|| format!("read the handshake of provider {exe}"))?,
        Err(_) => {
            bail!(
                "provider {} printed no handshake line in {}s",
                exe,
                HANDSHAKE_TIMEOUT.as_secs()
            );
        }
    };
    let address = match parse_handshake(&line) {
        Ok(a) => a,
        Err(e) => {
            let status = child.kill();
            return Err(match (line.is_empty(), status) {
                (true, Some(s)) => anyhow!("provider {} exited before its handshake ({s})", exe),
                _ => e.context(format!("provider {exe}")),
            });
        }
    };
    let stdin = child.0.stdin.take();
    Ok(Started {
        child,
        stdin,
        address,
    })
}

/// `dform-provider|1|tcp://127.0.0.1:PORT` (or a bare `PORT`, or
/// `HOST:PORT`), or `dform-provider|1|unix:///PATH`: the address to dial,
/// as a URI (`transport::dial`).
pub fn parse_handshake(line: &str) -> Result<String> {
    let line = line.trim_end();
    let mut parts = line.splitn(3, '|');
    let (Some(MAGIC), Some(version), Some(addr)) = (parts.next(), parts.next(), parts.next())
    else {
        bail!("expected a handshake line `{MAGIC}|{VERSION}|ADDRESS`, got {line:?}");
    };
    if version != VERSION.to_string() {
        bail!("it speaks protocol version {version}; this dform speaks {VERSION}");
    }
    if let Some(path) = addr.strip_prefix("unix://") {
        if !path.starts_with('/') {
            bail!("its unix socket must be an absolute path: {addr:?}");
        }
        return Ok(addr.to_string());
    }
    let hostport = addr.strip_prefix("tcp://").unwrap_or(addr);
    if hostport.parse::<u16>().is_ok() {
        return Ok(format!("http://127.0.0.1:{hostport}"));
    }
    match hostport.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() && port.parse::<u16>().is_ok() => {
            Ok(format!("http://{hostport}"))
        }
        _ => bail!("its handshake names no address dform can dial: {addr:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_lines() {
        assert_eq!(
            parse_handshake("dform-provider|1|tcp://127.0.0.1:4000\n").unwrap(),
            "http://127.0.0.1:4000"
        );
        assert_eq!(
            parse_handshake("dform-provider|1|4001").unwrap(),
            "http://127.0.0.1:4001"
        );
        let e = parse_handshake("dform-provider|2|4001").unwrap_err();
        assert!(e.to_string().contains("version 2"), "{e}");
        assert!(parse_handshake("hello").is_err());
        assert_eq!(
            parse_handshake("dform-provider|1|unix:///tmp/s").unwrap(),
            "unix:///tmp/s"
        );
        assert!(parse_handshake("dform-provider|1|unix://s").is_err());
    }
}
