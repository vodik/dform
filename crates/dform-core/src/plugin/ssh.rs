//! The built-in `ssh` fact provider (`use ssh`): two externs dform
//! answers itself, over an SSH client in process (russh; never the `ssh`
//! binary, the operator's ssh config or PATH).
//!
//! ```text
//! ssh.read(+host, +user, +path, -content: secret(string))   SFTP
//! ssh.run(+host, +user, +command, -stdout: string)          exec
//! ```
//!
//! `host` is an `ip` or a string, `NAME` or `NAME:PORT` (22 when none).
//! The key is the operator's, never one in the program, and never asked
//! for: every key the agent (`SSH_AUTH_SOCK`) holds, then the key the
//! program names (`use ssh { key = "k3s-admin" }`: an agent key by its
//! comment or fingerprint, offered first, or the credential
//! `ssh:k3s-admin`'s unencrypted file), else the unencrypted
//! `~/.ssh/id_ed25519`, `id_ecdsa`, `id_rsa`. A key file with a passphrase
//! is not decrypted: the agent uses such a key. A refusal says what was
//! offered and what was not, and what to do.
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

use crate::ast::{ExternFn, Program, Stmt, Term};
use crate::plugin::credentials;
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
    /// The key the program names (`use ssh { key = .. }`).
    key: Option<String>,
}

/// What one call came to.
enum Outcome {
    Answered(String),
    /// The host, or the file, is not there yet.
    NotYet,
}

