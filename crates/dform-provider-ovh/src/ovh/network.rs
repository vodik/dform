//! Private networks (`ovh.network`): the API's
//! `/cloud/project/{p}/network/private/{id}`, a VLAN of the vRack the
//! project is on, in some of its regions. The only network a program
//! makes: the public one is OVH's. The vRack itself is the account's,
//! attached to the project outside dform (as a DNS zone is the
//! account's): a project without one is refused at Plan naming it, not
//! with the API's text. A network is named by its name; it is added to a
//! region in place (`POST /region`), and leaves none (another VLAN or
//! fewer regions replace it).

use super::*;

impl Ovh {
    pub(super) fn read_network(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<Json>> {
        a.client.get_opt(&network_path(p, remote))
    }

    /// Whether the project is on a vRack, asked once; a project that is
    /// not refuses a private network naming it. An account that cannot be
    /// reached is not judged here.
    pub(super) fn check_vrack(&self, at: &str) -> Result<()> {
        let Ok((a, p)) = self.project(at) else {
            return Ok(());
        };
        let known = self
            .vrack
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&p)
            .copied();
        let on = match known {
            Some(on) => on,
            None => {
                let on = a
                    .client
                    .get_opt(&format!("/cloud/project/{p}/vrack"))?
                    .is_some();
                self.vrack
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(p.clone(), on);
                on
            }
        };
        if !on {
            bail!("{at}: {}", no_vrack(&p));
        }
        Ok(())
    }

    pub(super) fn create_network(
        &self,
        at: &str,
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let mut body = json!({"name": need(at, config, "name")?});
        if let Some(rs) = regions(at, config)? {
            body["regions"] = json!(rs);
        }
        if let Some(v) = config.get("vlan_id") {
            body["vlanId"] = v
                .as_i64()
                .map(Json::from)
                .ok_or_else(|| refused(at, format!("vlan_id {v} is not a number")))?;
        }
        let o = a
            .client
            .post(&format!("/cloud/project/{p}/network/private"), &body)
            .map_err(|e| match e.status() {
                // The API's refusal of a project with no vRack is said as
                // what it is.
                Some(400..500)
                    if matches!(
                        a.client.get_opt(&format!("/cloud/project/{p}/vrack")),
                        Ok(None)
                    ) =>
                {
                    refused(at, no_vrack(&p))
                }
                _ => failed(at, e),
            })?;
        let id = s(&o, "id").unwrap_or_default().to_string();
        let path = network_path(&p, &id);
        let o = self.settle(
            &a,
            at,
            &path,
            o,
            &["ACTIVE"],
            &["ERROR"],
            CREATE_WAIT,
            notes,
            say,
        )?;
        let (attrs, computed) = map::network(&o);
        Ok((id, attrs, computed))
    }

    /// Its name in place, and the regions the program adds.
    pub(super) fn update_network(
        &self,
        at: &str,
        remote: &str,
        now: &(Json, Json),
        config: &Json,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = network_path(&p, remote);
        let name = need(at, config, "name")?;
        if s(&now.0, "name") != Some(name) {
            a.client
                .put(&path, &json!({"name": name}))
                .map_err(|e| failed(at, e))?;
        }
        let has: Vec<&str> = now
            .1
            .get("regions")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(Json::as_str)
            .collect();
        let mut last = None;
        for r in regions(at, config)?.into_iter().flatten() {
            if !has.contains(&r.as_str()) {
                last = Some(
                    a.client
                        .post(&format!("{path}/region"), &json!({"region": r}))
                        .map_err(|e| failed(at, e))?,
                );
            }
        }
        if let Some(o) = last {
            self.settle(
                &a,
                at,
                &path,
                o,
                &["ACTIVE"],
                &["ERROR"],
                CREATE_WAIT,
                notes,
                say,
            )?;
        }
        Ok(())
    }
}

fn network_path(p: &str, remote: &str) -> String {
    format!("/cloud/project/{p}/network/private/{}", escape(remote))
}

/// Why a private network cannot be made in project `p`.
fn no_vrack(p: &str) -> String {
    format!(
        "project {p} is not on a vRack, and a private network is a VLAN of one: attach the \
         account's vRack to the project in the OVH control panel (it is the account's, \
         outside dform)"
    )
}

/// The OpenStack id of `net` (a network as the API answers it, public or
/// private) in `region`.
fn openstack_id(net: &Json, region: &str) -> Option<String> {
    net.get("regions")?
        .as_array()?
        .iter()
        .find(|r| s(r, "region") == Some(region))
        .and_then(|r| s(r, "openstackId"))
        .map(str::to_string)
}

/// An instance document's private networks, by their ids: none when it
/// names none.
fn networks_of<'a>(at: &str, doc: &'a Json) -> std::result::Result<Vec<&'a str>, Failed> {
    match doc.get("networks") {
        None | Some(Json::Null) => Ok(Vec::new()),
        Some(Json::Array(ns)) => ns
            .iter()
            .map(|n| {
                n.as_str().ok_or_else(|| {
                    refused(
                        at,
                        format!(
                            "networks: {} is not a network's id",
                            provider::fmt_value(Some(n))
                        ),
                    )
                })
            })
            .collect(),
        Some(v) => Err(refused(
            at,
            format!("networks is {}, not a list", provider::fmt_value(Some(v))),
        )),
    }
}

