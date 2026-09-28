//! Provider schemas as facts.
//!
//! A provider's schema is a file of plain Datalog facts,
//! `providers/<name>/schema.df`:
//!
//!   type_attr(T, Path, Ty, Flags).    % Flags drawn from required, computed,
//!                                     % id, sensitive, nullable, optional_computed
//!   type_list_key(T, Path, Keys).     % merge keys of a list attribute
//!   type_provider(T, P).              % which provider owns T
//!   type_mint(T, Path, Value).        % optional: what the mock mints for a
//!                                     % computed value; in a string,
//!                                     % {type} {name} {attr} {hash} {n},
//!                                     % {doc:PATH} (the program's value)
//!
//! The facts are injected into the program as EDB (so `dform query type_attr`
//! lists them) and folded into [`Schema`], the in-memory view the partition
//! pass, the evaluator's prelude and the fake provider read.
//!
//! Proposal E §2.2: the null class of `ref(T, A, Attr)` comes from the schema.
//! `computed` + `id` is fresh, `computed` + `sensitive` is secret, `computed`
//! alone is open. `optional_computed` (Terraform's Optional+Computed) is kept
//! apart in [`Schema::optional_computed`]: the user may set it, and when they
//! do not the provider picks a value at Apply.

use crate::ast::{Atom, Stmt, Term};
use crate::value::{NullClass, Value};
use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

pub const FLAGS: [&str; 6] = [
    "required",
    "computed",
    "id",
    "sensitive",
    "nullable",
    "optional_computed",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrSpec {
    pub ty: String,
    pub flags: BTreeSet<String>,
}

impl AttrSpec {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }
}

#[derive(Debug, Clone, Default)]
pub struct Schema {
    /// (type, attr) -> null class, for every computed attribute.
    pub computed: BTreeMap<(String, String), NullClass>,
    /// type -> provider name. Used for "provider config carries a null"
    /// phase assignment.
    pub provider_of: BTreeMap<String, String>,
    /// (type, attr) -> null class, for every Optional+Computed attribute.
    pub optional_computed: BTreeMap<(String, String), NullClass>,
    /// (type, attr) -> type and flags, for every declared attribute.
    pub attrs: BTreeMap<(String, String), AttrSpec>,
    /// (type, list attr) -> merge keys.
    pub list_keys: BTreeMap<(String, String), Vec<String>>,
    /// (type, attr) -> what the fake provider mints (a string is a template).
    pub mints: BTreeMap<(String, String), Value>,
    /// The facts the schema was built from, to inject as EDB.
    pub facts: Vec<Atom>,
}

impl Schema {
    pub fn class_of(&self, typ: &str, attr: &str) -> Option<NullClass> {
        self.computed
            .get(&(typ.to_string(), attr.to_string()))
            .copied()
    }
    pub fn computed_of(&self, typ: &str) -> Vec<(String, NullClass)> {
        self.computed
            .iter()
            .filter(|((t, _), _)| t == typ)
            .map(|((_, a), c)| (a.clone(), *c))
            .collect()
    }
    /// Types with a computed attribute.
    pub fn types(&self) -> Vec<String> {
        let mut v: Vec<String> = self.computed.keys().map(|(t, _)| t.clone()).collect();
        v.sort();
        v.dedup();
        v
    }
    /// The class an Optional+Computed attribute's provider-picked value has.
    pub fn optional_computed_class(&self, typ: &str, attr: &str) -> Option<NullClass> {
        self.optional_computed
            .get(&(typ.to_string(), attr.to_string()))
            .copied()
    }
    pub fn optional_computed_of(&self, typ: &str) -> Vec<(String, NullClass)> {
        self.optional_computed
            .iter()
            .filter(|((t, _), _)| t == typ)
            .map(|((_, a), c)| (a.clone(), *c))
            .collect()
    }
    pub fn attr(&self, typ: &str, attr: &str) -> Option<&AttrSpec> {
        self.attrs.get(&(typ.to_string(), attr.to_string()))
    }
    pub fn list_key(&self, typ: &str, attr: &str) -> Option<&[String]> {
        self.list_keys
            .get(&(typ.to_string(), attr.to_string()))
            .map(|v| v.as_slice())
    }
    /// Whether `path` lies inside a list or set element (an ancestor path is
    /// declared `list` or `set`). Such paths describe every element; the mock
    /// neither mints nor requires them at the resource's top level.
    pub fn in_list(&self, typ: &str, path: &str) -> bool {
        std::iter::successors(path.rsplit_once('.').map(|x| x.0), |p| {
            p.rsplit_once('.').map(|x| x.0)
        })
        .any(|p| {
            self.attr(typ, p)
                .is_some_and(|a| a.ty == "list" || a.ty == "set")
        })
    }

