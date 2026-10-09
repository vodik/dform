//! The vRack, the public network, and private networks (BUILDING then
//! ACTIVE, added to a region) with their subnets.

use super::*;

impl World {
    /// A call under the network routes.
    pub(super) fn network_api(
        &mut self,
        method: &str,
        path: &str,
        segs: &[&str],
        body: &Json,
    ) -> (u16, Json) {
        match (method, segs) {
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
            _ => (404, json!({"message": format!("no route {method} {path}")})),
        }
    }
}

/// The public network, in every region (`cloud.network.Network`).
fn public_network() -> Json {
    json!({"id": "ext-net", "name": "Ext-Net", "type": "public", "vlanId": null,
           "status": "ACTIVE",
           "regions": REGIONS.iter().map(|r| json!({"region": r, "status": "ACTIVE",
                                                    "openstackId": format!("ext-{r}")}))
                             .collect::<Vec<_>>()})
}

/// The vRack the project is on.
const VRACK: &str = "pn-1000123";
