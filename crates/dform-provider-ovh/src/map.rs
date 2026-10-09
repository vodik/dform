//! The API's objects as the schema's documents: each a pair of the
//! configured attributes and the computed values (what Read and Apply
//! answer), and the data sources' rows.

use dform_core::quantity::Quantity;
use dform_core::value::Value;
use serde_json::{Map, Value as Json, json};

fn str_of<'a>(o: &'a Json, k: &str) -> Option<&'a str> {
    o.get(k).and_then(Json::as_str)
}

/// An instance (`cloud.instance.Instance`), its flavor's and image's names
/// given (the API answers ids; it embeds the objects when it can).
/// `user_data` is not in the answer: the API keeps it write-only, so it is
/// left out here (`Ovh::plan` compares it with what the provider sent).
/// `nets` names the private networks by the OpenStack id an address's
/// `networkId` is: the instance's `networks` are those it has an address
/// on, sorted, and `private_ips` its IPv4 address on each, by network.
pub fn instance(
    o: &Json,
    flavor: Option<&str>,
    image: Option<&str>,
    nets: &std::collections::BTreeMap<String, String>,
) -> (Json, Json) {
    let mut attrs = Map::new();
    let mut put = |k: &str, v: Option<&str>| {
        if let Some(v) = v {
            attrs.insert(k.into(), json!(v));
        }
    };
    put("name", str_of(o, "name"));
    put("region", str_of(o, "region"));
    put(
        "flavor",
        flavor.or_else(|| o.get("flavor").and_then(|f| str_of(f, "name"))),
    );
    put(
        "image",
        image.or_else(|| o.get("image").and_then(|i| str_of(i, "name"))),
    );
    put("ssh_key", str_of(o, "sshKeyId"));
    let mut computed = Map::new();
    if let Some(id) = str_of(o, "id") {
        computed.insert("id".into(), json!(id));
    }
    if let Some(s) = str_of(o, "status") {
        computed.insert("status".into(), json!(s));
    }
    let addr = |kind: &str| {
        o.get("ipAddresses")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .find(|a| str_of(a, "type") == Some(kind) && a.get("version") == Some(&json!(4)))
            .and_then(|a| str_of(a, "ip"))
    };
    // No address yet (BUILD): an open value, which a tick that reads it
    // waits on (R-81).
    if let Some(ip) = addr("public") {
        computed.insert("public_ip".into(), json!(ip));
    }
    computed.insert(
        "private_ip".into(),
        addr("private").map_or(Json::Null, |ip| json!(ip)),
    );
    let mut private_ips = Map::new();
    for a in o
        .get("ipAddresses")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter(|a| str_of(a, "type") == Some("private") && a.get("version") == Some(&json!(4)))
    {
        if let (Some(net), Some(ip)) = (
            str_of(a, "networkId").and_then(|n| nets.get(n)),
            str_of(a, "ip"),
        ) {
            private_ips.entry(net.clone()).or_insert(json!(ip));
        }
    }
    if !private_ips.is_empty() {
        let networks: Vec<&String> = private_ips.keys().collect();
        attrs.insert("networks".into(), json!(networks));
    }
    computed.insert("private_ips".into(), Json::Object(private_ips));
    (Json::Object(attrs), Json::Object(computed))
}

/// An SSH key (`cloud.sshkey.SshKeyDetail`).
pub fn ssh_key(o: &Json) -> (Json, Json) {
    let attrs = json!({
        "name": str_of(o, "name").unwrap_or_default(),
        "public_key": str_of(o, "publicKey").unwrap_or_default(),
    });
    let computed = json!({"id": str_of(o, "id").unwrap_or_default()});
    (attrs, computed)
}

/// A record's remote id: `ZONE/ID`.
pub fn record_remote(zone: &str, id: i64) -> String {
    format!("{zone}/{id}")
}

/// A DNS record (`domain.zone.Record`). The apex has no subdomain; the
/// ttl is also computed, 0 meaning the zone's default.
pub fn record(o: &Json) -> (Json, Json) {
    let mut attrs = Map::new();
    let zone = str_of(o, "zone").unwrap_or_default();
    attrs.insert("zone".into(), json!(zone));
    if let Some(s) = str_of(o, "subDomain").filter(|s| !s.is_empty()) {
        attrs.insert("subdomain".into(), json!(s));
    }
    attrs.insert(
        "type".into(),
        json!(str_of(o, "fieldType").unwrap_or_default()),
    );
    attrs.insert(
        "target".into(),
        json!(str_of(o, "target").unwrap_or_default()),
    );
    let ttl = o.get("ttl").and_then(Json::as_i64).unwrap_or(0);
    if ttl != 0 {
        attrs.insert("ttl".into(), json!(ttl));
    }
    let id = o.get("id").and_then(Json::as_i64).unwrap_or(0);
    let computed = json!({"id": record_remote(zone, id), "ttl": ttl});
    (Json::Object(attrs), computed)
}

