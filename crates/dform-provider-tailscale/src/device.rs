//! `tailscale.device`: a node on the tailnet, which a program adopts by
//! its hostname and then manages by its id. The tailnet makes a device
//! when a node joins it with an auth key, so a create is refused: a
//! program adopts it (`adopt(d, "k3s-1")`). A hostname is not unique (a
//! node replaced while the old one is still listed has the same one), so
//! an adopt by a hostname two devices have is refused naming both; the
//! ephemeral key is why that passes: the old device leaves once it is
//! offline.
//!
//! What a program writes: `tags`, `routes` (the subnet routes approved of
//! those it advertises), `authorized` and `name` (its MagicDNS name), each
//! its own call of the API; the rest is the device's (`id`, `addresses`,
//! `os`, `last_seen`). A delete removes the device from the tailnet.
//! Health is the device's own: on the tailnet and connected, waiting for
//! approval, or not connected.

use crate::client::Body;
use crate::{Tailscale, at};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource, health, pb};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};

pub const TYPE: &str = "tailscale.device";

/// A device.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(type = "tailscale.device", health)]
pub struct Device {
    /// The node's own hostname, what a program adopts it by.
    #[dform(required)]
    pub hostname: String,
    /// Its tags (`tag:k8s`): a tagged device belongs to its tags, not to
    /// the user who joined it.
    #[dform(ty = "set(string)", optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tags: Option<Vec<String>>,
    /// The subnet routes approved, of those it advertises.
    #[dform(ty = "set(string)", optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routes: Option<Vec<String>>,
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorized: Option<bool>,
    /// Its machine name, the first label of its MagicDNS name.
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Its tailnet addresses, IPv4 then IPv6.
    #[dform(ty = "list(ip)", computed)]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[dform(computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub os: Option<String>,
    #[dform(ty = "time", computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<String>,
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect()
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

/// Its id: `nodeId`, the API's preferred one, else the legacy `id`.
fn id_of(d: &Json) -> Option<&str> {
    d["nodeId"].as_str().or_else(|| d["id"].as_str())
}

/// `d` as the API answers it, its approved routes `routes`.
fn from_api(d: &Json, routes: Vec<String>) -> Device {
    Device {
        hostname: d["hostname"].as_str().unwrap_or_default().to_string(),
        tags: Some(sorted(strings(&d["tags"]))),
        routes: Some(sorted(routes)),
        authorized: d["authorized"].as_bool(),
        name: d["name"]
            .as_str()
            .map(|n| n.split('.').next().unwrap_or(n).to_string()),
        id: id_of(d).map(str::to_string),
        addresses: strings(&d["addresses"]),
        os: d["os"].as_str().map(str::to_string),
        last_seen: d["lastSeen"].as_str().map(str::to_string),
    }
}

/// Every device on the tailnet.
pub fn list(p: &Tailscale) -> Result<Vec<Json>> {
    let a = p
        .client
        .call("GET", &p.client.tailnet_path("/devices"), Body::None)?;
    Ok(a.body["devices"].as_array().cloned().unwrap_or_default())
}

/// The one device whose hostname is `hostname`: none is `None`, two are
/// refused, naming each.
fn by_hostname(p: &Tailscale, hostname: &str) -> Result<Option<Json>> {
    let mut found: Vec<Json> = list(p)?
        .into_iter()
        .filter(|d| d["hostname"].as_str() == Some(hostname))
        .collect();
    match found.len() {
        0 => Ok(None),
        1 => Ok(found.pop()),
        _ => {
            let ids: Vec<String> = found
                .iter()
                .map(|d| {
                    format!(
                        "{} (last seen {})",
                        id_of(d).unwrap_or("?"),
                        d["lastSeen"].as_str().unwrap_or("never")
                    )
                })
                .collect();
            Err(Error::Refused(format!(
                "{}: {} devices have the hostname {hostname:?}: {}. A hostname is not a \
                 device's identity: remove the one that is gone (an ephemeral key's node leaves \
                 by itself once offline), or adopt one by its id",
                at(TYPE, hostname),
                found.len(),
                ids.join(", ")
            )))
        }
    }
}

/// The device `remote` (its id) and its approved routes.
fn fetch(p: &Tailscale, remote: &str) -> Result<Option<Device>> {
    let Some(d) = p.client.get(&format!("/device/{remote}?fields=all"))? else {
        return Ok(None);
    };
    let routes = match p.client.get(&format!("/device/{remote}/routes"))? {
        Some(r) => strings(&r.body["enabledRoutes"]),
        None => strings(&d.body["enabledRoutes"]),
    };
    Ok(Some(from_api(&d.body, routes)))
}

impl Lifecycle<Tailscale> for Device {
    const NOT_CREATED: Option<&'static str> = Some(
        "a device joins the tailnet with an auth key; adopt it (`adopt(RESOURCE, \"HOSTNAME\")`) \
         once it has joined",
    );

