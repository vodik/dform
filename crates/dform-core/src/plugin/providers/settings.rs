//! The program's settings of its providers (`provider_config`): each
//! provider configured again once its settings are known and changed, and
//! asked again for the kinds a CRD it made defines (R-126).

use super::Providers;
use crate::ast::{Atom, Term};
use crate::provider::{self};
use crate::secrets::held;
use crate::value::{NullClass, Value};
use anyhow::{Context, Result};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};

impl Providers {
    /// Configure each provider the program configures
    /// (`provider_config(Name, Settings)` in `facts`) whose settings are
    /// known now and changed: Configure again with them as `settings`.
    /// Settings holding a null (a cluster not created yet) wait. A secret
    /// another provider holds (a resource's sensitive computed attribute,
    /// another stack's held output, an extern's secret column) is revealed
    /// by that provider into this call (R-45, the `Reveal` call): once the
    /// object that holds it exists. The providers configured, by the name
    /// the program gives them: then what they read is read again. The
    /// settings go to the provider in that call and are kept here only to
    /// tell a change, a revealed secret as its label and where it is held
    /// (never written, printed or digested: a kubeconfig may be among
    /// them).
    pub fn configure_from<'a>(
        &self,
        facts: impl IntoIterator<Item = &'a Atom>,
    ) -> Result<Vec<String>> {
        let facts: Vec<&Atom> = facts.into_iter().collect();
        // The object each address maps to, for a secret its attribute holds.
        let identity: BTreeMap<(String, String), String> = facts
            .iter()
            .filter(|a| a.pred == "identity")
            .filter_map(|a| match a.args.as_slice() {
                [
                    Term::Val(Value::Str(t)),
                    Term::Val(Value::Str(n)),
                    Term::Val(Value::Str(r)),
                ] => Some(((t.clone(), n.clone()), r.clone())),
                _ => None,
            })
            .collect();
        let mut changed = Vec::new();
        for a in facts.iter().filter(|a| a.pred == "provider_config") {
            let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() else {
                continue;
            };
            let Some(i) = self.link_for(name) else {
                continue;
            };
            // What the settings are kept as, and what goes to the provider.
            let Some(kept) = self.settings_of(v, &identity) else {
                continue;
            };
            // A stand-in is no setting (R-164): the provider waits for a
            // run that holds the master.
            if self.secrets.standins().carries(&kept) {
                continue;
            }
            if self.settings.borrow().get(&i) == Some(&kept) {
                continue;
            }
            let mut shown = Vec::new();
            let settings = self
                .revealed(v, &identity, &mut shown)
                .with_context(|| format!("configure provider {name}"))?;
            let mut config = self.bases.get(i).cloned().unwrap_or_else(|| json!({}));
            config["settings"] = settings;
            let account = self
                .configure_link(i, config)
                .with_context(|| format!("configure provider {name} from provider_config"))?;
            // An account that is a revealed secret prints as its label.
            let secret = account.as_ref().and_then(|a| {
                shown
                    .iter()
                    .find(|(_, s)| s.expose() == a.as_bytes())
                    .map(|(l, _)| l.clone())
            });
            match secret {
                Some(l) => self.secret_accounts.borrow_mut().insert(i, l),
                None => self.secret_accounts.borrow_mut().remove(&i),
            };
            let mut accounts = self.accounts.borrow_mut();
            match account {
                Some(a) => accounts.insert(i, a),
                None => accounts.remove(&i),
            };
            drop(accounts);
            self.settings.borrow_mut().insert(i, kept);
            self.awaiting.borrow_mut().remove(&i);
            self.forget(i);
            if self.loaded.get().is_some() {
                self.learn(i)
                    .with_context(|| format!("the schema of provider {name}, configured"))?;
            }
            changed.push(name.clone());
        }
        Ok(changed)
    }

    /// Settings as they are kept to tell a change: every value as it is,
    /// a secret as its label and where it is held. `None` while one is not
    /// known yet: an open null, a secret no provider holds yet.
    fn settings_of(
        &self,
        v: &Value,
        identity: &BTreeMap<(String, String), String>,
    ) -> Option<Json> {
        Some(match v {
            Value::Null {
                label,
                class: NullClass::Secret,
                ..
            } => provider::held_json(label, &self.holder(label, identity)?.1),
            // A template over held secrets as it is, once each has its
            // holder (R-218).
            Value::Str(t) if held::carries(t) => {
                for l in held::labels(t) {
                    self.holder(&l, identity)?;
                }
                Json::String(t.clone())
            }
            Value::List(xs) => Json::Array(
                xs.iter()
                    .map(|x| self.settings_of(x, identity))
                    .collect::<Option<_>>()?,
            ),
            Value::Obj(m) => Json::Object(
                m.iter()
                    .map(|(k, x)| Some((k.clone(), self.settings_of(x, identity)?)))
                    .collect::<Option<_>>()?,
            ),
            other => known_json(other)?,
        })
    }

    /// The kinds of `types` (none in any schema, their namespace's
    /// provider one the program configures or a cluster's, [`Providers::reached`]) a
    /// CRD the program made defines (R-126): each such provider is
    /// configured again with the settings it has (none, one the environment
    /// configures), told the kinds (`kinds`, which it may wait a moment for the
    /// cluster to serve), and what it serves now learned. Each kind is
    /// asked for once a run. Whether the schema learned any.
    pub fn relearn(&self, types: &BTreeSet<String>) -> Result<bool> {
        let mut by_link: BTreeMap<usize, Vec<String>> = BTreeMap::new();
        for t in types {
            if self.relearned.borrow().contains(t) || self.schema().knows_type(t) {
                continue;
            }
            // A provider the program configures, with the settings it
            // has; one the environment configures, as it was.
            let Some(i) = self.reached(t) else {
                continue;
            };
            if self.settings.borrow().contains_key(&i) || !self.by_program.contains(&i) {
                by_link.entry(i).or_default().push(t.clone());
            }
        }
        let mut grew = false;
        for (i, kinds) in by_link {
            self.relearned.borrow_mut().extend(kinds.iter().cloned());
            let mut config = self.bases.get(i).cloned().unwrap_or_else(|| json!({}));
            if let Some(settings) = self.settings.borrow().get(&i) {
                config["settings"] = settings.clone();
            }
            config["kinds"] = json!(kinds);
            self.configure_link(i, config)
                .with_context(|| format!("configure provider {} again", self.names[i]))?;
            if self.loaded.get().is_some() {
                self.learn(i)
                    .with_context(|| format!("the schema of provider {}", self.names[i]))?;
            }
            let learned = kinds.iter().any(|t| self.schema().knows_type(t));
            if learned {
                self.forget(i);
            }
            grew |= learned;
        }
        Ok(grew)
    }
}

/// A provider_config value as the provider receives it, if it is known:
/// no null, no ref to resolve.
pub(super) fn known_json(v: &Value) -> Option<Json> {
    Some(match v {
        Value::Str(s) => json!(s),
        Value::Int(i) => json!(i),
        Value::Float(f) => json!(f.get()),
        Value::Bool(b) => json!(b),
        Value::List(xs) => Json::Array(xs.iter().map(known_json).collect::<Option<_>>()?),
        Value::Obj(m) => Json::Object(
            m.iter()
                .map(|(k, x)| Some((k.clone(), known_json(x)?)))
                .collect::<Option<_>>()?,
        ),
        Value::Ip(n) => json!(crate::value::u32_to_ipv4(*n)),
        Value::IpNet { addr, prefix } => json!(crate::value::ipnet_to_string(*addr, *prefix)),
        Value::Range(r) => json!(r.to_string()),
        Value::Quantity(_) | Value::Time(_) | Value::Uri(_) | Value::Oci(_) | Value::Semver(_) => {
            json!(v.wire_text())
        }
        Value::Ref { .. } | Value::CloudRef { .. } | Value::Null { .. } => return None,
    })
}
