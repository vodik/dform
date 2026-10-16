//! What dform keeps of a value it sent: a sensitive leaf as its keyed
//! digest, a write-only one compared by the digest of what was sent
//! (R-106), and what a run without the master proves unchanged (R-164).

use super::{Providers, World, key};
use crate::address::Address;
use crate::provider::{self, Action, ActionKind, get_path, set_path};
use crate::secrets::held;
use crate::state::{self, State, StateEntry};
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

impl Providers {
    /// A world document as dform keeps it beyond the run (the in-flight
    /// record, the controller's baseline) and compares with what it kept:
    /// each leaf at a path the schema marks sensitive as its keyed digest,
    /// `"(sensitive hmac-sha256:..)"` (`"(sensitive)"` with no key), never
    /// its value. A marker, and a leaf already kept so, stays.
    pub fn stored(&self, typ: &str, doc: &Json) -> Json {
        fn walk(p: &Providers, typ: &str, v: &Json, path: &str) -> Json {
            if provider::marker(v).is_some()
                || v.as_str().is_some_and(|s| s.starts_with("(sensitive"))
            {
                return v.clone();
            }
            if !path.is_empty() && p.schema().is_sensitive(typ, &provider::norm_path(path)) {
                return Json::String(match &p.digest_key {
                    Some(k) => format!(
                        "(sensitive hmac-sha256:{})",
                        k.digest(crate::approval::canonical_json(v).as_bytes())
                    ),
                    None => "(sensitive)".into(),
                });
            }
            let join = |k: &str| crate::address::path_join(path, k);
            match v {
                Json::Object(m) => Json::Object(
                    m.iter()
                        .map(|(k, x)| (k.clone(), walk(p, typ, x, &join(k))))
                        .collect(),
                ),
                Json::Array(xs) => Json::Array(
                    xs.iter()
                        .enumerate()
                        .map(|(i, x)| walk(p, typ, x, &format!("{path}[{i}]")))
                        .collect(),
                ),
                v => v.clone(),
            }
        }
        walk(self, typ, doc, "")
    }

