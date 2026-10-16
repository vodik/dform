//! Secrets another provider holds, revealed (R-45, R-218): into a
//! provider's settings, into an Apply call's document, or for
//! `output --reveal`; never kept, written or printed.

use super::settings::known_json;
use super::{Providers, not_sent};
use crate::address::Address;
use crate::plugin::pb;
use crate::provider::{self};
use crate::secrets::held;
use crate::state::State;
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, anyhow};
use serde_json::{Value as Json, json};
use std::collections::BTreeMap;

impl Providers {
    /// Where the secret labeled `label` is held, and the provider that
    /// holds it: an extern's or another stack's, as it was answered; a
    /// resource's sensitive attribute (`T/N#P`), by the provider serving
    /// `T`, once the object exists (`identity`).
    pub(super) fn holder(
        &self,
        label: &str,
        identity: &BTreeMap<(String, String), String>,
    ) -> Option<(usize, provider::Held)> {
        if let Some(h) = self.held.borrow().get(label) {
            return Some((self.link_named(&h.provider)?, h.clone()));
        }
        let (typ, name, path) = crate::value::null_parts(label)?;
        let remote = identity.get(&(typ.clone(), name))?;
        let i = self.route(&typ);
        let deployment = self.bases.get(i)?.get("stack")?.as_str()?.to_string();
        Some((
            i,
            provider::Held {
                provider: self.links[i].borrow().name.clone(),
                deployment,
                typ,
                remote: remote.clone(),
                path,
                digest: String::new(),
            },
        ))
    }

