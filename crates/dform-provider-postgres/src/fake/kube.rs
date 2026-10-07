//! A Kubernetes API for tests, over HTTP/1.1 on 127.0.0.1: one service
//! in one namespace, selecting one ready pod, whose port-forward (the
//! WebSocket `v4.channel.k8s.io` protocol, as the API server speaks it)
//! carries bytes to a port on 127.0.0.1, the fake Postgres's. It answers
//! the three calls a port-forward to a service makes: the service, the
//! pods it selects, and the pod's `portforward`. [`Kube::kubeconfig`] is
//! a kubeconfig that names it.

use serde_json::json;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "fake-kube-token";

pub struct Kube {
    pub port: u16,
    pub namespace: String,
    pub service: String,
    /// Every request, `METHOD PATH`.
    seen: Arc<Mutex<Vec<String>>>,
}

impl Kube {
    /// The API, its service `namespace/service` forwarding to `target`
    /// (a port on 127.0.0.1) from the service port 5432.
    pub fn start(namespace: &str, service: &str, target: u16) -> Kube {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (ns, svc, s) = (namespace.to_string(), service.to_string(), seen.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (ns, svc, s) = (ns.clone(), svc.clone(), s.clone());
                std::thread::spawn(move || {
                    let _ = serve(stream, &ns, &svc, target, &s);
                });
            }
        });
        Kube {
            port,
            namespace: namespace.into(),
            service: service.into(),
            seen,
        }
    }

    pub fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    pub fn kubeconfig(&self) -> String {
        format!(
            "apiVersion: v1\nkind: Config\nclusters:\n- name: fake\n  cluster:\n    server: \
             http://127.0.0.1:{}\ncontexts:\n- name: fake\n  context:\n    cluster: fake\n    \
             user: fake\ncurrent-context: fake\nusers:\n- name: fake\n  user:\n    token: {TOKEN}\n",
            self.port
        )
    }
}

fn respond(s: &mut TcpStream, status: &str, body: &serde_json::Value) -> std::io::Result<()> {
    let body = body.to_string();
    write!(
        s,
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{body}",
        body.len()
    )
}

fn serve(
    stream: TcpStream,
    ns: &str,
    svc: &str,
    target: u16,
    seen: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut out = stream;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let mut parts = line.split_whitespace();
        let (method, path) = (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        );
        let mut headers = Vec::new();
        loop {
            let mut h = String::new();
            reader.read_line(&mut h)?;
            let h = h.trim_end();
            if h.is_empty() {
                break;
            }
            if let Some((k, v)) = h.split_once(':') {
                headers.push((k.trim().to_lowercase(), v.trim().to_string()));
            }
        }
        let header = |k: &str| {
            headers
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap_or_default()
        };
        seen.lock().unwrap().push(format!("{method} {path}"));
        if header("authorization") != format!("Bearer {TOKEN}") {
            respond(
                &mut out,
                "401 Unauthorized",
                &json!({"kind": "Status", "code": 401}),
            )?;
            continue;
        }
        let pod = format!("{svc}-0");
        let (route, _) = path.split_once('?').unwrap_or((&path, ""));
        if route == format!("/api/v1/namespaces/{ns}/services/{svc}") {
            respond(
                &mut out,
                "200 OK",
                &json!({
                    "apiVersion": "v1", "kind": "Service",
                    "metadata": {"name": svc, "namespace": ns},
                    "spec": {"selector": {"app": svc}, "ports": [{"port": 5432, "targetPort": "postgres"}]},
                }),
            )?;
        } else if route == format!("/api/v1/namespaces/{ns}/pods") {
            respond(
                &mut out,
                "200 OK",
                &json!({
                    "apiVersion": "v1", "kind": "PodList", "metadata": {},
                    "items": [{
                        "metadata": {"name": pod, "namespace": ns, "labels": {"app": svc}},
                        "spec": {"containers": [{"name": "postgres", "ports": [{"name": "postgres", "containerPort": 5432}]}]},
                        "status": {"phase": "Running", "conditions": [{"type": "Ready", "status": "True"}]},
                    }],
                }),
            )?;
        } else if route == format!("/api/v1/namespaces/{ns}/pods/{pod}/portforward")
            && header("upgrade").eq_ignore_ascii_case("websocket")
        {
            use base64::Engine;
            use sha1::Digest;
            let key = header("sec-websocket-key");
            let accept = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(
                format!("{key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11").as_bytes(),
            ));
            write!(
                out,
                "HTTP/1.1 101 Switching Protocols\r\nupgrade: websocket\r\nconnection: Upgrade\r\n\
                 sec-websocket-accept: {accept}\r\nsec-websocket-protocol: v4.channel.k8s.io\r\n\r\n"
            )?;
            return forward(reader, out, target);
        } else {
            respond(
                &mut out,
                "404 Not Found",
                &json!({"kind": "Status", "code": 404}),
            )?;
        }
    }
}

/// One unmasked binary frame.
fn frame(payload: &[u8]) -> Vec<u8> {
    let mut f = vec![0x82];
    match payload.len() {
        n if n < 126 => f.push(n as u8),
        n if n < 65536 => {
            f.push(126);
            f.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            f.push(127);
            f.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    f.extend_from_slice(payload);
    f
}

/// A client's frame: its opcode and unmasked payload.
fn read_frame(r: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut h = [0u8; 2];
    r.read_exact(&mut h)?;
    let mut len = u64::from(h[1] & 0x7f);
    if len == 126 {
        let mut b = [0u8; 2];
        r.read_exact(&mut b)?;
        len = u64::from(u16::from_be_bytes(b));
    } else if len == 127 {
        let mut b = [0u8; 8];
        r.read_exact(&mut b)?;
        len = u64::from_be_bytes(b);
    }
    let mut mask = [0u8; 4];
    if h[1] & 0x80 != 0 {
        r.read_exact(&mut mask)?;
    }
    let mut p = vec![0u8; len as usize];
    r.read_exact(&mut p)?;
    for (i, b) in p.iter_mut().enumerate() {
        *b ^= mask[i % 4];
    }
    Ok((h[0] & 0x0f, p))
}

/// The forward: channel 0 the data, 1 its errors, each first saying the
/// port (little-endian), then channel 0 both ways to 127.0.0.1:`target`.
fn forward(
    mut from: impl Read + Send + 'static,
    mut to: TcpStream,
    target: u16,
) -> std::io::Result<()> {
    let port = 5432u16.to_le_bytes();
    to.write_all(&frame(&[0, port[0], port[1]]))?;
    to.write_all(&frame(&[1, port[0], port[1]]))?;
    let pod = TcpStream::connect(("127.0.0.1", target))?;
    let mut pod_in = pod.try_clone()?;
    let mut pod_out = pod;
    let mut ws_out = to.try_clone()?;
    std::thread::spawn(move || {
        let mut buf = [0u8; 16384];
        loop {
            match pod_out.read(&mut buf) {
                Ok(0) | Err(_) => {
                    let _ = ws_out.write_all(&[0x88, 0]);
                    return;
                }
                Ok(n) => {
                    let mut p = vec![0];
                    p.extend_from_slice(&buf[..n]);
                    if ws_out.write_all(&frame(&p)).is_err() {
                        return;
                    }
                }
            }
        }
    });
    loop {
        let (op, p) = read_frame(&mut from)?;
        match op {
            8 => {
                let _ = pod_in.shutdown(std::net::Shutdown::Both);
                return Ok(());
            }
            2 if p.first() == Some(&0) => pod_in.write_all(&p[1..])?,
            _ => {}
        }
    }
}
