//! `tailscale.acl`: the tailnet's policy file, one per tailnet, its remote
//! id the tailnet. The program renders the policy (`policy =
//! json.encode({ acls: [..], tagOwners: {..} })`); Read answers the file
//! the API has, in the same canonical form (`hujson`), so a console edit of
//! a rule is drift and a comment or a reordering is not. HuJSON on the
//! wire, written with `If-Match` on the ETag the write read, so an edit
//! made in between is not overwritten.
//!
//! The file always exists: a create writes the program's policy over the
//! tailnet's default and refuses one somebody wrote (adopt it), and a
//! destroy writes the default back, and says so.

use crate::client::Body;
use crate::{Tailscale, at, hujson};
use dform_sdk::typed::{Error, Result};
use dform_sdk::{Lifecycle, Progress, Resource};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

pub const TYPE: &str = "tailscale.acl";

/// The policy file a tailnet starts with: every device reaches every
/// other, and Tailscale SSH to one's own devices with a check.
pub const DEFAULT: &str = r#"{"acls":[{"action":"accept","dst":["*:*"],"src":["*"]}],"ssh":[{"action":"check","dst":["autogroup:self"],"src":["autogroup:member"],"users":["autogroup:nonroot","root"]}]}"#;

/// The policy file.
#[derive(Resource, Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[dform(type = "tailscale.acl")]
pub struct Acl {
    /// The policy, as `json.encode` writes it.
    #[dform(required)]
    pub policy: String,
    /// The tailnet it is of.
    #[dform(computed, id)]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tailnet: Option<String>,
}

/// The file the API has: its canonical text and its ETag.
fn current(p: &Tailscale, at: &str) -> Result<(String, Option<String>)> {
    let path = p.client.tailnet_path("/acl");
    let a = p.client.send("GET", &path, &Body::None, None, true)?;
    let a = p.client.expect("GET", &path, a)?;
    let text = a.body.as_str().unwrap_or_default();
    let policy = hujson::canonical(text)
        .map_err(|e| Error::Refused(format!("{at}: the policy file the API answers: {e}")))?;
    Ok((policy, a.etag))
}

/// Write `policy` over the file whose ETag is `etag`.
fn write(p: &Tailscale, at: &str, policy: &str, etag: Option<&str>) -> Result<()> {
    let path = p.client.tailnet_path("/acl");
    let a = p.client.send(
        "POST",
        &path,
        &Body::HuJson(policy.to_string()),
        etag,
        false,
    )?;
    match a.status {
        412 => Err(Error::Refused(format!(
            "retryable: {at}: the policy file changed while it was written (an edit in the \
             admin console?): it is written again over what it is now"
        ))),
        _ => p.client.expect("POST", &path, a).map(|_| ()),
    }
}

/// The remote id is the tailnet the provider is configured for.
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

impl Lifecycle<Tailscale> for Acl {
    fn read(p: &Tailscale, remote: &str) -> Result<Option<Acl>> {
        same_tailnet(p, remote)?;
        let (policy, _) = current(p, &at(TYPE, remote))?;
        Ok(Some(Acl {
            policy,
            tailnet: Some(remote.to_string()),
        }))
    }

    fn create(p: &Tailscale, desired: Acl, _: &str, progress: &Progress) -> Result<(String, Acl)> {
        let tailnet = p.client.tailnet().to_string();
        let at = at(TYPE, &tailnet);
        let (now, etag) = current(p, &at)?;
        if now != desired.policy && now != DEFAULT {
            return Err(Error::Refused(format!(
                "{at}: the tailnet's policy file is not its default: someone wrote it. Adopt it \
                 to manage it from here (`adopt(RESOURCE, {tailnet:?})`); the plan then shows \
                 what the program changes in it"
            )));
        }
        progress.message("writing the policy file");
        write(p, &at, &desired.policy, etag.as_deref())?;
        Ok((
            tailnet.clone(),
            Acl {
                tailnet: Some(tailnet),
                ..desired
            },
        ))
    }

    fn update(
        p: &Tailscale,
        remote: &str,
        _: Acl,
        desired: Acl,
        progress: &Progress,
    ) -> Result<Acl> {
        same_tailnet(p, remote)?;
        let at = at(TYPE, remote);
        let (_, etag) = current(p, &at)?;
        progress.message("writing the policy file");
        write(p, &at, &desired.policy, etag.as_deref())?;
        Ok(Acl {
            tailnet: Some(remote.to_string()),
            ..desired
        })
    }

    /// The file stays: the tailnet's default is written in its place.
    fn delete(p: &Tailscale, remote: &str, progress: &Progress) -> Result<()> {
        same_tailnet(p, remote)?;
        let at = at(TYPE, remote);
        let (_, etag) = current(p, &at)?;
        write(p, &at, DEFAULT, etag.as_deref())?;
        progress.note(&format!(
            "{at}: the tailnet's policy file is its default again (every device reaches every \
             other); a tailnet always has one"
        ));
        Ok(())
    }

    /// A policy the provider can compare: HuJSON, in the canonical form
    /// `json.encode` writes.
    fn check(_: &Tailscale, desired: &Json) -> Result<()> {
        let Some(policy) = desired.get("policy").and_then(Json::as_str) else {
            return Ok(());
        };
        let canonical = hujson::canonical(policy)
            .map_err(|e| Error::Refused(format!("policy is not a policy file: {e}")))?;
        if canonical != policy {
            return Err(Error::Refused(
                "policy is not in the form the provider compares (JSON, keys sorted, no space): \
                 render it with `policy = json.encode({ acls: [..], .. })`, or the plan would \
                 show it changed at every run"
                    .into(),
            ));
        }
        Ok(())
    }
}
