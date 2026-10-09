//! A fake OVH API for tests, over HTTP/1.1 on 127.0.0.1: one project
//! (`PROJECT`, described as `DESCRIPTION`), the regions `BHS5` and
//! `ca-east-tor` with three flavors and two images each, and the instance,
//! SSH key, DNS record, S3 container, user, volume and private network
//! endpoints the provider calls, answering as the API's models are
//! shaped. It checks every signed call's signature against
//! `APPLICATION_SECRET` and `CONSUMER_KEY`, and every other call's bearer
//! token against those it minted at `/auth/oauth2/token` for `CLIENT_ID`
//! and `CLIENT_SECRET`; `/auth/currentCredential` describes the consumer
//! key ([`Server::key_expires`], [`Server::key_rules`]). A new instance is BUILD, with
//! no address, for [`Server::build_polls`] reads, then ACTIVE; one resized
//! (`POST /resize`, to a flavor no smaller) is RESIZE as long; a volume,
//! a user and a private network change status the same way (`creating`
//! then `available`, `attaching` then `in-use`, `BUILDING` then
//! `ACTIVE`). The project is on a vRack until [`Server::no_vrack`]. Every DNS zone is the
//! account's until [`Server::hosting`] names them, each served by
//! [`NAMESERVERS`]; beside it, a DNS
//! resolver over UDP answers the NS records [`Server::delegated`] gives.

mod http;
mod instance;
mod network;
mod record;
mod ssh_key;
mod storage;
mod user;
mod volume;

use http::{resolve, serve};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::{Arc, Mutex};
use storage::container;
use user::role;
use volume::volume;

pub const APPLICATION_KEY: &str = "fake-application-key";
pub const APPLICATION_SECRET: &str = "fake-application-secret";
pub const CONSUMER_KEY: &str = "fake-consumer-key";
pub const CLIENT_ID: &str = "fake-client-id";
pub const CLIENT_SECRET: &str = "fake-client-secret";
pub const PROJECT: &str = "0123456789abcdef0123456789abcdef";
pub const DESCRIPTION: &str = "lab";
/// The nameservers the API says serve every zone it hosts.
pub const NAMESERVERS: [&str; 2] = ["dns200.anycast.me", "ns200.anycast.me"];

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
    /// Each instance's interfaces: the networks' OpenStack ids, the
    /// public one's included; none for the public network alone. One is
    /// attached (`POST /interface`) and detached (`DELETE
    /// /interface/{id}`, its id `if-` and the network's).
    nics: BTreeMap<String, Vec<String>>,
    /// S3 containers by region and name.
    pub containers: BTreeMap<(String, String), Json>,
    /// Users by id, and each one's S3 credentials (access, secret).
    pub users: BTreeMap<i64, Json>,
    pub s3: BTreeMap<i64, Vec<(String, String)>>,
    pub volumes: BTreeMap<String, Json>,
    pub networks: BTreeMap<String, Json>,
    /// Subnets by their network's id.
    pub subnets: BTreeMap<String, Vec<Json>>,
    /// The project has no vRack (`no_vrack`).
    no_vrack: bool,
    /// Objects whose status changes after reads: `kind/id` to the reads
    /// left and the fields it then has (`null`: it is gone).
    settling: BTreeMap<String, (u32, Json)>,
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
    /// Its clock's offset from the local one, in seconds (`skew`): a
    /// signed call stamped more than [`TIME_WINDOW`] away from it is
    /// refused, as the real API refuses a stale timestamp.
    skew: i64,
    /// How many times `/auth/time` was asked.
    times_asked: usize,
    /// The bearer tokens minted and still good, and how many were minted.
    tokens: BTreeSet<String>,
    minted: usize,
    /// How long a minted token is good for, in seconds (`expires_in`).
    token_life: u64,
    /// The consumer key's expiry as the API gives it (`None`: never),
    /// its rights (`None`: every method on `/*`), and whether it is
    /// revoked.
    key_expires: Option<String>,
    key_rules: Option<Vec<(String, String)>>,
    key_revoked: bool,
    /// How many times `/auth/currentCredential` was asked.
    key_asked: usize,
}

/// How far a signed call's timestamp may be from the fake's clock.
const TIME_WINDOW: i64 = 30;

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

