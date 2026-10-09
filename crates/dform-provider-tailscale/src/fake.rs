//! A fake Tailscale API for tests, over HTTP/1.1 on 127.0.0.1, one tailnet:
//! an OAuth client's token (`POST /api/v2/oauth/token`) or the API key
//! [`API_KEY`]; the policy file as HuJSON with an ETag and `If-Match` (a
//! write over a stale ETag is a 412); auth keys, the key answered once by
//! the create and never again, a key an OAuth client makes refused without
//! a tag; the DNS settings' four calls; devices, which [`Server::join`]
//! adds as a node joining with a key does, with the same hostname as often
//! as it is asked; users. [`Server::console_edit`] writes the policy file
//! as the admin console does.

use serde_json::{Value as Json, json};
use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

pub const CLIENT_ID: &str = "kFakeClient1CNTRL";
pub const CLIENT_SECRET: &str = "tskey-client-kFakeClient1CNTRL-fakesecret";
pub const API_KEY: &str = "tskey-api-kFakeApi1CNTRL-fakeapikey";

/// When the fake's clock says it is: every key is made then.
pub const NOW: i64 = 1_791_450_000; // 2026-10-08T09:00:00Z

/// The policy file a tailnet starts with, as the console writes it.
pub const DEFAULT_POLICY: &str = r#"// Example/default ACLs for unrestricted connections.
{
	// Define access control lists for users, groups, autogroups, tags,
	// Tailscale IP addresses, and subnet ranges.
	"acls": [
		// Allow all connections.
		{"action": "accept", "src": ["*"], "dst": ["*:*"]},
	],
	"ssh": [
		{
			"action": "check",
			"src":    ["autogroup:member"],
			"dst":    ["autogroup:self"],
			"users":  ["autogroup:nonroot", "root"],
		},
	],
}
"#;

/// One request the server answered: method, path with its query, its
/// `If-Match`, and its body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub if_match: Option<String>,
    pub body: String,
}

struct Key {
    /// As the API lists it, without the key.
    meta: Json,
    key: String,
    revoked: bool,
    uses: usize,
}

struct World {
    tailnet: String,
    tokens: BTreeMap<String, bool>,
    minted: usize,
    policy: String,
    etag: u64,
    keys: BTreeMap<String, Key>,
    devices: Vec<Json>,
    nameservers: Vec<String>,
    magic_dns: bool,
    search_paths: Vec<String>,
    split: BTreeMap<String, Vec<String>>,
    users: Vec<Json>,
    seen: Vec<Seen>,
    next: u64,
}

pub struct Server {
    pub port: u16,
    world: Arc<Mutex<World>>,
}

/// `2026-10-08T09:00:00Z` of seconds since the epoch.
pub fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86400), secs.rem_euclid(86400));
    // The civil date of a day count (Howard Hinnant's algorithm).
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem / 60 % 60,
        rem % 60
    )
}