    pub fn knows_type(&self, typ: &str) -> bool {
        self.provider_of.contains_key(typ) || self.attrs.keys().any(|(t, _)| t == typ)
    }
    /// Whether a value at `path` (normalized: dotted, no indices) is
    /// sensitive: the path or an ancestor is declared `sensitive`.
    pub fn is_sensitive(&self, typ: &str, path: &str) -> bool {
        let mut p = path;
        loop {
            if self.attr(typ, p).is_some_and(|a| a.has("sensitive")) {
                return true;
            }
            match p.rsplit_once('.') {
                Some((parent, _)) => p = parent,
                None => return false,
            }
        }
    }

    /// Build a schema from `type_attr`, `type_list_key`, `type_provider` and
    /// `type_mint` facts. Other predicates are carried into `facts` untouched.
    pub fn from_facts(facts: &[Atom]) -> Result<Schema> {
        let mut s = Schema::default();
        for f in facts {
            let args: Vec<Value> = f.args.iter().map(ground).collect::<Result<_>>()?;
            let bad = || {
                anyhow!(
                    "schema fact {}/{}: wrong arity or argument kinds",
                    f.pred,
                    args.len()
                )
            };
            match f.pred.as_str() {
                "type_attr" => {
                    let [Value::Str(t), Value::Str(p), ty, Value::List(flags)] = args.as_slice()
                    else {
                        return Err(bad());
                    };
                    let ty = match ty {
                        Value::Str(s) => s.clone(),
                        _ => return Err(bad()),
                    };
                    let mut fs = BTreeSet::new();
                    for fl in flags {
                        let Value::Str(fl) = fl else {
                            bail!("type_attr({t}, {p}): flags must be symbols");
                        };
                        if !FLAGS.contains(&fl.as_str()) {
                            bail!(
                                "type_attr({t}, {p}): unknown flag '{fl}' (expected one of {})",
                                FLAGS.join(", ")
                            );
                        }
                        fs.insert(fl.clone());
                    }
                    if fs.contains("computed") && fs.contains("optional_computed") {
                        bail!("type_attr({t}, {p}): computed and optional_computed are exclusive");
                    }
                    let class = if fs.contains("sensitive") {
                        NullClass::Secret
                    } else if fs.contains("id") {
                        NullClass::Fresh
                    } else {
                        NullClass::Open
                    };
                    let key = (t.clone(), p.clone());
                    if fs.contains("computed") {
                        s.computed.insert(key.clone(), class);
                    }
                    if fs.contains("optional_computed") {
                        s.optional_computed.insert(key.clone(), class);
                    }
                    if s.attrs.insert(key, AttrSpec { ty, flags: fs }).is_some() {
                        bail!("type_attr({t}, {p}) declared twice");
                    }
                }
                "type_list_key" => {
                    let [Value::Str(t), Value::Str(p), Value::List(keys)] = args.as_slice() else {
                        return Err(bad());
                    };
                    let keys = keys
                        .iter()
                        .map(|k| k.as_str().map(str::to_string).ok_or_else(bad))
                        .collect::<Result<Vec<_>>>()?;
                    s.list_keys.insert((t.clone(), p.clone()), keys);
                }
                "type_provider" => {
                    let [Value::Str(t), Value::Str(p)] = args.as_slice() else {
                        return Err(bad());
                    };
                    if let Some(prev) = s.provider_of.insert(t.clone(), p.clone())
                        && &prev != p
                    {
                        bail!("type_provider({t}): claimed by both {prev} and {p}");
                    }
                }
                "type_mint" => {
                    let [Value::Str(t), Value::Str(p), v] = args.as_slice() else {
                        return Err(bad());
                    };
                    s.mints.insert((t.clone(), p.clone()), v.clone());
                }
                _ => {}
            }
            s.facts.push(Atom {
                pred: f.pred.clone(),
                args: args.into_iter().map(Term::Val).collect(),
                record: None,
            });
        }
        for (t, p) in s.list_keys.keys() {
            if let Some(a) = s.attr(t, p)
                && a.ty != "list"
            {
                bail!(
                    "type_list_key({t}, {p}): attribute has type {}, not list",
                    a.ty
                );
            }
        }
        Ok(s)
    }

