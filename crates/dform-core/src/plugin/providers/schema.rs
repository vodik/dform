//! The providers' schema: loaded once from their Schema answers, shared
//! between the names a provider has, and extended as a provider configured
//! from the program's settings learns more (R-110, R-126).

use super::Providers;
use crate::ast::{Atom, Term};
use crate::plugin::pb;
use crate::plugin::wire;
use crate::schema::Schema;
use crate::value::Value;
use anyhow::{Context, Result, bail};
use std::cell::{OnceCell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

/// A link's schema is its provider's, the types in `namespace`: one
/// Schema answer of a spec serves each link of it ([`Providers::load_schema`]).
pub(super) struct Share {
    pub(super) spec: String,
    pub(super) namespace: String,
    /// The provider a `use .. as` renames (R-115).
    pub(super) of: Option<String>,
}

/// What the providers' Schema calls answered.
pub(super) struct Loaded {
    /// The schema as loaded, then each extension of it in the run.
    pub(super) schema: Generation,
    /// The catalog facts of the types learned so, for the evaluation.
    pub(super) learned: RefCell<Vec<Atom>>,
    /// Type -> the provider serving it.
    pub(super) owner: BTreeMap<String, usize>,
    /// Extern predicate -> the provider answering it.
    pub(super) externs: BTreeMap<String, usize>,
    /// The types asked for; `None`: every one.
    pub(super) scope: Option<BTreeSet<String>>,
}

/// The providers' schema, and what a provider configured from the
/// program's settings later serves beyond it ([`Providers::learn`]): each
/// extension a new generation after the last, none changed once set, so a
/// schema read earlier in the run stays as it was read.
pub(super) struct Generation {
    schema: Schema,
    next: OnceCell<Box<Generation>>,
}

impl Generation {
    fn new(schema: Schema) -> Generation {
        Generation {
            schema,
            next: OnceCell::new(),
        }
    }

    fn latest(&self) -> &Generation {
        let mut g = self;
        while let Some(n) = g.next.get() {
            g = n;
        }
        g
    }
}

impl Providers {
    /// Ask every provider for its schema: the rows of the `scope` types
    /// (and the types they alias) and every `type_provider` and
    /// `type_alias` row, or, with no scope, all of it. Once per run; a
    /// second call is an error.
    pub fn load_schema(&self, scope: Option<&BTreeSet<String>>) -> Result<()> {
        let mut schema = Schema::default();
        let mut owner = BTreeMap::new();
        let mut externs = BTreeMap::new();
        let types = scope.map(|s| pb::TypeFilter {
            names: s.iter().cloned().collect(),
        });
        // Each spec's answer and the namespace it is in: a provider's
        // other names take it renamed rather than ask again (R-115).
        let mut asked: BTreeMap<&str, (pb::SchemaResponse, &str)> = BTreeMap::new();
        for i in 0..self.links.len() {
            let resp = self.schema_answer(i, types.as_ref(), &mut asked)?;
            let s = self.link_schema(i, &resp)?;
            for t in s.provider_of.keys().chain(s.attrs.keys().map(|(t, _)| t)) {
                owner.entry(t.clone()).or_insert(i);
            }
            for e in resp.externs.iter().map(|e| &e.pred).chain(s.externs.keys()) {
                externs.entry(e.clone()).or_insert(i);
            }
            schema = schema.merge(s)?;
        }
        self.wire_names(&schema);
        let loaded = Loaded {
            schema: Generation::new(schema),
            learned: RefCell::new(Vec::new()),
            owner,
            externs,
            scope: scope.cloned(),
        };
        if self.loaded.set(loaded).is_err() {
            bail!("internal: the providers' schema is loaded once");
        }
        Ok(())
    }

    /// Link `i`'s Schema answer for `types`: another name's of its
    /// provider, renamed, when one was asked (`asked`, R-115); else asked
    /// for the types under each of its names, and kept in `asked`.
    fn schema_answer<'s>(
        &'s self,
        i: usize,
        types: Option<&pb::TypeFilter>,
        asked: &mut BTreeMap<&'s str, (pb::SchemaResponse, &'s str)>,
    ) -> Result<pb::SchemaResponse> {
        let share = self.shares.get(i).and_then(Option::as_ref);
        if let Some((resp, ns)) = share.and_then(|s| asked.get(s.spec.as_str())) {
            let mut resp = resp.clone();
            let to = share.map_or(*ns, |s| s.namespace.as_str());
            wire::Rename::new(*ns, to).schema(&mut resp);
            return Ok(resp);
        }
        // The types each name of the provider is asked for, under this
        // one's.
        let mut types = types.cloned();
        if let (Some(t), Some(s)) = (&mut types, share) {
            let others = self.shares.iter().flatten();
            for o in others.filter(|o| o.spec == s.spec && o.namespace != s.namespace) {
                let r = wire::Rename::new(o.namespace.as_str(), s.namespace.as_str());
                let more: Vec<String> = t.names.iter().map(|n| r.name(n)).collect();
                t.names.extend(more);
            }
            t.names.sort();
            t.names.dedup();
        }
        let ask = |names: Option<Vec<String>>| -> Result<pb::SchemaResponse> {
            let types = names.map(|names| pb::TypeFilter { names });
            self.call(
                i,
                pb::SchemaRequest {
                    types,
                    ..Default::default()
                },
            )
        };
        // A link of the tree's pool is asked once for what the tree's
        // deployments name.
        let resp = match (self.pooled.get(i).and_then(Option::as_ref), &self.pool) {
            (Some(e), Some(pool)) => {
                let named: Option<BTreeSet<String>> = types.map(|t| t.names.into_iter().collect());
                let basis = crate::plugin::pool::basis(self.sent.borrow().get(&i));
                e.schema(pool, basis, named.as_ref(), ask)?
            }
            _ => ask(types.map(|t| t.names))?,
        };
        if let Some(s) = share {
            asked.insert(&s.spec, (resp.clone(), &s.namespace));
        }
        Ok(resp)
    }

    /// Link `i`'s schema, from its Schema answer `resp`: its types under
    /// the name the program gives it, which refinements it checks, and
    /// the settings its handshake declares.
    fn link_schema(&self, i: usize, resp: &pb::SchemaResponse) -> Result<Schema> {
        let share = self.shares.get(i).and_then(Option::as_ref);
        let facts = resp
            .facts
            .iter()
            .map(Atom::try_from)
            .collect::<Result<Vec<Atom>>>()?;
        let mut s = Schema::from_facts(&facts)
            .and_then(Schema::imported)
            .with_context(|| format!("the schema of provider {}", self.names[i]))?;
        // A provider takes another name only when its types are under its
        // own (`ovh.instance`): one it names otherwise (the fake cloud's
        // `net.vpc`) stays as it is under every name.
        if let Some(Share {
            namespace,
            of: Some(of),
            ..
        }) = share
            && let Some(t) = s
                .provider_of
                .keys()
                .chain(s.attrs.keys().map(|(t, _)| t))
                .find(|t| !t.starts_with(&format!("{namespace}.")))
        {
            bail!(
                "`use {of} as {namespace}`: provider {of} serves {t}, a type not named under \
                 it, which no other name renames: only a provider whose types are `{of}.T` \
                 takes another name"
            );
        }
        if resp.checks_refinements {
            s.checks_refinements = s
                .provider_of
                .keys()
                .chain(s.attrs.keys().map(|(t, _)| t))
                .cloned()
                .collect();
        }
        self.declare_settings(i, &mut s);
        Ok(s)
    }

    /// The settings link `i`'s handshake declares, in its schema `s`
    /// under each name the program gives it; so its connection (R-193),
    /// which its schema flags.
    fn declare_settings(&self, i: usize, s: &mut Schema) {
        let declared = &self.links[i].borrow().settings;
        if declared.is_empty() {
            return;
        }
        let connection: BTreeSet<String> = s.connection.values().flatten().cloned().collect();
        let names = self
            .blocks
            .iter()
            .filter(|&(_, &j)| j == i)
            .map(|(b, _)| b.clone())
            .chain([self.names[i].clone()]);
        for n in names {
            s.settings
                .entry(n.clone())
                .or_default()
                .extend(declared.iter().cloned());
            s.connection
                .entry(n)
                .or_default()
                .extend(connection.iter().cloned());
        }
    }

    /// Each link sends the provider its own names of the attributes the
    /// program names otherwise (`Schema::wire`): a type is one link's, so
    /// each is given them all.
    fn wire_names(&self, schema: &Schema) {
        let names = wire::Names::new(&schema.wire);
        for l in &self.links {
            l.borrow_mut().names(names.clone());
        }
    }

    pub(super) fn loaded(&self) -> &Loaded {
        self.loaded
            .get()
            .expect("internal: Providers::load_schema before the schema is read")
    }

    /// The providers' schema as the run knows it now: as loaded, and what
    /// a provider configured since serves ([`Providers::learn`]).
    pub fn schema(&self) -> &Schema {
        &self.loaded().schema.latest().schema
    }

    /// The catalog facts of the types learned since the schema was loaded
    /// ([`Providers::learn`]), for each evaluation after.
    pub fn learned(&self) -> Vec<Atom> {
        self.loaded().learned.borrow().clone()
    }

    /// The kinds the provider `i`, configured now from the program's
    /// settings, serves that no schema declared when the run loaded it (a
    /// cluster's CRDs, which the provider cached at that Configure,
    /// R-110): of the types the run names (every one, with no scope),
    /// asked for and added to dform's schema for the rest of the run, so a
    /// later tick plans them typed, as the next run would.
    pub(super) fn learn(&self, i: usize) -> Result<()> {
        let l = self.loaded();
        let known = self.schema();
        let missing = l.scope.as_ref().map(|scope| {
            scope
                .iter()
                .filter(|t| !known.knows_type(t))
                .filter(|t| {
                    t.split_once('.')
                        .is_some_and(|(ns, _)| self.link_for(ns) == Some(i))
                })
                .cloned()
                .collect::<Vec<_>>()
        });
        if missing.as_ref().is_some_and(Vec::is_empty) {
            return Ok(());
        }
        let req = pb::SchemaRequest {
            types: missing.clone().map(|names| pb::TypeFilter { names }),
            ..Default::default()
        };
        let resp: pb::SchemaResponse = self.call(i, req)?;
        let new = |t: &str| {
            !known.knows_type(t) && missing.as_ref().is_none_or(|m| m.iter().any(|x| x == t))
        };
        let facts = resp
            .facts
            .iter()
            .map(Atom::try_from)
            .collect::<Result<Vec<Atom>>>()?
            .into_iter()
            .filter(|f| match f.args.first() {
                Some(Term::Val(Value::Str(t))) => new(t),
                _ => false,
            })
            .collect::<Vec<_>>();
        if facts.is_empty() {
            return Ok(());
        }
        let s = Schema::from_facts(&facts)
            .and_then(Schema::imported)
            .with_context(|| format!("the schema of provider {}", self.names[i]))?;
        let merged = known.clone().merge(s)?;
        self.wire_names(&merged);
        if l.schema
            .latest()
            .next
            .set(Box::new(Generation::new(merged)))
            .is_err()
        {
            bail!("internal: the providers' schema extended twice at once");
        }
        l.learned
            .borrow_mut()
            .extend(facts.into_iter().filter(|f| f.pred != "type_doc"));
        Ok(())
    }

    /// The providers' schema facts (`type_attr`, `type_list_key`,
    /// `type_provider`, `type_mint`, ...), injected into the program as EDB:
    /// those of the `named` types ([`Schema::facts_for`]), or all of them;
    /// never `type_doc`, the descriptions the language server shows (a
    /// derived Kubernetes schema's are as many rows as its attributes, and
    /// long). A schema loaded for fewer types than asked for is an error.
    pub fn catalog(&self, named: Option<&BTreeSet<String>>) -> Result<Vec<Atom>> {
        let l = self.loaded();
        if let Some(scope) = &l.scope
            && named.is_none_or(|n| !n.is_subset(scope))
        {
            bail!("internal: the schema was loaded for fewer types than the run names");
        }
        let schema = self.schema();
        let mut facts = match named {
            Some(named) => schema.facts_for(named),
            None => schema.facts.clone(),
        };
        facts.retain(|f| f.pred != "type_doc");
        Ok(facts)
    }
}