/// The program's regions, if it names them.
fn regions(at: &str, config: &Json) -> std::result::Result<Option<Vec<String>>, Failed> {
    match config.get("regions") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Array(rs)) => rs
            .iter()
            .map(|r| {
                r.as_str()
                    .map(str::to_string)
                    .ok_or_else(|| refused(at, format!("regions: {r} is not a region's name")))
            })
            .collect::<std::result::Result<Vec<_>, _>>()
            .map(Some),
        Some(v) => Err(refused(
            at,
            format!(
                "regions is {}, not a list of names",
                provider::fmt_value(Some(v))
            ),
        )),
    }
}

/// Subnets (`ovh.subnet`): the API's
/// `/cloud/project/{p}/network/private/{id}/subnet/{subnet}`, a range of a
/// private network in one region and the pool of it OpenStack hands out.
/// It is its own object (it has an id), named by its network, region and
/// range; nothing of it changes in place.
impl Ovh {
    /// A network's subnets; none when the network is gone.
    pub(super) fn subnets(&self, a: &Account, p: &str, network: &str) -> api::Result<Vec<Json>> {
        Ok(a.client
            .get_opt(&format!("{}/subnet", network_path(p, network)))?
            .and_then(|l| l.as_array().cloned())
            .unwrap_or_default())
    }

    /// The API has no GET of one subnet: its network's are listed.
    pub(super) fn read_subnet(
        &self,
        a: &Account,
        p: &str,
        remote: &str,
    ) -> api::Result<Option<Json>> {
        let (network, id) = map::subnet_parts(remote);
        Ok(self
            .subnets(a, p, network)?
            .into_iter()
            .find(|o| s(o, "id") == Some(id)))
    }

    pub(super) fn create_subnet(
        &self,
        at: &str,
        config: &Json,
    ) -> std::result::Result<(String, Json, Json), Failed> {
        let (a, p) = self.project_for(at)?;
        let network = need(at, config, "network")?;
        let flag = |k: &str| config.get(k).and_then(Json::as_bool).unwrap_or(false);
        let range = need(at, config, "range")?;
        let pool = match s(config, "pool") {
            Some(p) => p.to_string(),
            None => map::default_pool(range, flag("no_gateway"))
                .ok_or_else(|| refused(at, format!("range {range} has no host for a pool")))?,
        };
        let (start, end) = map::pool_ends(&pool)
            .ok_or_else(|| refused(at, format!("pool {pool} is not a range of addresses")))?;
        let body = json!({
            "region": need(at, config, "region")?,
            "network": range,
            "start": start,
            "end": end,
            "dhcp": flag("dhcp"),
            "noGateway": flag("no_gateway"),
        });
        let o = a
            .client
            .post(&format!("{}/subnet", network_path(&p, network)), &body)
            .map_err(|e| failed(at, e))?;
        let (attrs, computed) = map::subnet(network, &o);
        let id = map::subnet_remote(network, s(&o, "id").unwrap_or_default());
        Ok((id, attrs, computed))
    }
}

/// A subnet's document as Plan sees it: its pool written as `pool`, not
/// as the API's `start` and `end`, inside its range's hosts and without
/// its gateway. The pool it is given when the program sets none
/// (`map::default_pool`).
pub(super) fn check_subnet(at: &str, d: &Json) -> Result<Option<String>> {
    if let Some(k) = ["start", "end"].into_iter().find(|k| d.get(*k).is_some()) {
        let pool = match (s(d, "start"), s(d, "end")) {
            (Some(a), Some(b)) => map::pool_text(a, b),
            _ => map::pool_text("FIRST", "LAST"),
        };
        bail!(
            "{at}: {k} is not an attribute of ovh.subnet: the pool's first and last address are one, `pool = \"{pool}\"`"
        );
    }
    let Some(range) = s(d, "range") else {
        return Ok(None);
    };
    let no_gateway = d.get("no_gateway").and_then(Json::as_bool).unwrap_or(false);
    let Some((first, last)) = map::pool_hosts(range, no_gateway) else {
        bail!("{at}: range {range} has no host for a pool");
    };
    // A pool not known yet (a null) is checked at Apply.
    let pool = match d.get("pool") {
        None => return Ok(map::default_pool(range, no_gateway)),
        Some(p) => p.as_str().unwrap_or_default(),
    };
    let Some((a, b)) = dform_core::range::Range::ips(pool) else {
        return Ok(None);
    };
    let ip = dform_core::value::u32_to_ipv4;
    let hosts = map::pool_text(&ip(first), &ip(last));
    // The gateway is the range's first host, the one before `first`.
    let gateway = first - 1;
    if !no_gateway && a <= gateway && gateway <= b {
        bail!(
            "{at}: pool {pool} holds the subnet's gateway, {}: its hosts are {hosts} (or no_gateway = true)",
            ip(gateway)
        );
    }
    if a < first || b > last {
        bail!("{at}: pool {pool} is not in range {range}: its hosts are {hosts}");
    }
    Ok(None)
}

