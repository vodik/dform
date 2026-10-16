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
use std::collections::BTreeSet;
use std::net::Ipv4Addr;

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
            .post(&format!("/cloud/project/{p}/network/private"), Some(&body))
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
                        .post(&format!("{path}/region"), Some(&json!({"region": r})))
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

    /// The private network named `name`, unless it is being deleted.
    pub(super) fn find_network(&self, name: &str) -> Result<Option<String>> {
        let (a, p) = self.project("find a private network")?;
        let list = a
            .client
            .get(&format!("/cloud/project/{p}/network/private"))?;
        Ok(list
            .as_array()
            .into_iter()
            .flatten()
            .filter(|o| s(o, "status") != Some("DELETING"))
            .find(|o| s(o, "name") == Some(name))
            .and_then(|o| s(o, "id"))
            .map(str::to_string))
    }

    pub(super) fn delete_network(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let path = format!("/cloud/project/{p}/network/private/{}", escape(remote));
        self.delete_at(&a, at, &path, true, notes, say)
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

/// An entry of an instance's `networks`: a private network
/// (`networks = [lab]`), or one and the address its port is made with
/// (`{ network = lab, ip = "10.42.0.10" }`), the address DHCP's otherwise.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Entry {
    /// The network's id; the label of the value it will be when it is not
    /// known yet (a network the same plan makes), `known` false.
    pub network: String,
    pub known: bool,
    /// The address asked, if one is and it is known.
    pub ip: Option<Ipv4Addr>,
}

impl Entry {
    /// The entry `n` writes: a network, or `{ network, ip }`.
    fn of(n: &Json) -> std::result::Result<Entry, String> {
        let shown = |v: &Json| provider::fmt_value(Some(v));
        let network = |v: &Json| match (v.as_str(), marker(v)) {
            (Some(id), _) => Ok((id.to_string(), true)),
            (_, Some((_, label))) => Ok((label.to_string(), false)),
            _ => Err(format!("networks: {} is not a network", shown(v))),
        };
        let Some(fields) = n.as_object().filter(|_| marker(n).is_none()) else {
            let (network, known) = network(n)?;
            return Ok(Entry {
                network,
                known,
                ip: None,
            });
        };
        if let Some(k) = fields
            .keys()
            .find(|k| !["network", "ip"].contains(&k.as_str()))
        {
            return Err(format!(
                "networks: an entry is a network or `{{ network, ip }}`, and {k} is neither"
            ));
        }
        let (network, known) = network(
            fields
                .get("network")
                .ok_or("networks: an entry `{ ip }` names its network too (`network = lab`)")?,
        )?;
        let ip = match fields.get("ip") {
            None | Some(Json::Null) => None,
            Some(v) if marker(v).is_some() => None,
            Some(v) => Some(
                v.as_str()
                    .and_then(|t| t.parse().ok())
                    .ok_or_else(|| format!("networks: ip {} is not an IPv4 address", shown(v)))?,
            ),
        };
        Ok(Entry { network, known, ip })
    }
}

/// An instance document's private networks, each once: none when it
/// names none.
pub(super) fn entries(doc: &Json) -> std::result::Result<Vec<Entry>, String> {
    let list = match doc.get("networks") {
        None | Some(Json::Null) => return Ok(Vec::new()),
        Some(Json::Array(ns)) => ns,
        Some(v) => {
            return Err(format!(
                "networks is {}, not a list",
                provider::fmt_value(Some(v))
            ));
        }
    };
    let mut out: Vec<Entry> = Vec::new();
    for n in list {
        let e = Entry::of(n)?;
        if out.iter().any(|o| o.network == e.network) {
            return Err(format!(
                "networks: network {} is named twice, and an instance has one port on a network",
                e.network
            ));
        }
        out.push(e);
    }
    Ok(out)
}

