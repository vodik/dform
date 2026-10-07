//! A fake OVH API for tests, over HTTP/1.1 on 127.0.0.1: one project
//! (`PROJECT`, described as `DESCRIPTION`), the regions `BHS5` and
//! `ca-east-tor` with two flavors and two images each, and the instance,
//! SSH key and DNS record endpoints the provider calls. It checks every
//! signed call's signature against `APPLICATION_SECRET` and
//! `CONSUMER_KEY`. A new instance is BUILD, with no address, for
//! [`Server::build_polls`] reads, then ACTIVE. Every DNS zone is the
//! account's until [`Server::hosting`] names them; beside it, a DNS
//! resolver over UDP answers the NS records [`Server::delegated`] gives.

use crate::sign::signature;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};

pub const APPLICATION_KEY: &str = "fake-application-key";
pub const APPLICATION_SECRET: &str = "fake-application-secret";
pub const CONSUMER_KEY: &str = "fake-consumer-key";
pub const PROJECT: &str = "0123456789abcdef0123456789abcdef";
pub const DESCRIPTION: &str = "lab";

/// One call the server answered: method, path (with its query), body.
#[derive(Debug, Clone)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub body: Json,
}

#[derive(Default)]
pub struct World {
    pub instances: BTreeMap<String, Json>,
    pub keys: BTreeMap<String, Json>,
    pub records: BTreeMap<i64, Json>,
    /// The DNS zones the account hosts; every one when `None`.
    zones: Option<BTreeSet<String>>,
    /// The NS records the resolver answers, by zone.
    nameservers: BTreeMap<String, Vec<String>>,
    /// Reads left before each BUILD instance is ACTIVE.
    building: BTreeMap<String, u32>,
    next: u64,
    /// Answer the next calls of a `METHOD /path` prefix with these
    /// statuses (`fail`).
    failing: Vec<(String, Vec<u16>)>,
    pub seen: Vec<Seen>,
    build_polls: u32,
    /// How long each call takes to answer (`slow`), as a far API's do.
    latency: std::time::Duration,
    /// Calls being answered now, the most there have been at once, and
    /// the connections accepted.
    answering: usize,
    most: usize,
    connections: usize,
}

pub struct Server {
    /// `http://127.0.0.1:PORT/1.0`, the provider's endpoint.
    pub endpoint: String,
    /// `127.0.0.1:PORT`, the DNS resolver's address.
    pub resolver: String,
    pub world: Arc<Mutex<World>>,
}

fn flavor(region: &str, name: &str, vcpus: i64, ram: i64, disk: i64) -> Json {
    json!({"id": format!("flavor-{name}-{region}"), "name": name, "region": region,
           "vcpus": vcpus, "ram": ram, "disk": disk, "osType": "linux",
           "type": "ovh.ssd.cpu", "available": true, "quota": 20,
           "planCodes": {"monthly": null, "hourly": format!("{name}.consumption")},
           "capabilities": []})
}

fn image(region: &str, name: &str) -> Json {
    let slug = name.to_lowercase().replace(' ', "-");
    json!({"id": format!("image-{slug}-{region}"), "name": name, "region": region,
           "type": "linux", "status": "active", "visibility": "public", "minDisk": 0,
           "minRam": 0, "size": 2.5, "user": "ubuntu",
           "creationDate": "2026-04-25T10:11:12Z", "flavorType": null, "tags": [],
           "planCode": null})
}

