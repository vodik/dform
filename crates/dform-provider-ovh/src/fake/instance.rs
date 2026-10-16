//! Instances: listed, made BUILD then ACTIVE, renamed, resized (RESIZE, to a
//! flavor no smaller), their interfaces attached and detached, deleted. A
//! port is made with the address asked of it (`networks[].ip` of the
//! create, `ip` of an attach), or one of its subnet's pool.

use super::*;

/// A port of an instance (`cloud.instanceInterface.Interface`): its id,
/// its network's OpenStack id (`ext-REGION` for the public network), and
/// the address an attach gave it or the create asked for it (another made
/// with the instance has the instance's address on its network).
#[derive(Debug, Clone)]
pub(super) struct Port {
    id: String,
    network: String,
    ip: Option<Json>,
}

impl Port {
    /// A port on `network`, `ports` the instance's others: the first on a
    /// network is `if-` and the network's id, another is numbered.
    fn new(ports: &[Port], network: &str, ip: Option<Json>) -> Port {
        let on = ports.iter().filter(|p| p.network == network).count();
        let id = match on {
            0 => format!("if-{network}"),
            n => format!("if-{network}-{}", n + 1),
        };
        Port {
            id,
            network: network.to_string(),
            ip,
        }
    }

    fn public(region: &str) -> Port {
        Port::new(&[], &format!("ext-{region}"), None)
    }

    fn is_public(&self) -> bool {
        self.network.starts_with("ext-")
    }
}

