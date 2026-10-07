//! A Postgres server for tests, over TCP on 127.0.0.1: the wire
//! protocol's startup (TLS when asked, [`Server::tls`]), SCRAM-SHA-256
//! authentication against the verifier each role keeps, and the simple
//! query protocol, answering the statements the provider sends (and
//! nothing else, saying so) over a catalog of roles, memberships and
//! databases kept in memory. A query of several statements is one
//! transaction: a failing one undoes those before it, and CREATE DATABASE
//! is refused inside it, as the server does. Every query is kept as it
//! arrived ([`Server::queries`]), so a test can say what crossed the wire.
//!
//! It starts with one superuser, [`ADMIN`] (password [`ADMIN_PASSWORD`]),
//! and the database `postgres`. [`kube`] is a Kubernetes API that
//! forwards a pod's port to it.

pub mod kube;

use crate::scram;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const ADMIN: &str = "dform_admin";
pub const ADMIN_PASSWORD: &str = "admin-password";

/// The CA the server's certificate is signed by (`root_cert` for
/// verify-full), and the server's chain and key: `localhost` and
/// 127.0.0.1, valid until 2126.
pub const CA: &str = include_str!("ca.pem");
const CHAIN: &str = include_str!("server.pem");
const KEY: &str = include_str!("server.key");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub superuser: bool,
    pub inherit: bool,
    pub createrole: bool,
    pub createdb: bool,
    pub login: bool,
    pub replication: bool,
    pub connlimit: i64,
    /// What `pg_authid.rolpassword` keeps.
    pub verifier: Option<String>,
    pub comment: Option<String>,
}

impl Default for Role {
    fn default() -> Role {
        Role {
            superuser: false,
            inherit: true,
            createrole: false,
            createdb: false,
            login: false,
            replication: false,
            connlimit: -1,
            verifier: None,
            comment: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Database {
    pub owner: String,
    pub encoding: String,
    pub collate: String,
    pub ctype: String,
    pub comment: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct World {
    pub roles: BTreeMap<String, Role>,
    /// (group, member).
    pub members: BTreeSet<(String, String)>,
    pub databases: BTreeMap<String, Database>,
}

#[derive(Default)]
struct Shared {
    world: World,
    /// Every query's text, as it arrived.
    queries: Vec<String>,
    /// Logins by role, and the TLS sessions among them.
    logins: Vec<(String, bool)>,
    tls: bool,
}

pub struct Server {
    pub port: u16,
    shared: Arc<Mutex<Shared>>,
}

impl Server {
    /// A server on a free port, serving until the test process ends.
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let port = listener.local_addr().unwrap().port();
        let mut world = World::default();
        world.roles.insert(
            ADMIN.into(),
            Role {
                superuser: true,
                createrole: true,
                createdb: true,
                login: true,
                replication: true,
                verifier: Some(scram::verifier(ADMIN_PASSWORD)),
                ..Role::default()
            },
        );
        world.databases.insert(
            "postgres".into(),
            Database {
                owner: ADMIN.into(),
                encoding: "UTF8".into(),
                collate: "en_US.utf8".into(),
                ctype: "en_US.utf8".into(),
                comment: None,
            },
        );
        let shared = Arc::new(Mutex::new(Shared {
            world,
            ..Shared::default()
        }));
        let s = shared.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let s = s.clone();
                std::thread::spawn(move || {
                    let _ = serve(stream, &s);
                });
            }
        });
        Server { port, shared }
    }

