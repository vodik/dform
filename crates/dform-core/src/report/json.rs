//! The report as JSON (`--json`, the plan file): its changes, lines, conflicts,
//! groups and policies, each value as the redactor shows it.

use super::deformation::{Deformation, Line, Op};
use super::errors::Diag;
use super::groups::{Group, group_address, group_copy};
use super::labels::kind_name;
use super::mask::Shown;
use super::policy;
use super::tree::Site;
use super::{Report, Why};
use crate::provider::ActionKind;
use serde_json::{Value as Json, json};

impl Report {
    /// The plan as one JSON document (`plan --json`): the ticks, each
    /// with what it waits on and its changes (kind, address, attribute
    /// changes, where each is derived and why it changed since the last
    /// apply), `later`, the diagnostics; a null as `{"null": LABEL,
    /// "class": C}`, a secret or a sensitive value as `{"sensitive": LABEL}`.
    pub fn json(&self) -> Json {
        let mut j = json!({
            "stack": self.stack,
            "up_to_date": self.up_to_date(),
            "summary": self.summary_json(),
            "ticks": self.ticks_json(),
            "later": self.later_json(),
            "policy": self.policy.iter().map(policy::Line::json).collect::<Vec<_>>(),
            "shadowed": self.shadowed.iter().map(diag_json).collect::<Vec<_>>(),
            "conflicts": self.conflicts.iter().map(diag_json).collect::<Vec<_>>(),
            "moved": self.moved.iter().map(|(old, new)| json!({
                "from": {"address": old.to_string(), "type": old.typ, "name": old.name},
                "to": {"address": new.to_string(), "type": new.typ, "name": new.name},
            })).collect::<Vec<_>>(),
            "denied": self.denies.iter().enumerate().map(|(i, text)| {
                let row = self.denied.get(i);
                json!({
                    "text": text,
                    "message": row.map(|r| r.message.clone()),
                    "address": row.map(|r| r.addr.clone()).filter(|a| !a.is_empty()),
                    "site": row.and_then(|r| r.site.clone()).filter(|_| self.why != Why::None),
                })
            }).collect::<Vec<_>>(),
            "held_for_approval": self.approvals.iter().map(|a| json!({
                "address": a.addr,
                "reason": a.reason,
                "site": a.site.as_ref().filter(|_| self.why != Why::None),
            })).collect::<Vec<_>>(),
            "apply": self.apply_line(),
        });
        // What derives no resource (R-120), only when something does not.
        if !self.not_planned.is_empty() {
            j["not_planned"] = self
                .not_planned
                .iter()
                .map(|n| {
                    json!({
                        "address": n.addr.to_string(),
                        "reason": n.reason,
                        "site": n.site.as_ref().filter(|_| self.why != Why::None),
                    })
                })
                .collect::<Vec<_>>()
                .into();
        }
        // Host labels a reader may mistake for others (R-134).
        let confusable = self.confusable_lines();
        if !confusable.is_empty() {
            j["confusable_hosts"] = confusable.into();
        }
        // The values given at creation only it keeps (R-198), only when
        // it keeps one.
        let kept = self.kept_json();
        if !kept.is_empty() {
            j["kept"] = kept.into();
        }
        // What the plan empties (R-80), only when it empties something.
        if !self.warnings.is_empty() {
            j["warnings"] = self.warnings_json().into();
        }
        j
    }

    /// The nulls `on` names, each with its class.
    fn nulls_json(&self, on: &mut dyn Iterator<Item = &String>) -> Json {
        on.map(|l| {
            let class = self
                .classes
                .get(l)
                .cloned()
                .unwrap_or_else(|| "unknown".into());
            json!({"null": crate::address::label(l), "class": class})
        })
        .collect()
    }

    /// Changes `ds`, each with the copy it is a resource of.
    fn changes_json(&self, ds: &[&Deformation]) -> Json {
        ds.iter()
            .map(|d| {
                let mut j = self.change_json(d);
                // The copy it is a resource of, innermost (R-67).
                if let Some(i) = self.instances.enclosing(&d.addr).first() {
                    j["instance"] = json!(i.to_string());
                }
                j
            })
            .collect()
    }

