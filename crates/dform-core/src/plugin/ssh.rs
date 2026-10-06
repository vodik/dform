//! The built-in `ssh` fact provider (`provider ssh`): two externs dform
//! answers itself, over an SSH client in process (russh; never the `ssh`
//! binary, the operator's ssh config or PATH).
//!
//! ```text
//! ssh.read(+host, +user, +path, -content: secret(string))   SFTP
//! ssh.run(+host, +user, +command, -stdout: string)          exec
//! ```
//!
//! `host` is an `ip` or a string, `NAME` or `NAME:PORT` (22 when none).
//! The key is the operator's: the agent's (`SSH_AUTH_SOCK`) first, then
//! `~/.ssh/id_ed25519` and `~/.ssh/id_rsa`; never one in the program.
//!
//! A host's key is recorded on first contact (`State::known_hosts`, kept
//! by the apply, [`Ssh::keep`]) and checked on every contact after: a
//! changed key is an error naming both fingerprints, until `dform state
//! forget-host HOST`.
//!
//! A host that does not answer yet (the connection refused, timed out
//! after [`CONNECT_TIMEOUT`], no route) and a `read` of a path that does
//! not exist yet (cloud-init still running) are "not yet": the answer's
//! output column is an open null, which an apply waits on (R-81,
//! `Externs::not_yet`). An authentication failure, a changed host key, a
//! command that fails are errors. Every answer is read again each run, as
//! any extern's is; `memo.first` keeps one where wanted.

use crate::ast::ExternFn;
use crate::state::{KnownHost, State};
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, anyhow, bail};
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh::{ChannelMsg, Disconnect};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// `ssh.read(+host, +user, +path, -content: secret(string))`.
pub const READ: &str = "ssh.read";
/// `ssh.run(+host, +user, +command, -stdout: string)`.
pub const RUN: &str = "ssh.run";

/// How long one attempt waits for the host to answer and agree a session;
/// past it the host is "not yet" (an apply asks again within its wait).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read or a command may take once connected; past it the call
/// is an error.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// The `ssh` provider of one run: the host keys known (state's, and those
/// met this run).
#[derive(Debug, Default)]
pub struct Ssh {
    known: RefCell<BTreeMap<String, KnownHost>>,
}

/// What one call came to.
enum Outcome {
    Answered(String),
    /// The host, or the file, is not there yet.
    NotYet,
}

impl Ssh {
    /// The provider, `known` the host keys state keeps.
    pub fn new(known: BTreeMap<String, KnownHost>) -> Ssh {
        Ssh {
            known: RefCell::new(known),
        }
    }

    /// The answer to `ssh.read` or `ssh.run`; `None` for another extern.
    pub fn answer(&self, f: &ExternFn, inputs: &[Value]) -> Option<Result<Vec<Vec<Value>>>> {
        if f.name != READ && f.name != RUN {
            return None;
        }
        let want = || {
            anyhow!(
                "{} takes the host (an ip or a string), the user and the {}",
                f.name,
                if f.name == READ { "path" } else { "command" }
            )
        };
        let [host, Value::Str(user), Value::Str(arg)] = inputs else {
            return Some(Err(want()));
        };
        let host = match host {
            Value::Ip(n) => crate::value::u32_to_ipv4(*n),
            Value::Str(s) => s.clone(),
            _ => return Some(Err(want())),
        };
        let op = match f.name.as_str() {
            READ => Op::Read(arg.clone()),
            _ => Op::Run(arg.clone()),
        };
        Some(self.call(&host, user, op).map(|o| {
            let out = match o {
                Outcome::Answered(s) => Value::Str(s),
                Outcome::NotYet => Value::Null {
                    label: crate::externs::secret_label(&f.name, inputs, f.args.len() - 1),
                    class: NullClass::Open,
                    ty: String::new(),
                },
            };
            vec![crate::externs::row(f, inputs, vec![out])]
        }))
    }

    /// Keep in `st` each host key met this run that it does not know yet.
    pub fn keep(&self, st: &mut State) {
        for (h, k) in self.known.borrow().iter() {
            st.known_hosts.entry(h.clone()).or_insert_with(|| k.clone());
        }
    }

    fn call(&self, host: &str, user: &str, op: Op) -> Result<Outcome> {
        let (name, port) = address(host)?;
        let expect = self.known.borrow().get(host).cloned();
        let seen = Arc::new(Mutex::new(None));
        let handler = Client {
            expect,
            seen: seen.clone(),
        };
        let what = match &op {
            Op::Read(p) => format!("{READ} {user}@{host}:{p}"),
            Op::Run(c) => format!("{RUN} {user}@{host} `{c}`"),
        };
        // On a thread of its own, with a runtime of its own: the caller
        // may be inside another (a controller's).
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("ssh: start a runtime")?
                    .block_on(session(&name, port, user, host, handler, &op))
            })
            .join()
            .unwrap_or_else(|_| Err(anyhow!("ssh: the client panicked")))
        })
        .with_context(|| what.clone())?;
        // A host met for the first time: its key, from now on.
        if let Some((key_type, fingerprint)) = seen.lock().ok().and_then(|s| s.clone()) {
            self.known
                .borrow_mut()
                .entry(host.to_string())
                .or_insert_with(|| KnownHost {
                    key_type,
                    fingerprint,
                    when: crate::memo::now(),
                });
        }
        Ok(out)
    }
}