/// An S3 container's remote id: `REGION/NAME`.
pub fn container_remote(region: &str, name: &str) -> String {
    format!("{region}/{name}")
}

/// The API's path of the S3 container `remote` names.
pub fn container_path(project: &str, remote: &str) -> String {
    let (region, name) = remote.split_once('/').unwrap_or((remote, ""));
    format!(
        "/cloud/project/{project}/region/{}/storage/{}",
        crate::api::escape(region),
        crate::api::escape(name)
    )
}

/// An S3 container (`cloud.StorageContainer`). Its versioning is on when
/// the API says `enabled` (off when `disabled` or `suspended`); its owner
/// is the user's id, the reference `owner = user` names. Both are the
/// program's when it sets them and the API's otherwise (optional
/// computed): they are computed values, which dform compares with the
/// program's only where it sets them.
pub fn container(o: &Json) -> (Json, Json) {
    let region = str_of(o, "region").unwrap_or_default();
    let name = str_of(o, "name").unwrap_or_default();
    let versioning = o
        .get("versioning")
        .and_then(|v| str_of(v, "status"))
        .is_some_and(|s| s == "enabled");
    let attrs = json!({"region": region, "name": name});
    let mut computed = json!({
        "id": container_remote(region, name),
        "versioning": versioning,
    });
    if let Some(owner) = o.get("ownerId").and_then(Json::as_i64) {
        computed["owner"] = json!(owner.to_string());
    }
    if let Some(h) = str_of(o, "virtualHost") {
        computed["virtual_host"] = json!(h);
    }
    (attrs, computed)
}

/// A user (`cloud.user.User`) and the access key of its first S3
/// credential, if it has one. Its roles are their names, sorted; its S3
/// secret is never here: where the credential exists, `s3_secret_key` is
/// `true`, which the provider answers as the secret's label.
pub fn user(o: &Json, access: Option<&str>) -> (Json, Json) {
    let mut roles: Vec<&str> = o
        .get("roles")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| str_of(r, "name"))
        .collect();
    roles.sort();
    roles.dedup();
    let attrs = json!({
        "description": str_of(o, "description").unwrap_or_default(),
        "roles": roles,
    });
    let id = o.get("id").and_then(Json::as_i64).unwrap_or(0);
    let mut computed = json!({
        "id": id.to_string(),
        "username": str_of(o, "username").unwrap_or_default(),
        "status": str_of(o, "status").unwrap_or_default(),
        "s3_access_key": access,
    });
    if access.is_some() {
        computed["s3_secret_key"] = json!(true);
    }
    (attrs, computed)
}

/// A volume (`cloud.volume.Volume`): its size in GiB, as the schema's
/// `bytes(gib)` takes it, and the instance it is attached to, the
/// reference `instance = server` names. Its image and snapshot are not in
/// the answer (write-only). Its type and description are the program's
/// when it sets them and the API's otherwise: computed values.
pub fn volume(o: &Json) -> (Json, Json) {
    let mut attrs = json!({
        "name": str_of(o, "name").unwrap_or_default(),
        "region": str_of(o, "region").unwrap_or_default(),
        "size": o.get("size").and_then(Json::as_i64).unwrap_or(0),
    });
    if let Some(i) = attached_to(o).first() {
        attrs["instance"] = json!(i);
    }
    let computed = json!({
        "id": str_of(o, "id").unwrap_or_default(),
        "status": str_of(o, "status").unwrap_or_default(),
        "type": str_of(o, "type").unwrap_or_default(),
        "description": str_of(o, "description").unwrap_or_default(),
    });
    (attrs, computed)
}

/// The instances a volume is attached to.
pub fn attached_to(o: &Json) -> Vec<String> {
    o.get("attachedTo")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect()
}