    /// Parse a schema file's source: plain facts only.
    pub fn parse(src: &str, origin: &str) -> Result<Schema> {
        let prog = crate::parser::parse_program(src).with_context(|| format!("parse {origin}"))?;
        let mut facts = Vec::new();
        for st in prog.statements {
            match st {
                Stmt::Fact(a) => facts.push(a),
                other => bail!("{origin}: a schema file holds facts only, found {other:?}"),
            }
        }
        Schema::from_facts(&facts).with_context(|| format!("schema {origin}"))
    }

    pub fn load(path: &Path) -> Result<Schema> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        Schema::parse(&src, &path.display().to_string())
    }

    /// Union of two schemas. A type may be claimed by one provider only.
    pub fn merge(mut self, other: Schema) -> Result<Schema> {
        for (t, p) in other.provider_of {
            if let Some(prev) = self.provider_of.get(&t)
                && prev != &p
            {
                bail!("type {t} is claimed by both providers {prev} and {p}");
            }
            self.provider_of.insert(t, p);
        }
        for (k, v) in other.attrs {
            if self.attrs.contains_key(&k) {
                bail!("type_attr({}, {}) declared by two schemas", k.0, k.1);
            }
            self.attrs.insert(k, v);
        }
        self.computed.extend(other.computed);
        self.optional_computed.extend(other.optional_computed);
        self.list_keys.extend(other.list_keys);
        self.mints.extend(other.mints);
        self.facts.extend(other.facts);
        Ok(self)
    }
}

fn ground(t: &Term) -> Result<Value> {
    match t {
        Term::Val(v) => Ok(v.clone()),
        Term::List(xs) => Ok(Value::List(xs.iter().map(ground).collect::<Result<_>>()?)),
        Term::Obj(m) => Ok(Value::Obj(
            m.iter()
                .map(|(k, x)| Ok((k.clone(), ground(x)?)))
                .collect::<Result<_>>()?,
        )),
        other => bail!("schema facts must be ground, found {other:?}"),
    }
}

/// Schemas shipped with the binary, by provider name. `providers/<name>/schema.df`
/// in the working directory takes precedence (see [`load_provider`]).
pub fn builtin(name: &str) -> Option<&'static str> {
    Some(match name {
        "fake" => include_str!("../providers/fake/schema.df"),
        "gke" => include_str!("../providers/gke/schema.df"),
        "k8s" => include_str!("../providers/k8s/schema.df"),
        "aws-mock" => include_str!("../providers/aws-mock/schema.df"),
        _ => return None,
    })
}

