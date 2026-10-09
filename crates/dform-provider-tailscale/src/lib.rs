//! The Tailscale provider, `dform-provider-tailscale` (R-197): one
//! tailnet's policy file (`tailscale.acl`), its auth keys
//! (`tailscale.auth_key`), its DNS settings (`tailscale.dns`) and the
//! devices on it (`tailscale.device`), and its users as a data source
//! (`tailscale.user`), over the Tailscale API v2. The devices are also a
//! data source under the type's name (`tailscale.device(tailnet, ..)`,
//! R-196): every device the tailnet lists, adopted or not, where `d in
//! tailscale.device` is the program's own. Written with the SDK
//! (`dform-sdk`): the schema is the types' derives, Plan the SDK's, every
//! request the host's.
//!
//! `config` is how it is configured (`use tailscale { tailnet }`, the
//! credential by Tailscale's names), `client` the API, `hujson` the policy
//! file's format and the canonical form a policy is compared in; `acl`,
//! `auth_key`, `dns` and `device` the types, `user` the data source.
//! `fake` (feature `fake`) is an API for tests.
//!
//! An auth key's value is answered once, by its create: the provider keeps
//! it in memory for the run and reveals it to the engine only (R-45);
//! dform has its label, never the bytes.

pub mod acl;
pub mod auth_key;
pub mod client;
pub mod config;
pub mod device;
pub mod dns;
#[cfg(feature = "fake")]
pub mod fake;
pub mod hujson;
pub mod user;

use dform_sdk::typed::{Error, Result};
use dform_sdk::{Provider, Typed, pb};
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::sync::Mutex;

/// The provider: one tailnet, as one credential.
pub struct Tailscale {
    pub client: client::Client,
    /// Each auth key made in this run, by its id: the value the API
    /// answered once.
    keys: Mutex<BTreeMap<String, String>>,
}

impl Tailscale {
    pub fn new(settings: config::Settings) -> Tailscale {
        Tailscale {
            client: client::Client::new(settings),
            keys: Mutex::new(BTreeMap::new()),
        }
    }

    /// Keep the value of the key `id`, made now.
    fn hold(&self, id: &str, key: String) {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(id.to_string(), key);
    }

    /// The tailnet a data source `pred` is asked of, its one input: the
    /// one the provider is configured for.
    fn asked<'a>(&self, pred: &str, inputs: &'a [Json]) -> Result<&'a str> {
        let [Json::String(tailnet)] = inputs else {
            return Err(Error::Refused(format!(
                "{pred} is asked with its tailnet, a string, bound"
            )));
        };
        if tailnet != self.client.tailnet() {
            return Err(Error::Refused(format!(
                "{pred}({tailnet:?}, ..): the provider is configured for the tailnet {:?}",
                self.client.tailnet()
            )));
        }
        Ok(tailnet)
    }

    fn held(&self, id: &str) -> Option<String> {
        self.keys
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }
}

/// `TYPE "REMOTE"`, a message's subject.
pub fn at(typ: &str, remote: &str) -> String {
    format!("{typ} {remote:?}")
}

impl Provider for Tailscale {
    const NAME: &'static str = "tailscale";
    const VERSION: &'static str = env!("CARGO_PKG_VERSION");

    fn configure(settings: &Json) -> Result<(Tailscale, Option<String>)> {
        let settings = config::Settings::parse(settings, &|k| std::env::var(k).ok())?;
        let account = settings.tailnet.clone();
        Ok((Tailscale::new(settings), Some(account)))
    }

    fn query(&self, pred: &str, inputs: &[Json]) -> Result<Vec<Vec<Json>>> {
        match pred {
            user::USER => user::rows(self, inputs),
            device::TYPE => device::rows(self, inputs),
            _ => Err(Error::Refused(format!(
                "provider tailscale answers no extern {pred}"
            ))),
        }
    }

    /// An auth key's value, made in this run.
    fn reveal(&self, held: &pb::Held) -> Result<Vec<u8>> {
        let at = format!("reveal {} {}#{}", held.r#type, held.remote, held.path);
        if held.r#type != auth_key::TYPE || held.path != "key" {
            return Err(Error::Refused(format!(
                "{at}: the tailscale provider holds no secret there (it holds an auth key's key)"
            )));
        }
        self.held(&held.remote)
            .map(String::into_bytes)
            .ok_or_else(|| Error::Refused(auth_key::not_held(&held.remote)))
    }
}

/// The provider: its types, its data source and its settings.
pub fn provider() -> Typed<Tailscale> {
    Typed::new()
        .resource::<acl::Acl>()
        .resource::<auth_key::AuthKey>()
        .resource::<dns::Dns>()
        .resource::<device::Device>()
        .facts_text(&format!(
            "extern_decl({:?}, \"+tailnet, -login, -role\")\nextern_decl({:?}, {:?})\n{}",
            user::USER,
            device::TYPE,
            device::LISTING,
            config::SETTINGS
                .iter()
                .map(|s| format!("provider_setting(\"tailscale\", {s:?}, [\"connection\"])"))
                .collect::<Vec<_>>()
                .join("\n")
        ))
}

dform_sdk::provider!(
    provider(),
    uses = ["dform:host/http", "dform:host/secrets", "dform:host/log"]
);

#[cfg(test)]
mod tests {
    #[test]
    fn the_schema_is_the_derives() {
        let p = super::provider();
        let facts = p.facts();
        for line in [
            r#"type_attr("tailscale.acl", "policy", "string", ["required"])"#,
            r#"type_attr("tailscale.auth_key", "key", "string", ["computed", "sensitive"])"#,
            r#"type_attr("tailscale.auth_key", "tags", "set(string)", ["force_new"])"#,
            r#"type_attr("tailscale.auth_key", "expiry", "duration(seconds)", ["optional_computed", "force_new"])"#,
            r#"type_lookup("tailscale.auth_key", ["description"])"#,
            r#"type_attr("tailscale.dns", "split", "map(list(ip))", [])"#,
            r#"type_attr("tailscale.device", "addresses", "list(ip)", ["computed"])"#,
            r#"type_attr("tailscale.device", "last_seen", "time", ["computed"])"#,
            r#"extern_decl("tailscale.user", "+tailnet, -login, -role")"#,
            r#"extern_decl("tailscale.device", "+tailnet, -hostname, -id, -addresses, -tags, -os, -authorized, -last_seen")"#,
        ] {
            assert!(facts.contains(line), "{line}\n{facts}");
        }
    }
}