impl World {
    /// A call under the instance routes.
    pub(super) fn instance_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        query: &BTreeMap<String, String>,
        body: &Json,
    ) -> (u16, Json) {
        let q = |k: &str| query.get(k).map(String::as_str);
        match (method, segs) {
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
                // in that region, as OpenStack names them, each with the
                // address asked of its port, if one is.
                let nics: Option<Vec<(String, Option<String>)>> =
                    body.get("networks").and_then(Json::as_array).map(|ns| {
                        ns.iter()
                            .filter_map(|n| {
                                let ip = n.get("ip").and_then(Json::as_str).map(str::to_string);
                                Some((n["networkId"].as_str()?.to_string(), ip))
                            })
                            .collect()
                    });
                for (nic, ip) in nics.iter().flatten() {
                    if let Some(why) = self.nic_refused(region, nic) {
                        return (400, json!({"message": why}));
                    }
                    if let Some(refused) = ip
                        .as_deref()
                        .and_then(|ip| self.ip_refused(region, nic, ip))
                    {
                        return refused;
                    }
                }
                let id = self.id("instance");
                if let Some(nics) = nics {
                    let mut ports = Vec::new();
                    for (nic, ip) in nics {
                        let ip = ip.map(|ip| self.private_ip(&nic, &ip));
                        let port = Port::new(&ports, &nic, ip);
                        ports.push(port);
                    }
                    self.nics.insert(id.clone(), ports);
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
                self.read_settling(&format!("instance/{id}"));
                self.read_lagging(&id);
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
            // A resize to a flavor of the region no smaller: RESIZE, then
            // ACTIVE on it (OVH confirms it); the instance keeps its id
            // and addresses.
            ("POST", ["cloud", "project", _, "instance", id, "resize"]) => {
                let id = id.to_string();
                let Some(o) = self.instances.get(&id).cloned() else {
                    return not_found("Instance");
                };
                let region = o["region"].as_str().unwrap_or_default();
                let to = body["flavorId"].as_str().unwrap_or_default();
                let (Some(was), Some(now)) = (
                    Self::flavor_size(region, o["flavorId"].as_str().unwrap_or_default()),
                    Self::flavor_size(region, to),
                ) else {
                    return (400, json!({"message": "Invalid flavorId"}));
                };
                if was.iter().zip(&now).any(|(w, n)| n < w) {
                    return (
                        400,
                        json!({"message": "Instance can only be resized to a bigger flavor"}),
                    );
                }
                let plan = format!(
                    "{}.consumption",
                    to.trim_start_matches("flavor-")
                        .trim_end_matches(&format!("-{region}"))
                );
                let o = self.instances.get_mut(&id).expect("looked up");
                o["status"] = json!("RESIZE");
                let answer = o.clone();
                self.settle(
                    format!("instance/{id}"),
                    json!({"status": "ACTIVE", "flavorId": to, "planCode": plan}),
                );
                (200, answer)
            }
            ("GET", ["cloud", "project", _, "instance", id, "interface"]) => {
                let Some(o) = self.instances.get(*id) else {
                    return not_found("Instance");
                };
                let region = o["region"].as_str().unwrap_or_default();
                let ports = self
                    .nics
                    .get(*id)
                    .cloned()
                    .unwrap_or_else(|| vec![Port::public(region)]);
                let all: Vec<Json> = ports.iter().map(|p| self.interface(id, p)).collect();
                (200, json!(all))
            }
            // An attach makes a port, on a network the instance is on
            // already as well (OVH does); its address, the one asked or
            // one of the pool, is the port's at once and the instance's
            // `ipAddresses` only later.
            ("POST", ["cloud", "project", _, "instance", id, "interface"]) => {
                let id = id.to_string();
                let Some(region) = self.instances.get(&id).map(|o| o["region"].clone()) else {
                    return not_found("Instance");
                };
                let region = region.as_str().unwrap_or_default();
                let nic = body["networkId"].as_str().unwrap_or_default().to_string();
                if let Some(why) = self.nic_refused(region, &nic) {
                    return (400, json!({"message": why}));
                }
                let asked = body.get("ip").and_then(Json::as_str);
                if let Some(refused) = asked.and_then(|ip| self.ip_refused(region, &nic, ip)) {
                    return refused;
                }
                // Another port on a network has another address.
                let on = self.nics.get(&id).into_iter().flatten();
                let n = self.instances.len() + 10 + on.filter(|p| p.network == nic).count();
                let ip = match asked {
                    Some(ip) => self.private_ip(&nic, ip),
                    None => self.private_address(&nic, n),
                };
                let ports = self
                    .nics
                    .entry(id.clone())
                    .or_insert_with(|| vec![Port::public(region)]);
                let port = Port::new(ports, &nic, Some(ip.clone()));
                ports.push(port.clone());
                match self.ip_lag {
                    0 => self.address_settled(&id, ip),
                    reads => self
                        .lagging
                        .entry(id.clone())
                        .or_default()
                        .push((reads, ip)),
                }
                (200, self.interface(&id, &port))
            }
            ("DELETE", ["cloud", "project", _, "instance", id, "interface", port]) => {
                let Some(ports) = self.nics.get_mut(*id) else {
                    return not_found("Interface");
                };
                let Some(i) = ports.iter().position(|p| p.id == *port && !p.is_public()) else {
                    return not_found("Interface");
                };
                let gone = ports.remove(i);
                let last = !ports.iter().any(|p| p.network == gone.network);
                // Its address goes with it: an attached port's own, or
                // the instance's on the network when no port is left there.
                let its = |a: &Json| match &gone.ip {
                    Some(ip) => a["ip"] == ip["ip"],
                    None => last && a["networkId"] == gone.network,
                };
                if let Some(ips) = self
                    .instances
                    .get_mut(*id)
                    .and_then(|o| o["ipAddresses"].as_array_mut())
                {
                    ips.retain(|a| !its(a));
                }
                if let Some(lagging) = self.lagging.get_mut(*id) {
                    lagging.retain(|(_, a)| !its(a));
                }
                (200, Json::Null)
            }
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
                        self.lagging.remove(*id);
                        (200, Json::Null)
                    }
                    None => not_found("Instance"),
                }
            }
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) fn instance(
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
        shaped(
            "instance",
            json!({
                "id": id, "name": name, "region": region,
                "flavorId": format!("flavor-{flavor_name}-{region}"),
                "imageId": format!("image-{}-{region}", image_name.to_lowercase().replace(' ', "-")),
                "sshKeyId": key, "status": if active { "ACTIVE" } else { "BUILD" },
                "ipAddresses": ips, "created": "2026-10-06T00:00:00Z",
                "planCode": format!("{flavor_name}.consumption"),
            }),
        )
    }

    /// A read of instance `id`: each lagging address with no reads left
    /// is in its `ipAddresses` from this one on, each other one read
    /// nearer.
    fn read_lagging(&mut self, id: &str) {
        let Some(lagging) = self.lagging.get_mut(id) else {
            return;
        };
        let mut due = Vec::new();
        lagging.retain_mut(|(left, ip)| match left {
            0 => {
                due.push(ip.clone());
                false
            }
            _ => {
                *left -= 1;
                true
            }
        });
        for ip in due {
            self.address_settled(id, ip);
        }
    }

    /// Instance `id`'s `ipAddresses` has `ip`.
    fn address_settled(&mut self, id: &str, ip: Json) {
        if let Some(ips) = self
            .instances
            .get_mut(id)
            .and_then(|o| o["ipAddresses"].as_array_mut())
        {
            ips.push(ip);
        }
    }

    /// An instance's addresses, the `n`th: the public network's, and one
    /// on each private network it was made on, the one asked or in its
    /// subnet's pool.
    fn addresses(&self, id: &str, n: usize) -> Json {
        let Some(ports) = self.nics.get(id) else {
            return ips(n);
        };
        let mut out = Vec::new();
        for port in ports {
            if port.is_public() {
                out.extend(ips(n).as_array().cloned().unwrap_or_default());
                continue;
            }
            out.push(match &port.ip {
                Some(ip) => ip.clone(),
                None => self.private_address(&port.network, n),
            });
        }
        json!(out)
    }

    /// An instance's address on the private network `nic` (its OpenStack
    /// id), the `n`th of its subnet's pool.
    fn private_address(&self, nic: &str, n: usize) -> Json {
        let pool = self
            .networks
            .values()
            .find(|net| {
                net["regions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["openstackId"] == nic)
            })
            .and_then(|net| self.subnets.get(net["id"].as_str().unwrap_or_default()))
            .and_then(|subs| subs.first())
            .and_then(|sub| sub["ipPools"][0]["start"].as_str().map(str::to_string));
        let ip = pool
            .and_then(|start| host_of(&start, n as u32))
            .unwrap_or_else(|| format!("10.0.0.{n}"));
        self.private_ip(nic, &ip)
    }

    /// The address `ip` on the private network `nic`, as the instance's
    /// `ipAddresses` has it.
    fn private_ip(&self, nic: &str, ip: &str) -> Json {
        shaped_at(
            "instance",
            "/ipAddresses/0",
            json!({"ip": ip, "type": "private", "version": 4,
                   "networkId": nic, "gatewayIp": null}),
        )
    }

    /// Why a port on `nic` in `region` cannot have the address `ip`: it is
    /// no host of a subnet of the network there (its gateway is not), or a
    /// port of the network has it already.
    fn ip_refused(&self, region: &str, nic: &str, ip: &str) -> Option<(u16, Json)> {
        let subnets = self
            .networks
            .values()
            .find(|net| {
                net["regions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["openstackId"] == nic)
            })
            .and_then(|net| self.subnets.get(net["id"].as_str().unwrap_or_default()))
            .into_iter()
            .flatten()
            .filter(|s| s["ipPools"][0]["region"] == region);
        let held = subnets.into_iter().any(|s| {
            let gateway = s["gatewayIp"].as_str();
            is_host(s["cidr"].as_str().unwrap_or_default(), ip) && gateway != Some(ip)
        });
        if !held {
            let why = format!("Invalid ip {ip}: it is in no subnet of network {nic} in {region}");
            return Some((400, json!({"message": why})));
        }
        let taken = self.nics.iter().any(|(id, ports)| {
            ports.iter().filter(|p| p.network == nic).any(|p| {
                self.interface(id, p)["fixedIps"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|f| f["ip"] == ip)
            })
        });
        taken.then(|| {
            let why = format!("IP address {ip} is already allocated on network {nic}");
            (409, json!({"message": why}))
        })
    }

    /// Instance `id`'s port `port`, with the IPv4 address it has
    /// (`cloud.instanceInterface.Interface`): an attached port's own, one
    /// made with the instance the instance's on the network.
    fn interface(&self, id: &str, port: &Port) -> Json {
        let nic = port.network.as_str();
        let (typ, on) = match port.is_public() {
            true => ("public", "ext"),
            false => ("private", nic),
        };
        let fixed_ip = |a: &Json| {
            shaped_at(
                "interface",
                "/fixedIps/0",
                json!({"ip": a["ip"], "subnetId": format!("sub-{nic}")}),
            )
        };
        let fixed: Vec<Json> = match &port.ip {
            Some(a) => vec![fixed_ip(a)],
            None => self
                .instances
                .get(id)
                .and_then(|o| o["ipAddresses"].as_array())
                .into_iter()
                .flatten()
                .filter(|a| a["networkId"] == on && a["version"] == 4)
                .map(fixed_ip)
                .collect(),
        };
        // A private interface names its network by the network's id
        // (`pn-…`), not the OpenStack id it was attached by, as OVH does.
        let network = self
            .networks
            .values()
            .find(|net| {
                net["regions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|r| r["openstackId"] == nic)
            })
            .and_then(|net| net["id"].as_str())
            .unwrap_or(nic);
        shaped(
            "interface",
            json!({"id": port.id, "networkId": network, "type": typ, "fixedIps": fixed}),
        )
    }

    /// Why `nic` (an OpenStack network id) cannot be an interface of an
    /// instance in `region`: not a network there, or a private one with
    /// no subnet there.
    fn nic_refused(&self, region: &str, nic: &str) -> Option<String> {
        let public = nic == format!("ext-{region}");
        let private = self.networks.values().find(|net| {
            net["regions"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|r| r["openstackId"] == nic && r["region"] == region)
        });
        if !public && private.is_none() {
            return Some(format!("Invalid networkId {nic}"));
        }
        if let Some(net) = private
            && self
                .subnets
                .get(net["id"].as_str().unwrap_or_default())
                .is_none_or(|s| !s.iter().any(|s| s["ipPools"][0]["region"] == region))
        {
            return Some(format!("Network {nic} has no subnet in {region}"));
        }
        None
    }

    /// A flavor of `region` by its id: its vCPUs, RAM and disk.
    fn flavor_size(region: &str, id: &str) -> Option<[i64; 3]> {
        let flavors = Self::flavors(region);
        let f = flavors.as_array()?.iter().find(|f| f["id"] == id)?;
        let n = |k: &str| f[k].as_i64().unwrap_or(0);
        Some([n("vcpus"), n("ram"), n("disk")])
    }
}

/// Whether `ip` is a host of `cidr`: neither its network's address nor
/// its broadcast.
fn is_host(cidr: &str, ip: &str) -> bool {
    let (Some((net, prefix)), Some(ip)) = (
        dform_core::value::parse_ipnet(cidr),
        dform_core::value::ipv4_to_u32(ip),
    ) else {
        return false;
    };
    let size = 1u64 << (32 - u32::from(prefix));
    let (first, last) = (u64::from(net) + 1, u64::from(net) + size - 2);
    (first..=last).contains(&u64::from(ip))
}

/// A public IPv6 and IPv4 address, the `n`th.
fn ips(n: usize) -> Json {
    let ip = |fields: Json| shaped_at("instance", "/ipAddresses/0", fields);
    json!([
        ip(
            json!({"ip": format!("2607:5300::{n}"), "type": "public", "version": 6,
                  "networkId": "ext", "gatewayIp": null})
        ),
        ip(
            json!({"ip": format!("51.79.0.{n}"), "type": "public", "version": 4,
                  "networkId": "ext", "gatewayIp": "51.79.0.1"})
        )
    ])
}