/// A private network (`cloud.network.Network`): its regions by name,
/// sorted, and each one's status. Its VLAN and regions are the program's
/// when it sets them and the API's otherwise: computed values.
pub fn network(o: &Json) -> (Json, Json) {
    let mut regions: Vec<(&str, &str)> = o
        .get("regions")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            Some((
                str_of(r, "region")?,
                str_of(r, "status").unwrap_or_default(),
            ))
        })
        .collect();
    regions.sort();
    let names: Vec<&str> = regions.iter().map(|(r, _)| *r).collect();
    let vlan = o.get("vlanId").and_then(Json::as_i64).unwrap_or(0);
    let attrs = json!({"name": str_of(o, "name").unwrap_or_default()});
    let status: Map<String, Json> = regions
        .iter()
        .map(|(r, s)| (r.to_string(), json!(s)))
        .collect();
    let computed = json!({
        "id": str_of(o, "id").unwrap_or_default(),
        "status": str_of(o, "status").unwrap_or_default(),
        "vlan_id": vlan,
        "regions": names,
        "regions_status": status,
    });
    (attrs, computed)
}

/// A subnet's remote id: `NETWORK/ID`, its network's id and its own (the
/// API reaches a subnet under its network).
pub fn subnet_remote(network: &str, id: &str) -> String {
    format!("{network}/{id}")
}

/// The network's id and the subnet's in a subnet's remote id.
pub fn subnet_parts(remote: &str) -> (&str, &str) {
    remote.split_once('/').unwrap_or((remote, ""))
}

/// A pool as a `range(ip)` prints (`10.0.0.2..=10.0.0.254`): its first
/// and last address.
pub fn pool_text(start: &str, end: &str) -> String {
    format!("{start}..={end}")
}

/// The ends of a pool the program writes (a `range(ip)`, `A..=B`), first
/// and last.
pub fn pool_ends(pool: &str) -> Option<(String, String)> {
    let (a, b) = dform_core::range::Range::ips(pool)?;
    Some((
        dform_core::value::u32_to_ipv4(a),
        dform_core::value::u32_to_ipv4(b),
    ))
}

/// The hosts of `range` a pool may hold, first and last: neither its
/// network's address nor its broadcast, nor its gateway (the first
/// host) when it has one. None when that leaves none (a `/31`, a `/32`,
/// a `/30` with a gateway has one).
pub fn pool_hosts(range: &str, no_gateway: bool) -> Option<(u32, u32)> {
    let (net, prefix) = dform_core::value::parse_ipnet(range)?;
    let size = 1u64 << (32 - u32::from(prefix));
    let first = u64::from(net) + if no_gateway { 1 } else { 2 };
    let last = u64::from(net) + size - 2;
    (size >= 4 && first <= last).then_some((first as u32, last as u32))
}

/// The pool a subnet of `range` is given when the program sets none, as
/// the OVH console fills it in: every host of the range after its
/// gateway, to the last before broadcast.
pub fn default_pool(range: &str, no_gateway: bool) -> Option<String> {
    let (a, b) = pool_hosts(range, no_gateway)?;
    Some(pool_text(
        &dform_core::value::u32_to_ipv4(a),
        &dform_core::value::u32_to_ipv4(b),
    ))
}

/// A subnet of the private network `network` (`cloud.network.Subnet`):
/// its region and range as the API's first pool has them; no gateway when
/// the API gives none. Its pool (the API's `start` and `end`), whether it
/// has DHCP and a gateway are the program's when it sets them and the
/// API's otherwise: computed values.
pub fn subnet(network: &str, o: &Json) -> (Json, Json) {
    let pool = o
        .get("ipPools")
        .and_then(Json::as_array)
        .and_then(|p| p.first())
        .cloned()
        .unwrap_or(Json::Null);
    let dhcp = o
        .get("dhcpEnabled")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    let gateway = o.get("gatewayIp").filter(|g| !g.is_null()).cloned();
    let attrs = json!({
        "network": network,
        "region": str_of(&pool, "region").unwrap_or_default(),
        "range": str_of(o, "cidr").or_else(|| str_of(&pool, "network")).unwrap_or_default(),
    });
    let computed = json!({
        "id": subnet_remote(network, str_of(o, "id").unwrap_or_default()),
        "pool": pool_text(
            str_of(&pool, "start").unwrap_or_default(),
            str_of(&pool, "end").unwrap_or_default()
        ),
        "no_gateway": gateway.is_none(),
        "gateway_ip": gateway.unwrap_or(Json::Null),
        "dhcp": dhcp,
    });
    (attrs, computed)
}