    /// By its id; by its hostname where it has no id yet (an adopt's plan).
    fn read(p: &Tailscale, remote: &str) -> Result<Option<Device>> {
        if let Some(d) = fetch(p, remote)? {
            return Ok(Some(d));
        }
        match by_hostname(p, remote)? {
            Some(d) => fetch(p, id_of(&d).unwrap_or_default()),
            None => Ok(None),
        }
    }

    fn adopt(p: &Tailscale, given: &str) -> Result<String> {
        if p.client.get(&format!("/device/{given}"))?.is_some() {
            return Ok(given.to_string());
        }
        let d = by_hostname(p, given)?.ok_or_else(|| {
            Error::Refused(format!(
                "{}: no device on the tailnet {:?} has that hostname: it joins with an auth key \
                 first",
                at(TYPE, given),
                p.client.tailnet()
            ))
        })?;
        Ok(id_of(&d).unwrap_or_default().to_string())
    }

    fn create(_: &Tailscale, _: Device, _: &str, _: &Progress) -> Result<(String, Device)> {
        Err(Error::Refused(
            Self::NOT_CREATED.expect("declared").to_string(),
        ))
    }

    fn update(
        p: &Tailscale,
        remote: &str,
        prior: Device,
        desired: Device,
        progress: &Progress,
    ) -> Result<Device> {
        let at = at(TYPE, &prior.hostname);
        if desired.hostname != prior.hostname {
            return Err(Error::Refused(format!(
                "{at}: hostname is the node's own, {:?}: a program adopts a device by it and \
                 cannot change it",
                prior.hostname
            )));
        }
        let post = |what: &str, body: Json| -> Result<()> {
            progress.message(&format!("writing {what}"));
            p.client
                .call(
                    "POST",
                    &format!("/device/{remote}/{what}"),
                    Body::Json(body),
                )
                .map(|_| ())
        };
        if let Some(tags) = &desired.tags
            && Some(sorted(tags.clone())) != prior.tags
        {
            post("tags", json!({"tags": tags}))?;
        }
        if let Some(routes) = &desired.routes
            && Some(sorted(routes.clone())) != prior.routes
        {
            post("routes", json!({"routes": routes}))?;
        }
        if let Some(a) = desired.authorized
            && Some(a) != prior.authorized
        {
            post("authorized", json!({"authorized": a}))?;
        }
        if let Some(n) = &desired.name
            && Some(n) != prior.name.as_ref()
        {
            post("name", json!({"name": n}))?;
        }
        fetch(p, remote)?.ok_or_else(|| Error::Refused(format!("{at}: gone during its update")))
    }

    /// Remove it from the tailnet.
    fn delete(p: &Tailscale, remote: &str, _: &Progress) -> Result<()> {
        let path = format!("/device/{remote}");
        let a = p.client.send("DELETE", &path, &Body::None, None, false)?;
        match a.status {
            404 => Ok(()),
            _ => p.client.expect("DELETE", &path, a).map(|_| ()),
        }
    }

    /// On the tailnet and connected; waiting for an admin's approval; not
    /// connected (as of the API's last word); gone.
    fn health(p: &Tailscale, remote: &str) -> Result<Option<pb::Health>> {
        let Some(d) = p.client.get(&format!("/device/{remote}?fields=all"))? else {
            return Ok(Some(health(
                pb::HealthState::Degraded,
                "not on the tailnet",
            )));
        };
        let d = d.body;
        let seen = d["lastSeen"].as_str().unwrap_or("never");
        Ok(Some(if d["authorized"].as_bool() == Some(false) {
            health(
                pb::HealthState::Degraded,
                "not authorized: an admin approves it, or the program sets authorized = true",
            )
        } else if d["connectedToControl"].as_bool() == Some(false) {
            health(
                pb::HealthState::Degraded,
                format!("not connected, last seen {seen}"),
            )
        } else {
            let addr = strings(&d["addresses"])
                .into_iter()
                .next()
                .unwrap_or_default();
            health(pb::HealthState::Healthy, format!("connected at {addr}"))
        }))
    }
}
