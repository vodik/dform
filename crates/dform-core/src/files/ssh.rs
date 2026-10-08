//! `ssh://USER@HOST[:PORT]/PATH` (R-153): a host's file over SFTP, and
//! `git-upload-pack` for a `git+ssh://` repository, through an SSH client
//! in process (russh; never the `ssh` binary, the operator's ssh config or
//! PATH). Nothing runs a command a program asked for (R-151): a file, a
//! package or a unit to manage is a resource of a provider whose apply
//! runs what it must.
//!
//! The user is the location's userinfo (the local user's name when it has
//! none). The key is the operator's, never one in the program, and never
//! asked for: the key dform.toml names for the location (`[io]
//! credentials = { "ssh://10.0.0.*" = "ssh:k3s-admin" }`: an agent key by
//! its comment or fingerprint, offered first, or the credential
//! `ssh:k3s-admin`'s unencrypted file), then every key the agent
//! (`SSH_AUTH_SOCK`) holds, else the unencrypted `~/.ssh/id_ed25519`,
//! `id_ecdsa`, `id_rsa`. A key file with a passphrase is not decrypted: the
//! agent uses such a key. A refusal says what was offered and what was
//! not, and what to do.
//!
//! A host's key is recorded on first contact (`State::known_hosts`, kept
//! by the apply, `Files::keep`) and checked on every contact after: a
//! changed key is an error naming both fingerprints, until `dform state
//! forget-host HOST`.
//!
//! A host that does not answer yet (the connection refused, timed out
//! after [`CONNECT_TIMEOUT`], no route) and a path that does not exist yet
//! (cloud-init still running) are "not yet" (`Failure::NotYet`), which an
//! apply waits on (R-81). An authentication failure, a changed host key
//! are errors.

use crate::git::Pipe;
use crate::plugin::credentials;
use crate::plugin::host::{Error, Failure};
use crate::state::KnownHost;
use crate::uri::Uri;
use anyhow::{Context, Result, anyhow, bail};
use russh::Disconnect;
use russh::client::{self, Handle};
use russh::keys::agent::AgentIdentity;
use russh::keys::agent::client::AgentClient;
use russh::keys::{HashAlg, PrivateKeyWithHashAlg, PublicKeyOrCertificate};
use russh_sftp::client::SftpSession;
use russh_sftp::client::error::Error as SftpError;
use russh_sftp::protocol::StatusCode;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// How long one attempt waits for the host to answer and agree a session;
/// past it the host is "not yet" (an apply asks again within its wait).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a read may take once connected; past it the call
/// is an error.
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Where a location says to connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The host's name or address, a v6 one without brackets.
    pub name: String,
    pub port: u16,
    pub user: String,
    /// How state keys the host: `HOST`, or `HOST:PORT` off port 22.
    pub label: String,
}

/// The local user's name: the user of a location that names none.
pub fn local_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "root".into())
}

impl Target {
    pub fn of(u: &Uri) -> Result<Target, Failure> {
        let host = u
            .host_ascii()
            .or_else(|| u.host.clone())
            .filter(|h| !h.is_empty())
            .ok_or_else(|| Error::fatal(format!("{u}: an ssh location names its host")))?;
        let name = host
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_string();
        let port = u.port.unwrap_or(22);
        let label = match u.port {
            Some(p) => format!("{host}:{p}"),
            None => host,
        };
        let user = u
            .user
            .as_deref()
            .map(|s| {
                percent_encoding::percent_decode_str(s)
                    .decode_utf8_lossy()
                    .into_owned()
            })
            .unwrap_or_else(local_user);
        Ok(Target {
            name,
            port,
            user,
            label,
        })
    }

    fn shown(&self) -> String {
        format!("ssh://{}@{}", self.user, self.label)
    }
}

/// What one call came to.
enum Outcome<T> {
    Answered(T),
    /// The host, or the file, is not there yet.
    NotYet(String),
}

/// Run `go` on a session with `t` (connected, its key checked against
/// `known` and recorded there when new, authenticated with `named` first),
/// on a thread and a runtime of its own: the caller may be inside another
/// (a controller's).
fn with_session<T: Send>(
    known: &Mutex<BTreeMap<String, KnownHost>>,
    t: &Target,
    named: Option<&str>,
    go: impl for<'a> FnOnce(
        &'a mut Handle<Client>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Outcome<T>>> + Send + 'a>,
    > + Send,
) -> Result<Outcome<T>> {
    let expect = known
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&t.label)
        .cloned();
    let seen = Arc::new(Mutex::new(None));
    let handler = Client {
        expect,
        seen: seen.clone(),
    };
    let out = std::thread::scope(|s| {
        s.spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .context("ssh: start a runtime")?
                .block_on(async move {
                    let Some(mut h) = session(t, named, handler).await? else {
                        return Ok(Outcome::NotYet(format!(
                            "{} does not answer yet",
                            t.shown()
                        )));
                    };
                    let out = go(&mut h).await;
                    let _ = h.disconnect(Disconnect::ByApplication, "", "en").await;
                    out
                })
        })
        .join()
        .unwrap_or_else(|_| Err(anyhow!("ssh: the client panicked")))
    })?;
    // A host met for the first time: its key, from now on.
    if let Some((key_type, fingerprint)) = seen.lock().ok().and_then(|s| s.clone()) {
        known
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(t.label.clone())
            .or_insert_with(|| KnownHost {
                key_type,
                fingerprint,
                when: crate::memo::now(),
            });
    }
    Ok(out)
}

