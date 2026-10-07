//! Provider schemas as facts.
//!
//! A provider's schema is a file of plain Datalog facts: the mock's
//! built-in ones are `crates/dform-mock/schemas/<name>.df`, a project's own
//! `providers/<name>/schema.df`:
//!
//!   type_attr(T, Path, Ty, Flags).    % Ty a type's text: string, int, bool,
//!                                     % inet, ref(T), list(..), map, object,
//!                                     % enum(..), or a quantity or time with
//!                                     % how the provider takes it (R-66):
//!                                     % bytes(quantity|gib|mib|bytes),
//!                                     % cpu(quantity|millicores),
//!                                     % duration(friendly|iso|seconds),
//!                                     % time(rfc3339); `Render`
//!                                     % Flags drawn from required, computed,
//!                                     % id, sensitive, nullable, optional_computed,
//!                                     % force_new (a change replaces the object),
//!                                     % name_like (the value names the object in
//!                                     % the cloud; `name`, `metadata.name` and
//!                                     % `bucket` always do)
//!   type_list_key(T, Path, Keys).     % merge keys of a list attribute
//!   type_default(T, Path, V).         % optional: the value the server gives
//!                                     % a merge key an element leaves out
//!                                     % (`spec.ports.protocol`, "TCP"); the
//!                                     % element has it before it is keyed
//!   type_provider(T, P).              % which provider owns T
//!   type_retry(T, Attempts).          % optional: how many times Read is tried
//!                                     % before an object is taken as gone
//!                                     % (default 3; eventual consistency)
//!   type_mint(T, Path, Value).        % optional: what the mock mints for a
//!                                     % computed value; in a string,
//!                                     % {type} {name} {attr} {hash} {n},
//!                                     % {doc:PATH} (the program's value)
//!   type_replace(T, Order).           % optional: how a replacement of T is
//!                                     % ordered: create_first, destroy_first,
//!                                     % or either (default; destroy first
//!                                     % unless lifecycle create_before_destroy)
//!   type_refine(T, Path, C).          % optional: a checkable refinement of
//!                                     % Path (`crate::refine`): range(Lo, Hi),
//!                                     % prefix_len_le(N), prefix_len_ge(N),
//!                                     % enum([..]), regex(S), len_le(N),
//!                                     % len_ge(N); carried as its text
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

pub const FLAGS: [&str; 9] = [
    "required",
    "computed",
    "id",
    "sensitive",
    "nullable",
    "optional_computed",
    "force_new",
    "name_like",
    "write_only",
];

/// The flag of an attribute the API takes and never answers (R-106): an
/// instance's user data. State keeps the digest of what was last applied
/// beside the resource (`state::StateEntry::written`), and Plan compares
/// the program's value with it.
pub const WRITE_ONLY: &str = "write_only";

/// The identity attribute (R-43): the computed path a reference to an
/// object resolves to at Apply, the id the provider's API takes where an
/// attribute typed `ref(T)` points at a `T`. Programs never read it: a
/// reference is the resource (`vpc = main`), and a null of this path
/// prints as the resource, `?T["A"]`.
pub const IDENTITY: &str = "id";

