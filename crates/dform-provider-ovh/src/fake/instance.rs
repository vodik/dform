//! Instances: listed, made BUILD then ACTIVE, renamed, resized (RESIZE, to a
//! flavor no smaller), their interfaces attached and detached, deleted.

use super::*;

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
                // in that region, as OpenStack names them.
                let nics: Option<Vec<String>> =
                    body.get("networks").and_then(Json::as_array).map(|ns| {
                        ns.iter()
                            .filter_map(|n| n["networkId"].as_str().map(str::to_string))
                            .collect()
                    });
                for nic in nics.iter().flatten() {
                    if let Some(why) = self.nic_refused(region, nic) {
                        return (400, json!({"message": why}));
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
                self.read_settling(&format!("instance/{id}"));
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
                let nics = self
                    .nics
                    .get(*id)
                    .cloned()
                    .unwrap_or_else(|| vec![format!("ext-{region}")]);
                let all: Vec<Json> = nics.iter().map(|nic| interface(nic)).collect();
                (200, json!(all))
            }
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
                let nics = self
                    .nics
                    .entry(id.clone())
                    .or_insert_with(|| vec![format!("ext-{region}")]);
                if nics.contains(&nic) {
                    return (
                        400,
                        json!({"message": format!("Network {nic} is attached already")}),
                    );
                }
                nics.push(nic.clone());
                let ip = self.private_address(&nic, self.instances.len() + 10);
                let o = self.instances.get_mut(&id).expect("looked up");
                if let Some(ips) = o["ipAddresses"].as_array_mut() {
                    ips.push(ip);
                }
                (200, interface(&nic))
            }
            ("DELETE", ["cloud", "project", _, "instance", id, "interface", nic]) => {
                let Some(nic) = nic.strip_prefix("if-") else {
                    return not_found("Interface");
                };
                let Some(nics) = self.nics.get_mut(*id) else {
                    return not_found("Interface");
                };
                if nic.starts_with("ext-") || !nics.iter().any(|n| n == nic) {
                    return not_found("Interface");
                }
                nics.retain(|n| n != nic);
                if let Some(ips) = self
                    .instances
                    .get_mut(*id)
                    .and_then(|o| o["ipAddresses"].as_array_mut())
                {
                    ips.retain(|a| a["networkId"] != nic);
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
            out.push(self.private_address(nic, n));
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
        json!({"ip": ip, "type": "private", "version": 4,
               "networkId": nic, "gatewayIp": null})
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

/// An instance's interface on the network `nic` (an OpenStack id)
/// (`cloud.instanceInterface.Interface`).
fn interface(nic: &str) -> Json {
    let typ = if nic.starts_with("ext-") {
        "public"
    } else {
        "private"
    };
    json!({"id": format!("if-{nic}"), "networkId": nic, "type": typ, "state": "ACTIVE",
           "macAddress": "fa:16:3e:00:00:01", "fixedIps": []})
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
