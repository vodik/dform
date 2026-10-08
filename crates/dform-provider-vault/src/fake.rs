//! A fake Vault for tests, over HTTP/1.1 on 127.0.0.1: KV version 2
//! mounts (`GET /v1/MOUNT/data/PATH[?version=N]`, the secret's data and
//! metadata, a deleted or destroyed version a 404 with its metadata, a
//! missing secret a 404 with no data), token auth (`X-Vault-Token` or
//! `Authorization: Bearer`, a token [`Server::token`] knows, else 403), and
//! AppRole's login (`POST /v1/auth/approle/login`), which issues a token.
//! [`Server::put`] writes a new version, as `vault kv put` does.

use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// The token the server knows from the start.
pub const TOKEN: &str = "hvs.fake-root-token";
pub const ROLE_ID: &str = "fake-role-id";
pub const SECRET_ID: &str = "fake-secret-id";

/// A version of a secret.
#[derive(Debug, Clone)]
struct Version {
    data: Json,
    deleted: bool,
    destroyed: bool,
}

/// One request the server answered: method, path with its query, and
/// the token it carried (`-` for none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub token: String,
}

#[derive(Default)]
struct World {
    secrets: BTreeMap<(String, String), Vec<Version>>,
    tokens: BTreeSet<String>,
    seen: Vec<Seen>,
    logins: usize,
}

pub struct Server {
    pub port: u16,
    world: Arc<Mutex<World>>,
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake Vault");
        let port = listener.local_addr().unwrap().port();
        let world = Arc::new(Mutex::new(World::default()));
        world.lock().unwrap().tokens.insert(TOKEN.to_string());
        let w = world.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let w = w.clone();
                std::thread::spawn(move || serve(conn, &w));
            }
        });
        Server { port, world }
    }

    /// `http://127.0.0.1:PORT`.
    pub fn address(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Write `data` as the next version of `MOUNT/PATH`: its number.
    pub fn put(&self, mount: &str, path: &str, data: Json) -> u64 {
        let mut w = self.world();
        let v = w
            .secrets
            .entry((mount.to_string(), path.to_string()))
            .or_default();
        v.push(Version {
            data,
            deleted: false,
            destroyed: false,
        });
        v.len() as u64
    }

    /// Delete version `n` of `MOUNT/PATH` (`vault kv delete -versions=N`).
    pub fn delete(&self, mount: &str, path: &str, n: u64) {
        let mut w = self.world();
        if let Some(v) = w
            .secrets
            .get_mut(&(mount.to_string(), path.to_string()))
            .and_then(|v| v.get_mut(n as usize - 1))
        {
            v.deleted = true;
        }
    }

    /// Let `token` in, beside [`TOKEN`].
    pub fn token(&self, token: &str) {
        self.world().tokens.insert(token.to_string());
    }

    /// Forget every token but [`TOKEN`] (an AppRole token that lapsed).
    pub fn expire_tokens(&self) {
        let mut w = self.world();
        w.tokens.retain(|t| t == TOKEN);
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.world().seen.clone()
    }

    /// How many AppRole logins it has answered.
    pub fn logins(&self) -> usize {
        self.world().logins
    }
}

fn serve(conn: TcpStream, world: &Mutex<World>) {
    let mut r = BufReader::new(conn.try_clone().unwrap());
    let mut conn = conn;
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (method, target) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        );
        let mut headers = BTreeMap::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            if h == "\r\n" || h == "\n" {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|l| l.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let token = headers
            .get("x-vault-token")
            .cloned()
            .or_else(|| {
                headers
                    .get("authorization")
                    .and_then(|a| a.strip_prefix("Bearer "))
                    .map(str::to_string)
            })
            .unwrap_or_else(|| "-".into());
        let (status, answer) = {
            let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
            w.seen.push(Seen {
                method: method.clone(),
                path: target.clone(),
                token: token.clone(),
            });
            answer(&mut w, &method, &target, &token, &body)
        };
        let text = answer.to_string();
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            403 => "Forbidden",
            405 => "Method Not Allowed",
            _ => "Not Found",
        };
        if write!(
            conn,
            "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{text}",
            text.len()
        )
        .is_err()
        {
            return;
        }
    }
}

fn answer(w: &mut World, method: &str, target: &str, token: &str, body: &[u8]) -> (u16, Json) {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let denied = || (403, json!({"errors": ["permission denied"]}));
    if method == "POST" && path == "/v1/auth/approle/login" {
        let b: Json = serde_json::from_slice(body).unwrap_or_default();
        if b["role_id"] != json!(ROLE_ID) || b["secret_id"] != json!(SECRET_ID) {
            return (400, json!({"errors": ["invalid role or secret ID"]}));
        }
        w.logins += 1;
        let t = format!("hvs.approle-{}", w.logins);
        w.tokens.insert(t.clone());
        return (
            200,
            json!({"auth": {"client_token": t, "lease_duration": 3600}}),
        );
    }
    if !w.tokens.contains(token) {
        return denied();
    }
    let Some((mount, secret)) = path
        .strip_prefix("/v1/")
        .and_then(|p| p.split_once("/data/"))
    else {
        return (404, json!({"errors": []}));
    };
    if method != "GET" {
        return (405, json!({"errors": ["the fake reads only"]}));
    }
    let Some(versions) = w.secrets.get(&(mount.to_string(), secret.to_string())) else {
        return (404, json!({"errors": []}));
    };
    let n = query
        .split('&')
        .find_map(|p| p.strip_prefix("version="))
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(versions.len());
    let Some(v) = versions.get(n - 1) else {
        return (404, json!({"errors": []}));
    };
    let metadata = json!({
        "version": n,
        "created_time": "2026-10-07T09:00:00Z",
        "deletion_time": if v.deleted { "2026-10-07T10:00:00Z" } else { "" },
        "destroyed": v.destroyed,
    });
    if v.deleted || v.destroyed {
        return (404, json!({"data": {"data": null, "metadata": metadata}}));
    }
    (200, json!({"data": {"data": v.data, "metadata": metadata}}))
}