/// The first address of `cidr` plus `n`, IPv4.
fn host_of(cidr: &str, n: u32) -> Option<String> {
    let (addr, _) = cidr.split_once('/').unwrap_or((cidr, ""));
    let a: std::net::Ipv4Addr = addr.parse().ok()?;
    Some(std::net::Ipv4Addr::from(u32::from(a) + n).to_string())
}

impl Server {
    pub fn start() -> Server {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind the fake OVH API");
        let endpoint = format!("http://{}/1.0", listener.local_addr().expect("its address"));
        let world = Arc::new(Mutex::new(World {
            build_polls: 1,
            next: 1,
            token_life: 3600,
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

    /// Its clock runs `secs` ahead of the local one (behind, negative).
    pub fn skew(&self, secs: i64) {
        self.world().skew = secs;
    }

    /// How many times its clock was asked (`GET /auth/time`), which
    /// `calls` does not list.
    pub fn times_asked(&self) -> usize {
        self.world().times_asked
    }

    /// The most calls it has answered at once.
    pub fn most_at_once(&self) -> usize {
        self.world().most
    }

    /// The connections it has accepted.
    pub fn connections(&self) -> usize {
        self.world().connections
    }

    /// How many bearer tokens it has minted.
    pub fn tokens_minted(&self) -> usize {
        self.world().minted
    }

    /// A bearer token good for its calls, as one minted elsewhere (the
    /// OVH console, `ovhcloud` login) is: not counted as minted.
    pub fn access_token(&self) -> String {
        let token = "fake-access-token".to_string();
        self.world().tokens.insert(token.clone());
        token
    }

    /// The tokens minted so far are no longer good: a call with one is
    /// refused with a 401.
    pub fn revoke_tokens(&self) {
        self.world().tokens.clear();
    }

    /// A token it mints is good for `secs`, as its `expires_in` says.
    pub fn token_life(&self, secs: u64) {
        self.world().token_life = secs;
    }

    /// The consumer key expires at `when` (as the API gives it,
    /// `2026-10-09T08:00:00+02:00`), or never.
    pub fn key_expires(&self, when: Option<&str>) {
        self.world().key_expires = when.map(str::to_string);
    }

    /// The consumer key's rights: method and path pattern.
    pub fn key_rules(&self, rules: &[(&str, &str)]) {
        self.world().key_rules = Some(
            rules
                .iter()
                .map(|(m, p)| (m.to_string(), p.to_string()))
                .collect(),
        );
    }

    /// The consumer key expired or was revoked: a signed call is refused
    /// as the API refuses it.
    pub fn revoke_key(&self) {
        self.world().key_revoked = true;
    }

    /// How many times `/auth/currentCredential` was asked, which `calls`
    /// does not list.
    pub fn key_asked(&self) -> usize {
        self.world().key_asked
    }

    /// How many reads a new instance stays BUILD for.
    pub fn build_polls(&self, n: u32) {
        self.world().build_polls = n;
    }

    /// Every instance still BUILD is ACTIVE at its next read: a test
    /// moves the world at a point it chose, not after so many reads.
    pub fn finish_builds(&self) {
        self.world()
            .building
            .values_mut()
            .for_each(|left| *left = 1);
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

    pub fn containers(&self) -> Vec<Json> {
        self.world().containers.values().cloned().collect()
    }

    pub fn users(&self) -> Vec<Json> {
        self.world().users.values().cloned().collect()
    }

    /// A user's S3 credentials, access and secret.
    pub fn s3_credentials(&self, user: i64) -> Vec<(String, String)> {
        self.world().s3.get(&user).cloned().unwrap_or_default()
    }

    pub fn volumes(&self) -> Vec<Json> {
        self.world().volumes.values().cloned().collect()
    }

    pub fn networks(&self) -> Vec<Json> {
        self.world().networks.values().cloned().collect()
    }

    pub fn subnets(&self) -> Vec<Json> {
        self.world().subnets.values().flatten().cloned().collect()
    }

    /// The project is not on a vRack: a private network cannot be made.
    pub fn no_vrack(&self) {
        self.world().no_vrack = true;
    }

    /// Put a volume there as if made elsewhere.
    pub fn add_volume(&self, name: &str, region: &str, gib: i64) -> String {
        let mut w = self.world();
        let id = w.id("volume");
        let o = volume(&id, name, region, gib, "classic", "", false);
        w.volumes.insert(id.clone(), o);
        id
    }

    /// Put a user there as if made elsewhere, ready, with one S3
    /// credential: its id.
    pub fn add_user(&self, description: &str) -> i64 {
        let mut w = self.world();
        w.next += 1;
        let id = 100_000 + w.next as i64;
        w.users.insert(
            id,
            json!({"id": id, "username": format!("user-{id:x}"), "description": description,
                   "status": "ok", "creationDate": "2026-10-07T00:00:00Z",
                   "openstackId": format!("os-{id}"), "roles": [role("objectstore_operator")]}),
        );
        w.s3.insert(id, vec![(format!("AK{id}"), format!("sk-{id}"))]);
        id
    }

    /// Put a DNS record there as if made elsewhere: its id.
    pub fn add_record(&self, zone: &str, subdomain: &str, typ: &str, target: &str) -> i64 {
        let mut w = self.world();
        w.next += 1;
        let id = 5_000_000_000 + w.next as i64;
        w.records.insert(
            id,
            json!({"id": id, "zone": zone, "subDomain": subdomain, "fieldType": typ,
                   "target": target, "ttl": 0}),
        );
        id
    }

    /// Put an S3 container there as if made elsewhere.
    pub fn add_container(&self, region: &str, name: &str) {
        let mut w = self.world();
        let o = container(region, name, None, "disabled");
        w.containers
            .insert((region.to_string(), name.to_string()), o);
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

    /// The environment that points the provider at this server with an
    /// access token ([`Server::access_token`]).
    pub fn token_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("OVH_ENDPOINT", self.endpoint.clone()),
            ("OVH_ACCESS_TOKEN", self.access_token()),
            ("DFORM_OVH_POLL_MS", "1".into()),
            ("DFORM_OVH_RESOLVER", self.resolver.clone()),
        ]
    }

    /// The environment that points the provider at this server with a
    /// service account's client credentials.
    pub fn oauth_env(&self) -> Vec<(&'static str, String)> {
        vec![
            ("OVH_ENDPOINT", self.endpoint.clone()),
            ("OVH_CLIENT_ID", CLIENT_ID.into()),
            ("OVH_CLIENT_SECRET", CLIENT_SECRET.into()),
            ("DFORM_OVH_POLL_MS", "1".into()),
            ("DFORM_OVH_RESOLVER", self.resolver.clone()),
        ]
    }
}

impl World {
    /// An authenticated call: a failure `fail` asked for, the key's
    /// description, or the answer, the call kept in `seen`.
    fn answer_seen(
        &mut self,
        method: &str,
        path: &str,
        target: &str,
        query: &BTreeMap<String, String>,
        body: Json,
    ) -> (u16, Json) {
        if let Some(s) = self.fail_next(&format!("{method} {path}")) {
            return (s, json!({"message": "Service Unavailable"}));
        }
        if (method, path) == ("GET", "/auth/currentCredential") {
            self.key_asked += 1;
            let rules = match &self.key_rules {
                Some(r) => r
                    .iter()
                    .map(|(m, p)| json!({"method": m, "path": p}))
                    .collect(),
                None => ["GET", "POST", "PUT", "DELETE"]
                    .map(|m| json!({"method": m, "path": "/*"}))
                    .to_vec(),
            };
            return (
                200,
                json!({"credentialId": 1, "applicationId": 1, "status": "validated",
                       "creation": "2026-10-01T00:00:00Z", "expiration": self.key_expires,
                       "lastUse": null, "ovhSupport": false, "allowedIPs": null,
                       "rules": rules}),
            );
        }
        self.seen.push(Seen {
            method: method.to_string(),
            path: target.strip_prefix("/1.0").unwrap_or(target).to_string(),
            body: body.clone(),
        });
        self.answer(method, path, query, &body)
    }

    /// `POST /auth/oauth2/token`: a bearer token for the client id and
    /// secret given as Basic auth, with the client-credentials grant.
    fn mint(&mut self, headers: &BTreeMap<String, String>, body: &str) -> (u16, Json) {
        use base64::Engine;
        let given = headers
            .get("authorization")
            .and_then(|a| a.strip_prefix("Basic "))
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok());
        if given.as_deref() != Some(format!("{CLIENT_ID}:{CLIENT_SECRET}").as_bytes()) {
            return (
                401,
                json!({"error": "invalid_client",
                       "error_description": "client authentication failed"}),
            );
        }
        let form: BTreeSet<&str> = body.split('&').collect();
        if !form.contains("grant_type=client_credentials") || !form.contains("scope=all") {
            return (400, json!({"error": "invalid_request"}));
        }
        self.minted += 1;
        let token = format!("fake-token-{}", self.minted);
        self.tokens.insert(token.clone());
        (
            200,
            json!({"access_token": token, "token_type": "Bearer",
                   "expires_in": self.token_life, "scope": "all"}),
        )
    }

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

    /// A status change `kind/id` makes after `build_polls` reads: at once
    /// when it is 0.
    fn settle(&mut self, what: String, then: Json) {
        if self.build_polls == 0 {
            self.settled(&what, then);
        } else {
            self.settling.insert(what, (self.build_polls, then));
        }
    }

    /// A read of `kind/id`: one read nearer its next status.
    fn read_settling(&mut self, what: &str) {
        let Some((left, _)) = self.settling.get_mut(what) else {
            return;
        };
        *left = left.saturating_sub(1);
        if *left == 0
            && let Some((_, then)) = self.settling.remove(what)
        {
            self.settled(what, then);
        }
    }

    fn settled(&mut self, what: &str, then: Json) {
        let (kind, id) = what.split_once('/').unwrap_or_default();
        let slot = match kind {
            "instance" => self.instances.get_mut(id),
            "volume" => self.volumes.get_mut(id),
            "network" => self.networks.get_mut(id),
            "user" => id.parse().ok().and_then(|i: i64| self.users.get_mut(&i)),
            _ => None,
        };
        let Some(o) = slot else { return };
        if then.is_null() {
            match kind {
                "volume" => self.volumes.remove(id),
                "network" => self.networks.remove(id),
                _ => None,
            };
            return;
        }
        for (k, v) in then.as_object().into_iter().flatten() {
            o[k] = v.clone();
        }
    }

    fn flavors(region: &str) -> Json {
        json!([
            flavor(region, "b2-7", 2, 7000, 50),
            flavor(region, "b2-15", 4, 15000, 100),
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
        let q = |k: &str| query.get(k).map(String::as_str);
        match (method, segs.as_slice()) {
            ("GET", ["cloud", "project"]) => (200, json!([PROJECT])),
            (_, ["domain", "zone", z, ..])
                if self.zones.as_ref().is_some_and(|zs| !zs.contains(*z)) =>
            {
                not_found("This service")
            }
            ("GET", ["domain", "zone", z]) => (
                200,
                json!({"name": z, "dnssecSupported": true, "nameServers": NAMESERVERS}),
            ),
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
            (_, ["cloud", "project", _, "instance", ..]) => {
                self.instance_api(method, path, &segs, query, body)
            }
            (_, ["cloud", "project", _, "sshkey", ..]) => {
                self.ssh_key_api(method, path, &segs, body)
            }
            (_, ["domain", "zone", _, "record", ..])
            | ("POST", ["domain", "zone", _, "refresh"]) => {
                self.record_api(method, path, &segs, query, body)
            }
            (_, ["cloud", "project", _, "vrack"]) | (_, ["cloud", "project", _, "network", ..]) => {
                self.network_api(method, path, &segs, body)
            }
            (_, ["cloud", "project", _, "role"]) | (_, ["cloud", "project", _, "user", ..]) => {
                self.user_api(method, path, &segs, body)
            }
            (_, ["cloud", "project", _, "region", _, "storage", ..]) => {
                self.storage_api(method, path, &segs, body)
            }
            (_, ["cloud", "project", _, "volume", ..]) => {
                self.volume_api(method, path, &segs, query, body)
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// The API's 404 for `what`.
fn not_found(what: &str) -> (u16, Json) {
    (404, json!({"message": format!("{what} does not exist")}))
}