impl Server {
    /// The API of the tailnet `tailnet`, its policy file the default, its
    /// DNS settings the defaults, one owner and no device.
    pub fn start(tailnet: &str) -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake Tailscale API");
        let port = listener.local_addr().unwrap().port();
        let world = Arc::new(Mutex::new(World {
            tailnet: tailnet.to_string(),
            tokens: BTreeMap::from([(API_KEY.to_string(), false)]),
            minted: 0,
            policy: DEFAULT_POLICY.to_string(),
            etag: 1,
            keys: BTreeMap::new(),
            devices: Vec::new(),
            nameservers: Vec::new(),
            magic_dns: true,
            search_paths: Vec::new(),
            split: BTreeMap::new(),
            users: vec![
                json!({"id": "u1", "loginName": "simon@example.com", "role": "owner"}),
                json!({"id": "u2", "loginName": "alice@example.com", "role": "member"}),
            ],
            seen: Vec::new(),
            next: 0,
        }));
        let w = world.clone();
        std::thread::spawn(move || {
            for conn in listener.incoming().flatten() {
                let w = w.clone();
                std::thread::spawn(move || serve(conn, &w));
            }
        });
        Server { port, world }
    }

    /// `http://127.0.0.1:PORT`, the provider's `base_url`.
    pub fn address(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn world(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The policy file as it is written.
    pub fn policy(&self) -> String {
        self.world().policy.clone()
    }

    /// Write the policy file as the admin console does: a new ETag.
    pub fn console_edit(&self, policy: &str) {
        let mut w = self.world();
        w.policy = policy.to_string();
        w.etag += 1;
    }

    /// The DNS settings, as the four calls answer them.
    pub fn dns(&self) -> Json {
        let w = self.world();
        json!({
            "nameservers": w.nameservers,
            "magicDNS": w.magic_dns,
            "searchPaths": w.search_paths,
            "split": w.split,
        })
    }

    /// Every key made, as listed (`revoked` set on a revoked one).
    pub fn keys(&self) -> Vec<Json> {
        self.world()
            .keys
            .values()
            .map(|k| {
                let mut m = k.meta.clone();
                m["revoked"] = json!(k.revoked);
                m
            })
            .collect()
    }

    /// The values of every key the API answered: what must never reach
    /// dform's files or output.
    pub fn issued(&self) -> Vec<String> {
        self.world().keys.values().map(|k| k.key.clone()).collect()
    }

    /// A node joins with the key `key` as `hostname`: a device, tagged and
    /// authorized as the key says. Its id; `None` when the key is refused
    /// (revoked, used once already, unknown).
    pub fn join(&self, key: &str, hostname: &str) -> Option<String> {
        let mut w = self.world();
        let k = w.keys.values_mut().find(|k| k.key == key)?;
        let create = k.meta["capabilities"]["devices"]["create"].clone();
        if k.revoked || (k.uses > 0 && create["reusable"] != json!(true)) {
            return None;
        }
        k.uses += 1;
        Some(w.add_device(
            hostname,
            create["tags"].clone(),
            create["preauthorized"] == json!(true),
        ))
    }

    /// A device `hostname` joined by a user: untagged, authorized.
    pub fn add_device(&self, hostname: &str) -> String {
        self.world().add_device(hostname, json!([]), true)
    }

    /// The device `id` as the API answers it, if it is there.
    pub fn device(&self, id: &str) -> Option<Json> {
        self.world()
            .devices
            .iter()
            .find(|d| d["nodeId"] == json!(id))
            .cloned()
    }

    /// The device `id` advertises `routes`.
    pub fn advertise(&self, id: &str, routes: &[&str]) {
        if let Some(d) = self.world().device_mut(id) {
            d["advertisedRoutes"] = json!(routes);
        }
    }

    /// The device `id` loses its connection to the control plane.
    pub fn disconnect(&self, id: &str) {
        if let Some(d) = self.world().device_mut(id) {
            d["connectedToControl"] = json!(false);
        }
    }

    pub fn seen(&self) -> Vec<Seen> {
        self.world().seen.clone()
    }
}

impl World {
    fn add_device(&mut self, hostname: &str, tags: Json, authorized: bool) -> String {
        self.next += 1;
        let n = self.next;
        let id = format!("n{n}FakeDevCNTRL");
        let suffix = match n {
            1 => String::new(),
            n => format!("-{}", n - 1),
        };
        self.devices.push(json!({
            "id": format!("{}", 1000 + n),
            "nodeId": id,
            "hostname": hostname,
            "name": format!("{hostname}{suffix}.tail1234.ts.net"),
            "addresses": [format!("100.64.0.{n}"), format!("fd7a:115c:a1e0::{n}")],
            "os": "linux",
            "authorized": authorized,
            "tags": tags,
            "lastSeen": rfc3339(NOW),
            "connectedToControl": true,
            "advertisedRoutes": [],
            "enabledRoutes": [],
        }));
        id
    }

    fn device_mut(&mut self, id: &str) -> Option<&mut Json> {
        self.devices
            .iter_mut()
            .find(|d| d["nodeId"] == json!(id) || d["id"] == json!(id))
    }
}

/// An answer: status, content type and body.
type Reply = (u16, &'static str, String, Option<String>);

fn reply(status: u16, body: Json) -> Reply {
    (status, "application/json", body.to_string(), None)
}

fn refuse(status: u16, message: &str) -> Reply {
    reply(status, json!({"message": message}))
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
        let body = String::from_utf8_lossy(&body).to_string();
        let (status, kind, text, etag) = {
            let mut w = world.lock().unwrap_or_else(|e| e.into_inner());
            w.seen.push(Seen {
                method: method.clone(),
                path: target.clone(),
                if_match: headers.get("if-match").cloned(),
                body: body.clone(),
            });
            answer(&mut w, &method, &target, &headers, &body)
        };
        let reason = match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            412 => "Precondition Failed",
            _ => "Not Found",
        };
        let etag = etag.map(|e| format!("etag: {e}\r\n")).unwrap_or_default();
        if write!(
            conn,
            "HTTP/1.1 {status} {reason}\r\ncontent-type: {kind}\r\n{etag}content-length: {}\r\n\r\n{text}",
            text.len()
        )
        .is_err()
        {
            return;
        }
    }
}