/// The entries of an Apply's document, each network known.
fn entries_known(at: &str, doc: &Json) -> std::result::Result<Vec<Entry>, Failed> {
    let all = entries(doc).map_err(|e| refused(at, e))?;
    match all.iter().find(|e| !e.known) {
        Some(e) => Err(refused(
            at,
            format!("networks: network {} is not known yet", e.network),
        )),
        None => Ok(all),
    }
}

/// The networks of `doc` it asks an address on.
pub(super) fn fixed(doc: &Json) -> BTreeSet<String> {
    entries(doc)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.ip.is_some())
        .map(|e| e.network)
        .collect()
}

/// An address asked on a network an instance is on that is not the one
/// it has there: no API call changes a port's address, so it replaces
/// the instance.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Readdress {
    network: String,
    /// The address it has, asked; none where it was DHCP's.
    had: Option<Ipv4Addr>,
    asked: Ipv4Addr,
}

impl std::fmt::Display for Readdress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (net, asked) = (&self.network, self.asked);
        match self.had {
            Some(had) => write!(
                f,
                "its address on network {net} changes from {had} to {asked}"
            ),
            None => write!(
                f,
                "its address on network {net} is asked ({asked}), and was DHCP's"
            ),
        }
    }
}

/// The addresses `d` asks anew of `prior` (an instance as Read answers
/// it): on each network it is on, an address asked that it has not.
pub(super) fn readdressed(prior: &Json, d: &Json) -> Vec<Readdress> {
    let (Ok(was), Ok(now)) = (entries(prior), entries(d)) else {
        return Vec::new();
    };
    now.into_iter()
        .filter_map(|e| {
            let asked = e.ip?;
            let had = was.iter().find(|w| w.network == e.network)?.ip;
            (had != Some(asked)).then_some(Readdress {
                network: e.network,
                had,
                asked,
            })
        })
        .collect()
}

/// The addresses `config` asks that `computed` (an instance's, as Read
/// answers it) has already, each `IP on network NET`.
pub(super) fn held(computed: &Json, config: &Json) -> Vec<String> {
    entries(config)
        .unwrap_or_default()
        .into_iter()
        .filter_map(|e| {
            let ip = e.ip?.to_string();
            let has = computed.get("private_ips").and_then(|m| s(m, &e.network));
            (has == Some(ip.as_str())).then(|| format!("{ip} on network {}", e.network))
        })
        .collect()
}

/// The addresses instances asked at Plan in this process, by network and
/// address, each with the instance that asked it: an address is one
/// port's, so a second instance asking it is refused.
#[derive(Debug, Default)]
pub(super) struct Asked(BTreeMap<(String, Ipv4Addr), String>);