    /// Answer an SSLRequest with TLS from now on (refuse it before).
    pub fn tls(&self) -> &Server {
        self.lock().tls = true;
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Shared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn world(&self) -> World {
        self.lock().world.clone()
    }

    pub fn with_world(&self, f: impl FnOnce(&mut World)) {
        f(&mut self.lock().world)
    }

    pub fn queries(&self) -> Vec<String> {
        self.lock().queries.clone()
    }

    /// Each login: its role, and whether it was over TLS.
    pub fn logins(&self) -> Vec<(String, bool)> {
        self.lock().logins.clone()
    }

    /// Whether `role` can log in with `password` (its verifier is of it).
    pub fn accepts(&self, role: &str, password: &str) -> bool {
        self.lock()
            .world
            .roles
            .get(role)
            .and_then(|r| r.verifier.as_deref())
            .is_some_and(|v| scram::matches(v, password))
    }

    /// The libpq environment that names this server, as `user` with
    /// `password`, `sslmode` as given.
    pub fn env(&self, user: &str, password: &str, sslmode: &str) -> Vec<(String, String)> {
        [
            ("PGHOST", "127.0.0.1".to_string()),
            ("PGPORT", self.port.to_string()),
            ("PGUSER", user.to_string()),
            ("PGPASSWORD", password.to_string()),
            ("PGSSLMODE", sslmode.to_string()),
            ("PGDATABASE", "postgres".to_string()),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect()
    }
}

/// A connection, TLS or not.
enum Conn {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for Conn {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.read(buf),
            Conn::Tls(s) => s.read(buf),
        }
    }
}

impl Write for Conn {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            Conn::Plain(s) => s.write(buf),
            Conn::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Conn::Plain(s) => s.flush(),
            Conn::Tls(s) => s.flush(),
        }
    }
}

fn tls_config() -> Arc<rustls::ServerConfig> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(CHAIN.as_bytes())
        .collect::<Result<_, _>>()
        .expect("the fake's chain");
    let key = PrivateKeyDer::from_pem_slice(KEY.as_bytes()).expect("the fake's key");
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(chain, key)
    .expect("the fake's certificate");
    Arc::new(config)
}

fn read_n(c: &mut impl Read, n: usize) -> std::io::Result<Vec<u8>> {
    let mut b = vec![0; n];
    c.read_exact(&mut b)?;
    Ok(b)
}

