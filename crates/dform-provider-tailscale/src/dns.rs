//! `tailscale.dns`: the tailnet's DNS settings, one per tailnet, its
//! remote id the tailnet: the global nameservers, MagicDNS, the search
//! paths and split DNS (a domain to the nameservers that answer for it).
//! Four calls of the API, one object here: an update writes the parts
//! that differ. Like the policy file, the settings always exist: a create
//! refuses settings somebody wrote (adopt them), and a destroy writes the
//! defaults back (no nameservers, no search paths, no split domain,
//! MagicDNS on), and says so.

use crate::client::Body;
use crate::{Tailscale, at};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource};
use serde::{Deserialize, Serialize};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

pub const TYPE: &str = "tailscale.dns";

/// The DNS settings.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(type = "tailscale.dns")]
pub struct Dns {
    /// The nameservers every device asks.
    #[dform(ty = "list(ip)")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nameservers: Vec<String>,
    /// Each device's name resolves on the tailnet.
    #[dform(optional_computed)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub magic_dns: Option<bool>,
    #[dform(ty = "list(string)")]
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub search_paths: Vec<String>,
    /// A domain to the nameservers that answer for it
    /// (`{ "home.example.com": ["192.168.1.1"] }`).
    #[dform(ty = "map(list(ip))")]
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub split: BTreeMap<String, Vec<String>>,
    /// The tailnet they are of.
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
}

impl Dns {
    /// A tailnet's settings before anyone writes them.
    fn defaults() -> Dns {
        Dns {
            magic_dns: Some(true),
            ..Dns::default()
        }
    }

    fn is_default(&self) -> bool {
        Dns {
            tailnet: None,
            ..self.clone()
        } == Dns::defaults()
    }
}

fn strings(v: &Json) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(Json::as_str)
        .map(str::to_string)
        .collect()
}

async fn get(p: &Tailscale, part: &str) -> Result<Json> {
    let path = p.client.tailnet_path(&format!("/dns/{part}"));
    Ok(p.client
        .get(&path)
        .await?
        .map(|a| a.body)
        .unwrap_or(Json::Null))
}

/// What the API has.
async fn current(p: &Tailscale) -> Result<Dns> {
    Ok(Dns {
        nameservers: strings(&get(p, "nameservers").await?["dns"]),
        magic_dns: get(p, "preferences").await?["magicDNS"].as_bool(),
        search_paths: strings(&get(p, "searchpaths").await?["searchPaths"]),
        split: get(p, "split-dns")
            .await?
            .as_object()
            .into_iter()
            .flatten()
            .map(|(d, ns)| (d.clone(), strings(ns)))
            .collect(),
        tailnet: Some(p.client.tailnet().to_string()),
    })
}

/// Write each part of `want` that `now` does not have as it is; MagicDNS
/// only where the program says.
async fn write(p: &Tailscale, now: &Dns, want: &Dns, progress: &Progress<'_>) -> Result<()> {
    let post = async |part: &str, method: &str, body: Json| -> Result<()> {
        progress.message(&format!("writing dns/{part}"));
        let path = p.client.tailnet_path(&format!("/dns/{part}"));
        p.client
            .call(method, &path, Body::Json(body))
            .await
            .map(|_| ())
    };
    if now.nameservers != want.nameservers {
        post("nameservers", "POST", json!({"dns": want.nameservers})).await?;
    }
    if now.search_paths != want.search_paths {
        post(
            "searchpaths",
            "POST",
            json!({"searchPaths": want.search_paths}),
        )
        .await?;
    }
    if now.split != want.split {
        post("split-dns", "PUT", json!(want.split)).await?;
    }
    if let Some(m) = want.magic_dns
        && now.magic_dns != Some(m)
    {
        post("preferences", "POST", json!({"magicDNS": m})).await?;
    }
    Ok(())
}

fn same_tailnet(p: &Tailscale, remote: &str) -> Result<()> {
    match remote == p.client.tailnet() {
        true => Ok(()),
        false => Err(Error::Refused(format!(
            "{}: the provider is configured for the tailnet {:?}",
            at(TYPE, remote),
            p.client.tailnet()
        ))),
    }
}

impl Lifecycle<Tailscale> for Dns {
    async fn read(p: &Tailscale, remote: &str) -> Result<Option<Dns>> {
        same_tailnet(p, remote)?;
        Ok(Some(current(p).await?))
    }

    async fn create(
        p: &Tailscale,
        desired: Dns,
        _: &str,
        progress: &Progress<'_>,
    ) -> Result<(String, Dns)> {
        let tailnet = p.client.tailnet().to_string();
        let now = current(p).await?;
        if !now.is_default() {
            return Err(Error::Refused(format!(
                "{}: the tailnet's DNS settings are not its defaults: someone wrote them. Adopt \
                 them to manage them from here (`adopt(RESOURCE, {tailnet:?})`); the plan then \
                 shows what the program changes",
                at(TYPE, &tailnet)
            )));
        }
        write(p, &now, &desired, progress).await?;
        Ok((tailnet, current(p).await?))
    }

    async fn update(
        p: &Tailscale,
        remote: &str,
        _: Dns,
        desired: Dns,
        progress: &Progress<'_>,
    ) -> Result<Dns> {
        same_tailnet(p, remote)?;
        let now = current(p).await?;
        write(p, &now, &desired, progress).await?;
        current(p).await
    }

    /// The settings stay: the defaults are written in their place.
    async fn delete(p: &Tailscale, remote: &str, progress: &Progress<'_>) -> Result<()> {
        same_tailnet(p, remote)?;
        let now = current(p).await?;
        write(p, &now, &Dns::defaults(), progress).await?;
        progress.note(&format!(
            "{}: the tailnet's DNS settings are its defaults again (no nameservers, no search \
             paths, no split domain, MagicDNS on); a tailnet always has them",
            at(TYPE, remote)
        ));
        Ok(())
    }
}
