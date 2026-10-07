//! A fake S3 server for tests: path-style GET, PUT (with `If-Match` and
//! `If-None-Match: *`), DELETE and ListObjectsV2 over HTTP/1.1 on
//! 127.0.0.1, its objects a `dform_core::store::MemoryStore`, so it keeps
//! S3's rules for ETags and conditional writes. It does not check
//! signatures, and every bucket exists. [`Server::ignoring_conditions`]
//! is a server that does not keep them, as some S3-compatible ones do not.

use dform_core::store::{Cond, MemoryStore, Store};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// A running server; it lives as long as the test process.
pub struct Server {
    /// `http://127.0.0.1:PORT`.
    pub endpoint: String,
    /// What it has served: how many connections, and each request as
    /// `METHOD /bucket/key` (the query left out).
    seen: Arc<Seen>,
}

#[derive(Default)]
struct Seen {
    connections: AtomicUsize,
    requests: Mutex<Vec<String>>,
}

impl Server {
    pub fn start() -> Server {
        Server::serve(false)
    }

    /// A server that takes every PUT, whatever its `If-Match` or
    /// `If-None-Match` say.
    pub fn ignoring_conditions() -> Server {
        Server::serve(true)
    }

    fn serve(lax: bool) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake S3 server");
        let endpoint = format!("http://{}", listener.local_addr().expect("its address"));
        let objects = Arc::new(MemoryStore::new());
        let seen = Arc::new(Seen::default());
        let counted = seen.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                counted.connections.fetch_add(1, Ordering::SeqCst);
                let (objects, seen) = (objects.clone(), counted.clone());
                std::thread::spawn(move || serve(conn, &objects, &seen, lax));
            }
        });
        Server { endpoint, seen }
    }

    /// How many connections it has accepted.
    pub fn connections(&self) -> usize {
        self.seen.connections.load(Ordering::SeqCst)
    }

    /// The requests it has answered, `METHOD /bucket/key`, in order.
    pub fn requests(&self) -> Vec<String> {
        self.seen
            .requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }
}

/// `%XX` decoded (and in a query, `+` as a space).
fn decode(s: &str, query: bool) -> String {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                let hex = std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("");
                match u8::from_str_radix(hex, 16) {
                    Ok(x) => {
                        out.push(x);
                        i += 3;
                        continue;
                    }
                    Err(_) => out.push(b'%'),
                }
            }
            b'+' if query => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

struct Answer {
    status: u16,
    etag: Option<String>,
    body: Vec<u8>,
}

fn error(status: u16, code: &str) -> Answer {
    Answer {
        status,
        etag: None,
        body: format!("<Error><Code>{code}</Code><Message>{code}</Message></Error>").into_bytes(),
    }
}

fn serve(conn: TcpStream, objects: &MemoryStore, seen: &Seen, lax: bool) {
    let mut out = match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    };
    let mut r = BufReader::new(conn);
    loop {
        let mut line = String::new();
        if r.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return;
        };
        let (method, target) = (method.to_string(), target.to_string());
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            if r.read_line(&mut h).unwrap_or(0) == 0 {
                return;
            }
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_ascii_lowercase(), v.trim().to_string()));
            }
        }
        let header = |k: &str| headers.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        let len: usize = header("content-length")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0; len];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        let answer = handle(&method, &target, &header, &body, objects, lax);
        let path = target.split_once('?').map_or(target.as_str(), |(p, _)| p);
        seen.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(format!("{method} {}", decode(path, false)));
        let reason = match answer.status {
            200 => "OK",
            204 => "No Content",
            404 => "Not Found",
            412 => "Precondition Failed",
            _ => "Error",
        };
        let mut head = format!(
            "HTTP/1.1 {} {reason}\r\nContent-Length: {}\r\n",
            answer.status,
            answer.body.len()
        );
        if let Some(e) = &answer.etag {
            head.push_str(&format!("ETag: {e}\r\n"));
        }
        head.push_str("\r\n");
        // One write: a head and a body written apart wait on the
        // client's delayed ACK (Nagle), 40ms an answer.
        let mut bytes = head.into_bytes();
        bytes.extend_from_slice(&answer.body);
        if out.write_all(&bytes).is_err() {
            return;
        }
    }
}

fn handle(
    method: &str,
    target: &str,
    header: &dyn Fn(&str) -> Option<String>,
    body: &[u8],
    objects: &MemoryStore,
    lax: bool,
) -> Answer {
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let path = decode(path.trim_start_matches('/'), false);
    let (bucket, key) = path.split_once('/').unwrap_or((&path, ""));
    let param = |name: &str| {
        query.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (decode(k, true) == name).then(|| decode(v, true))
        })
    };
    let full = format!("{bucket}/{key}");
    let done = |status: u16| Answer {
        status,
        etag: None,
        body: Vec::new(),
    };
    match (method, key) {
        ("PUT", "") => done(200),
        ("GET", "") if param("list-type").as_deref() == Some("2") => {
            let prefix = param("prefix").unwrap_or_default();
            let keys = objects
                .list(&format!("{bucket}/{prefix}"))
                .unwrap_or_default();
            let mut xml_out = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\
                 <ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">\
                 <Name>{}</Name><Prefix>{}</Prefix><KeyCount>{}</KeyCount>\
                 <MaxKeys>1000</MaxKeys><IsTruncated>false</IsTruncated>",
                xml(bucket),
                xml(&prefix),
                keys.len()
            );
            for k in keys {
                let Ok(Some(o)) = objects.get(&k) else {
                    continue;
                };
                let k = k.strip_prefix(&format!("{bucket}/")).unwrap_or(&k);
                xml_out.push_str(&format!(
                    "<Contents><Key>{}</Key><LastModified>2026-01-01T00:00:00.000Z</LastModified>\
                     <ETag>{}</ETag><Size>{}</Size><StorageClass>STANDARD</StorageClass></Contents>",
                    xml(k),
                    xml(&o.etag),
                    o.bytes.len()
                ));
            }
            xml_out.push_str("</ListBucketResult>");
            Answer {
                status: 200,
                etag: None,
                body: xml_out.into_bytes(),
            }
        }
        ("GET", _) => match objects.get(&full) {
            Ok(Some(o)) => Answer {
                status: 200,
                etag: Some(o.etag),
                body: o.bytes,
            },
            _ => error(404, "NoSuchKey"),
        },
        ("PUT", _) => {
            let cond = match (header("if-match"), header("if-none-match")) {
                (Some(e), _) => Cond::IfMatch(e),
                (None, Some(s)) if s == "*" => Cond::IfAbsent,
                _ => Cond::Any,
            };
            let cond = if lax { Cond::Any } else { cond };
            match objects.put(&full, body, &cond) {
                Ok(Some(etag)) => Answer {
                    status: 200,
                    etag: Some(etag),
                    body: Vec::new(),
                },
                _ => error(412, "PreconditionFailed"),
            }
        }
        ("DELETE", _) => {
            let _ = objects.delete(&full);
            done(204)
        }
        _ => error(400, "NotImplemented"),
    }
}