/// `a=b&c=d` decoded (`+` and `%XX`).
fn form(body: &str) -> BTreeMap<String, String> {
    let decode = |s: &str| {
        let b = s.replace('+', " ").into_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while i < b.len() {
            match (b[i], b.get(i + 1..i + 3)) {
                (b'%', Some(h)) => {
                    let h = std::str::from_utf8(h).unwrap_or("00");
                    out.push(u8::from_str_radix(h, 16).unwrap_or(0));
                    i += 3;
                }
                (c, _) => {
                    out.push(c);
                    i += 1;
                }
            }
        }
        String::from_utf8_lossy(&out).to_string()
    };
    body.split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect()
}

fn answer(
    w: &mut World,
    method: &str,
    target: &str,
    headers: &BTreeMap<String, String>,
    body: &str,
) -> Reply {
    let (path, _query) = target.split_once('?').unwrap_or((target, ""));
    let Some(path) = path.strip_prefix("/api/v2") else {
        return refuse(404, "not found");
    };
    if method == "POST" && path == "/oauth/token" {
        let f = form(body);
        if f.get("client_id").map(String::as_str) != Some(CLIENT_ID)
            || f.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
        {
            return reply(401, json!({"error": "invalid_client"}));
        }
        w.minted += 1;
        let t = format!("fake-oauth-token-{}", w.minted);
        w.tokens.insert(t.clone(), true);
        return reply(
            200,
            json!({"access_token": t, "token_type": "Bearer", "expires_in": 3600, "scope": "all"}),
        );
    }
    let token = headers
        .get("authorization")
        .and_then(|a| a.strip_prefix("Bearer "))
        .unwrap_or("-");
    let Some(&oauth) = w.tokens.get(token) else {
        return refuse(401, "API token invalid");
    };
    let json_body = || serde_json::from_str::<Json>(body).unwrap_or(Json::Null);
    let parts: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match parts.as_slice() {
        ["tailnet", t, rest @ ..] => {
            if *t != w.tailnet && *t != "-" {
                return refuse(404, "tailnet not found");
            }
            tailnet(w, method, rest, headers, body, json_body(), oauth)
        }
        ["device", id] => match method {
            "GET" => match w.device_mut(id) {
                Some(d) => reply(200, d.clone()),
                None => refuse(404, "device not found"),
            },
            "DELETE" => {
                let before = w.devices.len();
                w.devices
                    .retain(|d| d["nodeId"] != json!(id) && d["id"] != json!(id));
                match before == w.devices.len() {
                    true => refuse(404, "device not found"),
                    false => reply(200, Json::Null),
                }
            }
            _ => refuse(405, "method not allowed"),
        },
        ["device", id, what] => {
            let Some(d) = w.device_mut(id) else {
                return refuse(404, "device not found");
            };
            let b = json_body();
            match (method, *what) {
                ("GET", "routes") => reply(
                    200,
                    json!({"advertisedRoutes": d["advertisedRoutes"], "enabledRoutes": d["enabledRoutes"]}),
                ),
                ("POST", "routes") => {
                    let advertised = strings(&d["advertisedRoutes"]);
                    let enabled: Vec<String> = strings(&b["routes"])
                        .into_iter()
                        .filter(|r| advertised.contains(r))
                        .collect();
                    d["enabledRoutes"] = json!(enabled);
                    reply(
                        200,
                        json!({"advertisedRoutes": advertised, "enabledRoutes": enabled}),
                    )
                }
                ("POST", "tags") => {
                    let tags = strings(&b["tags"]);
                    if tags.iter().any(|t| !t.starts_with("tag:")) {
                        return refuse(400, "tags must start with tag:");
                    }
                    d["tags"] = json!(tags);
                    reply(200, Json::Null)
                }
                ("POST", "authorized") => {
                    d["authorized"] = b["authorized"].clone();
                    reply(200, Json::Null)
                }
                ("POST", "name") => {
                    let n = b["name"].as_str().unwrap_or_default();
                    d["name"] = json!(format!("{n}.tail1234.ts.net"));
                    reply(200, Json::Null)
                }
                _ => refuse(404, "not found"),
            }
        }
        _ => refuse(404, "not found"),
    }
}