const REGIONS: [&str; 2] = ["BHS5", "ca-east-tor"];

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake OVH API");
        let endpoint = format!("http://{}/1.0", listener.local_addr().expect("its address"));
        let world = Arc::new(Mutex::new(World {
            build_polls: 1,
            next: 1,
            ..World::default()
        }));
        let w = world.clone();
        let base = endpoint.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let w = w.clone();
                w.lock().unwrap_or_else(|e| e.into_inner()).connections += 1;
                let base = base.clone();
                std::thread::spawn(move || serve(conn, &w, &base));
            }
        });
        let dns = UdpSocket::bind("127.0.0.1:0").expect("bind the fake resolver");
        let resolver = dns.local_addr().expect("its address").to_string();
        let w = world.clone();
        std::thread::spawn(move || resolve(dns, &w));
        Server {
            endpoint,
            resolver,
            world,
        }
    }

    /// The account hosts these DNS zones, and no other.
    pub fn hosting(&self, zones: &[&str]) {
        self.world().zones = Some(zones.iter().map(|z| z.to_string()).collect());
    }

    /// The resolver answers `zone`'s NS records with `ns`.
    pub fn delegated(&self, zone: &str, ns: &[&str]) {
        self.world()
            .nameservers
            .insert(zone.to_string(), ns.iter().map(|n| n.to_string()).collect());
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Each call takes `latency` to answer (the real API's is about a
    /// second from Toronto); calls on separate connections overlap.
    pub fn slow(&self, latency: std::time::Duration) {
        self.world().latency = latency;
    }

    /// The most calls it has answered at once.
    pub fn most_at_once(&self) -> usize {
        self.world().most
    }

    /// The connections it has accepted.
    pub fn connections(&self) -> usize {
        self.world().connections
    }

    /// How many reads a new instance stays BUILD for.
    pub fn build_polls(&self, n: u32) {
        self.world().build_polls = n;
    }

    /// Answer the next calls whose `METHOD /path` starts with `call`
    /// (`POST /cloud/project`) with these statuses, one each.
    pub fn fail(&self, call: &str, statuses: &[u16]) {
        self.world()
            .failing
            .push((call.to_string(), statuses.to_vec()));
    }

    pub fn instances(&self) -> Vec<Json> {
        self.world().instances.values().cloned().collect()
    }

    pub fn keys(&self) -> Vec<Json> {
        self.world().keys.values().cloned().collect()
    }

    pub fn records(&self) -> Vec<Json> {
        self.world().records.values().cloned().collect()
    }

    /// The calls answered, `METHOD PATH`, in order.
    pub fn calls(&self) -> Vec<String> {
        self.world()
            .seen
            .iter()
            .map(|s| format!("{} {}", s.method, s.path))
            .collect()
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.world().seen.clone()
    }

    /// Put an instance there as if made elsewhere.
    pub fn add_instance(&self, name: &str, region: &str) -> String {
        let mut w = self.world();
        let id = w.id("instance");
        let o = w.instance(&id, name, region, "b2-7", "Ubuntu 24.04", None, true);
        w.instances.insert(id.clone(), o);
        id
    }

    /// The environment that points the provider at this server.
    pub fn env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("OVH_ENDPOINT", self.endpoint.clone()),
            ("OVH_APPLICATION_KEY", APPLICATION_KEY.into()),
            ("OVH_APPLICATION_SECRET", APPLICATION_SECRET.into()),
            ("OVH_CONSUMER_KEY", CONSUMER_KEY.into()),
            ("DFORM_OVH_POLL_MS", "1".into()),
            ("DFORM_OVH_RESOLVER", self.resolver.clone()),
        ]
    }
}

impl World {
    fn fail_next(&mut self, call: &str) -> Option<u16> {
        let (_, left) = self
            .failing
            .iter_mut()
            .find(|(c, left)| call.starts_with(c.as_str()) && !left.is_empty())?;
        Some(left.remove(0))
    }

    fn id(&mut self, what: &str) -> String {
        self.next += 1;
        format!("{what}-{:04}", self.next)
    }

    #[allow(clippy::too_many_arguments)]
    fn instance(
        &self,
        id: &str,
        name: &str,
        region: &str,
        flavor_name: &str,
        image_name: &str,
        key: Option<&str>,
        active: bool,
    ) -> Json {
        let ips = if active {
            ips(self.instances.len() + 10)
        } else {
            json!([])
        };
        json!({
            "id": id, "name": name, "region": region,
            "flavorId": format!("flavor-{flavor_name}-{region}"),
            "imageId": format!("image-{}-{region}", image_name.to_lowercase().replace(' ', "-")),
            "sshKeyId": key, "status": if active { "ACTIVE" } else { "BUILD" },
            "ipAddresses": ips, "created": "2026-10-06T00:00:00Z",
            "monthlyBilling": null, "planCode": format!("{flavor_name}.consumption"),
            "operationIds": [], "flavor": null, "image": null, "sshKey": null,
        })
    }

    fn flavors(region: &str) -> Json {
        json!([
            flavor(region, "b2-7", 2, 7000, 50),
            flavor(region, "d2-2", 1, 2000, 25)
        ])
    }

    fn images(region: &str) -> Json {
        json!([image(region, "Ubuntu 24.04"), image(region, "Debian 13")])
    }

