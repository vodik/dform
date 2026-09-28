//! Starting a provider's executable and reading its handshake line. The
//! mock provider is `dform-provider-fake`, found beside the `dform`
//! executable (or named by `DFORM_PROVIDER_FAKE`); which sources are
//! executables is `dform_core::plugin::source`.

use anyhow::{Context, Result, anyhow, bail};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::Duration;

/// The first stdout line of a provider: `dform-provider|VERSION|ADDRESS`.
pub const MAGIC: &str = dform_core::plugin::source::MAGIC;
pub use dform_core::plugin::backend::VERSION;
/// The mock provider's executable.
pub const FAKE: &str = "dform-provider-fake";

/// How long a provider may take to print its handshake line.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(20);

/// The mock provider's executable: `DFORM_PROVIDER_FAKE`, else beside the
/// running executable (or one directory up, where a test binary's
/// `deps/` sits under the directory cargo builds binaries into).
pub fn fake_executable() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("DFORM_PROVIDER_FAKE") {
        return Ok(PathBuf::from(p));
    }
    let exe = std::env::current_exe().context("locate the running executable")?;
    let dir = exe.parent().unwrap_or(Path::new("."));
    for d in [Some(dir), dir.parent()].into_iter().flatten() {
        let p = d.join(FAKE);
        if p.is_file() {
            return Ok(p);
        }
    }
    bail!(
        "the mock provider {FAKE} is not beside {} (build it with `cargo build`, or name it \
         with DFORM_PROVIDER_FAKE)",
        exe.display()
    )
}

/// A started provider: the process, its stdin (closing it asks it to
/// exit), and the address its handshake named.
pub struct Started {
    pub child: Child,
    pub stdin: Option<ChildStdin>,
    pub address: String,
}

/// Start `exe` and read its handshake line. Its stderr is dform's; what it
/// prints on stdout after the handshake goes to dform's stderr.
pub fn start(exe: &Path) -> Result<Started> {
    let mut child = Command::new(exe)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .with_context(|| format!("start provider {}", exe.display()))?;
    let stdout = child.stdout.take().expect("piped stdout");
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
        Ok(r) => r.with_context(|| format!("read the handshake of provider {}", exe.display()))?,
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "provider {} printed no handshake line in {}s",
                exe.display(),
                HANDSHAKE_TIMEOUT.as_secs()
            );
        }
    };
    let address = match parse_handshake(&line) {
        Ok(a) => a,
        Err(e) => {
            let _ = child.kill();
            let status = child.wait().ok();
            return Err(match (line.is_empty(), status) {
                (true, Some(s)) => anyhow!(
                    "provider {} exited before its handshake ({s})",
                    exe.display()
                ),
                _ => e.context(format!("provider {}", exe.display())),
            });
        }
    };
    let stdin = child.stdin.take();
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