/// The file at `path` on `t`, over SFTP.
pub fn read(
    known: &Mutex<BTreeMap<String, KnownHost>>,
    t: &Target,
    named: Option<&str>,
    path: &str,
) -> Result<Vec<u8>, Failure> {
    let path = path.to_string();
    let out = with_session(known, t, named, move |h| {
        Box::pin(async move {
            tokio::time::timeout(CALL_TIMEOUT, sftp_read(h, &path))
                .await
                .map_err(|_| anyhow!("did not finish within {}s", CALL_TIMEOUT.as_secs()))?
        })
    });
    match out {
        Ok(Outcome::Answered(b)) => Ok(b),
        Ok(Outcome::NotYet(why)) => Err(Failure::NotYet(why)),
        Err(e) => Err(Error::fatal(format!("{e:#}")).into()),
    }
}

/// `command` running on `t` (`git-upload-pack 'PATH'`): its stdout to
/// read and its stdin to write, bridged to the session's thread.
pub fn pipe(
    known: &Mutex<BTreeMap<String, KnownHost>>,
    t: &Target,
    named: Option<&str>,
    command: &str,
) -> Result<Pipe, Error> {
    use std::sync::mpsc;
    let (out_tx, out_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    let (in_tx, in_rx) = tokio::sync::mpsc::unbounded_channel::<Option<Vec<u8>>>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
    let (known_c, t_c, named_c, command) = (
        // The session runs on past this call: its own copy of the keys,
        // merged back when it has met the host.
        Arc::new(Mutex::new(
            known.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        )),
        t.clone(),
        named.map(str::to_string),
        command.to_string(),
    );
    let known_back = known_c.clone();
    std::thread::spawn(move || {
        let ready = ready_tx.clone();
        let r = with_session(&known_c, &t_c, named_c.as_deref(), move |h| {
            Box::pin(async move {
                let ch = h.channel_open_session().await?;
                ch.exec(true, command.as_bytes()).await?;
                let (mut rd, mut wr) = tokio::io::split(ch.into_stream());
                let _ = ready.send(Ok(()));
                let mut in_rx = in_rx;
                let writer = async move {
                    use tokio::io::AsyncWriteExt;
                    while let Some(m) = in_rx.recv().await {
                        match m {
                            Some(b) => wr.write_all(&b).await?,
                            None => break,
                        }
                    }
                    wr.shutdown().await?;
                    Ok::<_, std::io::Error>(())
                };
                let reader = async move {
                    use tokio::io::AsyncReadExt;
                    let mut buf = vec![0u8; 64 * 1024];
                    loop {
                        match rd.read(&mut buf).await {
                            Ok(0) => break,
                            Ok(n) => {
                                if out_tx.send(Ok(buf[..n].to_vec())).is_err() {
                                    break;
                                }
                            }
                            Err(e) => {
                                let _ = out_tx.send(Err(e));
                                break;
                            }
                        }
                    }
                };
                let (w, ()) = tokio::join!(writer, reader);
                w?;
                Ok(Outcome::Answered(()))
            })
        });
        let _ = match r {
            Ok(Outcome::Answered(())) => Ok(()),
            Ok(Outcome::NotYet(why)) => ready_tx.send(Err(why)),
            Err(e) => ready_tx.send(Err(format!("{e:#}"))),
        };
    });
    match ready_rx.recv() {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(Error::fatal(format!("{}: {e}", t.shown()))),
        Err(_) => return Err(Error::fatal(format!("{}: the session ended", t.shown()))),
    }
    // The host's key, met now, is kept.
    let met = known_back.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let mut k = known.lock().unwrap_or_else(|e| e.into_inner());
    for (h, v) in met {
        k.entry(h).or_insert(v);
    }
    Ok(Pipe {
        read: Box::new(Reader {
            rx: out_rx,
            buf: Vec::new(),
            at: 0,
        }),
        write: Box::new(Writer(in_tx)),
    })
}

/// The remote command's stdout, as it arrives.
struct Reader {
    rx: std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    buf: Vec<u8>,
    at: usize,
}

impl std::io::Read for Reader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.at == self.buf.len() {
            match self.rx.recv() {
                Ok(Ok(b)) => {
                    self.buf = b;
                    self.at = 0;
                }
                Ok(Err(e)) => return Err(e),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.at);
        out[..n].copy_from_slice(&self.buf[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// The remote command's stdin; dropped, it is closed.
struct Writer(tokio::sync::mpsc::UnboundedSender<Option<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0
            .send(Some(b.to_vec()))
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::BrokenPipe))?;
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        let _ = self.0.send(None);
    }
}

/// Connect to `t`, check the host key and authenticate: `None` when the
/// host is not there yet.
async fn session(
    t: &Target,
    named: Option<&str>,
    handler: Client,
) -> Result<Option<Handle<Client>>> {
    let config = Arc::new(client::Config {
        inactivity_timeout: Some(CALL_TIMEOUT),
        ..Default::default()
    });
    let (name, port, host) = (t.name.as_str(), t.port, t.label.as_str());
    let connect = client::connect(config, (name, port), handler);
    let mut h = match tokio::time::timeout(CONNECT_TIMEOUT, connect).await {
        Err(_) => return Ok(None),
        Ok(Err(Refused::HostKey { was, now })) => bail!(
            "the host key of {host} changed: state recorded {} {} on {}, the host now offers \
             {} {}; if the host was rebuilt, `dform state forget-host {host}` and run again",
            was.key_type,
            was.fingerprint,
            was.when,
            now.0,
            now.1
        ),
        Ok(Err(Refused::Ssh(e))) => match not_yet(&e) {
            true => return Ok(None),
            false => return Err(anyhow!(e).context(format!("connect to {name}:{port}"))),
        },
        Ok(Ok(h)) => h,
    };
    authenticate(&mut h, &t.user, host, named).await?;
    Ok(Some(h))
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

/// The key dform.toml names first (`[io] credentials`, `ssh:NAME`: the agent's
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
        let deploy_key = "name an unencrypted deploy key in dform.toml: [io] credentials = \
                          { \"ssh://HOST/*\" = \"ssh:k3s-admin\" }";
        let todo = match (&self.missing, self.locked.first()) {
            (None, _) if self.hung_up && named.is_none() => {
                "name the key the host holds in dform.toml, and it is offered first: [io] \
                 credentials = { \"ssh://HOST/*\" = \"ssh:k3s-admin\" }"
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

/// The file at `path`, over SFTP; one that does not exist is "not yet".
async fn sftp_read(h: &Handle<Client>, path: &str) -> Result<Outcome<Vec<u8>>> {
    let ch = h.channel_open_session().await?;
    ch.request_subsystem(true, "sftp").await?;
    let sftp = SftpSession::new(ch.into_stream())
        .await
        .context("start SFTP")?;
    let bytes = match sftp.read(path).await {
        Ok(b) => b,
        Err(SftpError::Status(s)) if s.status_code == StatusCode::NoSuchFile => {
            return Ok(Outcome::NotYet(format!("{path} is not there yet")));
        }
        Err(e) => bail!("read {path}: {e}"),
    };
    let _ = sftp.close().await;
    Ok(Outcome::Answered(bytes))
}

/// The client's side of the handshake: the host key checked against the
/// one recorded, and the one seen kept for the caller to record.
struct Client {
    expect: Option<KnownHost>,
    seen: Arc<Mutex<Option<(String, String)>>>,
}

#[derive(Debug)]
enum Refused {
    Ssh(russh::Error),
    /// The key recorded, and the one offered (type, fingerprint).
    HostKey {
        was: KnownHost,
        now: (String, String),
    },
}

impl From<russh::Error> for Refused {
    fn from(e: russh::Error) -> Self {
        Refused::Ssh(e)
    }
}

impl client::Handler for Client {
    type Error = Refused;

    async fn check_server_key(&mut self, key: &PublicKeyOrCertificate) -> Result<bool, Refused> {
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
            return Err(Refused::HostKey {
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
    fn a_location_is_a_user_a_host_and_a_port() {
        let t = |l: &str| Target::of(&Uri::parse(l).unwrap()).unwrap();
        let a = t("ssh://ubuntu@10.0.0.5/etc/k3s.yaml");
        assert_eq!(
            (a.name.as_str(), a.port, a.user.as_str()),
            ("10.0.0.5", 22, "ubuntu")
        );
        assert_eq!(a.label, "10.0.0.5");
        let b = t("ssh://u@h.example:2222/x");
        assert_eq!((b.port, b.label.as_str()), (2222, "h.example:2222"));
        let c = t("ssh://u@[::1]:2200/x");
        assert_eq!((c.name.as_str(), c.label.as_str()), ("::1", "[::1]:2200"));
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