/// The paths that name an object in the cloud for every type: two
/// deployments that write the same value there collide. A schema adds a
/// type's own with the flag `name_like`.
pub const NAME_LIKE: [&str; 3] = ["name", "metadata.name", "bucket"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttrSpec {
    pub ty: String,
    pub flags: BTreeSet<String>,
}

impl AttrSpec {
    pub fn has(&self, flag: &str) -> bool {
        self.flags.contains(flag)
    }

    /// The type's kind, its name without arguments: `list` for
    /// `list(ref(net.subnet))`, `ref` for `ref(net.vpc)`.
    pub fn kind(&self) -> &str {
        self.ty.split('(').next().unwrap_or(&self.ty).trim()
    }

    /// How the provider takes a quantity or time attribute (R-66); `None`
    /// for any other type.
    pub fn render(&self) -> Option<Render> {
        let arg = self
            .ty
            .split_once('(')
            .and_then(|(_, r)| r.strip_suffix(')'))
            .map(|a| a.trim().trim_matches('"'));
        Render::parse(self.kind(), arg)
    }
}

/// How a provider takes a quantity or a time (R-66): the schema says, per
/// attribute, so `storage = 20Gi` is one spelling for every provider.
/// The program's value is the same whatever the form; only what the
/// provider is sent (and what the plan shows of it) differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Render {
    /// The canonical text, Kubernetes's quantity string for bytes and cpu
    /// (`1536Mi`, `500m`), the friendly form for a duration (`1h30m`).
    Text(crate::quantity::Dim),
    /// Bytes as a whole number of GiB, MiB or bytes: `bytes(gib)`.
    Bytes(&'static str),
    /// A cpu as a whole number of millicores: `cpu(millicores)`.
    Millicores,
    /// A duration in ISO 8601 (`PT1H30M`): `duration(iso)`.
    Iso,
    /// A duration as whole seconds: `duration(seconds)`.
    Seconds,
    /// A time in RFC 3339 with its offset (`2026-10-02T09:00:00+02:00`).
    Rfc3339,
}

impl Render {
    /// The form `kind(arg)` names, `arg` defaulted per kind.
    pub fn parse(kind: &str, arg: Option<&str>) -> Option<Render> {
        use crate::quantity::Dim;
        Some(match (kind, arg) {
            ("bytes", None | Some("quantity")) => Render::Text(Dim::Bytes),
            ("bytes", Some("gib")) => Render::Bytes("Gi"),
            ("bytes", Some("mib")) => Render::Bytes("Mi"),
            ("bytes", Some("bytes")) => Render::Bytes(""),
            ("cpu", None | Some("quantity")) => Render::Text(Dim::Cpu),
            ("cpu", Some("millicores")) => Render::Millicores,
            ("duration", None | Some("friendly")) => Render::Text(Dim::Duration),
            ("duration", Some("iso")) => Render::Iso,
            ("duration", Some("seconds")) => Render::Seconds,
            ("time", None | Some("rfc3339")) => Render::Rfc3339,
            _ => return None,
        })
    }

    /// What a provider holds at the attribute, read as the program's value
    /// (parsed at the edge): `20` in a `bytes(gib)` is `20Gi`, `"512Mi"` in
    /// a `bytes(quantity)` is `512Mi`. A value the form does not hold is
    /// left as it is.
    pub fn read_back(self, v: Value) -> Value {
        use crate::quantity::{self as q, Dim, Quantity};
        let read = match (self, &v) {
            (Render::Text(d), Value::Str(s)) => q::read(d, s).ok().map(Value::Quantity),
            (Render::Bytes(u), Value::Int(n)) => q::read(Dim::Bytes, &format!("{n}{u}"))
                .ok()
                .map(Value::Quantity),
            (Render::Millicores, Value::Int(n)) => Some(Value::Quantity(Quantity::Cpu(*n))),
            (Render::Seconds, Value::Int(n)) => q::read(Dim::Duration, &format!("{n}s"))
                .ok()
                .map(Value::Quantity),
            (Render::Iso, Value::Str(s)) => q::read(Dim::Duration, s).ok().map(Value::Quantity),
            (Render::Rfc3339, Value::Str(s)) => crate::time::Time::parse(s).ok().map(Value::Time),
            _ => None,
        };
        read.unwrap_or(v)
    }

    /// `v` as the provider takes it, or why it cannot be: a value of
    /// another type, or not a whole number of the unit.
    pub fn apply(self, v: &Value) -> std::result::Result<Value, String> {
        use crate::quantity::{self as q, Dim, Quantity};
        let dim = match self {
            Render::Text(d) => Some(d),
            Render::Bytes(_) => Some(Dim::Bytes),
            Render::Millicores => Some(Dim::Cpu),
            Render::Iso | Render::Seconds => Some(Dim::Duration),
            Render::Rfc3339 => None,
        };
        // A value that is not yet typed (a string or an int a variable
        // carried here) is read as the attribute's type first.
        let v = match (dim, v) {
            (_, Value::Null { .. }) => return Ok(v.clone()),
            (Some(d), Value::Str(s)) => Value::Quantity(q::read(d, s)?),
            (Some(Dim::Bytes), Value::Int(n)) => Value::Quantity(Quantity::Bytes(*n)),
            (Some(Dim::Cpu), Value::Int(n)) => {
                Value::Quantity(Quantity::Cpu(n.checked_mul(1000).ok_or("out of range")?))
            }
            // A decimal is cores (R-75, R-66).
            (Some(Dim::Cpu), Value::Float(f)) => {
                Value::Quantity(q::read(Dim::Cpu, &f.to_string())?)
            }
            (None, Value::Str(s)) => Value::Time(crate::time::Time::parse(s)?),
            (_, v) => v.clone(),
        };
        let wrong = |v: &Value| {
            format!(
                "is {}, not {}",
                dim.map_or("time", Dim::name),
                crate::partition::fmt_value(v)
            )
        };
        let whole = |n: Option<i64>, unit: &str, v: &Value| {
            n.map(Value::Int).ok_or_else(|| {
                format!(
                    "is sent to the provider in whole {unit}, and {} is not",
                    crate::partition::fmt_value(v)
                )
            })
        };
        match (self, &v) {
            (Render::Text(d), Value::Quantity(x)) if x.dim() == d => Ok(Value::Str(x.to_string())),
            (Render::Bytes(u), Value::Quantity(x @ Quantity::Bytes(_))) => whole(
                q::to_unit(x, u),
                match u {
                    "Gi" => "GiB",
                    "Mi" => "MiB",
                    _ => "bytes",
                },
                &v,
            ),
            (Render::Millicores, Value::Quantity(x @ Quantity::Cpu(_))) => {
                whole(q::to_unit(x, "m"), "millicores", &v)
            }
            (Render::Seconds, Value::Quantity(x @ Quantity::Duration(_))) => {
                whole(q::to_unit(x, "s"), "seconds", &v)
            }
            (Render::Iso, Value::Quantity(Quantity::Duration(s))) => s
                .to_jiff()
                .map(|j| Value::Str(j.to_string()))
                .ok_or_else(|| format!("{s} is out of range")),
            (Render::Rfc3339, Value::Time(t)) => t
                .zoned()
                .map(|z| Value::Str(z.timestamp().display_with_offset(z.offset()).to_string()))
                .ok_or_else(|| format!("{t} is out of range")),
            (_, v) => Err(wrong(v)),
        }
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
    /// type -> Read attempts before an object is taken as gone.
    pub retries: BTreeMap<String, u32>,
    /// type -> how a replacement is ordered (`type_replace`).
    pub replace: BTreeMap<String, ReplaceOrder>,
    /// The facts the schema was built from, to inject as EDB.
    pub facts: Vec<Atom>,
    /// Types whose provider's Schema declares `checks_refinements`: a
    /// refinement on a sensitive path of one is an Apply assertion (F
    /// DR-13 revised); of any other type, E0306.
    pub checks_refinements: BTreeSet<String>,
    /// The provider's data sources (R-106): `extern_decl(Pred, Signature)`,
    /// the signature as an `extern` line writes it (`"+region, -name, -id,
    /// -distribution"`, `"-vcpus: int"`), so a program reads one with no
    /// `extern` line of its own.
    pub externs: BTreeMap<String, crate::ast::ExternFn>,
}

/// A schema's data source declaration (R-106): `extern_decl(Pred, Sig)`.
pub const EXTERN_DECL: &str = "extern_decl";

/// `extern_decl(pred, sig)`'s declaration: each column `+name` or
/// `-name`, then `: type` if it is typed.
fn extern_decl(pred: &str, sig: &str) -> Result<crate::ast::ExternFn> {
    let args = sig
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            let (input, rest) = match c.split_at(1) {
                ("+", r) => (true, r),
                ("-", r) => (false, r),
                _ => bail!("extern_decl({pred}): column `{c}` is `+name` or `-name`"),
            };
            let (name, ty) = match rest.split_once(':') {
                Some((n, t)) => (n.trim(), Some(crate::externs::type_expr(t.trim()))),
                None => (rest.trim(), None),
            };
            Ok(crate::ast::BindArg {
                input,
                name: name.to_string(),
                ty,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if args.is_empty() {
        bail!("extern_decl({pred}): no columns");
    }
    Ok(crate::ast::ExternFn {
        name: pred.to_string(),
        args,
        span: Default::default(),
    })
}

/// Read attempts for a type without `type_retry`.
pub const DEFAULT_READ_ATTEMPTS: u32 = 3;

/// `type_replace(T, Order)`: whether the provider can hold the old and the
/// new object of a type at once. `CreateFirst`: the replacement is created
/// before the old object is deleted (a Deployment rolls). `DestroyFirst`:
/// the old object must go first (a name that must be unique, a Namespace).
/// `Either`: destroy first, unless `lifecycle(r, create_before_destroy)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceOrder {
    CreateFirst,
    DestroyFirst,
    Either,
}

impl ReplaceOrder {
    pub const NAMES: [&str; 3] = ["create_first", "destroy_first", "either"];

    pub fn parse(s: &str) -> Option<ReplaceOrder> {
        match s {
            "create_first" => Some(ReplaceOrder::CreateFirst),
            "destroy_first" => Some(ReplaceOrder::DestroyFirst),
            "either" => Some(ReplaceOrder::Either),
            _ => None,
        }
    }
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

    /// A `typ`'s attributes as its provider takes them (R-66): every
    /// quantity or time at a path the schema gives a render form, at its
    /// own path or nested in an object or list, in that form; any other
    /// quantity or time as its canonical text. `Err((path, why))` at the
    /// first value its form cannot hold.
    pub fn render(&self, typ: &str, attrs: &Value) -> std::result::Result<Value, (String, String)> {
        self.render_at(typ, "", attrs)
    }

    fn render_at(
        &self,
        typ: &str,
        path: &str,
        v: &Value,
    ) -> std::result::Result<Value, (String, String)> {
        if let Some(r) = self.attr(typ, path).and_then(AttrSpec::render) {
            return r.apply(v).map_err(|why| (path.to_string(), why));
        }
        let join = |k: &str| crate::ir::path_join(path, k);
        Ok(match v {
            Value::Obj(m) => Value::Obj(
                m.iter()
                    .map(|(k, x)| {
                        Ok::<_, (String, String)>((k.clone(), self.render_at(typ, &join(k), x)?))
                    })
                    .collect::<std::result::Result<_, _>>()?,
            ),
            Value::List(xs) => Value::List(
                xs.iter()
                    .map(|x| self.render_at(typ, path, x))
                    .collect::<std::result::Result<_, _>>()?,
            ),
            Value::Quantity(_) | Value::Time(_) => Value::Str(v.typed_text().unwrap_or_default()),
            v => v.clone(),
        })
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
                .is_some_and(|a| a.kind() == "list" || a.kind() == "set")
        })
    }

    /// Whether `{}` at `path` (normalized) is a value, "present and
    /// empty", rather than the join's identity (absent, E DR-6): the path
    /// is declared `object` (a structure, not a `map`) and none of its own
    /// fields is `required` (a field's own required fields bind only where
    /// the field is set), so the empty object is a complete one
    /// (Kubernetes' `podSelector: {}`, every pod).
    pub fn empty_is_present(&self, typ: &str, path: &str) -> bool {
        if self.attr(typ, path).is_none_or(|a| a.ty != "object") {
            return false;
        }
        let under = format!("{path}.");
        !self
            .attrs
            .range((typ.to_string(), under.clone())..)
            .take_while(|((t, p), _)| t == typ && p.starts_with(&under))
            .any(|((_, p), a)| !p[under.len()..].contains('.') && a.has("required"))
    }

    /// Whether changing the value at `path` (normalized: dotted, no
    /// indices) replaces the object: the path or an ancestor is declared
    /// `force_new`.
    /// The write-only attributes of `typ` ([`WRITE_ONLY`]), by path.
    pub fn write_only_of(&self, typ: &str) -> Vec<&str> {
        self.attrs
            .range((typ.to_string(), String::new())..)
            .take_while(|((t, _), _)| t == typ)
            .filter(|(_, a)| a.has(WRITE_ONLY))
            .map(|((_, p), _)| p.as_str())
            .collect()
    }

    pub fn forces_new(&self, typ: &str, path: &str) -> bool {
        std::iter::successors(Some(path), |p| p.rsplit_once('.').map(|x| x.0))
            .any(|p| self.attr(typ, p).is_some_and(|a| a.has("force_new")))
    }

    /// How a replacement of `typ` is ordered (`type_replace`, default
    /// `Either`).
    pub fn replace_order(&self, typ: &str) -> ReplaceOrder {
        self.replace
            .get(typ)
            .copied()
            .unwrap_or(ReplaceOrder::Either)
    }

    /// How many times Read is tried for an object of `typ` that state maps
    /// but the world does not return, before it is taken as gone.
    pub fn read_attempts(&self, typ: &str) -> u32 {
        self.retries
            .get(typ)
            .copied()
            .unwrap_or(DEFAULT_READ_ATTEMPTS)
    }

    /// The paths of `typ` that name the object in the cloud: [`NAME_LIKE`]
    /// and the type's paths declared `name_like`.
    pub fn name_like_paths(&self, typ: &str) -> BTreeSet<String> {
        NAME_LIKE
            .iter()
            .map(|p| p.to_string())
            .chain(
                self.attrs
                    .iter()
                    .filter(|((t, _), a)| t == typ && a.has("name_like"))
                    .map(|((_, p), _)| p.clone()),
            )
            .collect()
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

    /// Each `type_doc(T, Path, Text)`: a type's (path `""`) and its
    /// attributes' descriptions, by type and path.
    pub fn docs(&self) -> BTreeMap<(&str, &str), &str> {
        self.facts
            .iter()
            .filter(|f| f.pred == "type_doc")
            .filter_map(|f| match f.args.as_slice() {
                [t, p, d] => Some(((t.as_str()?, p.as_str()?), d.as_str()?)),
                _ => None,
            })
            .collect()
    }

    /// Build a schema from `type_attr`, `type_list_key`, `type_provider` and
    /// `type_mint` facts. Other predicates are carried into `facts` untouched.
    pub fn from_facts(facts: &[Atom]) -> Result<Schema> {
        let mut s = Schema::default();
        for f in facts {
            let args: Vec<Value> = if f.pred == crate::refine::TYPE_REFINE {
                refine_args(f)?
            } else {
                f.args.iter().map(ground).collect::<Result<_>>()?
            };
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
                    if fs.contains(WRITE_ONLY)
                        && (fs.contains("computed") || fs.contains("optional_computed"))
                    {
                        bail!("type_attr({t}, {p}): a write_only attribute is not computed");
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
                "type_retry" => {
                    let [Value::Str(t), Value::Int(n)] = args.as_slice() else {
                        return Err(bad());
                    };
                    if *n < 1 {
                        bail!("type_retry({t}, {n}): at least one attempt");
                    }
                    s.retries.insert(t.clone(), *n as u32);
                }
                "type_replace" => {
                    let [Value::Str(t), Value::Str(o)] = args.as_slice() else {
                        return Err(bad());
                    };
                    let Some(order) = ReplaceOrder::parse(o) else {
                        bail!(
                            "type_replace({t}, {o}): unknown order (expected one of {})",
                            ReplaceOrder::NAMES.join(", ")
                        );
                    };
                    if let Some(prev) = s.replace.insert(t.clone(), order)
                        && prev != order
                    {
                        bail!("type_replace({t}): declared both {prev:?} and {o}");
                    }
                }
                "type_mint" => {
                    let [Value::Str(t), Value::Str(p), v] = args.as_slice() else {
                        return Err(bad());
                    };
                    s.mints.insert((t.clone(), p.clone()), v.clone());
                }
                EXTERN_DECL => {
                    let [Value::Str(p), Value::Str(sig)] = args.as_slice() else {
                        return Err(bad());
                    };
                    s.externs.insert(p.clone(), extern_decl(p, sig)?);
                }
                _ => {}
            }
            s.facts.push(Atom {
                pred: f.pred.clone(),
                args: args.into_iter().map(Term::Val).collect(),
                record: None,
                span: Default::default(),
            });
        }
        for (t, p) in s.list_keys.keys() {
            if let Some(a) = s.attr(t, p)
                && a.kind() != "list"
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
        self.retries.extend(other.retries);
        for (t, o) in other.replace {
            if let Some(prev) = self.replace.insert(t.clone(), o)
                && prev != o
            {
                bail!("type_replace({t}) declared {prev:?} and {o:?} by two schemas");
            }
        }
        self.facts.extend(other.facts);
        self.checks_refinements.extend(other.checks_refinements);
        for (p, f) in other.externs {
            if self.externs.contains_key(&p) {
                bail!("extern_decl({p}) declared by two schemas");
            }
            self.externs.insert(p, f);
        }
        Ok(self)
    }

    /// The facts to inject into a run whose program and given facts name
    /// the types `named` ([`named_types`]): every `type_provider` and
    /// `type_alias` row, and the rows of the named types and of the types
    /// they alias. A type nothing names derives no `want`, so its rows (and
    /// the computed prelude expanded from them) change nothing but a read
    /// of the schema itself.
    pub fn facts_for(&self, named: &BTreeSet<String>) -> Vec<Atom> {
        let mut keep = named.clone();
        loop {
            let before = keep.len();
            for f in self.facts.iter().filter(|f| f.pred == "type_alias") {
                if let [Term::Val(Value::Str(a)), Term::Val(Value::Str(t))] = f.args.as_slice()
                    && keep.contains(a)
                {
                    keep.insert(t.clone());
                }
            }
            if keep.len() == before {
                break;
            }
        }
        self.facts
            .iter()
            .filter(|f| {
                !PER_TYPE.contains(&f.pred.as_str())
                    || matches!(f.args.first(), Some(Term::Val(Value::Str(t))) if keep.contains(t))
            })
            .cloned()
            .collect()
    }
}

/// Schema predicates with a row per type (the type in the first column).
const PER_TYPE: [&str; 8] = [
    "type_attr",
    "type_doc",
    "type_list_key",
    "type_default",
    "type_retry",
    "type_replace",
    "type_mint",
    crate::refine::TYPE_REFINE,
];

/// Whether `pred` is a schema predicate: a read of it sees the whole schema.
pub fn is_schema_pred(pred: &str) -> bool {
    PER_TYPE.contains(&pred) || matches!(pred, "type_provider" | "type_alias")
}

/// The types a lowered program and the facts given to it name: every
/// symbol they hold, and the type of every ref. `None` when a rule reads a
/// per-type schema predicate for a type it does not spell out
/// (`type_attr(T, ...)`), or wants a resource whose type is not a constant
/// in the rule's head (built at runtime, `T = str.format("k8s.%s", K)`): such
/// a program sees the whole schema.
pub fn named_types(program: &crate::ast::Program, facts: &[Atom]) -> Option<BTreeSet<String>> {
    use crate::ast::Lit;
    fn value(v: &Value, out: &mut BTreeSet<String>) {
        match v {
            Value::Str(s) => {
                out.insert(s.clone());
            }
            Value::Ref { typ, .. } | Value::CloudRef { typ, .. } => {
                out.insert(typ.clone());
            }
            Value::List(xs) => xs.iter().for_each(|x| value(x, out)),
            Value::Obj(m) => m.values().for_each(|x| value(x, out)),
            _ => {}
        }
    }
    fn term(t: &Term, out: &mut BTreeSet<String>) -> Option<()> {
        match t {
            Term::Val(v) => value(v, out),
            Term::Func { args, .. } | Term::List(args) => {
                args.iter().try_for_each(|a| term(a, out))?
            }
            Term::Obj(m) => m.values().try_for_each(|a| term(a, out))?,
            Term::ListComp { item, body } => {
                term(item, out)?;
                body.iter().try_for_each(|l| lit(l, out))?
            }
            Term::Var(_) | Term::Wildcard => {}
        }
        Some(())
    }
    fn atom(a: &Atom, out: &mut BTreeSet<String>) -> Option<()> {
        a.args.iter().try_for_each(|t| term(t, out))?;
        a.record
            .iter()
            .flat_map(|r| r.values())
            .try_for_each(|t| term(t, out))
    }
    fn lit(l: &Lit, out: &mut BTreeSet<String>) -> Option<()> {
        match l {
            Lit::Pos(a) | Lit::Not(a) => {
                if PER_TYPE.contains(&a.pred.as_str())
                    && !matches!(a.args.first(), Some(Term::Val(_)))
                {
                    return None;
                }
                atom(a, out)
            }
            Lit::Eq(x, y)
            | Lit::Neq(x, y)
            | Lit::Gt(x, y)
            | Lit::Ge(x, y)
            | Lit::Lt(x, y)
            | Lit::Le(x, y) => {
                term(x, out)?;
                term(y, out)
            }
        }
    }
    let mut out = BTreeSet::new();
    for s in &program.statements {
        match s {
            Stmt::Fact(a) => atom(a, &mut out)?,
            Stmt::Rule(r) => {
                if r.head.pred == "want" && !matches!(r.head.args.first(), Some(Term::Val(_))) {
                    return None;
                }
                atom(&r.head, &mut out)?;
                r.body.iter().try_for_each(|l| lit(l, &mut out))?
            }
            _ => {}
        }
    }
    for f in facts {
        atom(f, &mut out)?;
    }
    Some(out)
}

/// `type_refine(T, Path, C)` with its constraint checked and carried as
/// its text (a schema file writes the term, the wire carries the text).
fn refine_args(f: &Atom) -> Result<Vec<Value>> {
    let [t, p, c] = f.args.as_slice() else {
        bail!(
            "schema fact type_refine/{}: expected (Type, Path, Constraint)",
            f.args.len()
        );
    };
    let (Value::Str(t), Value::Str(p)) = (ground(t)?, ground(p)?) else {
        bail!("type_refine: type and path must be symbols");
    };
    let c = crate::refine::from_term(c).map_err(|e| anyhow!("type_refine({t}, {p}, ...): {e}"))?;
    Ok(vec![
        Value::Str(t),
        Value::Str(p),
        Value::Str(c.to_string()),
    ])
}

fn ground(t: &Term) -> Result<Value> {
    t.ground()
        .ok_or_else(|| anyhow!("schema facts must be ground, found {t:?}"))
}

/// The names of the schemas shipped with the binary ([`builtin`]).
pub const BUILTINS: [&str; 4] = ["fake", "gke", "k8s", "aws-mock"];

/// The known provider schemas that declare `typ`: the built-in ones and
/// each `providers/<name>/schema.df` under the working directory, by name.
/// What an error about an undeclared type suggests.
pub fn declaring(typ: &str) -> Vec<String> {
    known_schemas()
        .into_iter()
        .filter(|n| load_provider(n).is_ok_and(|s| s.knows_type(typ)))
        .collect()
}

/// The built-in schemas' names and each `providers/<name>/schema.df`'s.
fn known_schemas() -> BTreeSet<String> {
    let mut names: BTreeSet<String> = BUILTINS.iter().map(|n| n.to_string()).collect();
    if let Ok(entries) = std::fs::read_dir("providers") {
        names.extend(
            entries
                .flatten()
                .filter(|e| e.path().join("schema.df").is_file())
                .filter_map(|e| e.file_name().into_string().ok()),
        );
    }
    names
}

/// A schema's `type_instead(Gone, Type, Path)` (R-158): `Gone` is no
/// resource type, a relationship whose state lives on one side, and is
/// written as attribute `Path` of `Type` (`iam.role_policy_attachment`
/// is `iam.role`'s `policies`). The first known schema's that says so.
pub fn instead(typ: &str) -> Option<(String, String)> {
    known_schemas().into_iter().find_map(|n| {
        load_provider(&n)
            .ok()?
            .facts
            .iter()
            .find_map(|f| match f.args.as_slice() {
                [
                    Term::Val(Value::Str(g)),
                    Term::Val(Value::Str(t)),
                    Term::Val(Value::Str(p)),
                ] if f.pred == "type_instead" && g == typ => Some((t.clone(), p.clone())),
                _ => None,
            })
    })
}

/// Schemas shipped with the binary, by provider name
/// (`crates/dform-mock/schemas/<name>.df`). `providers/<name>/schema.df` in
/// the working directory takes precedence (see [`load_provider`]).
pub fn builtin(name: &str) -> Option<&'static str> {
    Some(match name {
        "fake" => include_str!("../../dform-mock/schemas/fake.df"),
        "gke" => include_str!("../../dform-mock/schemas/gke.df"),
        "k8s" => include_str!("../../dform-mock/schemas/k8s.df"),
        "aws-mock" => include_str!("../../dform-mock/schemas/aws-mock.df"),
        _ => return None,
    })
}

/// The extern answers a built-in schema's mock gives
/// (`crates/dform-mock/schemas/<name>.externs.df`), as
/// `providers/<name>/externs.df` does for a project's own schema.
pub fn builtin_answers(name: &str) -> Option<&'static str> {
    Some(match name {
        "aws-mock" => include_str!("../../dform-mock/schemas/aws-mock.externs.df"),
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
    Schema::parse(src, &format!("crates/dform-mock/schemas/{name}.df"))
}

fn builtin_schema(name: &str) -> Schema {
    Schema::parse(builtin(name).unwrap(), name).expect("built-in schema parses")
}

/// The fake provider behind the examples:
/// `crates/dform-mock/schemas/fake.df`.
pub fn fake() -> Schema {
    builtin_schema("fake")
}

/// C's GKE example as E §7.4 spells it: `crates/dform-mock/schemas/gke.df`.
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
        assert_eq!(s.types().len(), 10);
        assert_eq!(
            s.provider_of.get("net.route_table").map(String::as_str),
            Some("fakecloud")
        );
        assert_eq!(s.computed.len(), 13);
    }

    #[test]
    fn gke_schema_classes_match_the_hand_written_one() {
        let s = gke();
        assert_eq!(
            s.class_of("google.client_config", "access_token"),
            Some(NullClass::Secret)
        );
        assert_eq!(s.class_of("k8s.namespace", "uid"), Some(NullClass::Fresh));
        assert_eq!(
            s.class_of("google.container_cluster", "zones"),
            Some(NullClass::Open)
        );
        assert_eq!(
            s.provider_of.get("k8s.secret").map(String::as_str),
            Some("k8s")
        );
        assert_eq!(s.provider_of.get("google.client_config"), None);
        assert_eq!(s.computed.len(), 17);
    }

    /// `name`, `metadata.name` and `bucket` name every type's objects; a
    /// schema adds its own with `name_like`.
    #[test]
    fn name_like_paths_are_the_defaults_and_the_flagged() {
        let s = Schema::parse(
            r#"
            type_attr("t", "name_prefix", "string", ["optional_computed", "name_like"])
            type_attr("u", "name_prefix", "string", [])
            "#,
            "test",
        )
        .unwrap();
        let paths = |t: &str| s.name_like_paths(t).into_iter().collect::<Vec<_>>();
        assert_eq!(
            paths("t"),
            ["bucket", "metadata.name", "name", "name_prefix"]
        );
        assert_eq!(paths("u"), ["bucket", "metadata.name", "name"]);
    }

    #[test]
    fn flags_decide_the_class_and_optional_computed_is_separate() {
        let s = Schema::parse(
            r#"
            type_attr("t", "id", "string", ["computed", "id"])
            type_attr("t", "endpoint", "string", ["computed"])
            type_attr("t", "password", "string", ["computed", "sensitive"])
            type_attr("t", "zone", "string", ["optional_computed"])
            type_attr("t", "data", "map", ["sensitive"])
            type_list_key("t", "ports", ["name", "protocol"])
            type_attr("t", "ports", "list", [])
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
        let e = Schema::parse(
            "type_attr(\"t\", \"id\", \"string\", [\"computd\"])",
            "test",
        )
        .unwrap_err();
        assert!(format!("{e:#}").contains("unknown flag 'computd'"), "{e:#}");
        assert!(Schema::parse("type_attr(\"t\", x, \"string\", []) where foo(x)", "test").is_err());
    }
}