enum Op {
    Read(String),
    Run(String),
}

/// `NAME`, `NAME:PORT`, `[V6]:PORT`: the name and the port, 22 by default.
fn address(host: &str) -> Result<(String, u16)> {
    let port = |p: &str| {
        p.parse::<u16>()
            .map_err(|_| anyhow!("ssh: host {host:?}: {p:?} is not a port"))
    };
    if let Some(rest) = host.strip_prefix('[') {
        let (name, after) = rest
            .split_once(']')
            .ok_or_else(|| anyhow!("ssh: host {host:?}: no closing `]`"))?;
        return Ok(match after.strip_prefix(':') {
            Some(p) => (name.to_string(), port(p)?),
            None => (name.to_string(), 22),
        });
    }
    match host.split_once(':') {
        Some((name, p)) if !p.contains(':') => Ok((name.to_string(), port(p)?)),
        _ => Ok((host.to_string(), 22)),
    }
}

/// Connect, check the host key, authenticate and do `op`.
async fn session(
    name: &str,
    port: u16,
    user: &str,
    host: &str,
    handler: Client,
    op: &Op,
) -> Result<Outcome> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(CALL_TIMEOUT),
        ..Default::default()
    });
    let connect = client::connect(config, (name, port), handler);
    let mut h = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Err(_) => return Ok(Outcome::NotYet),
        Ok(Err(Failure::HostKey { was, now })) => bail!(
            "the host key of {host} changed: state recorded {} {} on {}, the host now offers \
             {} {}; if the host was rebuilt, `dform state forget-host {host}` and run again",
            was.key_type,
            was.fingerprint,
            was.when,
            now.0,
            now.1
        ),
        Ok(Err(Failure::Ssh(e))) => match not_yet(&e) {
            true => return Ok(Outcome::NotYet),
            false => return Err(anyhow!(e).context(format!("connect to {name}:{port}"))),
        },
        Ok(Ok(h)) => h,
    };
    authenticate(&mut h, user, host).await?;
    let out = tokio::time::timeout(CALL_TIMEOUT, run(&h, op))
        .await
        .map_err(|_| anyhow!("did not finish within {}s", CALL_TIMEOUT.as_secs()))??;
    let _ = h.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(out)
}

/// Whether a failure to connect is "not yet": the host is not up, or not
/// listening, yet; else an error.
fn not_yet(e: &russh::Error) -> bool {
    use std::io::ErrorKind as K;
    match e {
        russh::Error::IO(io) => matches!(
            io.kind(),
            K::ConnectionRefused
                | K::TimedOut
                | K::HostUnreachable
                | K::NetworkUnreachable
                | K::ConnectionReset
                | K::ConnectionAborted
                | K::NotConnected
                | K::UnexpectedEof
        ),
        // Closed during the handshake: an sshd still starting.
        russh::Error::Disconnect | russh::Error::HUP | russh::Error::ConnectionTimeout => true,
        _ => false,
    }
}