    /// One call: its status and answer.
    fn answer(
        &mut self,
        method: &str,
        path: &str,
        query: &BTreeMap<String, String>,
        body: &Json,
    ) -> (u16, Json) {
        let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
        let not_found = |what: &str| (404, json!({"message": format!("{what} does not exist")}));
        let q = |k: &str| query.get(k).map(String::as_str);
        match (method, segs.as_slice()) {
            ("GET", ["cloud", "project"]) => (200, json!([PROJECT])),
            (_, ["domain", "zone", z, ..])
                if self.zones.as_ref().is_some_and(|zs| !zs.contains(*z)) =>
            {
                not_found("This service")
            }
            ("GET", ["domain", "zone", z]) => (200, json!({"name": z, "dnssecSupported": true})),
            (_, ["cloud", "project", p, ..]) if *p != PROJECT => not_found("This service"),
            ("GET", ["cloud", "project", _]) => (
                200,
                json!({"project_id": PROJECT, "description": DESCRIPTION,
                       "projectName": DESCRIPTION, "status": "ok"}),
            ),
            ("GET", ["cloud", "project", _, "region"]) => (200, json!(REGIONS)),
            ("GET", ["cloud", "project", _, "region", r]) if REGIONS.contains(r) => {
                (200, json!({"name": r, "status": "UP", "type": "region"}))
            }
            ("GET", ["cloud", "project", _, "flavor"]) => match q("region") {
                Some(r) => (200, Self::flavors(r)),
                None => (200, json!([])),
            },
            ("GET", ["cloud", "project", _, "flavor", id]) => REGIONS
                .iter()
                .flat_map(|r| Self::flavors(r).as_array().cloned().unwrap_or_default())
                .find(|f| f["id"] == *id)
                .map_or_else(|| not_found("Flavor"), |f| (200, f)),
            ("GET", ["cloud", "project", _, "image"]) => match q("region") {
                Some(r) => (200, Self::images(r)),
                None => (200, json!([])),
            },
            ("GET", ["cloud", "project", _, "image", id]) => REGIONS
                .iter()
                .flat_map(|r| Self::images(r).as_array().cloned().unwrap_or_default())
                .find(|i| i["id"] == *id)
                .map_or_else(|| not_found("Image"), |i| (200, i)),
            ("GET", ["cloud", "project", _, "instance"]) => {
                let all: Vec<Json> = self
                    .instances
                    .values()
                    .filter(|i| q("region").is_none_or(|r| i["region"] == r))
                    .cloned()
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "instance"]) => {
                let s = |k: &str| body.get(k).and_then(Json::as_str).unwrap_or_default();
                let region = s("region");
                let flavor = Self::flavors(region)
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|f| f["id"] == s("flavorId"))
                    .and_then(|f| f["name"].as_str().map(str::to_string));
                let image = Self::images(region)
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|i| i["id"] == s("imageId"))
                    .and_then(|i| i["name"].as_str().map(str::to_string));
                let (Some(flavor), Some(image)) = (flavor, image) else {
                    return (400, json!({"message": "Invalid flavorId or imageId"}));
                };
                let key = body.get("sshKeyId").and_then(Json::as_str);
                if key.is_some_and(|k| !self.keys.contains_key(k)) {
                    return (400, json!({"message": "Invalid sshKeyId"}));
                }
                let id = self.id("instance");
                let active = self.build_polls == 0;
                let o = self.instance(&id, s("name"), region, &flavor, &image, key, active);
                if !active {
                    self.building.insert(id.clone(), self.build_polls);
                }
                self.instances.insert(id, o.clone());
                (200, o)
            }
            ("GET", ["cloud", "project", _, "instance", id]) => {
                let id = id.to_string();
                if let Some(left) = self.building.get_mut(&id) {
                    *left = left.saturating_sub(1);
                    if *left == 0 {
                        self.building.remove(&id);
                        let n = self.instances.len() + 10;
                        if let Some(o) = self.instances.get_mut(&id) {
                            o["status"] = json!("ACTIVE");
                            o["ipAddresses"] = ips(n);
                        }
                    }
                }
                self.instances
                    .get(&id)
                    .cloned()
                    .map_or_else(|| not_found("Instance"), |o| (200, o))
            }
            ("PUT", ["cloud", "project", _, "instance", id]) => match self.instances.get_mut(*id) {
                Some(o) => {
                    o["name"] = body["instanceName"].clone();
                    (200, Json::Null)
                }
                None => not_found("Instance"),
            },
            ("DELETE", ["cloud", "project", _, "instance", id]) => {
                match self.instances.remove(*id) {
                    Some(_) => (200, Json::Null),
                    None => not_found("Instance"),
                }
            }
            ("GET", ["cloud", "project", _, "sshkey"]) => {
                (200, json!(self.keys.values().cloned().collect::<Vec<_>>()))
            }
            ("POST", ["cloud", "project", _, "sshkey"]) => {
                let id = self.id("key");
                let o = json!({"id": id, "name": body["name"], "publicKey": body["publicKey"],
                               "fingerPrint": "00:11", "regions": REGIONS});
                self.keys.insert(id, o.clone());
                (200, o)
            }
            ("GET", ["cloud", "project", _, "sshkey", id]) => self
                .keys
                .get(*id)
                .cloned()
                .map_or_else(|| not_found("SSH key"), |o| (200, o)),
            ("DELETE", ["cloud", "project", _, "sshkey", id]) => match self.keys.remove(*id) {
                Some(_) => (200, Json::Null),
                None => not_found("SSH key"),
            },
            ("GET", ["domain", "zone", z, "record"]) => {
                let ids: Vec<i64> = self
                    .records
                    .iter()
                    .filter(|(_, r)| r["zone"] == *z)
                    .filter(|(_, r)| q("fieldType").is_none_or(|t| r["fieldType"] == t))
                    .filter(|(_, r)| q("subDomain").is_none_or(|s| r["subDomain"] == s))
                    .map(|(id, _)| *id)
                    .collect();
                (200, json!(ids))
            }
            ("POST", ["domain", "zone", z, "record"]) => {
                self.next += 1;
                let id = 5_000_000_000 + self.next as i64;
                let o = json!({"id": id, "zone": z, "subDomain": body["subDomain"],
                               "fieldType": body["fieldType"], "target": body["target"],
                               "ttl": body.get("ttl").cloned().unwrap_or(json!(0))});
                self.records.insert(id, o.clone());
                (200, o)
            }
            ("POST", ["domain", "zone", _, "refresh"]) => (200, Json::Null),
            (m, ["domain", "zone", z, "record", id]) => {
                let Ok(id) = id.parse::<i64>() else {
                    return not_found("Record");
                };
                match (m, self.records.get_mut(&id)) {
                    (_, None) => not_found("Record"),
                    (_, Some(r)) if r["zone"] != *z => not_found("Record"),
                    ("GET", Some(r)) => (200, r.clone()),
                    ("PUT", Some(r)) => {
                        for (k, v) in body.as_object().into_iter().flatten() {
                            r[k] = v.clone();
                        }
                        (200, Json::Null)
                    }
                    ("DELETE", Some(_)) => {
                        self.records.remove(&id);
                        (200, Json::Null)
                    }
                    _ => (405, json!({"message": "method"})),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// A public IPv6 and IPv4 address, the `n`th.
fn ips(n: usize) -> Json {
    json!([
        {"ip": format!("2607:5300::{n}"), "type": "public", "version": 6,
         "networkId": "ext", "gatewayIp": null},
        {"ip": format!("51.79.0.{n}"), "type": "public", "version": 4,
         "networkId": "ext", "gatewayIp": "51.79.0.1"}
    ])
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

fn serve(conn: TcpStream, world: &Mutex<World>, base: &str) {
    let mut reader = BufReader::new(match conn.try_clone() {
        Ok(c) => c,
        Err(_) => return,
    });
    let mut out = conn;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            return;
        }
        let mut parts = line.split_whitespace();
        let (Some(method), Some(target)) = (parts.next(), parts.next()) else {
            return;
        };
        let (method, target) = (method.to_string(), target.to_string());
        let mut headers = BTreeMap::new();
        loop {
            let mut h = String::new();
            if reader.read_line(&mut h).unwrap_or(0) == 0 {
                return;
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
            return;
        }
        let body = String::from_utf8_lossy(&body).into_owned();
        let (path, query) = match target.split_once('?') {
            Some((p, q)) => (p.to_string(), q.to_string()),
            None => (target.clone(), String::new()),
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
        let path = decode(&path);
        let latency = {
            let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
            w.answering += 1;
            w.most = w.most.max(w.answering);
            w.latency
        };
        std::thread::sleep(latency);
        let (status, answer) = {
            let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
            let json_body = serde_json::from_str(&body).unwrap_or(Json::Null);
            if path == "/auth/time" {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (200, json!(now))
            } else if let Some(why) = bad_signature(
                &headers,
                &method,
                &format!("{base}{}", target.strip_prefix("/1.0").unwrap_or(&target)),
                &body,
            ) {
                (403, json!({"message": why}))
            } else if let Some(s) = w.fail_next(&format!("{method} {path}")) {
                (s, json!({"message": "Service Unavailable"}))
            } else {
                w.seen.push(Seen {
                    method: method.clone(),
                    path: target.strip_prefix("/1.0").unwrap_or(&target).to_string(),
                    body: json_body.clone(),
                });
                w.answer(&method, &path, &query, &json_body)
            }
        };
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
fn resolve(sock: UdpSocket, world: &Mutex<World>) {
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