    /// Settings as Configure takes them: each secret revealed by the
    /// provider that holds it, its bytes in this document only (and in
    /// `shown`, by its label, zeroed when dropped). Only called with
    /// settings [`Providers::settings_of`] knows.
    pub(super) fn revealed(
        &self,
        v: &Value,
        identity: &BTreeMap<(String, String), String>,
        shown: &mut Vec<(String, crate::plugin::credentials::Secret)>,
    ) -> Result<Json> {
        Ok(match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } => json!(self.revealed_text(label, identity, shown)?),
            Value::Str(t) if held::carries(t) => {
                json!(held::fill(t, |l| self.revealed_text(l, identity, shown))?)
            }
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.revealed(x, identity, shown))
                    .collect::<Result<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Ok((k.clone(), self.revealed(x, identity, shown)?)))
                    .collect::<Result<_>>()?,
            ),
            other => known_json(other).ok_or_else(|| anyhow!("a setting is not known yet"))?,
        })
    }

    /// The secret labeled `label` as text, revealed by the provider that
    /// holds it (and kept in `shown`, by its label, zeroed when dropped).
    fn revealed_text(
        &self,
        label: &str,
        identity: &BTreeMap<(String, String), String>,
        shown: &mut Vec<(String, crate::plugin::credentials::Secret)>,
    ) -> Result<String> {
        let (i, held) = self.holder(label, identity).ok_or_else(|| {
            anyhow!(
                "no object of this deployment holds the secret {} (it is not made, or it is \
                 another deployment's)",
                crate::address::label(label)
            )
        })?;
        let bytes = self.reveal(i, &held, label)?;
        let text = std::str::from_utf8(bytes.expose())
            .map_err(|_| {
                anyhow!(
                    "the secret {} that provider {} revealed is not text",
                    crate::address::label(label),
                    held.provider
                )
            })?
            .to_string();
        shown.push((label.to_string(), bytes));
        Ok(text)
    }

    /// `v`, one of the stack's own outputs, as JSON with each secret in it
    /// revealed by the provider that holds it, under this run's lease: what
    /// `output NAME --reveal` prints, and nothing else asks for. An open
    /// null is an error: the value is not known yet.
    pub fn reveal_output(&self, v: &Value, state: &State) -> Result<Json> {
        self.revealed(v, &self.identities(state), &mut Vec::new())
    }

    /// The object each address of `state` maps to, for a secret its
    /// attribute holds ([`Providers::holder`]).
    fn identities(&self, state: &State) -> BTreeMap<(String, String), String> {
        self.entries(&state.resources)
            .map(|(a, e)| ((a.typ, a.name), e.remote.clone()))
            .collect()
    }

    /// `doc`, an Apply call's document for `addr`, with each secret another
    /// provider holds revealed into it (R-218, as into a Configure): at a
    /// path the schema marks sensitive, one written whole that the writing
    /// provider does not hold itself (one it holds it materializes, as it
    /// always has), and every one inside a template. Each is revealed by
    /// the provider that holds it under this run's lease; the bytes are in
    /// this call only. A reveal that does not happen is the call's error,
    /// naming the attribute.
    pub(super) fn reveal_into(&self, addr: &Address, doc: &mut Json, state: &State) -> Result<()> {
        if self.revealed_attrs(&addr.typ, doc).is_empty() {
            return Ok(());
        }
        let identity = self.identities(state);
        let writer = self.route(&addr.typ);
        self.reveal_at(addr, writer, &identity, doc, "", "")
    }

    fn reveal_at(
        &self,
        addr: &Address,
        writer: usize,
        identity: &BTreeMap<(String, String), String>,
        v: &mut Json,
        path: &str,
        norm: &str,
    ) -> Result<()> {
        let sensitive = !norm.is_empty() && self.schema().is_sensitive(&addr.typ, norm);
        let text = |label: &str| {
            self.revealed_text(label, identity, &mut Vec::new())
                .map_err(|e| unrevealed(addr, path, label, &e))
        };
        if let Some((key, label)) = provider::marker(v) {
            if sensitive && key == provider::SECRET_KEY && self.holder_link(label) != Some(writer) {
                *v = Json::String(text(label)?);
            }
            return Ok(());
        }
        match v {
            Json::String(t) if sensitive && held::carries(t) => *t = held::fill(t, text)?,
            Json::Object(m) => {
                for (k, x) in m.iter_mut() {
                    let (p, n) = (
                        crate::address::path_join(path, k),
                        crate::address::path_join(norm, k),
                    );
                    self.reveal_at(addr, writer, identity, x, &p, &n)?;
                }
            }
            Json::Array(xs) => {
                for (i, x) in xs.iter_mut().enumerate() {
                    self.reveal_at(addr, writer, identity, x, &format!("{path}[{i}]"), norm)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The link of the provider that holds the secret labeled `label`:
    /// an extern's or another stack's, as it was answered; a resource's
    /// sensitive attribute, the one serving its type.
    fn holder_link(&self, label: &str) -> Option<usize> {
        if let Some(h) = self.held.borrow().get(label) {
            return self.link_named(&h.provider);
        }
        let (typ, _, _) = crate::value::null_parts(label)?;
        Some(self.route(&typ))
    }

    /// The attributes of a document for `typ` that [`Providers::reveal_into`]
    /// reveals a secret into, by path: what its provider is sent is not
    /// what dform has, so each is compared as a write-only one is, by the
    /// digest state keeps of what was sent (R-106).
    pub(super) fn revealed_attrs(&self, typ: &str, doc: &Json) -> Vec<String> {
        let writer = self.route(typ);
        let reveals = |v: &Json, norm: &str| {
            let mut found = false;
            self.reveals(typ, writer, v, norm, &mut found);
            found
        };
        doc.as_object()
            .into_iter()
            .flatten()
            .filter(|(k, v)| reveals(v, &crate::address::path_join("", k)))
            .map(|(k, _)| crate::address::path_join("", k))
            .collect()
    }

    fn reveals(&self, typ: &str, writer: usize, v: &Json, norm: &str, found: &mut bool) {
        let sensitive = || self.schema().is_sensitive(typ, norm);
        match v {
            _ if *found => {}
            v if let Some((key, label)) = provider::marker(v) => {
                *found = key == provider::SECRET_KEY
                    && self.holder_link(label) != Some(writer)
                    && sensitive();
            }
            Json::String(t) => *found = held::carries(t) && sensitive(),
            Json::Object(m) => {
                for (k, x) in m {
                    self.reveals(typ, writer, x, &crate::address::path_join(norm, k), found);
                }
            }
            Json::Array(xs) => {
                for x in xs {
                    self.reveals(typ, writer, x, norm, found);
                }
            }
            _ => {}
        }
    }

    /// The secret `held` names, from provider `i`, which holds it: under
    /// this run's holder identity (`WHO pid PID`, as the deployment's lease
    /// names its holder), which a provider refuses a reveal without.
    fn reveal(
        &self,
        i: usize,
        held: &provider::Held,
        label: &str,
    ) -> Result<crate::plugin::credentials::Secret> {
        let req = pb::RevealRequest {
            held: Some(pb::Held {
                provider: held.provider.clone(),
                deployment: held.deployment.clone(),
                r#type: held.typ.clone(),
                remote: held.remote.clone(),
                path: held.path.clone(),
                digest: held.digest.clone(),
            }),
            lease: format!("{} pid {}", crate::audit::who(), std::process::id()),
            ..Default::default()
        };
        let r: pb::RevealResponse = self.call(i, req).with_context(|| {
            format!(
                "provider {} reveals the secret {}",
                held.provider,
                crate::address::label(label)
            )
        })?;
        Ok(crate::plugin::credentials::Secret::new(r.value))
    }
}

/// An Apply call not sent because the secret `label` at `path` of its
/// document was not revealed (R-218): why, as the holder said it.
fn unrevealed(addr: &Address, path: &str, label: &str, e: &anyhow::Error) -> anyhow::Error {
    let why = e
        .chain()
        .last()
        .map(ToString::to_string)
        .unwrap_or_default();
    not_sent(
        addr,
        &format!(
            "{} holds the secret {}, which was not revealed: {why}",
            crate::report::attribute(addr, path),
            crate::address::label(label)
        ),
    )
}