fn int(b: &[u8]) -> i32 {
    i32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// A backend message: its type and body.
fn message(t: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![t];
    m.extend_from_slice(&((body.len() + 4) as i32).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn cstr(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    b.push(0);
    b
}

fn auth(code: i32, data: &[u8]) -> Vec<u8> {
    let mut b = code.to_be_bytes().to_vec();
    b.extend_from_slice(data);
    message(b'R', &b)
}

fn error(code: &str, text: &str) -> Vec<u8> {
    let mut b = Vec::new();
    for (f, v) in [(b'S', "ERROR"), (b'V', "ERROR"), (b'C', code), (b'M', text)] {
        b.push(f);
        b.extend(cstr(v));
    }
    b.push(0);
    message(b'E', &b)
}

/// A frontend message: its type and body.
fn frontend(c: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let t = read_n(c, 1)?[0];
    let len = int(&read_n(c, 4)?) as usize;
    Ok((t, read_n(c, len.saturating_sub(4))?))
}

fn serve(stream: TcpStream, shared: &Mutex<Shared>) -> std::io::Result<()> {
    let lock = || shared.lock().unwrap_or_else(|e| e.into_inner());
    let mut conn = Conn::Plain(stream);
    // The startup packet, after an SSLRequest when one comes first.
    let params = loop {
        let len = int(&read_n(&mut conn, 4)?) as usize;
        let body = read_n(&mut conn, len - 4)?;
        match int(&body[..4]) {
            // SSLRequest.
            80877103 => {
                let tls = lock().tls;
                conn.write_all(if tls { b"S" } else { b"N" })?;
                if tls && let Conn::Plain(tcp) = conn {
                    let server = rustls::ServerConnection::new(tls_config())
                        .map_err(std::io::Error::other)?;
                    conn = Conn::Tls(Box::new(rustls::StreamOwned::new(server, tcp)));
                }
            }
            // GSSENCRequest: no.
            80877104 => conn.write_all(b"N")?,
            196608 => {
                let mut params = BTreeMap::new();
                let mut parts = body[4..]
                    .split(|b| *b == 0)
                    .map(|p| String::from_utf8_lossy(p).to_string());
                while let (Some(k), Some(v)) = (parts.next(), parts.next()) {
                    if k.is_empty() {
                        break;
                    }
                    params.insert(k, v);
                }
                break params;
            }
            other => {
                conn.write_all(&error(
                    "08P01",
                    &format!("unsupported startup code {other}"),
                ))?;
                return Ok(());
            }
        }
    };
    let user = params.get("user").cloned().unwrap_or_default();
    let database = params
        .get("database")
        .cloned()
        .unwrap_or_else(|| user.clone());
    let is_tls = matches!(conn, Conn::Tls(_));
    // SCRAM-SHA-256, against the verifier the role keeps.
    let verifier = {
        let s = lock();
        s.world
            .roles
            .get(&user)
            .filter(|r| r.login)
            .and_then(|r| r.verifier.clone())
            .and_then(|v| scram::parse(&v))
    };
    conn.write_all(&auth(10, b"SCRAM-SHA-256\0\0"))?;
    conn.flush()?;
    let (t, body) = frontend(&mut conn)?;
    if t != b'p' {
        return Ok(());
    }
    let mech_end = body.iter().position(|b| *b == 0).unwrap_or(0);
    let first = String::from_utf8_lossy(&body[mech_end + 5..]).to_string();
    let Some((gs2, bare)) = first
        .strip_prefix("n,,")
        .map(|b| ("n,,", b))
        .or_else(|| first.strip_prefix("y,,").map(|b| ("y,,", b)))
    else {
        conn.write_all(&error(
            "08P01",
            "a SCRAM client-first-message the fake does not read",
        ))?;
        return Ok(());
    };
    let client_nonce = bare
        .split(',')
        .find_map(|a| a.strip_prefix("r="))
        .unwrap_or_default()
        .to_string();
    let failed = |conn: &mut Conn| {
        conn.write_all(&error(
            "28P01",
            &format!("password authentication failed for user \"{user}\""),
        ))
    };
    let Some(v) = verifier else {
        failed(&mut conn)?;
        return Ok(());
    };
    let nonce = format!("{client_nonce}fakeServerNonce");
    let server_first = format!(
        "r={nonce},s={},i={}",
        STANDARD.encode(&v.salt),
        v.iterations
    );
    conn.write_all(&auth(11, server_first.as_bytes()))?;
    conn.flush()?;
    let (_, body) = frontend(&mut conn)?;
    let client_final = String::from_utf8_lossy(&body).to_string();
    let (without_proof, proof) = client_final.rsplit_once(",p=").unwrap_or(("", ""));
    let auth_message = format!("{bare},{server_first},{without_proof}");
    let signature = scram::hmac(&v.stored, auth_message.as_bytes());
    let proof = STANDARD.decode(proof).unwrap_or_default();
    let client_key: Vec<u8> = proof
        .iter()
        .zip(signature.iter())
        .map(|(a, b)| a ^ b)
        .collect();
    use sha2::Digest;
    let ok = proof.len() == 32
        && sha2::Sha256::digest(&client_key).as_slice() == v.stored.as_slice()
        && without_proof.contains(&format!("r={nonce}"))
        && without_proof.contains(&format!("c={}", STANDARD.encode(gs2)));
    if !ok {
        failed(&mut conn)?;
        return Ok(());
    }
    let server_sig = scram::hmac(&v.server, auth_message.as_bytes());
    conn.write_all(&auth(
        12,
        format!("v={}", STANDARD.encode(server_sig)).as_bytes(),
    ))?;
    conn.write_all(&auth(0, b""))?;
    if !lock().world.databases.contains_key(&database) {
        conn.write_all(&error(
            "3D000",
            &format!("database \"{database}\" does not exist"),
        ))?;
        return Ok(());
    }
    lock().logins.push((user.clone(), is_tls));
    for (k, v) in [
        ("server_version", "18.0"),
        ("server_encoding", "UTF8"),
        ("client_encoding", "UTF8"),
        ("standard_conforming_strings", "on"),
        ("integer_datetimes", "on"),
        ("DateStyle", "ISO, MDY"),
    ] {
        let mut b = cstr(k);
        b.extend(cstr(v));
        conn.write_all(&message(b'S', &b))?;
    }
    conn.write_all(&message(b'K', &[0, 0, 0, 1, 0, 0, 0, 2]))?;
    conn.write_all(&message(b'Z', b"I"))?;
    conn.flush()?;
    let session = Session { user, database };
    loop {
        let (t, body) = match frontend(&mut conn) {
            Ok(m) => m,
            Err(_) => return Ok(()),
        };
        match t {
            b'X' => return Ok(()),
            b'Q' => {
                let text = String::from_utf8_lossy(&body)
                    .trim_end_matches('\0')
                    .to_string();
                let out = {
                    let mut s = lock();
                    s.queries.push(text.clone());
                    session.query(&mut s.world, &text)
                };
                conn.write_all(&out)?;
                conn.write_all(&message(b'Z', b"I"))?;
                conn.flush()?;
            }
            other => {
                conn.write_all(&error(
                    "08P01",
                    &format!(
                        "the fake speaks the simple query protocol only (got {:?})",
                        other as char
                    ),
                ))?;
                conn.write_all(&message(b'Z', b"I"))?;
                conn.flush()?;
            }
        }
    }
}

/// A token of a statement.
#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// A bare word, lower-cased, or a quoted identifier as it is.
    Word(String, bool),
    Str(String),
    Num(i64),
    Punct(char),
}

fn tokens(sql: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = sql.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '"' {
            let mut s = String::new();
            i += 1;
            loop {
                match c.get(i) {
                    None => return Err("an unterminated identifier".into()),
                    Some('"') if c.get(i + 1) == Some(&'"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(x) => {
                        s.push(*x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Word(s, true));
        } else if ch == '\'' || ((ch == 'E' || ch == 'e') && c.get(i + 1) == Some(&'\'')) {
            let escape = ch != '\'';
            i += if escape { 2 } else { 1 };
            let mut s = String::new();
            loop {
                match c.get(i) {
                    None => return Err("an unterminated string".into()),
                    Some('\'') if c.get(i + 1) == Some(&'\'') => {
                        s.push('\'');
                        i += 2;
                    }
                    Some('\'') => {
                        i += 1;
                        break;
                    }
                    Some('\\') if escape => {
                        s.push(*c.get(i + 1).unwrap_or(&'\\'));
                        i += 2;
                    }
                    Some(x) => {
                        s.push(*x);
                        i += 1;
                    }
                }
            }
            out.push(Tok::Str(s));
        } else if ch.is_ascii_digit()
            || (ch == '-' && c.get(i + 1).is_some_and(char::is_ascii_digit))
        {
            let start = i;
            i += 1;
            while c.get(i).is_some_and(char::is_ascii_digit) {
                i += 1;
            }
            let n: String = c[start..i].iter().collect();
            out.push(Tok::Num(n.parse().map_err(|_| format!("number {n}"))?));
        } else if ch.is_alphanumeric() || ch == '_' {
            let start = i;
            while c
                .get(i)
                .is_some_and(|x| x.is_alphanumeric() || *x == '_' || *x == '$')
            {
                i += 1;
            }
            out.push(Tok::Word(
                c[start..i].iter().collect::<String>().to_lowercase(),
                false,
            ));
        } else {
            out.push(Tok::Punct(ch));
            i += 1;
        }
    }
    Ok(out)
}

/// The statements of a query: split at each `;` outside a string or a
/// quoted identifier.
fn statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let (mut quote, mut chars) = (None::<char>, sql.chars().peekable());
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, ';') => {
                out.push(std::mem::take(&mut cur));
                continue;
            }
            (None, '\'' | '"') => quote = Some(ch),
            (Some(q), x) if x == q => {
                if chars.peek() == Some(&q) {
                    cur.push(ch);
                    cur.push(chars.next().unwrap());
                    continue;
                }
                quote = None;
            }
            _ => {}
        }
        cur.push(ch);
    }
    out.push(cur);
    out.into_iter().filter(|s| !s.trim().is_empty()).collect()
}