/// Resolve `--provider NAME`: a path to a `.df` file; else
/// `providers/NAME/schema.df` under the working directory; else a built-in.
pub fn load_provider(name: &str) -> Result<Schema> {
    if name.ends_with(".df") || name.contains('/') {
        return Schema::load(Path::new(name));
    }
    let local = Path::new("providers").join(name).join("schema.df");
    if local.exists() {
        return Schema::load(&local);
    }
    let src = builtin(name).ok_or_else(|| {
        anyhow!(
            "unknown provider '{name}': no {} and no built-in schema by that name",
            local.display()
        )
    })?;
    Schema::parse(src, &format!("providers/{name}/schema.df"))
}

fn builtin_schema(name: &str) -> Schema {
    Schema::parse(builtin(name).unwrap(), name).expect("built-in schema parses")
}

/// The fake provider behind dform.df / dform-advanced.df / the examples:
/// `providers/fake/schema.df`.
pub fn fake() -> Schema {
    builtin_schema("fake")
}

/// C's GKE example as E §7.4 spells it: `providers/gke/schema.df`.
pub fn gke() -> Schema {
    builtin_schema("gke")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_schema_classes_match_the_hand_written_one() {
        let s = fake();
        assert_eq!(s.class_of("net.vpc", "id"), Some(NullClass::Fresh));
        assert_eq!(s.class_of("db.postgres", "endpoint"), Some(NullClass::Open));
        assert_eq!(s.class_of("k8s.cluster", "ca_cert"), Some(NullClass::Open));
        assert_eq!(s.types().len(), 11);
        assert_eq!(
            s.provider_of.get("net.route").map(String::as_str),
            Some("fakecloud")
        );
        assert_eq!(s.computed.len(), 14);
    }

    #[test]
    fn gke_schema_classes_match_the_hand_written_one() {
        let s = gke();
        assert_eq!(
            s.class_of("google.client_config", "access_token"),
            Some(NullClass::Secret)
        );
        assert_eq!(s.class_of("k8s.namespace", "uid"), Some(NullClass::Fresh));
        assert_eq!(s.class_of("gke_cluster", "zones"), Some(NullClass::Open));
        assert_eq!(
            s.provider_of.get("k8s.secret").map(String::as_str),
            Some("kubernetes")
        );
        assert_eq!(s.provider_of.get("google.client_config"), None);
        assert_eq!(s.computed.len(), 17);
    }

    #[test]
    fn flags_decide_the_class_and_optional_computed_is_separate() {
        let s = Schema::parse(
            r#"
            type_attr(t, id, string, [computed, id]).
            type_attr(t, endpoint, string, [computed]).
            type_attr(t, password, string, [computed, sensitive]).
            type_attr(t, zone, string, [optional_computed]).
            type_attr(t, data, map, [sensitive]).
            type_list_key(t, ports, [name, protocol]).
            type_attr(t, ports, list, []).
            "#,
            "test",
        )
        .unwrap();
        assert_eq!(s.class_of("t", "id"), Some(NullClass::Fresh));
        assert_eq!(s.class_of("t", "endpoint"), Some(NullClass::Open));
        assert_eq!(s.class_of("t", "password"), Some(NullClass::Secret));
        assert_eq!(s.class_of("t", "zone"), None);
        assert_eq!(
            s.optional_computed_class("t", "zone"),
            Some(NullClass::Open)
        );
        assert!(s.is_sensitive("t", "data.key"));
        assert!(!s.is_sensitive("t", "endpoint"));
        assert_eq!(
            s.list_key("t", "ports"),
            Some(&["name".to_string(), "protocol".to_string()][..])
        );
    }

    #[test]
    fn unknown_flags_and_rules_are_rejected() {
        let e = Schema::parse("type_attr(t, id, string, [computd]).", "test").unwrap_err();
        assert!(format!("{e:#}").contains("unknown flag 'computd'"), "{e:#}");
        assert!(Schema::parse("type_attr(t, X, string, []) :- foo(X).", "test").is_err());
    }
}