/// The agent's keys first, then `~/.ssh/id_ed25519` and `~/.ssh/id_rsa`.
async fn authenticate(h: &mut Handle<Client>, user: &str, host: &str) -> Result<()> {
    let mut tried: Vec<String> = Vec::new();
    if let Ok(mut agent) = AgentClient::connect_env().await {
        let ids = agent.request_identities().await.unwrap_or_default();
        for id in ids {
            let AgentIdentity::PublicKey { key, comment } = id else {
                continue;
            };
            let hash = match key.algorithm().is_rsa() {
                true => h.best_supported_rsa_hash().await.ok().flatten().flatten(),
                false => None,
            };
            tried.push(format!("the agent's {}", show_key(&comment, &key)));
            if let Ok(r) = h
                .authenticate_publickey_with(user, key, hash, &mut agent)
                .await
                && r.success()
            {
                return Ok(());
            }
        }
    }
    let home = std::env::home_dir().unwrap_or_default();
    for file in ["id_ed25519", "id_rsa"] {
        let path = home.join(".ssh").join(file);
        if !path.exists() {
            continue;
        }
        let key = match russh::keys::load_secret_key(&path, None) {
            Ok(k) => k,
            Err(e) => {
                tried.push(format!(
                    "~/.ssh/{file} (not read: {e}; a key with a passphrase is used through the agent)"
                ));
                continue;
            }
        };
        let hash = match key.algorithm().is_rsa() {
            true => h.best_supported_rsa_hash().await.ok().flatten().flatten(),
            false => None,
        };
        tried.push(format!("~/.ssh/{file}"));
        let r = h
            .authenticate_publickey(user, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
            .await
            .with_context(|| format!("authenticate as {user} with ~/.ssh/{file}"))?;
        if r.success() {
            return Ok(());
        }
    }
    match tried.is_empty() {
        true => bail!(
            "no key to authenticate as {user} at {host}: no agent (SSH_AUTH_SOCK) and no \
             ~/.ssh/id_ed25519 or ~/.ssh/id_rsa"
        ),
        false => bail!(
            "{host} refused {user}: authentication failed with {}",
            tried.join(", ")
        ),
    }
}

fn show_key(comment: &str, key: &russh::keys::PublicKey) -> String {
    match comment.is_empty() {
        true => key.fingerprint(HashAlg::Sha256).to_string(),
        false => comment.to_string(),
    }
}

async fn run(h: &Handle<Client>, op: &Op) -> Result<Outcome> {
    match op {
        Op::Read(path) => {
            let ch = h.channel_open_session().await?;
            ch.request_subsystem(true, "sftp").await?;
            let sftp = SftpSession::new(ch.into_stream())
                .await
                .context("start SFTP")?;
            let bytes = match sftp.read(path.as_str()).await {
                Ok(b) => b,
                Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
                    return Ok(Outcome::NotYet);
                }
                Err(e) => bail!("read {path}: {e}"),
            };
            let _ = sftp.close().await;
            String::from_utf8(bytes)
                .map(Outcome::Answered)
                .map_err(|_| anyhow!("read {path}: not UTF-8 text"))
        }
        Op::Run(command) => {
            let mut ch = h.channel_open_session().await?;
            ch.exec(true, command.as_str()).await?;
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut status = None;
            let mut signal = None;
            while let Some(m) = ch.wait().await {
                match m {
                    ChannelMsg::Data { data } => out.extend_from_slice(&data),
                    ChannelMsg::ExtendedData { data, ext: 1 } => err.extend_from_slice(&data),
                    ChannelMsg::ExitStatus { exit_status } => status = Some(exit_status),
                    ChannelMsg::ExitSignal { signal_name, .. } => signal = Some(signal_name),
                    _ => {}
                }
            }
            let stderr = String::from_utf8_lossy(&err);
            let stderr = stderr.trim();
            let failed = match (status, signal) {
                (Some(0), _) => None,
                (Some(n), _) => Some(format!("exited with status {n}")),
                (None, Some(s)) => Some(format!("was killed by signal {s:?}")),
                (None, None) => Some("ended with no exit status".to_string()),
            };
            if let Some(f) = failed {
                match stderr.is_empty() {
                    true => bail!("the command {f}, with nothing on stderr"),
                    false => bail!("the command {f}; stderr: {stderr}"),
                }
            }
            String::from_utf8(out)
                .map(Outcome::Answered)
                .map_err(|_| anyhow!("its stdout is not UTF-8 text"))
        }
    }
}

/// The client's side of the handshake: the host key checked against the
/// one recorded, and the one seen kept for the caller to record.
struct Client {
    expect: Option<KnownHost>,
    seen: Arc<Mutex<Option<(String, String)>>>,
}

#[derive(Debug)]
enum Failure {
    Ssh(russh::Error),
    /// The key recorded, and the one offered (type, fingerprint).
    HostKey {
        was: KnownHost,
        now: (String, String),
    },
}

impl From<russh::Error> for Failure {
    fn from(e: russh::Error) -> Self {
        Failure::Ssh(e)
    }
}

impl client::Handler for Client {
    type Error = Failure;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Failure> {
        let data = match key {
            PublicKeyOrCertificate::PublicKey { key, .. } => key.key_data(),
            PublicKeyOrCertificate::Certificate(c) => c.public_key(),
        };
        let now = (
            data.algorithm().as_str().to_string(),
            data.fingerprint(HashAlg::Sha256).to_string(),
        );
        if let Some(was) = &self.expect
            && was.fingerprint != now.1
        {
            return Err(Failure::HostKey {
                was: was.clone(),
                now,
            });
        }
        if let Ok(mut s) = self.seen.lock() {
            *s = Some(now);
        }
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_address_is_a_name_and_a_port() {
        assert_eq!(address("10.0.0.5").unwrap(), ("10.0.0.5".into(), 22));
        assert_eq!(
            address("h.example:2222").unwrap(),
            ("h.example".into(), 2222)
        );
        assert_eq!(address("[::1]:2200").unwrap(), ("::1".into(), 2200));
        assert_eq!(address("::1").unwrap(), ("::1".into(), 22));
        assert!(address("h:x").is_err());
    }

    #[test]
    fn a_refused_connection_is_not_yet_and_a_bad_key_is_an_error() {
        let refused = std::io::Error::from(std::io::ErrorKind::ConnectionRefused);
        assert!(not_yet(&russh::Error::IO(refused)));
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        assert!(!not_yet(&russh::Error::IO(denied)));
        assert!(!not_yet(&russh::Error::WrongServerSig));
    }
}