/// An instance's private networks (`networks = [lab]` on `ovh.instance`):
/// made on them, its interfaces are the public network's and one on each,
/// as the API takes them, by the networks' OpenStack ids in its region.
/// A network added later is an interface attached (`POST
/// /instance/{id}/interface`), one left an interface detached (`DELETE
/// /instance/{id}/interface/{interface}`), on the instance's id.
impl Ovh {
    /// The `networks` an instance's Create sends: none when the program
    /// sets none (the public network alone, as the API makes it); else the
    /// public network's first, then each private one's, in the instance's
    /// region.
    pub(super) fn instance_networks(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        region: &str,
        config: &Json,
    ) -> std::result::Result<Option<Vec<Json>>, Failed> {
        let ids = networks_of(at, config)?;
        if ids.is_empty() {
            return Ok(None);
        }
        let public = a
            .client
            .get(&format!("/cloud/project/{p}/network/public"))
            .map_err(|e| failed(at, e))?;
        let ext = public
            .as_array()
            .into_iter()
            .flatten()
            .find_map(|net| openstack_id(net, region))
            .ok_or_else(|| refused(at, format!("region {region} has no public network")))?;
        let mut out = vec![json!({"networkId": ext})];
        for id in ids {
            out.push(json!({"networkId": self.private_network_in(a, p, at, region, id)?}));
        }
        Ok(Some(out))
    }

    /// The OpenStack id of private network `id` in `region`, as an
    /// interface names it; a network not in the region is refused naming
    /// the regions it is in.
    fn private_network_in(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        region: &str,
        id: &str,
    ) -> std::result::Result<String, Failed> {
        let net = self
            .read_network(a, p, id)
            .map_err(|e| failed(at, e))?
            .ok_or_else(|| refused(at, format!("network {id} is not there")))?;
        openstack_id(&net, region).ok_or_else(|| {
            let (_, computed) = map::network(&net);
            refused(
                at,
                format!(
                    "network {} is not in region {region} (it is in {})",
                    s(&net, "name").unwrap_or(id),
                    computed["regions"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Json::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            )
        })
    }

    /// The instance `remote` brought onto the program's private networks
    /// (`config`) from those it is on (`now`, as Read answered it): an
    /// interface attached on each network added, and the one on each
    /// network left detached, each said.
    pub(super) fn update_instance_networks(
        &self,
        at: &str,
        remote: &str,
        now: &Json,
        config: &Json,
        notes: &mut Vec<String>,
    ) -> std::result::Result<(), Failed> {
        let want = networks_of(at, config)?;
        let has = networks_of(at, now)?;
        let (added, left): (Vec<&str>, Vec<&str>) = (
            want.iter().copied().filter(|n| !has.contains(n)).collect(),
            has.iter().copied().filter(|n| !want.contains(n)).collect(),
        );
        if added.is_empty() && left.is_empty() {
            return Ok(());
        }
        let (a, p) = self.project_for(at)?;
        let region = need(at, config, "region")?;
        let path = format!("/cloud/project/{p}/instance/{}/interface", escape(remote));
        for id in added {
            let os = self.private_network_in(&a, &p, at, region, id)?;
            a.client
                .post(&path, &json!({"networkId": os}))
                .map_err(|e| failed(at, e))?;
            notes.push(format!("{at}: an interface on network {id} is attached"));
        }
        if left.is_empty() {
            return Ok(());
        }
        let interfaces = a.client.get(&path).map_err(|e| failed(at, e))?;
        for id in left {
            let os = self.private_network_in(&a, &p, at, region, id)?;
            let Some(nic) = interfaces
                .as_array()
                .into_iter()
                .flatten()
                .find(|i| s(i, "networkId") == Some(os.as_str()))
                .and_then(|i| s(i, "id"))
            else {
                continue;
            };
            match a.client.delete(&format!("{path}/{}", escape(nic))) {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(failed(at, e)),
            }
            notes.push(format!("{at}: its interface on network {id} is detached"));
        }
        Ok(())
    }

    /// The project's private networks by the OpenStack ids of their
    /// regions, when the instance `o` has a private address (else none is
    /// asked for).
    pub(super) fn private_networks(
        &self,
        a: &Account,
        p: &str,
        o: &Json,
    ) -> BTreeMap<String, String> {
        let private = o
            .get("ipAddresses")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .any(|x| s(x, "type") == Some("private"));
        if !private {
            return BTreeMap::new();
        }
        let Ok(list) = a.client.get(&format!("/cloud/project/{p}/network/private")) else {
            return BTreeMap::new();
        };
        let mut out = BTreeMap::new();
        for net in list.as_array().into_iter().flatten() {
            let Some(id) = s(net, "id") else { continue };
            for r in net
                .get("regions")
                .and_then(Json::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(os) = s(r, "openstackId") {
                    out.insert(os.to_string(), id.to_string());
                }
            }
        }
        out
    }
}