/// `ovh.zone(+name, -id, -nameservers)`: the zone `name` asked for
/// (`domain.zone.Zone`), its id the name the API answers (the identity a
/// record's `zone` names) and its nameservers as the API lists them.
pub fn zone_row(name: &str, zone: &Json) -> Vec<Value> {
    let nameservers = zone
        .get("nameServers")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(|n| Value::Str(n.trim_end_matches('.').to_string()))
        .collect();
    vec![
        Value::Str(name.into()),
        Value::Str(str_of(zone, "name").unwrap_or(name).into()),
        Value::List(nameservers),
    ]
}

/// `ovh.flavor(+region, -name, -vcpus: int, -ram: bytes, -disk: bytes)`:
/// the flavors offered in `region` (`cloud.flavor.Flavor[]`), available
/// ones, a name once. The API counts RAM in MiB and disk in GiB, as
/// OpenStack does.
pub fn flavor_rows(region: &str, flavors: &Json) -> Vec<Vec<Value>> {
    let mut seen = std::collections::BTreeSet::new();
    flavors
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| f.get("available") != Some(&json!(false)))
        .filter_map(|f| {
            let name = str_of(f, "name")?;
            if !seen.insert(name.to_string()) {
                return None;
            }
            let n = |k: &str| f.get(k).and_then(Json::as_i64).unwrap_or(0);
            Some(vec![
                Value::Str(region.into()),
                Value::Str(name.into()),
                Value::Int(n("vcpus")),
                Value::Quantity(Quantity::Bytes(n("ram") << 20)),
                Value::Quantity(Quantity::Bytes(n("disk") << 30)),
            ])
        })
        .collect()
}

/// An image's distribution: the first word of its name (`Ubuntu 24.04`
/// is `Ubuntu`); the API says only `linux` or `windows`.
pub fn distribution(name: &str) -> String {
    name.split_whitespace()
        .next()
        .unwrap_or_default()
        .to_string()
}

/// `ovh.image(+region, -name, -id, -distribution)`: the images of
/// `region` (`cloud.image.Image[]`) that are active.
pub fn image_rows(region: &str, images: &Json) -> Vec<Vec<Value>> {
    let mut rows: Vec<Vec<Value>> = images
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| str_of(i, "status").is_none_or(|s| s == "active"))
        .filter_map(|i| {
            let name = str_of(i, "name")?;
            Some(vec![
                Value::Str(region.into()),
                Value::Str(name.into()),
                Value::Str(str_of(i, "id")?.into()),
                Value::Str(distribution(name)),
            ])
        })
        .collect();
    rows.sort();
    rows.dedup();
    rows
}

/// The id of the flavor named `name` among `flavors`, an available one
/// first.
pub fn flavor_id(flavors: &Json, name: &str) -> Option<String> {
    let all: Vec<&Json> = flavors
        .as_array()
        .into_iter()
        .flatten()
        .filter(|f| str_of(f, "name") == Some(name))
        .collect();
    all.iter()
        .find(|f| f.get("available") != Some(&json!(false)))
        .or(all.first())
        .and_then(|f| str_of(f, "id"))
        .map(str::to_string)
}

/// The id of the active image named `name` among `images`, the newest
/// first (an image is republished under its name).
pub fn image_id(images: &Json, name: &str) -> Option<String> {
    images
        .as_array()
        .into_iter()
        .flatten()
        .filter(|i| str_of(i, "name") == Some(name))
        .filter(|i| str_of(i, "status").is_none_or(|s| s == "active"))
        .max_by_key(|i| str_of(i, "creationDate").unwrap_or_default().to_string())
        .and_then(|i| str_of(i, "id"))
        .map(str::to_string)
}