/// What one connection is: who, in which database.
struct Session {
    user: String,
    database: String,
}

/// A statement's failure: its SQLSTATE and message.
type Refused = (&'static str, String);

/// What a statement answers: a result set (columns, rows) or a command
/// tag.
enum Answer {
    Rows(Vec<&'static str>, Vec<Vec<Option<String>>>),
    Done(&'static str),
}

/// A parser over one statement's tokens.
struct P {
    t: Vec<Tok>,
    i: usize,
}

impl P {
    fn peek_kw(&self, kw: &str) -> bool {
        matches!(self.t.get(self.i), Some(Tok::Word(w, false)) if w == kw)
    }

    fn kw(&mut self, kw: &str) -> bool {
        let yes = self.peek_kw(kw);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn expect(&mut self, kw: &str) -> Result<(), Refused> {
        match self.kw(kw) {
            true => Ok(()),
            false => Err(syntax(&format!(
                "expected {kw} at {:?}",
                self.t.get(self.i)
            ))),
        }
    }

    fn name(&mut self) -> Result<String, Refused> {
        match self.t.get(self.i).cloned() {
            Some(Tok::Word(w, _)) => {
                self.i += 1;
                Ok(w)
            }
            other => Err(syntax(&format!("expected a name at {other:?}"))),
        }
    }

    fn string(&mut self) -> Result<String, Refused> {
        match self.t.get(self.i).cloned() {
            Some(Tok::Str(s)) => {
                self.i += 1;
                Ok(s)
            }
            other => Err(syntax(&format!("expected a string at {other:?}"))),
        }
    }

    fn num(&mut self) -> Result<i64, Refused> {
        match self.t.get(self.i).cloned() {
            Some(Tok::Num(n)) => {
                self.i += 1;
                Ok(n)
            }
            other => Err(syntax(&format!("expected a number at {other:?}"))),
        }
    }

    fn comma(&mut self) -> bool {
        let yes = self.t.get(self.i) == Some(&Tok::Punct(','));
        if yes {
            self.i += 1;
        }
        yes
    }

    fn done(&self) -> bool {
        self.i >= self.t.len()
    }
}

fn syntax(m: &str) -> Refused {
    ("42601", format!("syntax error: {m}"))
}

fn no_role(n: &str) -> Refused {
    ("42704", format!("role \"{n}\" does not exist"))
}

fn no_database(n: &str) -> Refused {
    ("3D000", format!("database \"{n}\" does not exist"))
}

fn denied(m: &str) -> Refused {
    ("42501", format!("permission denied {m}"))
}

fn b(v: bool) -> Option<String> {
    Some(if v { "t" } else { "f" }.to_string())
}

impl Session {
    /// A query's backend messages: each statement's answer, the first
    /// failure undoing those before it.
    fn query(&self, world: &mut World, text: &str) -> Vec<u8> {
        let stmts = statements(text);
        if stmts.is_empty() {
            return message(b'I', b"");
        }
        let before = world.clone();
        let mut out = Vec::new();
        for s in &stmts {
            match self.statement(world, s, stmts.len() > 1) {
                Ok(Answer::Done(tag)) => out.extend(message(b'C', &cstr(tag))),
                Ok(Answer::Rows(cols, rows)) => {
                    let mut d = (cols.len() as i16).to_be_bytes().to_vec();
                    for c in &cols {
                        d.extend(cstr(c));
                        d.extend_from_slice(&0i32.to_be_bytes()); // table oid
                        d.extend_from_slice(&0i16.to_be_bytes()); // column
                        d.extend_from_slice(&25i32.to_be_bytes()); // text
                        d.extend_from_slice(&(-1i16).to_be_bytes());
                        d.extend_from_slice(&(-1i32).to_be_bytes());
                        d.extend_from_slice(&0i16.to_be_bytes()); // text format
                    }
                    out.extend(message(b'T', &d));
                    for r in &rows {
                        let mut d = (r.len() as i16).to_be_bytes().to_vec();
                        for v in r {
                            match v {
                                None => d.extend_from_slice(&(-1i32).to_be_bytes()),
                                Some(s) => {
                                    d.extend_from_slice(&(s.len() as i32).to_be_bytes());
                                    d.extend_from_slice(s.as_bytes());
                                }
                            }
                        }
                        out.extend(message(b'D', &d));
                    }
                    out.extend(message(b'C', &cstr(&format!("SELECT {}", rows.len()))));
                }
                Err((code, m)) => {
                    *world = before;
                    out.extend(error(code, &m));
                    return out;
                }
            }
        }
        out
    }

    fn may_create_role(&self, world: &World) -> Result<(), Refused> {
        match world.roles.get(&self.user) {
            Some(r) if r.superuser || r.createrole => Ok(()),
            _ => Err(denied("to create role")),
        }
    }

    fn superuser(&self, world: &World) -> bool {
        world.roles.get(&self.user).is_some_and(|r| r.superuser)
    }

    /// `[WITH] opt..` of CREATE ROLE and ALTER ROLE, onto `r`.
    fn role_options(p: &mut P, r: &mut Role) -> Result<Vec<String>, Refused> {
        p.kw("with");
        let mut in_role = Vec::new();
        while !p.done() {
            let w = p.name()?;
            match w.as_str() {
                "login" => r.login = true,
                "nologin" => r.login = false,
                "superuser" => r.superuser = true,
                "nosuperuser" => r.superuser = false,
                "createdb" => r.createdb = true,
                "nocreatedb" => r.createdb = false,
                "createrole" => r.createrole = true,
                "nocreaterole" => r.createrole = false,
                "inherit" => r.inherit = true,
                "noinherit" => r.inherit = false,
                "replication" => r.replication = true,
                "noreplication" => r.replication = false,
                "connection" => {
                    p.expect("limit")?;
                    r.connlimit = p.num()?;
                }
                "password" => {
                    let pw = p.string()?;
                    // As the server keeps it: a verifier as it is, a
                    // plaintext hashed.
                    r.verifier = Some(match pw.starts_with("SCRAM-SHA-256$") {
                        true => pw,
                        false => scram::verifier(&pw),
                    });
                }
                "in" => {
                    p.expect("role")?;
                    loop {
                        in_role.push(p.name()?);
                        if !p.comma() {
                            break;
                        }
                    }
                }
                other => {
                    return Err(syntax(&format!(
                        "a role option the fake does not know: {other}"
                    )));
                }
            }
        }
        Ok(in_role)
    }

    fn statement(&self, world: &mut World, sql: &str, in_block: bool) -> Result<Answer, Refused> {
        if sql.trim_start().to_lowercase().starts_with("select") {
            return self.select(world, sql);
        }
        let mut p = P {
            t: tokens(sql).map_err(|e| syntax(&e))?,
            i: 0,
        };
        if p.kw("create") {
            if p.kw("role") {
                self.may_create_role(world)?;
                let name = p.name()?;
                if world.roles.contains_key(&name) {
                    return Err(("42710", format!("role \"{name}\" already exists")));
                }
                let mut r = Role::default();
                let groups = Self::role_options(&mut p, &mut r)?;
                for g in &groups {
                    if !world.roles.contains_key(g) {
                        return Err(no_role(g));
                    }
                }
                world.roles.insert(name.clone(), r);
                world
                    .members
                    .extend(groups.into_iter().map(|g| (g, name.clone())));
                return Ok(Answer::Done("CREATE ROLE"));
            }
            p.expect("database")?;
            if in_block {
                return Err((
                    "25001",
                    "CREATE DATABASE cannot run inside a transaction block".into(),
                ));
            }
            let name = p.name()?;
            if world.databases.contains_key(&name) {
                return Err(("42P04", format!("database \"{name}\" already exists")));
            }
            let mut d = Database {
                owner: self.user.clone(),
                encoding: "UTF8".into(),
                collate: "en_US.utf8".into(),
                ctype: "en_US.utf8".into(),
                comment: None,
            };
            p.kw("with");
            while !p.done() {
                match p.name()?.as_str() {
                    "template" => {
                        let t = p.name()?;
                        if !world.databases.contains_key(&t) && t != "template0" {
                            return Err(no_database(&t));
                        }
                    }
                    "owner" => {
                        let o = p.name()?;
                        if !world.roles.contains_key(&o) {
                            return Err(no_role(&o));
                        }
                        d.owner = o;
                    }
                    "encoding" => d.encoding = p.string()?.to_uppercase(),
                    "lc_collate" => d.collate = p.string()?,
                    "lc_ctype" => d.ctype = p.string()?,
                    other => {
                        return Err(syntax(&format!(
                            "a database option the fake does not know: {other}"
                        )));
                    }
                }
            }
            world.databases.insert(name, d);
            return Ok(Answer::Done("CREATE DATABASE"));
        }
        if p.kw("alter") {
            if p.kw("role") {
                let name = p.name()?;
                let mut r = world
                    .roles
                    .get(&name)
                    .cloned()
                    .ok_or_else(|| no_role(&name))?;
                if !self.superuser(world) && name != self.user {
                    self.may_create_role(world)?;
                }
                let groups = Self::role_options(&mut p, &mut r)?;
                if !groups.is_empty() {
                    return Err(syntax("IN ROLE in ALTER ROLE"));
                }
                world.roles.insert(name, r);
                return Ok(Answer::Done("ALTER ROLE"));
            }
            p.expect("database")?;
            let name = p.name()?;
            p.expect("owner")?;
            p.expect("to")?;
            let owner = p.name()?;
            if !world.roles.contains_key(&owner) {
                return Err(no_role(&owner));
            }
            let d = world
                .databases
                .get_mut(&name)
                .ok_or_else(|| no_database(&name))?;
            d.owner = owner;
            return Ok(Answer::Done("ALTER DATABASE"));
        }
        if p.kw("drop") {
            let role = p.kw("role");
            if !role {
                p.expect("database")?;
            }
            let if_exists = p.kw("if") && p.kw("exists");
            let name = p.name()?;
            if role {
                if !world.roles.contains_key(&name) {
                    return match if_exists {
                        true => Ok(Answer::Done("DROP ROLE")),
                        false => Err(no_role(&name)),
                    };
                }
                if name == self.user {
                    return Err(("55006", "current user cannot be dropped".into()));
                }
                if world.databases.values().any(|d| d.owner == name) {
                    return Err((
                        "2BP01",
                        format!(
                            "role \"{name}\" cannot be dropped because some objects depend on it"
                        ),
                    ));
                }
                world.roles.remove(&name);
                world.members.retain(|(g, m)| g != &name && m != &name);
                return Ok(Answer::Done("DROP ROLE"));
            }
            if in_block {
                return Err((
                    "25001",
                    "DROP DATABASE cannot run inside a transaction block".into(),
                ));
            }
            if name == self.database {
                return Err(("55006", "cannot drop the currently open database".into()));
            }
            if world.databases.remove(&name).is_none() && !if_exists {
                return Err(no_database(&name));
            }
            return Ok(Answer::Done("DROP DATABASE"));
        }
        let grant = p.kw("grant");
        if grant || p.kw("revoke") {
            let group = p.name()?;
            p.expect(if grant { "to" } else { "from" })?;
            let member = p.name()?;
            for r in [&group, &member] {
                if !world.roles.contains_key(r) {
                    return Err(no_role(r));
                }
            }
            match grant {
                true => world.members.insert((group, member)),
                false => world.members.remove(&(group, member)),
            };
            return Ok(Answer::Done(if grant {
                "GRANT ROLE"
            } else {
                "REVOKE ROLE"
            }));
        }
        if p.kw("comment") {
            p.expect("on")?;
            let role = p.kw("role");
            if !role {
                p.expect("database")?;
            }
            let name = p.name()?;
            p.expect("is")?;
            let c = p.string()?;
            match role {
                true => {
                    world
                        .roles
                        .get_mut(&name)
                        .ok_or_else(|| no_role(&name))?
                        .comment = Some(c)
                }
                false => {
                    world
                        .databases
                        .get_mut(&name)
                        .ok_or_else(|| no_database(&name))?
                        .comment = Some(c)
                }
            }
            return Ok(Answer::Done("COMMENT"));
        }
        Err(syntax(&format!(
            "the fake does not answer {:?}",
            sql.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
        )))
    }

    /// The SELECTs the provider sends, known by what they read, the row
    /// by the last string literal (its WHERE).
    fn select(&self, world: &World, sql: &str) -> Result<Answer, Refused> {
        let key = tokens(sql)
            .map_err(|e| syntax(&e))?
            .into_iter()
            .rev()
            .find_map(|t| match t {
                Tok::Str(s) => Some(s),
                _ => None,
            })
            .unwrap_or_default();
        if sql.contains("FROM pg_roles r WHERE r.rolname =") {
            let rows = world
                .roles
                .get(&key)
                .map(|r| {
                    let groups: Vec<&String> = world
                        .members
                        .iter()
                        .filter(|(_, m)| m == &key)
                        .map(|(g, _)| g)
                        .collect();
                    vec![
                        Some(key.clone()),
                        b(r.superuser),
                        b(r.inherit),
                        b(r.createrole),
                        b(r.createdb),
                        b(r.login),
                        b(r.replication),
                        Some(r.connlimit.to_string()),
                        r.comment.clone(),
                        Some(json!(groups).to_string()),
                    ]
                })
                .into_iter()
                .collect();
            return Ok(Answer::Rows(
                vec![
                    "name",
                    "superuser",
                    "inherit",
                    "createrole",
                    "createdb",
                    "login",
                    "replication",
                    "connection_limit",
                    "comment",
                    "member_of",
                ],
                rows,
            ));
        }
        if sql.contains("FROM pg_authid WHERE rolname =") {
            if !self.superuser(world) {
                return Err(denied("for table pg_authid"));
            }
            let rows = world
                .roles
                .get(&key)
                .map(|r| vec![r.verifier.clone()])
                .into_iter()
                .collect();
            return Ok(Answer::Rows(vec!["rolpassword"], rows));
        }
        if sql.contains("FROM pg_database d WHERE d.datname =") {
            let rows = world
                .databases
                .get(&key)
                .map(|d| {
                    vec![
                        Some(key.clone()),
                        Some(d.owner.clone()),
                        Some(d.encoding.clone()),
                        Some(d.collate.clone()),
                        Some(d.ctype.clone()),
                        d.comment.clone(),
                    ]
                })
                .into_iter()
                .collect();
            return Ok(Answer::Rows(
                vec![
                    "name",
                    "owner",
                    "encoding",
                    "lc_collate",
                    "lc_ctype",
                    "comment",
                ],
                rows,
            ));
        }
        Err(syntax("a SELECT the fake does not answer"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statements_split_outside_quotes() {
        assert_eq!(
            statements("CREATE ROLE \"a;b\" PASSWORD 'x;''y';\nCOMMENT ON ROLE \"a;b\" IS 'k'"),
            [
                "CREATE ROLE \"a;b\" PASSWORD 'x;''y'",
                "\nCOMMENT ON ROLE \"a;b\" IS 'k'"
            ]
        );
        assert_eq!(
            tokens("ROLE \"A\"\"b\" E'a\\\\b''c' -1").unwrap(),
            [
                Tok::Word("role".into(), false),
                Tok::Word("A\"b".into(), true),
                Tok::Str("a\\b'c".into()),
                Tok::Num(-1),
            ]
        );
    }
}
