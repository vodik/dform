//! The fake's transport: HTTP/1.1 requests read off a connection and
//! answered as the API authenticates them (a signature, a bearer token,
//! the clock), and the DNS resolver beside it over UDP.

use super::*;
use crate::sign::signature;
use std::io::{BufRead, BufReader, Write};

/// One request as it came: its method, target (the path with its query),
/// headers (lower-case names) and body.
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: String,
}

impl Request {
    /// The next request on `reader`, or none when the connection ends.
    fn read(reader: &mut impl BufRead) -> Option<Request> {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return None;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return None;
        };
        let (method, target) = (method.to_string(), target.to_string());
        let mut headers = BTreeMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return None;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        let len: usize = headers
            .get("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if reader.read_exact(&mut body).is_err() {
            return None;
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        Some(Request {
            method,
            target,
            headers,
            body,
        })
    }

    /// The path, without the API's `/1.0`, and the query, both decoded.
    fn path_and_query(&self) -> (String, BTreeMap<String, String>) {
        let (path, query) = match self.target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (self.target.clone(), String::new()),
        };
        let path = path.strip_prefix("/1.0").unwrap_or(&path).to_string();
        let query: BTreeMap<String, String> = query
            .split('&')
            .filter(|kv| !kv.is_empty())
            .map(|kv| match kv.split_once('=') {
                Some((k, v)) => (decode(k), decode(v)),
                None => (decode(kv), String::new()),
            })
            .collect();
        (decode(&path), query)
    }
}

pub(super) fn serve(conn: TcpStream, world: &Mutex<World>, base: &str) {
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut out = conn;
    while let Some(req) = Request::read(&mut reader) {
        let latency = {
            let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
            w.answering += 1;
            w.most = w.most.max(w.answering);
            w.latency
        };
        std::thread::sleep(latency);
        let (status, answer) = world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .respond(&req, base);
        world.lock().unwrap_or_else(|e| e.into_inner()).answering -= 1;
        let text = answer.to_string();
        let reason = if status < 300 { "OK" } else { "Error" };
        let resp = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{text}",
            text.len()
        );
        if out.write_all(resp.as_bytes()).is_err() {
            return;
        }
    }
}

impl World {
    /// `req` answered as the API would: the clock and a token at once, a
    /// call with a bearer token it minted or a valid signature (at
    /// `base`) within the time window, refused otherwise.
    fn respond(&mut self, req: &Request, base: &str) -> (u16, Json) {
        let (path, query) = req.path_and_query();
        let (method, target, headers, body) = (&req.method, &req.target, &req.headers, &req.body);
        let json_body = serde_json::from_str(body).unwrap_or(Json::Null);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
            + self.skew;
        let bearer = headers
            .get("authorization")
            .and_then(|a| a.strip_prefix("Bearer "));
        if path == "/auth/time" {
            self.times_asked += 1;
            (200, json!(now))
        } else if path == "/auth/oauth2/token" {
            self.mint(headers, body)
        } else if let Some(token) = bearer {
            if self.tokens.contains(token) {
                self.answer_seen(method, &path, target, &query, json_body)
            } else {
                (401, json!({"message": "Invalid token"}))
            }
        } else if let Some((status, why)) = bad_signature(
            headers,
            method,
            &format!("{base}{}", target.strip_prefix("/1.0").unwrap_or(target)),
            body,
        )
        .map(|why| (403, json!({"message": why})))
        .or_else(|| {
            self.key_revoked.then(|| {
                (
                    403,
                    json!({"errorCode": "INVALID_CREDENTIAL", "httpCode": "403 Forbidden",
                           "message": "This credential is not valid"}),
                )
            })
        }) {
            (status, why)
        } else if headers
            .get("x-ovh-timestamp")
            .and_then(|t| t.parse::<i64>().ok())
            .is_some_and(|t| (t - now).abs() > TIME_WINDOW)
        {
            (400, json!({"message": "Query out of time"}))
        } else {
            self.answer_seen(method, &path, target, &query, json_body)
        }
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or(""), 16) {
                    Ok(x) => {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Why a signed call's headers do not hold, if they do not.
fn bad_signature(
    headers: &BTreeMap<String, String>,
    method: &str,
    url: &str,
    body: &str,
) -> Option<String> {
    let h = |k: &str| headers.get(k).map(String::as_str).unwrap_or_default();
    if h("x-ovh-application") != APPLICATION_KEY {
        return Some("Invalid application key".into());
    }
    if h("x-ovh-consumer") != CONSUMER_KEY {
        return Some("This credential does not exist".into());
    }
    let ts: i64 = h("x-ovh-timestamp").parse().unwrap_or(0);
    let want = signature(APPLICATION_SECRET, CONSUMER_KEY, method, url, body, ts);
    (h("x-ovh-signature") != want).then(|| "Invalid signature".to_string())
}

/// The fake resolver: each NS query answered from `nameservers`, an
/// empty answer for a zone it does not know.
pub(super) fn resolve(sock: UdpSocket, world: &Mutex<World>) {
    let mut buf = [0u8; 512];
    while let Ok((n, from)) = sock.recv_from(&mut buf) {
        let q = &buf[..n];
        let Some((zone, after)) = crate::dns::name(q, 12) else {
            continue;
        };
        let ns = world
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .nameservers
            .get(&zone)
            .cloned()
            .unwrap_or_default();
        let mut m = q[..after + 4].to_vec();
        m[2] = 0x81;
        m[3] = 0x80;
        m[6..8].copy_from_slice(&(ns.len() as u16).to_be_bytes());
        for host in ns {
            let rdata = crate::dns::encode(&host).unwrap_or_default();
            // The question's name; NS, IN, a TTL of an hour.
            m.extend_from_slice(&[0xc0, 12, 0, 2, 0, 1, 0, 0, 0x0e, 0x10]);
            m.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
            m.extend_from_slice(&rdata);
        }
        let _ = sock.send_to(&m, from);
    }
}