impl Asked {
    /// `who`'s addresses now `asks`, the ones it asked before dropped;
    /// the first of them another instance asked, with that instance.
    fn ask(
        &mut self,
        who: &str,
        asks: &[(String, Ipv4Addr)],
    ) -> Option<(String, Ipv4Addr, String)> {
        self.0.retain(|_, w| w != who);
        if let Some(((net, ip), other)) = asks.iter().find_map(|a| self.0.get_key_value(a)) {
            return Some((net.clone(), *ip, other.clone()));
        }
        for a in asks {
            self.0.insert(a.clone(), who.to_string());
        }
        None
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

/// The subnets of a network as a refusal names them: each range and its
/// region, sorted.
fn subnet_list(subnets: &[(Json, Json)]) -> String {
    let mut all: Vec<String> = subnets
        .iter()
        .map(|(attrs, _)| {
            format!(
                "{} in {}",
                s(attrs, "range").unwrap_or_default(),
                s(attrs, "region").unwrap_or_default()
            )
        })
        .collect();
    all.sort();
    all.join(", ")
}

/// Whether `ip` is one of the hosts a port of subnet `sub` (its document,
/// `map::subnet`) may have: in its range, not its network's address,
/// broadcast or gateway.
fn holds(sub: &(Json, Json), ip: Ipv4Addr) -> bool {
    let no_gateway = sub
        .1
        .get("no_gateway")
        .and_then(Json::as_bool)
        .unwrap_or(false);
    s(&sub.0, "range")
        .and_then(|r| map::pool_hosts(r, no_gateway))
        .is_some_and(|(first, last)| (first..=last).contains(&u32::from(ip)))
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
            .post(
                &format!("{}/subnet", network_path(&p, network)),
                Some(&body),
            )
            .map_err(|e| failed(at, e))?;
        let (attrs, computed) = map::subnet(network, &o);
        let id = map::subnet_remote(network, s(&o, "id").unwrap_or_default());
        Ok((id, attrs, computed))
    }

    /// The subnet of `network` in `region` over `range`.
    pub(super) fn find_subnet(
        &self,
        network: &str,
        region: &str,
        range: &str,
    ) -> Result<Option<String>> {
        let (a, p) = self.project("find a subnet")?;
        Ok(self
            .subnets(&a, &p, network)?
            .iter()
            .map(|o| map::subnet(network, o))
            .find(|(attrs, _)| {
                s(attrs, "region") == Some(region) && s(attrs, "range") == Some(range)
            })
            .and_then(|(_, computed)| s(&computed, "id").map(str::to_string)))
    }

    pub(super) fn delete_subnet(
        &self,
        at: &str,
        remote: &str,
        notes: &mut Vec<String>,
        say: Say,
    ) -> std::result::Result<(), Failed> {
        let (a, p) = self.project_for(at)?;
        let (network, id) = map::subnet_parts(remote);
        let path = format!(
            "/cloud/project/{p}/network/private/{}/subnet/{}",
            escape(network),
            escape(id)
        );
        self.delete_at(&a, at, &path, false, notes, say)
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
/// An Update brings its ports to one on each network against them as
/// they are (`PortSteps`): a network added is an interface attached
/// (`POST /instance/{id}/interface`), a port beside its own or on a
/// network left an interface detached (`DELETE
/// /instance/{id}/interface/{interface}`), on the instance's id.
impl Ovh {
    /// The `networks` an instance's Create sends: none when the program
    /// sets none (the public network alone, as the API makes it); else the
    /// public network's first, then each private one's, in the instance's
    /// region, with the address asked of its port where one is.
    pub(super) fn instance_networks(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        region: &str,
        config: &Json,
    ) -> std::result::Result<Option<Vec<Json>>, Failed> {
        let entries = entries_known(at, config)?;
        if entries.is_empty() {
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
        for e in entries {
            out.push(self.port_body(a, p, at, region, &e)?);
        }
        Ok(Some(out))
    }

    /// What makes a port on `e`'s network in `region`, the instance's
    /// create's `networks[]` and an attach's body alike: the network's
    /// OpenStack id, and the address asked, checked against its subnets.
    fn port_body(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        region: &str,
        e: &Entry,
    ) -> std::result::Result<Json, Failed> {
        let os = self.private_network_in(a, p, at, region, &e.network)?;
        let Some(ip) = e.ip else {
            return Ok(json!({"networkId": os}));
        };
        let held = self
            .address_held(a, p, region, &e.network, ip)
            .map_err(|e| failed(at, e))?;
        held.map_err(|why| refused(at, why))?;
        Ok(json!({"networkId": os, "ip": ip.to_string()}))
    }

    /// Whether `ip` is a host of a subnet of `network` in `region`, as the
    /// API lists them: why not, naming the network's subnets. Not judged
    /// (held) where the network has no subnet in the region yet.
    fn address_held(
        &self,
        a: &Account,
        p: &str,
        region: &str,
        network: &str,
        ip: Ipv4Addr,
    ) -> api::Result<std::result::Result<(), String>> {
        let subnets: Vec<(Json, Json)> = self
            .subnets(a, p, network)?
            .iter()
            .map(|o| map::subnet(network, o))
            .collect();
        let here: Vec<&(Json, Json)> = subnets
            .iter()
            .filter(|(attrs, _)| s(attrs, "region") == Some(region))
            .collect();
        if here.is_empty() || here.iter().any(|sub| holds(sub, ip)) {
            return Ok(Ok(()));
        }
        Ok(Err(format!(
            "networks: {ip} is no host of a subnet of network {network} in {region} (nor its \
             gateway): its subnets are {}",
            subnet_list(&subnets)
        )))
    }

    /// An instance's `networks` at Plan (`d`, `who` the instance as the
    /// plan names it, `ovh.instance server`): each
    /// entry a network or `{ network, ip }`, each network once; each
    /// address asked a host of a subnet of its network in the instance's
    /// region, where the account says (a network the plan makes, or one
    /// with no subnet there yet, is judged at Apply); and none asked by
    /// another instance this provider planned (`Asked`).
    pub(super) fn check_networks(&self, at: &str, who: &str, d: &Json) -> Result<()> {
        let entries = entries(d).map_err(|e| anyhow!("{at}: {e}"))?;
        let asks: Vec<(String, Ipv4Addr)> = entries
            .iter()
            .filter_map(|e| Some((e.network.clone(), e.ip?)))
            .collect();
        let taken = self
            .asked
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .ask(who, &asks);
        if let Some((net, ip, other)) = taken {
            let mut both = [who, other.as_str()];
            both.sort();
            bail!(
                "{at}: networks: {ip} on network {net} is asked by both {} and {}, and an \
                 address is one port's: give each instance its own",
                both[0],
                both[1]
            );
        }
        let (Ok((a, p)), Some(region)) = (self.project(at), s(d, "region")) else {
            return Ok(());
        };
        for e in entries.iter().filter(|e| e.known) {
            if let Some(ip) = e.ip {
                self.address_held(&a, &p, region, &e.network, ip)?
                    .map_err(|why| anyhow!("{at}: {why}"))?;
            }
        }
        Ok(())
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

    /// The steps that bring instance `remote` to one port on each of the
    /// program's private networks (`config`) and none on another, from
    /// its ports as they are (a fresh read, not Read's answer), `owned`
    /// which are its own (its prior's `ports`). Refused, before anything
    /// changes, where a network has several ports and none is known as
    /// its own (which to keep is a person's choice), and where the port
    /// it keeps has another address than the one asked (Plan replaces
    /// the instance for that).
    pub(super) fn instance_ports(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        remote: &str,
        config: &Json,
        owned: &map::Owned,
    ) -> std::result::Result<PortSteps, Failed> {
        let entries = entries_known(at, config)?;
        let want: Vec<&str> = entries.iter().map(|e| e.network.as_str()).collect();
        let nics = self.interfaces(a, p, remote).map_err(|e| failed(at, e))?;
        let steps = PortSteps::new(&want, &nics.private, owned);
        let refusal = steps.refusal(at, &interface_path(p, remote));
        match refusal.or_else(|| steps.readdressed(at, &entries)) {
            Some(e) => Err(e),
            None => Ok(steps),
        }
    }

    /// `steps` taken on instance `remote`, each said: an interface
    /// attached on each network it has none on (with the address asked
    /// there), the others beside its own and the ones on a network left
    /// detached. Its own port on each network now, by network: the one it
    /// had, the one taken as its own, or the one attached (`map::Owned`).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn converge_ports(
        &self,
        a: &Account,
        p: &str,
        at: &str,
        remote: &str,
        config: &Json,
        steps: PortSteps,
        notes: &mut Vec<String>,
    ) -> std::result::Result<Vec<(String, String)>, Failed> {
        let path = interface_path(p, remote);
        let mut own: Vec<(String, String)> = steps
            .keep
            .into_iter()
            .map(|(net, port)| (net, port.id))
            .collect();
        for (net, port) in steps.adopt {
            notes.push(format!(
                "{at}: its one interface on network {net}, {}, is taken as its own",
                port.id
            ));
            own.push((net, port.id));
        }
        let entries = entries_known(at, config)?;
        for e in entries.iter().filter(|e| steps.attach.contains(&e.network)) {
            let (net, region) = (e.network.clone(), need(at, config, "region")?);
            let body = self.port_body(a, p, at, region, e)?;
            let port = a
                .client
                .post(&path, Some(&body))
                .map_err(|e| failed(at, e))?;
            let with = match e.ip {
                Some(ip) => format!(", with address {ip}"),
                None => String::new(),
            };
            notes.push(format!(
                "{at}: an interface on network {net} is attached{with}"
            ));
            if let Some(port) = s(&port, "id") {
                own.push((net, port.to_string()));
            }
        }
        for (net, port, why) in steps.detach {
            match a.client.delete(&format!("{path}/{}", escape(&port.id))) {
                Ok(_) => {}
                Err(e) if e.is_not_found() => {}
                Err(e) => return Err(failed(at, e)),
            }
            notes.push(format!(
                "{at}: its interface {} on network {net} is detached{why}",
                port.id
            ));
        }
        for (net, port) in steps.foreign {
            notes.push(format!(
                "{at}: interface {} on network {net} is not its own, and is left",
                port.id
            ));
        }
        Ok(own)
    }

    /// Instance `id`'s ports: the public one's address, and those on its
    /// private networks, by network, each with its address
    /// (`map::interfaces`). A failed call fails the read: a network left
    /// out would be attached again.
    pub(super) fn interfaces(
        &self,
        a: &Account,
        p: &str,
        id: &str,
    ) -> api::Result<map::Interfaces> {
        let ifaces = a.client.get(&interface_path(p, id))?;
        if ifaces.as_array().is_none_or(Vec::is_empty) {
            return Ok(map::Interfaces::default());
        }
        Ok(map::interfaces(&ifaces, &self.openstack_ids(a, p)?))
    }

    /// The project's private networks by each id an interface's
    /// `networkId` may be: the network's own (`pn-…`, what the API
    /// answers) and the OpenStack ids of its regions (what an attach
    /// sends).
    fn openstack_ids(&self, a: &Account, p: &str) -> api::Result<BTreeMap<String, String>> {
        let list = a
            .client
            .get(&format!("/cloud/project/{p}/network/private"))?;
        let mut out = BTreeMap::new();
        for net in list.as_array().into_iter().flatten() {
            let Some(id) = s(net, "id") else { continue };
            out.insert(id.to_string(), id.to_string());
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
        Ok(out)
    }
}

/// `/instance/{remote}/interface` of project `p`.
fn interface_path(p: &str, remote: &str) -> String {
    format!("/cloud/project/{p}/instance/{}/interface", escape(remote))
}

/// What brings an instance to one port on each of its private networks
/// and none on a network it left, against its ports as they are, by
/// network id. A port is the instance's own when its prior names it
/// (`map::Owned`); only a network whose own port is known has the others
/// beside it detached, and a network with one port and none known takes
/// that one as its own. A network with several and none known is
/// `unknown`, refused: never "all but one" chosen blindly.
#[derive(Debug, Default, PartialEq)]
pub(super) struct PortSteps {
    /// Networks it has no port on: one attached on each.
    attach: Vec<String>,
    /// Its own port on each network it keeps, known.
    keep: Vec<(String, map::Port)>,
    /// The one port on a network it keeps, none known as its own: taken
    /// as its own.
    adopt: Vec<(String, map::Port)>,
    /// Ports detached, each with why: another beside its own, or its own
    /// (or the one port) on a network left.
    detach: Vec<(String, map::Port, String)>,
    /// Ports on a network left that are not its own while its own is
    /// known: someone's, left as they are.
    foreign: Vec<(String, map::Port)>,
    /// Networks with several ports and none known as its own.
    unknown: Vec<(String, Vec<map::Port>)>,
}

impl PortSteps {
    /// The steps from `ports` (the instance's, by network) to one on each
    /// network of `want`, `owned` its own.
    pub(super) fn new(want: &[&str], ports: &map::Ports, owned: &map::Owned) -> PortSteps {
        let mut steps = PortSteps::default();
        for net in want {
            match ports.get(*net).map(Vec::as_slice).unwrap_or_default() {
                [] => steps.attach.push(net.to_string()),
                on => steps.kept(net, on, owned.on(net, on)),
            }
        }
        for (net, on) in ports.iter().filter(|(n, _)| !want.contains(&n.as_str())) {
            steps.left(net, on, owned.on(net, on));
        }
        steps
    }

    /// Network `net` kept, `on` its ports there, `own` its own among them.
    fn kept(&mut self, net: &str, on: &[map::Port], own: Option<&map::Port>) {
        match (own, on) {
            (Some(own), _) => {
                self.keep.push((net.to_string(), own.clone()));
                let why = format!(", a second port beside its own ({})", own.id);
                self.detach
                    .extend(others(on, own).map(|p| (net.to_string(), p.clone(), why.clone())));
            }
            (None, [only]) => self.adopt.push((net.to_string(), only.clone())),
            (None, _) => self.unknown.push((net.to_string(), on.to_vec())),
        }
    }

    /// Network `net` left, `on` its ports there, `own` its own among them.
    fn left(&mut self, net: &str, on: &[map::Port], own: Option<&map::Port>) {
        match (own, on) {
            (Some(own), _) => {
                self.detach
                    .push((net.to_string(), own.clone(), String::new()));
                self.foreign
                    .extend(others(on, own).map(|p| (net.to_string(), p.clone())));
            }
            (None, [only]) => self
                .detach
                .push((net.to_string(), only.clone(), String::new())),
            (None, _) => self.unknown.push((net.to_string(), on.to_vec())),
        }
    }

    /// The refusal of a port it keeps whose address is not the one
    /// `entries` (the program's) asks there: no API call changes it, and
    /// Plan replaces the instance for it.
    fn readdressed(&self, at: &str, entries: &[Entry]) -> Option<Failed> {
        let asked = |net: &str| entries.iter().find(|e| e.network == net)?.ip;
        let (net, port, ip) = self
            .keep
            .iter()
            .chain(&self.adopt)
            .find_map(|(net, port)| {
                let ip = asked(net).filter(|ip| port.ipv4 != Some(ip.to_string()))?;
                Some((net, port, ip))
            })?;
        Some(refused(
            at,
            format!(
                "its port {} on network {net} has {}, not {ip} as asked, and no API call changes \
                 a port's address: plan again, which replaces the instance",
                port.id,
                port.ipv4.as_deref().unwrap_or("no address")
            ),
        ))
    }

    /// The refusal of the networks whose ports none is known as the
    /// instance's own, each port named with its address, so a person
    /// chooses which to keep; `path` the instance's interfaces.
    fn refusal(&self, at: &str, path: &str) -> Option<Failed> {
        if self.unknown.is_empty() {
            return None;
        }
        let nets: Vec<String> = self
            .unknown
            .iter()
            .map(|(net, on)| {
                let ports: Vec<String> = on.iter().map(port_text).collect();
                format!(
                    "network {net} has {} ports and none is known as its own: {}",
                    on.len(),
                    ports.join(", ")
                )
            })
            .collect();
        Some(refused(
            at,
            format!(
                "{}; it keeps one port on a network, and which one is not dform's to guess: \
                 detach the others (`DELETE {path}/PORT`, or in the OVH control panel), keeping \
                 the one in use, and apply again: the one left is taken as its own",
                nets.join("; ")
            ),
        ))
    }
}

/// The ports of `on` other than `own`.
fn others<'a>(on: &'a [map::Port], own: &'a map::Port) -> impl Iterator<Item = &'a map::Port> {
    on.iter().filter(move |p| p.id != own.id)
}

/// A port as a refusal names it: its id and its address.
fn port_text(p: &map::Port) -> String {
    match &p.ipv4 {
        Some(ip) => format!("{} ({ip})", p.id),
        None => format!("{} (no address)", p.id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn port(id: &str, ip: &str) -> map::Port {
        map::Port {
            id: id.into(),
            ipv4: Some(ip.into()),
        }
    }

    /// The loop's state on network `lab`: three ports.
    fn three() -> map::Ports {
        [(
            "lab".to_string(),
            vec![
                port("if-1", "10.0.0.11"),
                port("if-2", "10.0.0.12"),
                port("if-3", "10.0.0.13"),
            ],
        )]
        .into()
    }

    fn owning(net: &str, id: &str) -> map::Owned {
        map::Owned::named([(net.to_string(), id.to_string())].into())
    }

    fn none() -> map::Owned {
        map::Owned::from(None)
    }

    /// A network with no port is attached; one whose own port is there is
    /// left as it is.
    #[test]
    fn a_network_without_a_port_attaches_and_one_with_its_own_is_kept() {
        let ports = [("lab".to_string(), vec![port("if-1", "10.0.0.11")])].into();
        let steps = PortSteps::new(&["lab", "db"], &ports, &owning("lab", "if-1"));
        assert_eq!(steps.attach, ["db"]);
        assert_eq!(steps.keep, [("lab".to_string(), port("if-1", "10.0.0.11"))]);
        assert!(steps.detach.is_empty() && steps.adopt.is_empty() && steps.unknown.is_empty());
    }

    /// Its own port known among the loop's three: the two others are
    /// detached, its own kept, whichever place it has in the list.
    #[test]
    fn the_others_beside_its_own_port_are_detached() {
        let steps = PortSteps::new(&["lab"], &three(), &owning("lab", "if-2"));
        assert_eq!(steps.keep, [("lab".to_string(), port("if-2", "10.0.0.12"))]);
        let gone: Vec<&str> = steps.detach.iter().map(|(_, p, _)| p.id.as_str()).collect();
        assert_eq!(gone, ["if-1", "if-3"]);
        assert!(steps.detach[0].2.contains("beside its own (if-2)"));
        assert!(steps.refusal("at", "/i").is_none());
    }

    /// None known as its own: the one port there is taken as its own; of
    /// several, none is chosen, and the refusal names each with its
    /// address. The prior's port gone is none known.
    #[test]
    fn with_none_known_one_port_is_taken_and_several_are_refused() {
        let one = [("lab".to_string(), vec![port("if-1", "10.0.0.11")])].into();
        let steps = PortSteps::new(&["lab"], &one, &none());
        assert_eq!(
            steps.adopt,
            [("lab".to_string(), port("if-1", "10.0.0.11"))]
        );
        assert!(steps.detach.is_empty());
        for owned in [none(), owning("lab", "if-gone")] {
            let steps = PortSteps::new(&["lab"], &three(), &owned);
            assert!(steps.detach.is_empty() && steps.keep.is_empty() && steps.adopt.is_empty());
            let Some(Failed::Refused(m)) = steps.refusal("apply x", "/i/vm/interface") else {
                panic!("not refused");
            };
            assert!(
                m.starts_with(
                    "apply x: network lab has 3 ports and none is known as its own: \
                     if-1 (10.0.0.11), if-2 (10.0.0.12), if-3 (10.0.0.13); "
                ),
                "{m}"
            );
            assert!(m.contains("`DELETE /i/vm/interface/PORT`"), "{m}");
        }
    }

    /// A network left: its own port detached and the others left (they
    /// are someone's); none known, the one port there detached, several
    /// refused.
    #[test]
    fn a_network_left_detaches_its_own_port_only() {
        let steps = PortSteps::new(&[], &three(), &owning("lab", "if-3"));
        let gone: Vec<&str> = steps.detach.iter().map(|(_, p, _)| p.id.as_str()).collect();
        assert_eq!(gone, ["if-3"]);
        let left: Vec<&str> = steps.foreign.iter().map(|(_, p)| p.id.as_str()).collect();
        assert_eq!(left, ["if-1", "if-2"]);
        let one = [("lab".to_string(), vec![port("if-1", "10.0.0.11")])].into();
        let steps = PortSteps::new(&[], &one, &none());
        assert_eq!(steps.detach.len(), 1);
        let steps = PortSteps::new(&[], &three(), &none());
        assert!(steps.detach.is_empty());
        assert!(steps.refusal("at", "/i").is_some());
    }
}
