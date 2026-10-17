//! Queries: an extern's answer, discovery's inventory relations, and
//! what an Apply Create with an idempotency key made (`CREATED`).

use super::{CREATED, INVENTORY, Providers};
use crate::address::Address;
use crate::ast::{Atom, Term};
use crate::plugin::backend::Ticket;
use crate::plugin::pb;
use crate::provider::{self};
use crate::value::{NullClass, Value};
use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// An extern's call sent to a provider, its answer not taken yet
/// ([`Providers::submit_extern`]).
pub struct Asked {
    link: usize,
    ticket: Ticket,
}

impl Providers {
    /// Query (E DR-18): an extern's answer, the rows of `pred` whose `+`
    /// columns (`plus`) are `inputs`. No row is an answer too.
    pub fn query(&self, pred: &str, plus: &[bool], inputs: &[Value]) -> Result<Vec<Vec<Value>>> {
        let i = self
            .loaded()
            .externs
            .get(pred)
            .copied()
            .unwrap_or(self.fallback);
        self.query_at(i, pred, plus, inputs)
    }

    /// An extern's answer (`query`), its secret columns (`secret(T)`) as
    /// the provider holds them: each a secret null, labeled
    /// [`crate::externs::secret_label`], whose place ([`provider::Held`])
    /// the run keeps for the Apply documents that carry it. A secret column
    /// answered with its value is refused: the bytes would be in the
    /// engine, the plan and state.
    pub fn query_extern(
        &self,
        f: &crate::ast::ExternFn,
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let asked = self.submit_extern(f, inputs)?;
        self.flush();
        self.answer_extern(f, inputs, asked)
    }

    /// An extern's call ([`Providers::query_extern`]) sent to its provider,
    /// its answer taken later by [`Providers::answer_extern`]: a round's
    /// calls are all sent before any answer is taken, so a provider that
    /// can answers them at once. Sent once [`Providers::flush`] is called.
    pub fn submit_extern(&self, f: &crate::ast::ExternFn, inputs: &[Value]) -> Result<Asked> {
        let i = self
            .loaded()
            .externs
            .get(&f.name)
            .copied()
            .unwrap_or(self.fallback);
        let ticket = self.submit(i, Self::extern_request(f, inputs))?;
        Ok(Asked { link: i, ticket })
    }

    /// The Query an extern's call is: its `+` columns the inputs, its
    /// secret columns marked (none marked when it has none).
    fn extern_request(f: &crate::ast::ExternFn, inputs: &[Value]) -> pb::QueryRequest {
        let secret: Vec<bool> = f.args.iter().map(crate::externs::is_secret).collect();
        pb::QueryRequest {
            pred: f.name.clone(),
            input: f.args.iter().map(|b| b.input).collect(),
            inputs: inputs.iter().map(pb::Value::from).collect(),
            secret: match secret.contains(&true) {
                true => secret,
                false => Vec::new(),
            },
            ..Default::default()
        }
    }

    /// The answer to the call `asked` ([`Providers::submit_extern`]) of `f`
    /// with `inputs`. One that failed is asked again alone, under its
    /// provider's retry policy.
    pub fn answer_extern(
        &self,
        f: &crate::ast::ExternFn,
        inputs: &[Value],
        asked: Asked,
    ) -> Result<Vec<Vec<Value>>> {
        let i = asked.link;
        let answer = {
            let mut link = self.links[i].borrow_mut();
            link.wait(asked.ticket)
                .and_then(|r| link.expect::<Vec<pb::Row>>("Query", r))
        };
        let rows = match answer {
            Ok(rows) => rows,
            Err(_) => self.call(i, Self::extern_request(f, inputs))?,
        };
        self.extern_rows(i, f, inputs, rows)
    }

