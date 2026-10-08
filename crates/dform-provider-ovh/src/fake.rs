//! A fake OVH API for tests, over HTTP/1.1 on 127.0.0.1: one project
//! (`PROJECT`, described as `DESCRIPTION`), the regions `BHS5` and
//! `ca-east-tor` with two flavors and two images each, and the instance,
//! SSH key, DNS record, S3 container, user, volume and private network
//! endpoints the provider calls, answering as the API's models are
//! shaped. It checks every signed call's signature against
//! `APPLICATION_SECRET` and `CONSUMER_KEY`, and every other call's bearer
//! token against those it minted at `/auth/oauth2/token` for `CLIENT_ID`
//! and `CLIENT_SECRET`; `/auth/currentCredential` describes the consumer
//! key ([`Server::key_expires`], [`Server::key_rules`]). A new instance is BUILD, with
//! no address, for [`Server::build_polls`] reads, then ACTIVE; a volume,
//! a user and a private network change status the same way (`creating`
//! then `available`, `attaching` then `in-use`, `BUILDING` then
//! `ACTIVE`). The project is on a vRack until [`Server::no_vrack`]. Every DNS zone is the
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
pub const CLIENT_ID: &str = "fake-client-id";
pub const CLIENT_SECRET: &str = "fake-client-secret";
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
    /// Each instance's interfaces as it was made: the networks' OpenStack
    /// ids, the public one's included; none for the public network alone.
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

/// The vRack the project is on.
const VRACK: &str = "pn-1000123";

/// The OpenStack roles a user may have (`cloud.user.RoleEnum`).
const ROLES: [&str; 6] = [
    "administrator",
    "compute_operator",
    "network_operator",
    "objectstore_operator",
    "volume_operator",
    "infrastructure_supervisor",
];

/// A volume (`cloud.volume.Volume`), available.
fn volume(id: &str, name: &str, region: &str, gib: i64, typ: &str, desc: &str, boot: bool) -> Json {
    json!({"id": id, "name": name, "region": region, "size": gib, "type": typ,
           "description": desc, "status": "available", "attachedTo": [],
           "bootable": boot, "availabilityZone": "nova",
           "creationDate": "2026-10-07T00:00:00Z",
           "planCode": format!("volume.{typ}.consumption")})
}

/// An S3 container (`cloud.StorageContainer`), empty.
fn container(region: &str, name: &str, owner: Option<i64>, versioning: &str) -> Json {
    let host = format!("{name}.s3.{}.io.cloud.ovh.net", region.to_lowercase());
    json!({"name": name, "region": region, "ownerId": owner,
           "objectsCount": 0, "objectsSize": 0,
           "createdAt": "2026-10-07T00:00:00Z",
           "arn": format!("arn:aws:s3:::{name}"), "virtualHost": host,
           "versioning": {"status": versioning},
           "encryption": {"sseAlgorithm": "plaintext"}, "tags": {},
           "objectLock": {"status": "disabled"}, "objects": []})
}

/// A role (`cloud.role.Role`).
fn role(name: &str) -> Json {
    json!({"id": format!("role-{name}"), "name": name,
           "description": format!("{name} role"), "permissions": []})
}