impl Ssh {
    /// The provider, `known` the host keys state keeps, `key` the one the
    /// program names ([`key_named`]).
    pub fn new(known: BTreeMap<String, KnownHost>, key: Option<String>) -> Ssh {
        Ssh {
            known: RefCell::new(known),
            key,
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
        let named = self.key.as_deref();
        // On a thread of its own, with a runtime of its own: the caller
        // may be inside another (a controller's).
        let out = std::thread::scope(|s| {
            s.spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .context("ssh: start a runtime")?
                    .block_on(session(&name, port, user, host, named, handler, &op))
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

/// The key `use ssh { key = "NAME" }` names: a string, the name of an
/// agent key (its comment or SHA-256 fingerprint) or of the credential
/// `ssh:NAME`.
pub fn key_named(program: &Program) -> Result<Option<String>> {
    for st in &program.statements {
        let head = match st {
            Stmt::Fact(a) => a,
            Stmt::Rule(r) => &r.head,
            _ => continue,
        };
        let [Term::Val(Value::Str(n)), Term::Obj(settings)] = head.args.as_slice() else {
            continue;
        };
        if head.pred != "provider_config" || n != "ssh" {
            continue;
        }
        match settings.get("key") {
            None => {}
            Some(Term::Val(Value::Str(k))) if !k.is_empty() => return Ok(Some(k.clone())),
            Some(_) => bail!(
                "use ssh {{ key = .. }} takes a string: the name of a key in the agent (its \
                 comment or SHA256 fingerprint) or of the credential ssh:NAME"
            ),
        }
    }
    Ok(None)
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
    named: Option<&str>,
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
    authenticate(&mut h, user, host, named).await?;
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

/// The files tried when no key is named, in this order.
const DEFAULT_KEYS: [&str; 3] = ["id_ed25519", "id_ecdsa", "id_rsa"];

/// The key the program names first (`use ssh { key = NAME }`: the agent's
/// of that comment or fingerprint, else the credential `ssh:NAME`'s file),
/// then every other key the agent holds, then, when none is named, the
/// unencrypted `~/.ssh/id_*`. The named key goes first because a host
/// hangs up after a few refusals (`MaxAuthTries`). Never a prompt: a key
/// with a passphrase is the agent's to use, and its file is not decrypted.
async fn authenticate(
    h: &mut Handle<Client>,
    user: &str,
    host: &str,
    named: Option<&str>,
) -> Result<()> {
    let mut tried = Tried::default();
    let home = std::env::home_dir().unwrap_or_default();
    let mut agent = match AgentClient::connect_env().await {
        Ok(a) => Some(a),
        Err(e) => {
            if std::env::var_os("SSH_AUTH_SOCK").is_some() {
                tried.agent = Agent::Silent(e.to_string());
            }
            None
        }
    };
    let mut ids = Vec::new();
    if let Some(a) = agent.as_mut() {
        match a.request_identities().await {
            Ok(all) => {
                tried.agent = Agent::Held;
                ids = all
                    .into_iter()
                    .filter_map(|id| match id {
                        AgentIdentity::PublicKey { key, comment } => Some((key, comment)),
                        _ => None,
                    })
                    .collect();
            }
            Err(e) => tried.agent = Agent::Silent(e.to_string()),
        }
    }
    let in_agent = named.is_some_and(|n| ids.iter().any(|(k, c)| names(n, c, k)));
    if let Some(n) = named {
        ids.sort_by_key(|(k, c)| !names(n, c, k));
    }
    // The named key's file, when it is not the agent's.
    let mut files: Vec<(String, std::path::PathBuf)> = Vec::new();
    match named {
        Some(_) if in_agent => {}
        // A fingerprint names an agent's key only.
        Some(n) if n.starts_with("SHA256:") => tried.missing = Some(None),
        Some(n) => {
            let path = credentials::file(&format!("ssh:{n}"))?;
            match path.exists() {
                true => files.push((tilde(&home, &path), path)),
                false => tried.missing = Some(Some(tilde(&home, &path))),
            }
        }
        None => {}
    }
    if try_files(h, user, &files, &mut tried).await? {
        return Ok(());
    }
    if let Some(a) = agent.as_mut() {
        for (key, comment) in ids {
            if tried.hung_up {
                break;
            }
            let hash = match key.algorithm().is_rsa() {
                true => h.best_supported_rsa_hash().await.ok().flatten().flatten(),
                false => None,
            };
            tried.offered.push(show_key(&comment, &key));
            match h.authenticate_publickey_with(user, key, hash, a).await {
                Ok(r) if r.success() => return Ok(()),
                Ok(_) => {}
                // Closed: the host stops listening after its MaxAuthTries.
                Err(_) => tried.hung_up = h.is_closed(),
            }
        }
    }
    if named.is_none() {
        let files: Vec<_> = DEFAULT_KEYS
            .iter()
            .map(|f| (format!("~/.ssh/{f}"), home.join(".ssh").join(f)))
            .filter(|(_, p)| p.exists())
            .collect();
        if try_files(h, user, &files, &mut tried).await? {
            return Ok(());
        }
    }
    bail!("{}", tried.refusal(host, user, named))
}

/// Offer each unencrypted key file in turn; one with a passphrase is
/// noted, not decrypted. Whether the host accepted one.
async fn try_files(
    h: &mut Handle<Client>,
    user: &str,
    files: &[(String, std::path::PathBuf)],
    tried: &mut Tried,
) -> Result<bool> {
    for (shown, path) in files {
        let text = std::fs::read_to_string(path).with_context(|| format!("read {shown}"))?;
        let key = match russh::keys::decode_secret_key(&text, None) {
            Ok(k) => k,
            Err(russh::keys::Error::KeyIsEncrypted) => {
                tried.locked.push(shown.clone());
                continue;
            }
            Err(e) => bail!("{shown} is not a private key dform reads: {e}"),
        };
        if tried.hung_up {
            continue;
        }
        let hash = match key.algorithm().is_rsa() {
            true => h.best_supported_rsa_hash().await.ok().flatten().flatten(),
            false => None,
        };
        tried.refused.push(shown.clone());
        let key = PrivateKeyWithHashAlg::new(Arc::new(key), hash);
        match h.authenticate_publickey(user, key).await {
            Ok(r) if r.success() => return Ok(true),
            Ok(_) => {}
            Err(_) if h.is_closed() => tried.hung_up = true,
            Err(e) => {
                return Err(e).with_context(|| format!("authenticate as {user} with {shown}"));
            }
        }
    }
    Ok(false)
}

/// What an authentication offered, and what it could not.
#[derive(Default)]
struct Tried {
    agent: Agent,
    /// The agent's keys offered, by comment (else fingerprint).
    offered: Vec<String>,
    /// Key files offered.
    refused: Vec<String>,
    /// Key files with a passphrase: not offered.
    locked: Vec<String>,
    /// The named key, when it is not in the agent and not on disk: its
    /// file (none for a fingerprint).
    missing: Option<Option<String>>,
    /// The host closed the connection after refusing some.
    hung_up: bool,
}

/// What the agent was.
#[derive(Default)]
enum Agent {
    /// No `SSH_AUTH_SOCK`.
    #[default]
    None,
    /// `SSH_AUTH_SOCK` names one that did not answer.
    Silent(String),
    /// It answered with the keys it holds (`Tried::offered`).
    Held,
}

impl Tried {
    /// The refusal: what was offered and what was not, in one line, and
    /// one line of what to do.
    fn refusal(&self, host: &str, user: &str, named: Option<&str>) -> String {
        let n = self.offered.len();
        let mut said = vec![match &self.agent {
            Agent::None => "no agent at SSH_AUTH_SOCK".to_string(),
            Agent::Silent(e) => format!("the agent at SSH_AUTH_SOCK did not answer ({e})"),
            Agent::Held if n == 0 => "the agent holds no keys".to_string(),
            Agent::Held => format!(
                "the agent offered {n} key{} ({}) and the host accepted none",
                if n == 1 { "" } else { "s" },
                self.offered.join(", ")
            ),
        }];
        if !self.refused.is_empty() {
            said.push(format!("the host did not accept {}", prose(&self.refused)));
        }
        match self.locked.as_slice() {
            [] => {}
            [one] => said.push(format!("{one} has a passphrase")),
            many => said.push(format!("{} have passphrases", prose(many))),
        }
        if let (Some(k), Some(file)) = (named, &self.missing) {
            said.push(match file {
                Some(path) => format!("the key {k:?} is not in the agent and there is no {path}"),
                None => format!("the key {k:?} is not in the agent"),
            });
        } else if named.is_none() && self.refused.is_empty() && self.locked.is_empty() {
            let files: Vec<String> = DEFAULT_KEYS.iter().map(|f| format!("~/.ssh/{f}")).collect();
            said.push(format!("there is no {}", prose_or(&files)));
        }
        if self.hung_up {
            said.push("the host hung up after those (its MaxAuthTries)".to_string());
        }
        let deploy_key = "name an unencrypted deploy key: use ssh { key = \"k3s-admin\" }";
        let todo = match (&self.missing, self.locked.first()) {
            (None, _) if self.hung_up && named.is_none() => {
                "name the key the host holds, and it is offered first: use ssh { key = \
                 \"k3s-admin\" }"
                    .to_string()
            }
            (Some(Some(path)), _) => format!(
                "`ssh-add` it (its comment or SHA256 fingerprint names it), or put the \
                 unencrypted key at {path}"
            ),
            (Some(None), _) => "`ssh-add` it".to_string(),
            _ if n == 0 && self.refused.is_empty() => match (&self.agent, self.locked.first()) {
                (Agent::Held, Some(f)) => {
                    format!("`ssh-add {f}`: a key with a passphrase is used through the agent")
                }
                (Agent::Held, None) => format!("`ssh-add` the key the host holds, or {deploy_key}"),
                _ => format!("start an agent and `ssh-add`, or {deploy_key}"),
            },
            (None, Some(f)) => format!(
                "`ssh-add {f}`, or add one of these keys to {user}'s authorized_keys on {host}"
            ),
            (None, None) => format!(
                "add one of these keys to {user}'s authorized_keys on {host}, or `ssh-add` the \
                 key it holds"
            ),
        };
        format!("{host} refused {user}: {}\n  {todo}", said.join(", and "))
    }
}

/// Whether the agent's key is the one `name` names: by its comment or its
/// SHA-256 fingerprint.
fn names(name: &str, comment: &str, key: &russh::keys::PublicKey) -> bool {
    comment == name || key.fingerprint(HashAlg::Sha256).to_string() == name
}

/// `path` under `home` as `~/..`.
fn tilde(home: &std::path::Path, path: &std::path::Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => path.display().to_string(),
    }
}

/// A list as prose: `a`, `a and b`, `a, b and c`.
fn prose(items: &[String]) -> String {
    prose_with(items, "and")
}

/// `a`, `a or b`, `a, b or c`.
fn prose_or(items: &[String]) -> String {
    prose_with(items, "or")
}

fn prose_with(items: &[String], conj: &str) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} {conj} {last}", init.join(", ")),
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

    /// An agent's key is named by its comment or its SHA-256 fingerprint
    /// (a test agent keeps no comments: the comment is checked here).
    #[test]
    fn a_key_is_named_by_its_comment_or_fingerprint() {
        use russh::keys::ssh_key::private::Ed25519Keypair;
        let k = russh::keys::PrivateKey::from(Ed25519Keypair::from_seed(&[3; 32]));
        let fp = k.public_key().fingerprint(HashAlg::Sha256).to_string();
        assert!(names("k3s-admin", "k3s-admin", k.public_key()));
        assert!(names(&fp, "", k.public_key()));
        assert!(!names("k3s-admin", "laptop", k.public_key()));
        assert_eq!(show_key("laptop", k.public_key()), "laptop");
        assert_eq!(show_key("", k.public_key()), fp);
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