    /// The rows a provider answered an extern's call with, as values: a
    /// secret column ([`Providers::query_extern`]) as where it is held.
    fn extern_rows(
        &self,
        i: usize,
        f: &crate::ast::ExternFn,
        inputs: &[Value],
        rows: Vec<pb::Row>,
    ) -> Result<Vec<Vec<Value>>> {
        let secret: Vec<bool> = f.args.iter().map(crate::externs::is_secret).collect();
        let mut out = Vec::new();
        for r in &rows {
            let mut row = Vec::new();
            for (c, v) in r.values.iter().enumerate() {
                if !secret.get(c).copied().unwrap_or(false) {
                    row.push(Value::try_from(v)?);
                    continue;
                }
                let held = match &v.kind {
                    Some(pb::value::Kind::Null(n)) if n.class == pb::NullClass::Secret as i32 => {
                        n.held.as_ref()
                    }
                    _ => None,
                };
                let Some(h) = held else {
                    bail!(
                        "provider {}: extern {} column {} is secret(T), and it answered with \
                         a value, not where it holds one (a SECRET null with `held`): a secret \
                         never enters dform",
                        self.names[i],
                        f.name,
                        c + 1
                    );
                };
                let label = crate::externs::secret_label(&f.name, inputs, c);
                self.held.borrow_mut().insert(
                    label.clone(),
                    provider::Held {
                        provider: h.provider.clone(),
                        deployment: h.deployment.clone(),
                        typ: h.r#type.clone(),
                        remote: h.remote.clone(),
                        path: h.path.clone(),
                        digest: h.digest.clone(),
                    },
                );
                let ty = match &f.args[c].ty {
                    Some(crate::ast::TypeExpr::Apply(_, a)) => match a.as_slice() {
                        [crate::ast::TypeExpr::Name(t)] => t.clone(),
                        _ => String::new(),
                    },
                    _ => String::new(),
                };
                row.push(Value::Null {
                    label,
                    class: NullClass::Secret,
                    ty,
                });
            }
            out.push(row);
        }
        Ok(out)
    }

    fn query_at(
        &self,
        i: usize,
        pred: &str,
        plus: &[bool],
        inputs: &[Value],
    ) -> Result<Vec<Vec<Value>>> {
        let rows: Vec<pb::Row> = self.call(
            i,
            pb::QueryRequest {
                pred: pred.to_string(),
                input: plus.to_vec(),
                inputs: inputs.iter().map(pb::Value::from).collect(),
                secret: Vec::new(),
                ..Default::default()
            },
        )?;
        rows.iter()
            .map(|r| r.values.iter().map(Value::try_from).collect())
            .collect()
    }

    /// Discovery: the inventory relations of every provider that has one,
    /// of the types `types` names (every type: `None`), each asked with its
    /// type bound.
    pub fn discover(&self, types: Option<&BTreeSet<String>>) -> Result<Vec<Atom>> {
        let mut out = Vec::new();
        for (i, link) in self.links.iter().enumerate() {
            if !link.borrow().has("inventory") {
                continue;
            }
            for (pred, arity) in INVENTORY {
                let mut rows = Vec::new();
                match types {
                    None => rows = self.query_at(i, pred, &vec![false; arity], &[])?,
                    Some(ts) => {
                        let mut plus = vec![false; arity];
                        plus[0] = true;
                        for t in ts {
                            rows.extend(self.query_at(i, pred, &plus, &[Value::Str(t.clone())])?);
                        }
                    }
                }
                for row in rows {
                    out.push(Atom {
                        pred: crate::ast::Name::user(pred),
                        args: row.into_iter().map(Term::Val).collect(),
                        record: None,
                        span: Default::default(),
                    });
                }
            }
        }
        Ok(out)
    }

    /// The object an Apply Create or Replace of `addr` with the
    /// idempotency key `key` made (`CREATED`): its remote id. `None` when
    /// it made none, or when its provider cannot say (no `managed`
    /// capability): then only a retry with the same key finds out.
    pub fn created(&self, addr: &Address, key: &str) -> Result<Option<String>> {
        let i = self.route(&addr.typ);
        if !self.link(i)?.borrow().has("managed") {
            return Ok(None);
        }
        let s = |x: &str| Value::Str(x.to_string());
        let rows = self.query_at(
            i,
            CREATED,
            &[true, true, true, false],
            &[s(&addr.typ), s(&addr.name), s(key)],
        )?;
        match rows.first().map(Vec::as_slice) {
            None => Ok(None),
            Some([_, _, _, Value::Str(remote)]) => Ok(Some(remote.clone())),
            Some(row) => bail!(
                "provider {}: {CREATED} answers (Type, Name, Key, RemoteId), not {row:?}",
                self.names[i]
            ),
        }
    }
}