/// The public network, in every region (`cloud.network.Network`).
fn public_network() -> Json {
    json!({"id": "ext-net", "name": "Ext-Net", "type": "public", "vlanId": null,
           "status": "ACTIVE",
           "regions": REGIONS.iter().map(|r| json!({"region": r, "status": "ACTIVE",
                                                    "openstackId": format!("ext-{r}")}))
                             .collect::<Vec<_>>()})
}

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
            self.addresses(id, self.instances.len() + 10)
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

    /// An instance's addresses, the `n`th: the public network's, and one
    /// on each private network it was made on, in its subnet's pool.
    fn addresses(&self, id: &str, n: usize) -> Json {
        let Some(nics) = self.nics.get(id) else {
            return ips(n);
        };
        let mut out = Vec::new();
        for nic in nics {
            if nic.starts_with("ext-") {
                out.extend(ips(n).as_array().cloned().unwrap_or_default());
                continue;
            }
            let pool = self
                .networks
                .values()
                .find(|net| {
                    net["regions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|r| r["openstackId"] == nic.as_str())
                })
                .and_then(|net| self.subnets.get(net["id"].as_str().unwrap_or_default()))
                .and_then(|subs| subs.first())
                .and_then(|sub| sub["ipPools"][0]["start"].as_str().map(str::to_string));
            let ip = pool
                .and_then(|start| host_of(&start, n as u32))
                .unwrap_or_else(|| format!("10.0.0.{n}"));
            out.push(json!({"ip": ip, "type": "private", "version": 4,
                            "networkId": nic, "gatewayIp": null}));
        }
        json!(out)
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
                // `networks`: the public network's id and the private ones',
                // in that region, as OpenStack names them.
                let nics: Option<Vec<String>> =
                    body.get("networks").and_then(Json::as_array).map(|ns| {
                        ns.iter()
                            .filter_map(|n| n["networkId"].as_str().map(str::to_string))
                            .collect()
                    });
                for nic in nics.iter().flatten() {
                    let public = *nic == format!("ext-{region}");
                    let private = self.networks.values().find(|net| {
                        net["regions"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|r| r["openstackId"] == nic.as_str() && r["region"] == region)
                    });
                    if !public && private.is_none() {
                        return (400, json!({"message": format!("Invalid networkId {nic}")}));
                    }
                    if let Some(net) = private
                        && self
                            .subnets
                            .get(net["id"].as_str().unwrap_or_default())
                            .is_none_or(|s| !s.iter().any(|s| s["ipPools"][0]["region"] == region))
                    {
                        return (
                            400,
                            json!({"message": format!("Network {nic} has no subnet in {region}")}),
                        );
                    }
                }
                let id = self.id("instance");
                if let Some(nics) = nics {
                    self.nics.insert(id.clone(), nics);
                }
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
                        let ips = self.addresses(&id, self.instances.len() + 10);
                        if let Some(o) = self.instances.get_mut(&id) {
                            o["status"] = json!("ACTIVE");
                            o["ipAddresses"] = ips;
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
                    Some(_) => {
                        // Its volumes are detached as it goes.
                        for v in self.volumes.values_mut() {
                            if v["attachedTo"]
                                .as_array()
                                .into_iter()
                                .flatten()
                                .any(|x| x == *id)
                            {
                                v["attachedTo"] = json!([]);
                                v["status"] = json!("available");
                            }
                        }
                        self.nics.remove(*id);
                        (200, Json::Null)
                    }
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
            // vRack and networks.
            ("GET", ["cloud", "project", _, "vrack"]) => match self.no_vrack {
                true => not_found("This vRack"),
                false => (
                    200,
                    json!({"id": VRACK, "name": "vrack", "description": ""}),
                ),
            },
            ("GET", ["cloud", "project", _, "network", "public"]) => {
                (200, json!([public_network()]))
            }
            ("GET", ["cloud", "project", _, "network", "private"]) => (
                200,
                json!(self.networks.values().cloned().collect::<Vec<_>>()),
            ),
            ("POST", ["cloud", "project", _, "network", "private"]) => {
                if self.no_vrack {
                    return (
                        400,
                        json!({"message": "Your project is not attached to a vRack",
                               "errorCode": "CLIENT_ERROR"}),
                    );
                }
                let vlan = body.get("vlanId").and_then(Json::as_i64).unwrap_or(0);
                if self.networks.values().any(|n| n["vlanId"] == vlan) {
                    return (
                        409,
                        json!({"message": format!("vlanId {vlan} is already used")}),
                    );
                }
                let regions: Vec<String> = match body.get("regions").and_then(Json::as_array) {
                    Some(rs) => rs
                        .iter()
                        .filter_map(|r| r.as_str().map(str::to_string))
                        .collect(),
                    None => REGIONS.iter().map(|r| r.to_string()).collect(),
                };
                if let Some(r) = regions.iter().find(|r| !REGIONS.contains(&r.as_str())) {
                    return (400, json!({"message": format!("Invalid region {r}")}));
                }
                let id = format!("{VRACK}_{vlan}");
                let region = |r: &String, status: &str| {
                    json!({"region": r, "status": status,
                           "openstackId": format!("net-{vlan}-{r}")})
                };
                let o = json!({"id": id, "name": body["name"], "vlanId": vlan,
                               "type": "private", "status": "BUILDING",
                               "regions": regions.iter().map(|r| region(r, "BUILDING"))
                                                 .collect::<Vec<_>>()});
                self.networks.insert(id.clone(), o.clone());
                let active: Vec<Json> = regions.iter().map(|r| region(r, "ACTIVE")).collect();
                self.settle(
                    format!("network/{id}"),
                    json!({"status": "ACTIVE", "regions": active}),
                );
                (200, o)
            }
            ("GET", ["cloud", "project", _, "network", "private", id]) => {
                self.read_settling(&format!("network/{id}"));
                self.networks
                    .get(*id)
                    .cloned()
                    .map_or_else(|| not_found("Network"), |o| (200, o))
            }
            ("PUT", ["cloud", "project", _, "network", "private", id]) => {
                match self.networks.get_mut(*id) {
                    Some(o) => {
                        o["name"] = body["name"].clone();
                        (200, Json::Null)
                    }
                    None => not_found("Network"),
                }
            }
            ("POST", ["cloud", "project", _, "network", "private", id, "region"]) => {
                let r = body["region"].as_str().unwrap_or_default().to_string();
                if !REGIONS.contains(&r.as_str()) {
                    return (400, json!({"message": format!("Invalid region {r}")}));
                }
                let Some(o) = self.networks.get_mut(*id) else {
                    return not_found("Network");
                };
                let vlan = o["vlanId"].as_i64().unwrap_or(0);
                if let Some(rs) = o["regions"].as_array_mut() {
                    rs.push(json!({"region": r, "status": "ACTIVE",
                                   "openstackId": format!("net-{vlan}-{r}")}));
                }
                (200, o.clone())
            }
            ("DELETE", ["cloud", "project", _, "network", "private", id]) => {
                if !self.networks.contains_key(*id) {
                    return not_found("Network");
                }
                if self.subnets.get(*id).is_some_and(|s| !s.is_empty()) {
                    return (400, json!({"message": "The network still has subnets"}));
                }
                if let Some(o) = self.networks.get_mut(*id) {
                    o["status"] = json!("DELETING");
                }
                self.settle(format!("network/{id}"), Json::Null);
                (200, Json::Null)
            }
            ("GET", ["cloud", "project", _, "network", "private", id, "subnet"]) => {
                match self.networks.contains_key(*id) {
                    true => (
                        200,
                        json!(self.subnets.get(*id).cloned().unwrap_or_default()),
                    ),
                    false => not_found("Network"),
                }
            }
            ("POST", ["cloud", "project", _, "network", "private", id, "subnet"]) => {
                let Some(net) = self.networks.get(*id) else {
                    return not_found("Network");
                };
                let region = body["region"].as_str().unwrap_or_default();
                let active = net["regions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["region"] == region && r["status"] == "ACTIVE");
                if !active {
                    return (
                        400,
                        json!({"message": format!("Network {id} is not active in region {region}")}),
                    );
                }
                // The pool's ends are required, as the API has them.
                if let Some(k) = ["start", "end"].into_iter().find(|k| !body[*k].is_string()) {
                    return (
                        400,
                        json!({"message": format!("[{k}] Property is mandatory")}),
                    );
                }
                let cidr = body["network"].as_str().unwrap_or_default();
                let no_gw = body
                    .get("noGateway")
                    .and_then(Json::as_bool)
                    .unwrap_or(false);
                let gw = if no_gw { None } else { host_of(cidr, 1) };
                let sid = self.id("subnet");
                let o = json!({"id": sid, "cidr": cidr,
                               "dhcpEnabled": body.get("dhcp").and_then(Json::as_bool).unwrap_or(false),
                               "gatewayIp": gw,
                               "ipPools": [{"dhcp": body["dhcp"], "start": body["start"],
                                            "end": body["end"], "network": cidr,
                                            "region": region}]});
                self.subnets
                    .entry(id.to_string())
                    .or_default()
                    .push(o.clone());
                (200, o)
            }
            (
                "DELETE",
                [
                    "cloud",
                    "project",
                    _,
                    "network",
                    "private",
                    id,
                    "subnet",
                    sid,
                ],
            ) => {
                let subs = self.subnets.entry(id.to_string()).or_default();
                match subs.iter().position(|o| o["id"] == *sid) {
                    Some(i) => {
                        subs.remove(i);
                        (200, Json::Null)
                    }
                    None => not_found("Subnet"),
                }
            }
            // Users and their S3 credentials.
            ("GET", ["cloud", "project", _, "role"]) => (
                200,
                json!({"roles": ROLES.iter().map(|r| role(r)).collect::<Vec<_>>(),
                       "services": []}),
            ),
            ("GET", ["cloud", "project", _, "user"]) => {
                let all: Vec<Json> = self
                    .users
                    .values()
                    .map(|u| {
                        let mut u = u.clone();
                        u.as_object_mut().map(|m| m.remove("password"));
                        u
                    })
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "user"]) => {
                let mut names: Vec<String> = body
                    .get("roles")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|r| r.as_str().map(str::to_string))
                    .collect();
                names.extend(body.get("role").and_then(Json::as_str).map(str::to_string));
                if let Some(r) = names.iter().find(|r| !ROLES.contains(&r.as_str())) {
                    return (400, json!({"message": format!("Invalid role {r}")}));
                }
                self.next += 1;
                let id = 100_000 + self.next as i64;
                let o = json!({"id": id, "username": format!("user-{id:x}"),
                               "description": body["description"], "status": "creating",
                               "creationDate": "2026-10-07T00:00:00Z",
                               "openstackId": format!("os-{id}"),
                               "roles": names.iter().map(|r| role(r)).collect::<Vec<_>>(),
                               "password": format!("pw-{id}")});
                self.users.insert(id, o.clone());
                self.settle(format!("user/{id}"), json!({"status": "ok"}));
                (200, o)
            }
            (m, ["cloud", "project", _, "user", id, rest @ ..]) => {
                let Ok(id) = id.parse::<i64>() else {
                    return not_found("User");
                };
                if m == "GET" && rest.is_empty() {
                    self.read_settling(&format!("user/{id}"));
                }
                let Some(u) = self.users.get(&id) else {
                    return not_found("User");
                };
                let ready = u["status"] == "ok";
                match (m, rest) {
                    ("GET", []) => {
                        let mut u = u.clone();
                        u.as_object_mut().map(|m| m.remove("password"));
                        (200, u)
                    }
                    ("DELETE", []) => {
                        self.users.remove(&id);
                        self.s3.remove(&id);
                        (200, Json::Null)
                    }
                    ("PUT", ["role"]) => {
                        let ids: Vec<&str> = body["rolesIds"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(Json::as_str)
                            .collect();
                        let roles: Option<Vec<Json>> = ids
                            .iter()
                            .map(|i| {
                                ROLES
                                    .iter()
                                    .find(|r| format!("role-{r}") == *i)
                                    .map(|r| role(r))
                            })
                            .collect();
                        let Some(roles) = roles else {
                            return (400, json!({"message": "Invalid role id"}));
                        };
                        let u = self.users.get_mut(&id).expect("looked up");
                        u["roles"] = json!(roles);
                        (200, u.clone())
                    }
                    ("GET", ["s3Credentials"]) => (
                        200,
                        json!(
                            self.s3
                                .get(&id)
                                .into_iter()
                                .flatten()
                                .map(|(a, _)| json!({"access": a, "userId": format!("os-{id}"),
                                                 "tenantId": PROJECT}))
                                .collect::<Vec<_>>()
                        ),
                    ),
                    ("POST", ["s3Credentials"]) if !ready => {
                        (400, json!({"message": format!("User {id} is not ready")}))
                    }
                    ("POST", ["s3Credentials"]) => {
                        self.next += 1;
                        let access = format!("AK{:014}", self.next);
                        let secret = format!("sk-{id}-{}", self.next);
                        self.s3
                            .entry(id)
                            .or_default()
                            .push((access.clone(), secret.clone()));
                        (
                            200,
                            json!({"access": access, "secret": secret,
                                   "userId": format!("os-{id}"), "tenantId": PROJECT}),
                        )
                    }
                    (m, ["s3Credentials", access, tail @ ..]) => {
                        let creds = self.s3.entry(id).or_default();
                        let Some(i) = creds.iter().position(|(a, _)| a == access) else {
                            return not_found("Credential");
                        };
                        match (m, tail) {
                            ("POST", ["secret"]) => (200, json!({"secret": creds[i].1})),
                            ("DELETE", []) => {
                                creds.remove(i);
                                (200, Json::Null)
                            }
                            _ => (405, json!({"message": "method"})),
                        }
                    }
                    _ => (404, json!({"message": format!("no route {method} {path}")})),
                }
            }
            // S3 containers.
            ("GET", ["cloud", "project", _, "region", r, "storage"]) => {
                let all: Vec<Json> = self
                    .containers
                    .iter()
                    .filter(|((cr, _), _)| cr == r)
                    .map(|(_, c)| c.clone())
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "region", r, "storage"]) => {
                if !REGIONS.contains(r) {
                    return (400, json!({"message": format!("Invalid region {r}")}));
                }
                let name = body["name"].as_str().unwrap_or_default().to_string();
                if self.containers.contains_key(&(r.to_string(), name.clone())) {
                    return (
                        409,
                        json!({"message": format!("Container {name} already exists")}),
                    );
                }
                let owner = match body.get("ownerId").and_then(Json::as_i64) {
                    Some(o) if !self.users.contains_key(&o) => {
                        return (400, json!({"message": format!("Invalid ownerId {o}")}));
                    }
                    Some(o) => o,
                    None => match self.s3.keys().next() {
                        Some(o) => *o,
                        None => {
                            return (400, json!({"message": "No S3 user found for the project"}));
                        }
                    },
                };
                let versioning = body["versioning"]["status"].as_str().unwrap_or("disabled");
                let o = container(r, &name, Some(owner), versioning);
                self.containers.insert((r.to_string(), name), o.clone());
                (200, o)
            }
            (m, ["cloud", "project", _, "region", r, "storage", name]) => {
                let k = (r.to_string(), name.to_string());
                let Some(c) = self.containers.get_mut(&k) else {
                    return not_found("Container");
                };
                match m {
                    "GET" => (200, c.clone()),
                    "PUT" => {
                        if let Some(v) = body["versioning"]["status"].as_str() {
                            if v == "disabled" && c["versioning"]["status"] != "disabled" {
                                return (
                                    400,
                                    json!({"message": "Versioning cannot be disabled once enabled"}),
                                );
                            }
                            c["versioning"]["status"] = json!(v);
                        }
                        (200, c.clone())
                    }
                    "DELETE" => {
                        if c["objectsCount"].as_i64().unwrap_or(0) > 0 {
                            return (409, json!({"message": "The container is not empty"}));
                        }
                        self.containers.remove(&k);
                        (200, Json::Null)
                    }
                    _ => (405, json!({"message": "method"})),
                }
            }
            // Volumes.
            ("GET", ["cloud", "project", _, "volume"]) => {
                let all: Vec<Json> = self
                    .volumes
                    .values()
                    .filter(|v| q("region").is_none_or(|r| v["region"] == r))
                    .cloned()
                    .collect();
                (200, json!(all))
            }
            ("POST", ["cloud", "project", _, "volume"]) => {
                let s = |k: &str| body.get(k).and_then(Json::as_str).unwrap_or_default();
                let region = s("region");
                if !REGIONS.contains(&region) {
                    return (400, json!({"message": format!("Invalid region {region}")}));
                }
                let typ = match s("type") {
                    "" => "classic",
                    t => t,
                };
                let types = [
                    "classic",
                    "classic-luks",
                    "high-speed",
                    "high-speed-luks",
                    "high-speed-gen2",
                    "high-speed-gen2-luks",
                ];
                if !types.contains(&typ) {
                    return (
                        400,
                        json!({"message": format!("Invalid volume type {typ}")}),
                    );
                }
                let size = body.get("size").and_then(Json::as_i64).unwrap_or(0);
                if size < 1 {
                    return (400, json!({"message": "size must be at least 1 GiB"}));
                }
                let image = s("imageId");
                let images = Self::images(region);
                if !image.is_empty()
                    && !images
                        .as_array()
                        .into_iter()
                        .flatten()
                        .any(|i| i["id"] == image)
                {
                    return (400, json!({"message": "Invalid imageId"}));
                }
                let id = self.id("volume");
                let mut o = volume(
                    &id,
                    s("name"),
                    region,
                    size,
                    typ,
                    s("description"),
                    !image.is_empty(),
                );
                o["status"] = json!("creating");
                self.volumes.insert(id.clone(), o.clone());
                self.settle(format!("volume/{id}"), json!({"status": "available"}));
                (200, o)
            }
            (m, ["cloud", "project", _, "volume", id, rest @ ..]) => {
                let id = id.to_string();
                if m == "GET" && rest.is_empty() {
                    self.read_settling(&format!("volume/{id}"));
                }
                let Some(v) = self.volumes.get(&id).cloned() else {
                    return not_found("Volume");
                };
                let status = v["status"].as_str().unwrap_or_default().to_string();
                let instance = body
                    .get("instanceId")
                    .and_then(Json::as_str)
                    .unwrap_or_default();
                let refuse = |why: String| (400, json!({"message": why}));
                match (m, rest) {
                    ("GET", []) => (200, v),
                    ("PUT", []) => {
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        for k in ["name", "description"] {
                            if let Some(x) = body.get(k) {
                                o[k] = x.clone();
                            }
                        }
                        (200, o.clone())
                    }
                    ("DELETE", []) if status == "in-use" => {
                        refuse(format!("Volume {id} is attached to an instance"))
                    }
                    ("DELETE", []) => {
                        self.volumes.get_mut(&id).expect("looked up")["status"] = json!("deleting");
                        self.settle(format!("volume/{id}"), Json::Null);
                        (200, Json::Null)
                    }
                    ("POST", ["attach"]) => {
                        if status != "available" {
                            return refuse(format!("Volume {id} is {status}, not available"));
                        }
                        let Some(i) = self.instances.get(instance) else {
                            return not_found("Instance");
                        };
                        if i["region"] != v["region"] {
                            return refuse("The instance is in another region".into());
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("attaching");
                        let o = o.clone();
                        self.settle(
                            format!("volume/{id}"),
                            json!({"status": "in-use", "attachedTo": [instance]}),
                        );
                        (200, o)
                    }
                    ("POST", ["detach"]) => {
                        if !v["attachedTo"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .any(|x| x == instance)
                        {
                            return refuse(format!("Volume {id} is not attached to {instance}"));
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("detaching");
                        let o = o.clone();
                        self.settle(
                            format!("volume/{id}"),
                            json!({"status": "available", "attachedTo": []}),
                        );
                        (200, o)
                    }
                    ("POST", ["upsize"]) => {
                        let size = body.get("size").and_then(Json::as_i64).unwrap_or(0);
                        if size <= v["size"].as_i64().unwrap_or(0) {
                            return refuse(
                                "The new size must be greater than the current one".into(),
                            );
                        }
                        let o = self.volumes.get_mut(&id).expect("looked up");
                        o["status"] = json!("extending");
                        o["size"] = json!(size);
                        let o = o.clone();
                        self.settle(format!("volume/{id}"), json!({"status": status}));
                        (200, o)
                    }
                    _ => (404, json!({"message": format!("no route {method} {path}")})),
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
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
                + w.skew;
            let bearer = headers
                .get("authorization")
                .and_then(|a| a.strip_prefix("Bearer "));
            if path == "/auth/time" {
                w.times_asked += 1;
                (200, json!(now))
            } else if path == "/auth/oauth2/token" {
                w.mint(&headers, &body)
            } else if let Some(token) = bearer {
                if w.tokens.contains(token) {
                    w.answer_seen(&method, &path, &target, &query, json_body)
                } else {
                    (401, json!({"message": "Invalid token"}))
                }
            } else if let Some((status, why)) = bad_signature(
                &headers,
                &method,
                &format!("{base}{}", target.strip_prefix("/1.0").unwrap_or(&target)),
                &body,
            )
            .map(|why| (403, json!({"message": why})))
            .or_else(|| {
                w.key_revoked.then(|| {
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
                w.answer_seen(&method, &path, &target, &query, json_body)
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