    /// A site, when the report says why.
    fn site_json(&self, s: &Option<Site>) -> Json {
        match s {
            Some(s) if self.why != Why::None => json!(s),
            _ => Json::Null,
        }
    }

    fn group_json(&self, g: &Group) -> Json {
        json!({
            "kind": "group",
            "address": group_address(g),
            "instance": group_copy(g),
            "reads": g.reads,
            "on": self.nulls_json(&mut g.on.iter()),
            "reason": g.reason,
            "after": g.resolves_after,
            "site": self.site_json(&g.site),
        })
    }

    /// How many changes, of each kind, and what else the plan holds.
    fn summary_json(&self) -> serde_json::Map<String, Json> {
        let mut summary = serde_json::Map::new();
        summary.insert("changes".into(), json!(self.changes()));
        for (k, n) in self.kinds() {
            // A forget is counted only where there is one (R-154).
            if k != "forget" || n > 0 {
                summary.insert(k.into(), json!(n));
            }
        }
        summary.insert("no_op".into(), json!(self.noops));
        summary.insert("ticks".into(), json!(self.sections().len()));
        summary.insert("approvals".into(), json!(self.approvals.len()));
        summary.insert("undetermined".into(), json!(self.policies.len()));
        summary.insert("conflicts".into(), json!(self.conflicts.len()));
        summary
    }

    /// Each tick: what it waits on, its changes, deposed objects and groups.
    fn ticks_json(&self) -> Vec<Json> {
        self.sections()
            .iter()
            .map(|(t, s)| {
                let mut j = json!({
                    "tick": t,
                    "after": (*t != self.tick).then(|| t - 1),
                    "waits_on": self.nulls_json(&mut s.waits.iter()),
                    "changes": self.changes_json(&s.changes),
                    "deposed": s.deposed.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "groups": s.groups.iter().map(|g| self.group_json(g)).collect::<Vec<_>>(),
                });
                // Only a tick planned against an offline schema says so.
                if !s.provisional.is_empty() {
                    j["provisional"] = json!(true);
                }
                j
            })
            .collect()
    }

    /// What no tick of this plan decides: groups, policies, held blocks.
    fn later_json(&self) -> Vec<Json> {
        let mut later: Vec<Json> = Vec::new();
        for g in self.groups.iter().filter(|g| g.resolves_after.is_none()) {
            later.push(self.group_json(g));
        }
        for p in &self.policies {
            later.push(json!({
                "kind": if p.refinement { "refinement" } else { "deny" },
                "message": p.message,
                "status": if p.refinement {
                    "deferred"
                } else if p.may_derive {
                    "may_derive"
                } else {
                    "undetermined"
                },
                "on": self.nulls_json(&mut p.on.iter()),
                "reason": p.reason,
                "after": p.after,
                "site": self.site_json(&p.site),
            }));
        }
        for b in self.pending.iter().filter(|b| b.resolves_after.is_none()) {
            let ds: Vec<&Deformation> = b.deformations.iter().collect();
            later.push(json!({
                "kind": "held",
                "on": self.nulls_json(&mut b.on.iter()),
                "provisional": b.provisional.is_some(),
                "changes": self.changes_json(&ds),
            }));
        }
        later
    }

    /// What the plan empties (R-80).
    fn warnings_json(&self) -> Vec<Json> {
        self.warnings
            .iter()
            .map(|w| {
                json!({
                    "rule": w.statement.as_ref().map(|_| w.name.clone()),
                    "statement": w.statement,
                    "relation": w.statement.is_none().then(|| w.name.clone()),
                    "deletes": w.deleted,
                    "rows_at_last_apply": w.statement.is_none().then_some(w.rows),
                    "because": w.because,
                })
            })
            .collect()
    }