/// The names in `list` (the field `name` of each), sorted, for a message.
pub fn names(list: &Json) -> String {
    let mut n: Vec<&str> = list
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| str_of(x, "name"))
        .collect();
    n.sort();
    n.dedup();
    n.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Json {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(name);
        serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn an_instance_as_the_schema_has_it() {
        let o = fixture("instance.json");
        let none = Default::default();
        let (attrs, computed) = instance(&o, None, None, &none);
        assert_eq!(
            attrs,
            json!({"name": "k8s-lab-server", "region": "ca-east-tor", "flavor": "b2-7",
                   "image": "Ubuntu 24.04", "ssh_key": "Wkdabc123"})
        );
        assert_eq!(
            computed,
            json!({"id": "6f8b2c1e-4a1d-4f7e-9d3c-2b1a0e9f8c7d", "status": "ACTIVE",
                   "public_ip": "51.79.10.20", "private_ip": "10.0.0.12",
                   "private_ips": {}})
        );
        // Its private address is on a network the project has: the
        // network, by its id, and the address on it.
        let nets = [(
            "a1b2c3d4e5f60718293a4b5c6d7e8f90".to_string(),
            "pn-1000123_42".to_string(),
        )]
        .into();
        let (attrs, computed) = instance(&o, None, None, &nets);
        assert_eq!(attrs["networks"], json!(["pn-1000123_42"]));
        assert_eq!(
            computed["private_ips"],
            json!({"pn-1000123_42": "10.0.0.12"})
        );
    }

    #[test]
    fn a_building_instance_has_no_address_yet() {
        let o = fixture("instance-building.json");
        let (attrs, computed) =
            instance(&o, Some("b2-7"), Some("Ubuntu 24.04"), &Default::default());
        assert_eq!(attrs["flavor"], "b2-7");
        assert_eq!(attrs.get("ssh_key"), None);
        assert_eq!(computed.get("public_ip"), None);
        assert_eq!(computed["private_ip"], Json::Null);
        assert_eq!(computed["status"], "BUILD");
    }

    #[test]
    fn an_ssh_key() {
        let (attrs, computed) = ssh_key(&fixture("sshkey.json"));
        assert_eq!(attrs["name"], "k8s-lab-admin");
        assert!(
            attrs["public_key"]
                .as_str()
                .unwrap()
                .starts_with("ssh-ed25519 ")
        );
        assert_eq!(computed, json!({"id": "Wkdabc123"}));
    }

    #[test]
    fn records_and_their_ttl() {
        let (attrs, computed) = record(&fixture("record.json"));
        assert_eq!(
            attrs,
            json!({"zone": "vodik.xyz", "subdomain": "matrix", "type": "A",
                   "target": "51.79.10.20"})
        );
        assert_eq!(computed, json!({"id": "vodik.xyz/5123456789", "ttl": 0}));
        let (apex, _) = record(&fixture("record-apex.json"));
        assert_eq!(apex.get("subdomain"), None);
        assert_eq!(apex["ttl"], 3600);
    }

    #[test]
    fn an_s3_container() {
        let (attrs, computed) = container(&fixture("container.json"));
        // Suspended versioning is off.
        assert_eq!(attrs, json!({"region": "BHS", "name": "lab-backups"}));
        assert_eq!(computed["versioning"], false);
        assert_eq!(computed["owner"], "482913");
        assert_eq!(computed["id"], "BHS/lab-backups");
        assert_eq!(
            computed["virtual_host"],
            "lab-backups.s3.bhs.io.cloud.ovh.net"
        );
        assert_eq!(
            container_path("p", "BHS/lab-backups"),
            "/cloud/project/p/region/BHS/storage/lab-backups"
        );
    }

    #[test]
    fn a_user_and_its_s3_credential() {
        let (attrs, computed) = user(&fixture("user.json"), Some("AKIA0EXAMPLE"));
        assert_eq!(
            attrs,
            json!({"description": "backup",
                   "roles": ["objectstore_operator", "volume_operator"]})
        );
        assert_eq!(
            computed,
            json!({"id": "482913", "username": "user-Xk3pQ9", "status": "ok",
                   "s3_access_key": "AKIA0EXAMPLE", "s3_secret_key": true})
        );
        // No credential: no secret, and the access key is null.
        let (_, none) = user(&fixture("user.json"), None);
        assert_eq!(none["s3_access_key"], Json::Null);
        assert_eq!(none.get("s3_secret_key"), None);
    }

    #[test]
    fn a_volume_and_its_instance() {
        let (attrs, computed) = volume(&fixture("volume.json"));
        assert_eq!(
            attrs,
            json!({"name": "lab-data", "region": "ca-east-tor", "size": 50,
                   "instance": "6f8b2c1e-4a1d-4f7e-9d3c-2b1a0e9f8c7d"})
        );
        assert_eq!(computed["type"], "high-speed-gen2");
        assert_eq!(computed["status"], "in-use");
        assert_eq!(computed["id"], "0d9e8f7a-6b5c-4d3e-2f1a-0b9c8d7e6f5a");
    }

    #[test]
    fn a_private_network_and_its_regions() {
        let (attrs, computed) = network(&fixture("network.json"));
        assert_eq!(attrs, json!({"name": "lab"}));
        assert_eq!(computed["vlan_id"], 42);
        assert_eq!(computed["regions"], json!(["BHS5", "ca-east-tor"]));
        assert_eq!(computed["id"], "pn-1000123_42");
        assert_eq!(
            computed["regions_status"],
            json!({"BHS5": "ACTIVE", "ca-east-tor": "BUILDING"})
        );
    }

    #[test]
    fn a_subnet_of_a_private_network() {
        let (attrs, computed) = subnet("pn-1000123_42", &fixture("subnet.json"));
        assert_eq!(
            attrs,
            json!({"network": "pn-1000123_42", "region": "BHS5", "range": "10.0.0.0/24"})
        );
        assert_eq!(computed["pool"], "10.0.0.10..=10.0.0.200");
        assert_eq!(
            (&computed["dhcp"], &computed["no_gateway"]),
            (&json!(true), &json!(false))
        );
        assert_eq!(
            computed["id"],
            "pn-1000123_42/7a6b5c4d-3e2f-4a1b-9c8d-7e6f5a4b3c2d"
        );
        assert_eq!(computed["gateway_ip"], "10.0.0.1");
        assert_eq!(
            subnet_parts(computed["id"].as_str().unwrap()),
            ("pn-1000123_42", "7a6b5c4d-3e2f-4a1b-9c8d-7e6f5a4b3c2d")
        );
    }

    /// A subnet's pool when the program sets none: the range's hosts
    /// after its gateway (the first host), or from the first host without
    /// one, to the last before broadcast; none where that leaves none.
    #[test]
    fn a_subnets_default_pool() {
        assert_eq!(
            default_pool("10.42.0.0/24", false).as_deref(),
            Some("10.42.0.2..=10.42.0.254")
        );
        assert_eq!(
            default_pool("10.42.0.0/24", true).as_deref(),
            Some("10.42.0.1..=10.42.0.254")
        );
        assert_eq!(
            default_pool("10.0.0.0/16", false).as_deref(),
            Some("10.0.0.2..=10.0.255.254")
        );
        assert_eq!(
            default_pool("10.0.0.0/30", false).as_deref(),
            Some("10.0.0.2..=10.0.0.2")
        );
        assert_eq!(default_pool("10.0.0.0/31", true), None);
        assert_eq!(default_pool("10.0.0.0/32", true), None);
        assert_eq!(default_pool("not a range", false), None);
        assert_eq!(
            pool_ends("10.0.0.3..10.0.0.10"),
            Some(("10.0.0.3".into(), "10.0.0.9".into()))
        );
    }

    #[test]
    fn flavors_as_rows() {
        let rows = flavor_rows("ca-east-tor", &fixture("flavors.json"));
        // d2-2 is not available; win-b2-7 is its own name.
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            vec![
                Value::Str("ca-east-tor".into()),
                Value::Str("b2-7".into()),
                Value::Int(2),
                Value::Quantity(Quantity::Bytes(7000 << 20)),
                Value::Quantity(Quantity::Bytes(50 << 30)),
            ]
        );
        let f = fixture("flavors.json");
        assert_eq!(
            flavor_id(&f, "b2-7").as_deref(),
            Some("3b4c5d6e-7f80-4a1b-8c2d-3e4f5a6b7c8d")
        );
        assert_eq!(flavor_id(&f, "b3-8"), None);
        assert_eq!(names(&f), "b2-7, d2-2, win-b2-7");
    }

    #[test]
    fn images_as_rows() {
        let i = fixture("images.json");
        let rows = image_rows("ca-east-tor", &i);
        assert_eq!(rows.len(), 3);
        let ubuntu = rows
            .iter()
            .find(|r| r[1] == Value::Str("Ubuntu 24.04".into()))
            .unwrap();
        assert_eq!(ubuntu[3], Value::Str("Ubuntu".into()));
        assert_eq!(
            image_id(&i, "Debian 13").as_deref(),
            Some("8b7c6d5e-4f3a-4b2c-1d0e-9f8a7b6c5d4e")
        );
        assert_eq!(
            distribution("Windows Server 2025 Standard (Desktop)"),
            "Windows"
        );
    }
}