fn tailnet(
    w: &mut World,
    method: &str,
    rest: &[&str],
    headers: &BTreeMap<String, String>,
    body: &str,
    b: Json,
    oauth: bool,
) -> Reply {
    match (method, rest) {
        ("GET", ["acl"]) => {
            let etag = Some(format!("\"{}\"", w.etag));
            let hujson = headers
                .get("accept")
                .is_some_and(|a| a.contains("application/hujson"));
            match hujson {
                true => (200, "application/hujson", w.policy.clone(), etag),
                false => {
                    let j = crate::hujson::parse(&w.policy).unwrap_or(Json::Null);
                    (200, "application/json", j.to_string(), etag)
                }
            }
        }
        ("POST", ["acl"]) => {
            if let Some(m) = headers.get("if-match")
                && *m != format!("\"{}\"", w.etag)
            {
                return refuse(412, "precondition failed, invalid old hash");
            }
            if let Err(e) = crate::hujson::parse(body) {
                return refuse(400, &format!("parsing HuJSON: {e}"));
            }
            w.policy = body.to_string();
            w.etag += 1;
            (
                200,
                "application/json",
                crate::hujson::parse(body).unwrap_or(Json::Null).to_string(),
                Some(format!("\"{}\"", w.etag)),
            )
        }
        ("POST", ["keys"]) => {
            let create = &b["capabilities"]["devices"]["create"];
            let tags = strings(&create["tags"]);
            if oauth && tags.is_empty() {
                return refuse(
                    400,
                    "requested tags [] are invalid or not permitted: an OAuth client's key needs tags",
                );
            }
            let expiry = b["expirySeconds"].as_i64().unwrap_or(90 * 86400);
            if expiry > 90 * 86400 {
                return refuse(400, "expirySeconds is greater than 90 days");
            }
            w.next += 1;
            let id = format!("k{}FakeKeyCNTRL", w.next);
            let key = format!("tskey-auth-{id}-fakesecret{}", w.next * 7919);
            let meta = json!({
                "id": id,
                "description": b["description"],
                "created": rfc3339(NOW),
                "expires": rfc3339(NOW + expiry),
                "capabilities": {"devices": {"create": {
                    "reusable": create["reusable"].as_bool().unwrap_or(false),
                    "ephemeral": create["ephemeral"].as_bool().unwrap_or(false),
                    "preauthorized": create["preauthorized"].as_bool().unwrap_or(false),
                    "tags": tags,
                }}},
            });
            let mut answered = meta.clone();
            answered["key"] = json!(key);
            w.keys.insert(
                id,
                Key {
                    meta,
                    key,
                    revoked: false,
                    uses: 0,
                },
            );
            reply(200, answered)
        }
        ("GET", ["keys"]) => reply(
            200,
            json!({"keys": w.keys.values().filter(|k| !k.revoked).map(|k| json!({"id": k.meta["id"]})).collect::<Vec<_>>()}),
        ),
        ("GET", ["keys", id]) => match w.keys.get(*id) {
            Some(k) if !k.revoked => reply(200, k.meta.clone()),
            _ => refuse(404, "key not found"),
        },
        ("DELETE", ["keys", id]) => match w.keys.get_mut(*id) {
            Some(k) if !k.revoked => {
                k.revoked = true;
                reply(200, Json::Null)
            }
            _ => refuse(404, "key not found"),
        },
        ("GET", ["dns", "nameservers"]) => reply(200, json!({"dns": w.nameservers})),
        ("POST", ["dns", "nameservers"]) => {
            w.nameservers = strings(&b["dns"]);
            reply(200, json!({"dns": w.nameservers, "magicDNS": w.magic_dns}))
        }
        ("GET", ["dns", "preferences"]) => reply(200, json!({"magicDNS": w.magic_dns})),
        ("POST", ["dns", "preferences"]) => {
            w.magic_dns = b["magicDNS"].as_bool().unwrap_or(w.magic_dns);
            reply(200, json!({"magicDNS": w.magic_dns}))
        }
        ("GET", ["dns", "searchpaths"]) => reply(200, json!({"searchPaths": w.search_paths})),
        ("POST", ["dns", "searchpaths"]) => {
            w.search_paths = strings(&b["searchPaths"]);
            reply(200, json!({"searchPaths": w.search_paths}))
        }
        ("GET", ["dns", "split-dns"]) => reply(200, json!(w.split)),
        ("PUT", ["dns", "split-dns"]) => {
            w.split = b
                .as_object()
                .into_iter()
                .flatten()
                .map(|(d, ns)| (d.clone(), strings(ns)))
                .filter(|(_, ns)| !ns.is_empty())
                .collect();
            reply(200, json!(w.split))
        }
        ("GET", ["devices"]) => reply(200, json!({"devices": w.devices})),
        ("GET", ["users"]) => reply(200, json!({"users": w.users})),
        _ => refuse(404, "not found"),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_clock_reads_as_the_api_writes_it() {
        assert_eq!(super::rfc3339(super::NOW), "2026-10-08T09:00:00Z");
        assert_eq!(super::rfc3339(0), "1970-01-01T00:00:00Z");
    }
}