    /// Each value given at creation only the plan keeps (R-198): of an
    /// object it leaves as it is, or one it changes otherwise.
    fn kept_json(&self) -> Vec<Json> {
        let pending = self.pending.iter().flat_map(|b| b.deformations.iter());
        let ds = self.kept.iter().chain(&self.definite).chain(pending);
        ds.flat_map(|d| {
            d.kept.iter().map(move |k| {
                let mut j = json!({
                    "address": d.addr.to_string(),
                    "type": d.addr.typ,
                    "name": d.addr.name,
                    "path": k.line.path,
                    "before": k.line.before.json(),
                    "after": k.line.after.json(),
                    "lifecycle": "bootstrap",
                    "site": d.site.as_ref().filter(|_| self.why != Why::None),
                });
                if let Some(n) = &k.note {
                    j["note"] = json!(n);
                }
                j
            })
        })
        .collect()
    }

    fn change_json(&self, d: &Deformation) -> Json {
        let explained = self.why != Why::None;
        let mut m = serde_json::Map::new();
        m.insert("kind".into(), json!(kind_name(&d.kind)));
        m.insert("address".into(), json!(d.addr.to_string()));
        m.insert("type".into(), json!(d.addr.typ));
        m.insert("name".into(), json!(d.addr.name));
        match d.kind {
            ActionKind::Replace { create_first } => {
                m.insert("create_first".into(), json!(create_first));
                m.insert("immutable".into(), json!(d.forces));
            }
            ActionKind::DeleteDeposed => {
                m.insert("deposed".into(), json!(true));
            }
            _ => {}
        }
        m.insert(
            "changes".into(),
            d.lines
                .iter()
                .map(|l| line_json(l, explained))
                .collect::<Vec<_>>()
                .into(),
        );
        let held: Vec<Json> = self
            .approvals
            .iter()
            .filter(|a| a.addr == d.addr.to_string())
            .map(|a| json!({"approval": a.reason, "site": a.site.as_ref().filter(|_| explained)}))
            .collect();
        if !held.is_empty() {
            m.insert("held".into(), held.into());
        }
        if explained {
            m.insert("site".into(), json!(d.site));
            m.insert("because".into(), json!(d.because));
            if let Some(c) = &d.custody {
                m.insert("custody".into(), json!(c));
            }
        }
        if let Some((_, why)) = &d.gone {
            let why = d.because.clone().unwrap_or_else(|| why.clone());
            m.insert("reason".into(), why.into());
        }
        Json::Object(m)
    }
}

fn line_json(l: &Line, explained: bool) -> Json {
    let op = match l.op {
        Op::Leaf => "set",
        Op::Add => "add",
        Op::Remove => "remove",
    };
    let mut m = serde_json::Map::new();
    m.insert("op".into(), json!(op));
    m.insert("path".into(), json!(l.path));
    m.insert("before".into(), l.before.json());
    m.insert("after".into(), l.after.json());
    // A host with a label that is not ASCII, in the form a provider
    // receives it (R-134).
    if let Shown::Value(Json::String(s)) = &l.after
        && let Some(a) = crate::uri::ascii_form(s)
    {
        m.insert("host_ascii".into(), json!(a));
    }
    if !l.leaves.is_empty() {
        m.insert(
            "leaves".into(),
            l.leaves
                .iter()
                .map(|x| line_json(x, explained))
                .collect::<Vec<_>>()
                .into(),
        );
    }
    if explained && let Some(s) = &l.site {
        m.insert("site".into(), json!(s));
    }
    if !l.chain.is_empty() {
        m.insert("chain".into(), json!(l.chain));
    }
    Json::Object(m)
}

fn diag_json(d: &Diag) -> Json {
    json!({
        "address": d.addr.attr(&d.path),
        "type": d.addr.typ,
        "name": d.addr.name,
        "path": d.path,
        "reason": d.reason,
        "rank": d.rank,
        "witnesses": d.witnesses.iter().map(|w| json!({
            "rank": w.rank,
            "value": w.value.json(),
            "from": w.from,
        })).collect::<Vec<_>>(),
    })
}