    /// A write-only attribute's value as the world document has it (R-106):
    /// the world never answers one, so the program's value when its digest
    /// is what state kept from the last apply (or state kept none: an
    /// object made before, or elsewhere), else `(write-only)`, a change.
    /// Each that differs where the digest tells which part, by path: its
    /// note ([`Written::Differs`]).
    pub(super) fn written_before(
        &self,
        typ: &str,
        entry: &StateEntry,
        want: &Json,
        doc: &mut Json,
        holders: &Holders,
    ) -> BTreeMap<String, &'static str> {
        let mut notes = BTreeMap::new();
        for p in self.compared_written(typ, want) {
            let Some(v) = get_path(want, &p) else {
                continue;
            };
            let written = match entry.written.get(&p) {
                Some(kept) => self.written_against(v, kept, holders),
                None => Written::Same,
            };
            let v = match written {
                Written::Same => v.clone(),
                Written::Differs(note) => {
                    notes.extend(note.map(|n| (p.clone(), n)));
                    Json::String("(write-only)".into())
                }
            };
            set_path(doc, &p, v);
        }
        notes
    }

    /// The paths of `doc` compared by the digest of what was sent: its
    /// type's write-only attributes, and those a secret is revealed into.
    fn compared_written(&self, typ: &str, doc: &Json) -> Vec<String> {
        let mut out: Vec<String> = self
            .schema()
            .write_only_of(typ)
            .into_iter()
            .map(str::to_string)
            .collect();
        for p in self.revealed_attrs(typ, doc) {
            if !out.contains(&p) {
                out.push(p);
            }
        }
        out
    }

    /// The digest state keeps of a write-only value: keyed with the
    /// deployment's master, `hmac-sha256:..`; none in a run that does not
    /// hold it (never an unkeyed digest: a short password's is a table
    /// lookup away from it, R-164). Of a value holding secrets a provider
    /// holds, two parts, its text's and its holders' (`hmac-sha256:T/H`,
    /// [`Providers::held_by`]), so a replaced holder is told from an
    /// edited value; holders this run cannot tell are `before`'s.
    fn written_digest(&self, v: &Json, holders: &Holders, before: Option<&str>) -> Option<String> {
        let k = self.digest_key.as_ref()?;
        let text = k.digest(crate::approval::canonical_json(v).as_bytes());
        let held = match self.held_text(v, holders) {
            HeldBy::Nothing => None,
            HeldBy::Known(t) => Some(k.digest(t.as_bytes())),
            HeldBy::Unknown => before
                .and_then(|b| WrittenDigest::parse(b)?.holders)
                .map(str::to_string),
        };
        Some(match held {
            Some(h) => format!("hmac-sha256:{text}/{h}"),
            None => format!("hmac-sha256:{text}"),
        })
    }

    /// How `v` compares with `kept`, the digest of what was last sent
    /// ([`Providers::written_digest`]). A run that does not hold the
    /// master cannot tell: the value differs, and is compared at apply (a
    /// leaf it derives may still be proven unchanged,
    /// [`Providers::derived_before`]). Holders this run cannot tell are
    /// taken as the same.
    fn written_against(&self, v: &Json, kept: &str, holders: &Holders) -> Written {
        let (Some(k), Some(kept)) = (&self.digest_key, WrittenDigest::parse(kept)) else {
            return Written::Differs(None);
        };
        if k.digest(crate::approval::canonical_json(v).as_bytes()) != kept.text {
            // A digest of one part (one written before the parts) says
            // nothing of which.
            return Written::Differs(kept.holders.map(|_| provider::TEMPLATE_CHANGED));
        }
        match (self.held_text(v, holders), kept.holders) {
            (HeldBy::Known(t), Some(h)) if k.digest(t.as_bytes()) != h => {
                Written::Differs(Some(provider::HOLDER_REPLACED))
            }
            _ => Written::Same,
        }
    }

    /// Who holds each secret a provider holds in `v` (a template's
    /// placeholder, a marker), as the digest of what was sent names it: a
    /// secret's label names where it is read, not its bytes, so a new
    /// object under the same label (a key replaced) is a new value.
    fn held_text(&self, v: &Json, holders: &Holders) -> HeldBy {
        let labels = held_in(v);
        if labels.is_empty() {
            return HeldBy::Nothing;
        }
        let mut text = String::new();
        for label in labels {
            let Some(by) = self.held_by(&label, holders) else {
                return HeldBy::Unknown;
            };
            text.push_str(&format!("{label} held by {by}\n"));
        }
        HeldBy::Known(text)
    }

    /// Who holds the secret `label`, one rule: the holder's provider and
    /// its remote id, and where the remote is the program's name
    /// ([`Schema::named_identity`], `metadata.name`), which a replace
    /// keeps, the object's computed `id` if its type has one. Empty while
    /// no object holds it (not made, or being replaced); `None` while this
    /// run cannot tell (that `id`, of an object it did not read).
    fn held_by(&self, label: &str, holders: &Holders) -> Option<String> {
        if let Some(h) = self.held.borrow().get(label) {
            return Some(format!("{} {} {}", h.provider, h.deployment, h.remote));
        }
        let Some((provider, remote, typ)) = crate::value::null_owner(label)
            .and_then(|o| holders.objects.get(&o).map(|(p, r)| (p, r, o.0)))
        else {
            return Some(String::new());
        };
        let schema = self.schema();
        if schema.named_identity(&typ).is_none()
            || schema.class_of(&typ, crate::schema::IDENTITY).is_none()
        {
            return Some(format!("{provider} {remote}"));
        }
        let id = get_path(
            &holders.world?.get(&key(&typ, remote))?.computed,
            crate::schema::IDENTITY,
        )?;
        Some(format!(
            "{provider} {remote} {}",
            crate::approval::canonical_json(id)
        ))
    }

    /// The digests of `doc`'s write-only attributes, by path; one this run
    /// cannot digest keeps what `before` had.
    pub(super) fn written(
        &self,
        typ: &str,
        doc: &Json,
        before: &BTreeMap<String, String>,
        holders: &Holders,
    ) -> BTreeMap<String, String> {
        self.compared_written(typ, doc)
            .into_iter()
            .filter_map(|p| {
                let was = before.get(&p);
                let d = self
                    .written_digest(get_path(doc, &p)?, holders, was.map(String::as_str))
                    .or_else(|| was.cloned())?;
                Some((p, d))
            })
            .collect()
    }

    /// The derivation digest of each leaf of `doc` that holds a derived
    /// value (`secrets::standin::Registry::digest`), by path: an object's
    /// members each, anything else (a string, a list) whole.
    pub fn derivations(&self, doc: &Json) -> BTreeMap<String, String> {
        fn walk(
            v: &Json,
            path: &str,
            standins: &crate::secrets::standin::Registry,
            out: &mut BTreeMap<String, String>,
        ) {
            match v {
                Json::Object(m) if provider::marker(v).is_none() => {
                    for (k, x) in m {
                        walk(x, &crate::address::path_join(path, k), standins, out);
                    }
                }
                _ if !path.is_empty() => {
                    if let Some(d) = standins.digest(v) {
                        out.insert(path.to_string(), d);
                    }
                }
                _ => {}
            }
        }
        let mut out = BTreeMap::new();
        walk(doc, "", &self.secrets.standins(), &mut out);
        out
    }

    /// In a run that does not hold the master (R-164): each leaf of `want`
    /// whose derivation digest is the one state recorded at the last apply
    /// is unchanged, so the world's side is taken to be it (the values
    /// are the stand-ins of what was applied), and the leaf is proven
    /// ([`Providers::proven`]). Any other leaf that holds a stand-in
    /// differs: a change that needs the master.
    pub(super) fn derived_before(
        &self,
        addr: &Address,
        entry: &StateEntry,
        want: &Json,
        doc: &mut Json,
    ) {
        let standins = self.secrets.standins();
        if !standins.active() {
            return;
        }
        let mut proven = Vec::new();
        for (path, d) in &entry.derived {
            let Some(w) = get_path(want, path) else {
                continue;
            };
            if standins.digest(w).as_ref() == Some(d) {
                set_path(doc, path, w.clone());
                proven.push(path.clone());
            }
        }
        if !proven.is_empty() {
            self.proven.borrow_mut().insert(addr.clone(), proven);
        }
    }

    /// Of `paths` of `addr`, those whose value the world answers (not a
    /// write-only attribute's): one a run without the master proves
    /// unchanged in the program, but cannot compare with the world.
    pub fn answered(&self, addr: &Address, paths: &[String]) -> Vec<String> {
        let wo = self.schema().write_only_of(&addr.typ);
        paths
            .iter()
            .filter(|p| {
                !wo.iter()
                    .any(|w| p.as_str() == *w || p.starts_with(&format!("{w}.")))
            })
            .cloned()
            .collect()
    }

    /// The leaves of `addr` the last plan proved unchanged without the
    /// master (R-164), by path.
    pub fn proven(&self, addr: &Address) -> Vec<String> {
        self.proven.borrow().get(addr).cloned().unwrap_or_default()
    }

    /// The paths of `a`'s changes a run that does not hold the master
    /// cannot make (R-164): each whose value holds a stand-in, and every
    /// one of a create's (or a replacement's) when its document `desired`
    /// holds one; an update whose unchanged secret leaf only stands in,
    /// where the world does not answer it (a write-only one), too. Empty
    /// when the run holds the master.
    pub fn needs_master(&self, a: &Action, desired: Option<&Json>) -> Vec<String> {
        let standins = self.secrets.standins();
        if !standins.active() {
            return Vec::new();
        }
        let carries = |j: &Json| standins.carries(j);
        let changed: Vec<String> = a
            .changes
            .iter()
            .filter(|c| c.after.as_ref().is_some_and(carries))
            .map(|c| c.path.clone())
            .collect();
        match a.kind {
            ActionKind::Create | ActionKind::Adopt | ActionKind::Replace { .. } => {
                match (changed.is_empty(), desired.is_some_and(carries)) {
                    (true, true) => vec![String::new()],
                    _ => changed,
                }
            }
            ActionKind::Update | ActionKind::Drift => {
                if !changed.is_empty() {
                    return changed;
                }
                // Its provider leaves the write-only one as it is.
                if self.keeps(&a.addr.typ) {
                    return Vec::new();
                }
                let wo = self.schema().write_only_of(&a.addr.typ);
                self.proven(&a.addr)
                    .into_iter()
                    .filter(|p| {
                        wo.iter()
                            .any(|w| p.as_str() == *w || p.starts_with(&format!("{w}.")))
                    })
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    /// [`Providers::stored`] of each document.
    pub fn stored_world(&self, docs: &BTreeMap<Address, Json>) -> BTreeMap<Address, Json> {
        docs.iter()
            .map(|(a, d)| (a.clone(), self.stored(&a.typ, d)))
            .collect()
    }
}

/// The labels of the secrets a provider holds in `v`, in order: each
/// template's placeholders and each secret marker.
fn held_in(v: &Json) -> Vec<String> {
    fn walk(v: &Json, out: &mut Vec<String>) {
        if let Some((key, label)) = provider::marker(v) {
            if key == provider::SECRET_KEY {
                out.push(label.to_string());
            }
            return;
        }
        match v {
            Json::String(t) => out.extend(held::labels(t)),
            Json::Object(m) => m.values().for_each(|x| walk(x, out)),
            Json::Array(xs) => xs.iter().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, &mut out);
    out
}

/// The objects a held secret is read from, as the digest of a write it
/// was revealed into names them ([`Providers::held_by`]): each address's
/// provider and remote id as state has them, whether or not its provider
/// is configured in this run, and the world this run read.
pub(super) struct Holders<'a> {
    objects: BTreeMap<(String, String), (String, String)>,
    world: Option<&'a World>,
}

impl<'a> Holders<'a> {
    pub(super) fn of(state: &State, world: Option<&'a World>) -> Holders<'a> {
        let objects = state
            .resources
            .iter()
            .filter_map(|(k, e)| {
                let a = state::parse_key(k)?;
                Some(((a.typ, a.name), (e.provider.clone(), e.remote.clone())))
            })
            .collect();
        Holders { objects, world }
    }

    /// Without the objects `retracted`: a replaced one is made anew.
    pub(super) fn without(mut self, retracted: &BTreeSet<Address>) -> Holders<'a> {
        self.objects.retain(|(typ, name), _| {
            !retracted.contains(&Address {
                typ: typ.clone(),
                name: name.clone(),
            })
        });
        self
    }
}

/// Who holds the secrets a provider holds in a value
/// ([`Providers::held_text`]).
enum HeldBy {
    /// It holds none.
    Nothing,
    Known(String),
    /// This run cannot tell who holds one.
    Unknown,
}

/// How a write-only value compares with what was last sent
/// ([`Providers::written_against`]).
enum Written {
    Same,
    /// It differs: where the digest's parts tell which, the note a kept
    /// line says ([`provider::HOLDER_REPLACED`],
    /// [`provider::TEMPLATE_CHANGED`]).
    Differs(Option<&'static str>),
}

/// The digest state keeps of a value sent, its parts
/// ([`Providers::written_digest`]): its text's, and its holders' where it
/// holds a secret a provider holds.
struct WrittenDigest<'a> {
    text: &'a str,
    holders: Option<&'a str>,
}

impl<'a> WrittenDigest<'a> {
    fn parse(kept: &'a str) -> Option<WrittenDigest<'a>> {
        let d = kept.strip_prefix("hmac-sha256:")?;
        Some(match d.split_once('/') {
            Some((text, holders)) => WrittenDigest {
                text,
                holders: Some(holders),
            },
            None => WrittenDigest {
                text: d,
                holders: None,
            },
        })
    }
}
