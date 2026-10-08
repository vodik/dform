//! `dform query` and the value spelling every printer shares: a pattern or
//! a conjunction over the final fact store, one column per variable,
//! secrets redacted.
//!
//! Redaction is by value. A value at a `sensitive` schema path of `arg`,
//! `attr`, `world_attr`, `cloud_attr` or `cloud_computed`, or of an input or
//! output declared `secret(T)` (`secret_cell/3`), is a secret, and so is
//! every scalar inside it; any printed value equal to a secret, or a
//! string containing a secret string, prints as `(sensitive T["A"].p)`, or
//! in a result set's cell as `secret(SIZE)`. A `secret` null prints as its
//! label the same way. So a rule that forwards a secret into another
//! predicate does not leak it either. The result set itself is
//! `report::table`'s.

use crate::ast::{Atom, Lit, Term};
use crate::engine;
use crate::partition;
use crate::schema::Schema;
use crate::value::{NullClass, Value};
use anyhow::{Result, bail};
use std::collections::{BTreeMap, BTreeSet};

/// What `dform query ARG` asks.
#[derive(Debug, Clone)]
pub enum Query {
    /// A bare predicate name: every fact of it.
    Pred(String),
    /// A conjunction of body literals; `vars` in order of first appearance.
    Body { body: Vec<Lit>, vars: Vec<String> },
}

/// An address as `plan` prints it (H-16, `ir::parse_address`), as a
/// pattern: `T["A"].p` is its attribute, `attr(T, A, "p", value)`; `T["A"]`
/// is the resource, `want(T, A)` for `why` and every `attr(T, A, path,
/// value)` for `query`. For `why`, a resource is also its path, as the
/// plan prints it, with its type before it or not (R-112): `k3s.admin`,
/// `ovh.ssh_key k3s.admin`. `None` when `src` is not an address; an
/// address with an old scope separator, `/` or `::`, is an error.
pub fn address(src: &str, why: bool) -> Result<Option<Query>> {
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let v = |x: &str| Term::Var(x.to_string());
    let (addr, path) = match crate::ir::parse_address(src) {
        Ok(a) => a,
        Err(e) if e.is::<crate::ir::OldScope>() => return Err(e),
        Err(_) if why => {
            let (typ, at) = match src.trim().split_once(char::is_whitespace) {
                Some((t, p)) if t.split('.').all(crate::lexer::is_word) => (s(t), p),
                Some(_) => return Ok(None),
                None => (v("type"), src),
            };
            let Some(name) = crate::ir::parse_path(at)? else {
                return Ok(None);
            };
            let mut vars = Vec::new();
            term_vars(&typ, &mut vars);
            return Ok(Some(Query::Body {
                body: vec![Lit::Pos(Atom {
                    pred: "want".into(),
                    args: vec![typ, s(&name)],
                    record: None,
                    span: Default::default(),
                })],
                vars,
            }));
        }
        Err(_) => return Ok(None),
    };
    Ok(Some(pattern(&addr, path, why)))
}

/// An address as the plan prints it (R-111), `ovh.ssh_key k3s.admin`, or
/// its path alone, `k3s.admin`, with an attribute path after it
/// (`k3s.server.public_ip`), read against the resources `facts` wants:
/// the longest prefix of the path that names one is the resource, the
/// rest its attribute. Every resource it names, of any type when none is
/// given; empty when it names none.
pub fn printed(src: &str, facts: &BTreeSet<Atom>) -> Vec<(crate::ir::Address, Option<String>)> {
    let src = src.trim();
    if src.is_empty() || src.contains('[') {
        return Vec::new();
    }
    let (typ, rest) = match src.split_once(char::is_whitespace) {
        Some((t, r)) if t.split('.').all(crate::lexer::is_word) => (Some(t), r.trim()),
        Some(_) => return Vec::new(),
        None => (None, src),
    };
    // As stored (R-112): a quoted segment keeps its quotes.
    let segs = crate::ir::path_segments(rest);
    fn s(t: &Term) -> Option<&str> {
        match t {
            Term::Val(Value::Str(s)) => Some(s.as_str()),
            _ => None,
        }
    }
    let wants: Vec<(&str, &str)> = facts
        .iter()
        .filter(|a| a.pred == "want" && a.args.len() == 2)
        .filter_map(|a| Some((s(&a.args[0])?, s(&a.args[1])?)))
        .filter(|(t, _)| typ.is_none_or(|x| x == *t))
        .collect();
    for k in (1..=segs.len()).rev() {
        let name = segs[..k].join(".");
        let path = segs[k..].join(".");
        let found: Vec<_> = wants
            .iter()
            .filter(|(_, a)| *a == name)
            .map(|(t, a)| {
                let addr = crate::ir::Address {
                    typ: t.to_string(),
                    name: a.to_string(),
                };
                (addr, (!path.is_empty()).then(|| path.clone()))
            })
            .collect();
        if !found.is_empty() {
            return found;
        }
    }
    Vec::new()
}

/// A cell by its path, as `why` takes it (R-176): a `let`'s, an input's
/// or a copy's output, the program's own (`agent_init`) or a used
/// module's or a copy's (`synapse.agent_init`), its scope the path's
/// first segments; a path past a cell's name reads a field of its value
/// (`nodes.count`). `None` when `src` names no cell `facts` hold.
pub fn cell(src: &str, facts: &BTreeSet<Atom>) -> Option<Query> {
    let src = src.trim();
    if src.is_empty()
        || !src
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return None;
    }
    let segs: Vec<&str> = src.split('.').collect();
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let v = |x: &str| Term::Var(x.to_string());
    let kinds = [
        crate::modules::LET,
        crate::modules::INPUT,
        crate::transform::OUTPUT,
    ];
    let held = |kind: &str, scope: &str, name: &str| {
        facts.iter().any(|a| {
            a.pred == "attr" && a.args.len() == 4 && a.args[..3] == [s(kind), s(scope), s(name)]
        })
    };
    // The whole path a cell's first, the program's own before a scope's
    // (as `why` reads it), then the longest cell with a field path after.
    for end in (1..=segs.len()).rev() {
        for at in 0..end {
            let (scope, name, field) = (
                segs[..at].join("."),
                segs[at..end].join("."),
                segs[end..].join("."),
            );
            let Some(kind) = kinds.into_iter().find(|k| held(k, &scope, &name)) else {
                continue;
            };
            let read = |value: Term| Atom {
                pred: "attr".into(),
                args: vec![s(kind), s(&scope), s(&name), value],
                record: None,
                span: Default::default(),
            };
            let body = match field.is_empty() {
                true => vec![Lit::Pos(read(v("value")))],
                false => vec![
                    Lit::Pos(read(v("__Cell"))),
                    Lit::Eq(
                        v("value"),
                        Term::Func {
                            name: "__path".into(),
                            args: vec![v("__Cell"), s(&field)],
                        },
                    ),
                ],
            };
            return Some(Query::Body {
                body,
                vars: vec!["value".into()],
            });
        }
    }
    None
}

/// Address `addr`, or its attribute `path`, as a pattern: for `why` the
/// resource is `want(T, A)`, for `query` every `attr(T, A, path, value)`.
pub fn pattern(addr: &crate::ir::Address, path: Option<String>, why: bool) -> Query {
    let s = |x: &str| Term::Val(Value::Str(x.to_string()));
    let v = |x: &str| Term::Var(x.to_string());
    // Below the top attribute (After R-124): the attribute holds the
    // object, and the value is the field read out of it.
    if !why
        && let Some(p) = &path
        && let [top, rest @ ..] = crate::ir::path_segments(p).as_slice()
        && !rest.is_empty()
    {
        let read = Atom {
            pred: "attr".into(),
            args: vec![s(&addr.typ), s(&addr.name), s(top), v("__Top")],
            record: None,
            span: Default::default(),
        };
        let field = Term::Func {
            name: "__path".into(),
            args: vec![v("__Top"), s(&rest.join("."))],
        };
        return Query::Body {
            body: vec![Lit::Pos(read), Lit::Eq(v("value"), field)],
            vars: vec!["value".into()],
        };
    }
    let (pred, args) = match (path, why) {
        (Some(p), _) => ("attr", vec![s(&addr.typ), s(&addr.name), s(&p), v("value")]),
        (None, true) => ("want", vec![s(&addr.typ), s(&addr.name)]),
        (None, false) => (
            "attr",
            vec![s(&addr.typ), s(&addr.name), v("path"), v("value")],
        ),
    };
    let atom = Atom {
        pred: pred.into(),
        args,
        record: None,
        span: Default::default(),
    };
    let mut vars = Vec::new();
    atom.args.iter().for_each(|t| term_vars(t, &mut vars));
    Query::Body {
        body: vec![Lit::Pos(atom)],
        vars,
    }
}

/// Parse an address (`address`), `pred`, or body literals such as
/// `attr(t, a, .cidr, c), want(t, a)`, with the program's own parser.
pub fn parse(src: &str) -> Result<Query> {
    if let Some(q) = address(src, false)? {
        return Ok(q);
    }
    let src = src.trim().trim_end_matches('.').trim();
    if !src.is_empty()
        && src
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        return Ok(Query::Pred(src.to_string()));
    }
    let program = crate::parser::parse_pattern(&format!("query__(0) where {src}"))
        .map_err(|e| anyhow::anyhow!("cannot parse query '{src}': {e:#}"))?;
    let [crate::ast::Stmt::Rule(r)] = program.statements.as_slice() else {
        bail!("cannot parse query '{src}': expected literals, `p(x), q(x)`");
    };
    let mut vars = Vec::new();
    for lit in &r.body {
        lit_vars(lit, &mut vars);
    }
    Ok(Query::Body {
        body: r.body.clone(),
        vars,
    })
}

fn lit_vars(l: &Lit, out: &mut Vec<String>) {
    l.terms().for_each(|t| term_vars(t, out));
}

/// The named variables of `t`, each once, in order.
fn term_vars(t: &Term, out: &mut Vec<String>) {
    t.for_each_var(&mut |v| {
        if !v.starts_with('_') && !out.iter().any(|o| o == v) {
            out.push(v.to_string());
        }
    });
}

/// The columns of the core's relations a bare predicate may name.
const CORE_COLUMNS: &[(&str, &[&str])] = &[
    ("want", &["type", "address"]),
    ("input", &["name", "value"]),
    ("attr", &["type", "address", "path", "value"]),
    ("arg", &["type", "address", "path", "value", "rank"]),
    ("world_attr", &["type", "address", "path", "value"]),
    ("cloud_attr", &["type", "address", "path", "value"]),
    ("deformation", &["kind", "resource", "before"]),
    ("world_digest", &["resource", "now"]),
    ("type_attr", &["type", "path", "ty", "flags"]),
    ("type_provider", &["type", "provider"]),
];

/// The columns `dform query PRED` prints `pred`'s facts of `arity` under:
/// its `decl`'s fields, a core relation's, else `a`, `b`, .. as an
/// undeclared relation's columns are named (`transform::columns`).
pub fn columns(pred: &str, arity: usize, program: &crate::ast::Program) -> Vec<String> {
    let declared = program.statements.iter().find_map(|s| match s {
        crate::ast::Stmt::Decl(d) if d.pred == pred && d.fields.len() == arity => {
            Some(d.fields.clone())
        }
        _ => None,
    });
    let core = || {
        CORE_COLUMNS
            .iter()
            .find(|(p, cs)| *p == pred && cs.len() == arity)
            .map(|(_, cs)| cs.iter().map(|c| c.to_string()).collect())
    };
    declared.or_else(core).unwrap_or_else(|| {
        crate::transform::columns(arity)
            .split(", ")
            .filter(|c| !c.is_empty())
            .map(String::from)
            .collect()
    })
}

/// One row per distinct binding of the variables, sorted.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub vars: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

/// Evaluate body literals against the final fact store.
pub fn table(body: &[Lit], vars: &[String], facts: &BTreeSet<Atom>) -> Result<Table> {
    let rows: BTreeSet<Vec<Value>> = engine::query(body, facts)?
        .into_iter()
        .map(|(b, _)| {
            vars.iter()
                .map(|v| b.get(v).cloned().unwrap_or(Value::Str("_".into())))
                .collect()
        })
        .collect();
    Ok(Table {
        vars: vars.to_vec(),
        rows: rows.into_iter().collect(),
    })
}

impl Table {
    /// The rows as a result set (`report::table`): one column per
    /// variable, values in surface spelling, secrets redacted.
    pub fn result(&self, r: &Redactor) -> crate::report::table::Table {
        use crate::report::table::{Cell, Table};
        let mut t = Table::new(self.vars.iter().cloned());
        for row in &self.rows {
            t.push(row.iter().map(|v| Cell::value(v, r)).collect());
        }
        t
    }
}

/// Positions `(type, addr, path, value)` of the predicates that carry an
/// attribute value.
const VALUE_PREDS: [&str; 5] = ["arg", "attr", "world_attr", "cloud_attr", "cloud_computed"];

/// Prints values with every secret replaced by its label.
#[derive(Debug, Default, Clone)]
pub struct Redactor {
    /// Secret value -> label `T/A#P`.
    secrets: BTreeMap<Value, String>,
    /// A secret `random.*` derived -> the call (`random.password("db")`).
    derived: BTreeMap<Value, String>,
}

impl Redactor {
    pub fn new(facts: &BTreeSet<Atom>, schema: &Schema) -> Redactor {
        let mut r = Redactor::default();
        // Inputs and outputs declared `secret(T)`: (type, scope, key).
        let cells: BTreeSet<(&Value, &Value, &Value)> = facts
            .iter()
            .filter(|a| a.pred == crate::transform::SECRET_CELL)
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(t), Term::Val(s), Term::Val(k)] => Some((t, s, k)),
                _ => None,
            })
            .collect();
        // The parts of attribute values a secret is written to, by type
        // and path (`secrets::taint`).
        let mut paths: BTreeMap<(&str, &str), Vec<&str>> = BTreeMap::new();
        for a in facts
            .iter()
            .filter(|a| a.pred == crate::secrets::SECRET_PATH)
        {
            if let [
                Term::Val(Value::Str(t)),
                Term::Val(Value::Str(p)),
                Term::Val(Value::Str(sub)),
            ] = a.args.as_slice()
            {
                paths.entry((t, p)).or_default().push(sub);
            }
        }
        // A `let` holding a secret: by the secret it holds, when that has
        // a label of its own (`random.signing_key("synapse")`).
        let mut lets = Vec::new();
        for a in facts {
            if !VALUE_PREDS.contains(&a.pred.as_str()) || a.args.len() < 4 {
                continue;
            }
            let [
                Term::Val(tv),
                Term::Val(addr),
                Term::Val(pv),
                Term::Val(v),
                ..,
            ] = a.args.as_slice()
            else {
                continue;
            };
            let (Some(t), Some(p)) = (tv.as_str(), pv.as_str()) else {
                continue;
            };
            if schema.is_sensitive(t, p) || cells.contains(&(tv, addr, pv)) {
                let label = crate::value::null_label(t, &partition::fmt_bare(addr), p);
                match t == crate::modules::LET {
                    true => lets.push((v, label)),
                    false => r.add(v, &label),
                }
            }
            // A field its object type declares secret (`conn.password`),
            // and where a rule writes a secret into the value (`data = {
            // yaml: "key: ${signing}" }`): that part of it, whole.
            let fields = cells
                .iter()
                .filter(|(ct, ca, _)| *ct == tv && *ca == addr)
                .filter_map(|(_, _, k)| k.as_str()?.strip_prefix(p)?.strip_prefix('.'))
                .chain(paths.get(&(t, p)).into_iter().flatten().copied());
            for field in fields {
                if let Some(x) = part(v, field) {
                    let addr = partition::fmt_bare(addr);
                    let label = crate::value::null_label(t, &addr, &crate::types::dotted(p, field));
                    match t == crate::modules::LET {
                        true => lets.push((x, label)),
                        false => r.add(x, &label),
                    }
                }
            }
        }
        // `env.var(NAME)` answers a `secret(string)` (R-60): its value is
        // `env.var/NAME`, as the plan file records it, wherever it goes.
        for a in facts
            .iter()
            .filter(|a| a.pred == crate::syntax::resolve::ENV_VAR)
        {
            if let [Term::Val(Value::Str(name)), Term::Val(v)] = a.args.as_slice() {
                r.add(v, &format!("{}/{name}", a.pred));
            }
        }
        // Another in-process extern's secret column: by its call's label,
        // `PRED/INPUTS#N`, from the first answer on.
        for a in facts {
            for col in crate::externs::secret_columns(&a.pred) {
                let vals: Option<Vec<Value>> = a
                    .args
                    .iter()
                    .map(|t| match t {
                        Term::Val(v) => Some(v.clone()),
                        _ => None,
                    })
                    .collect();
                let Some(vals) = vals else { continue };
                let inputs = crate::externs::inputs_of(&a.pred, &vals);
                if let Some(v) = vals.get(col) {
                    r.add(v, &crate::externs::secret_label(&a.pred, &inputs, col));
                }
            }
        }
        // A secret `random.*` derived this run: by its call.
        for (v, l) in crate::functions::random::derived() {
            r.add(&v, &l);
            r.derived.insert(v, l);
        }
        for (v, l) in lets {
            r.add(v, &l);
        }
        // A memo that keeps a secret: its candidate is one too, of the
        // same label (a new master's password, say, not kept).
        for a in facts.iter().filter(|a| a.pred == crate::memo::FIRST) {
            if let [_, Term::Val(c), Term::Val(v)] = a.args.as_slice()
                && let Some(l) = r.secret(v)
            {
                r.add(c, &l);
            }
        }
        // A column of a relation the program derives that the pass found
        // secret (`leak(s) :- ..., s = str.format("pw=%s", p)`): its value at
        // the secret path, by the relation's column when no secret it
        // holds names it better.
        let mut columns: BTreeMap<&str, Vec<(usize, &str)>> = BTreeMap::new();
        for a in facts
            .iter()
            .filter(|a| a.pred == crate::secrets::SECRET_COLUMN)
        {
            if let [
                Term::Val(Value::Str(p)),
                Term::Val(Value::Int(c)),
                Term::Val(Value::Str(path)),
            ] = a.args.as_slice()
            {
                columns
                    .entry(p.as_str())
                    .or_default()
                    .push((*c as usize, path.as_str()));
            }
        }
        for a in facts {
            let Some(cols) = columns.get(a.pred.as_str()) else {
                continue;
            };
            for (c, path) in cols {
                let Some(Term::Val(v)) = a.args.get(*c) else {
                    continue;
                };
                if let Some(x) = part(v, path) {
                    r.add(
                        x,
                        &format!("{}#{}", a.pred, crate::types::dotted(&c.to_string(), path)),
                    );
                }
            }
        }
        r
    }

    /// Each secret value the run holds, with its label (`dform secrets
    /// list`, R-161): never printed, only matched.
    pub fn labelled(&self) -> impl Iterator<Item = (&Value, &str)> {
        self.secrets.iter().map(|(v, l)| (v, l.as_str()))
    }

    /// `v` is a secret, as a whole: its parts are not, by themselves (a
    /// plain word in a secret object prints where it is plain; R-128).
    fn add(&mut self, v: &Value, label: &str) {
        match v {
            Value::Null { .. } | Value::Bool(_) => {}
            Value::Str(s) if s.is_empty() => {}
            _ => {
                self.secrets
                    .entry(v.clone())
                    .or_insert_with(|| label.to_string());
            }
        }
    }

    /// The label a value prints as, if it is a secret: a secret marker, or
    /// a value the program holds at a secret place, whole. Never by its
    /// text: a value that contains a secret's bytes is not one (R-128).
    fn secret(&self, v: &Value) -> Option<String> {
        if let Value::Null {
            label,
            class: NullClass::Secret,
            ..
        } = v
        {
            return Some(label.clone());
        }
        self.secrets.get(v).cloned()
    }

    /// The secrets the run holds that are not derived (an input's, an
    /// environment variable's, an extern's column), as text: those a
    /// derivation digest must not say anything of (`secrets::standin`).
    /// A value built from a derived one is not one of them.
    pub fn sources(&self) -> Vec<String> {
        self.secrets
            .keys()
            .filter(|v| !self.derived.contains_key(*v))
            .filter_map(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .filter(|s| !crate::secrets::standin::derived(s))
            .collect()
    }

    /// The call that derived `v`, when it is a secret `random.*` gave
    /// (`random.password("db")`).
    pub fn derived(&self, v: &Value) -> Option<&str> {
        self.derived.get(v).map(String::as_str)
    }

    pub fn is_secret(&self, v: &Value) -> bool {
        self.secret(v).is_some()
    }

    /// `partition::fmt_value`, with secrets as `(sensitive T["A"].p)` and
    /// nulls as `?T["A"].p` (`ir::label`).
    pub fn fmt(&self, v: &Value) -> String {
        self.spell(v, Spelling::Core)
    }

    /// A value as the program would write it: `fmt`, with a reference as
    /// the address it names as the plan prints it (R-111), `T k3s.server`
    /// or `T k3s.server.p`, and a null as the attribute it stands for.
    pub fn surface(&self, v: &Value) -> String {
        self.spell(v, Spelling::Surface)
    }

    /// A value in a result set's cell (`report::table`): `surface`, with a
    /// secret as `secret(SIZE)`, its size and never its label or bytes;
    /// `secret(?)` while its value is unknown.
    pub fn cell(&self, v: &Value) -> String {
        self.spell(v, Spelling::Cell)
    }

    fn spell(&self, v: &Value, how: Spelling) -> String {
        if let Some(l) = self.secret(v) {
            return match (how, v) {
                (Spelling::Cell, Value::Null { .. }) => "secret(?)".into(),
                (Spelling::Cell, Value::Str(s)) => format!("secret({})", size(s.len())),
                (Spelling::Cell, v) => format!("secret({})", size(partition::fmt_value(v).len())),
                (Spelling::Core, _) => format!("(sensitive {})", crate::ir::label(&l)),
                _ => format!("(sensitive {})", crate::report::attribute_label(&l)),
            };
        }
        let surface = how != Spelling::Core;
        match v {
            Value::List(xs) => format!(
                "[{}]",
                xs.iter()
                    .map(|x| self.spell(x, how))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Obj(m) => format!(
                "{{{}}}",
                m.iter()
                    .map(|(k, x)| format!("{k}: {}", self.spell(x, how)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Null { label, .. } if surface => {
                format!("?{}", crate::report::attribute_label(label))
            }
            Value::Null { label, .. } => format!("?{}", crate::ir::label(label)),
            Value::Ref { typ, name, attr } if surface => crate::report::attribute(
                &crate::ir::Address {
                    typ: typ.clone(),
                    name: name.clone(),
                },
                attr,
            ),
            Value::CloudRef { typ, name, attr } if surface => format!(
                "cloud_ref({}, {}, {})",
                crate::ir::string_literal(typ),
                crate::ir::string_literal(name),
                crate::ir::string_literal(attr)
            ),
            v => partition::fmt_value(v),
        }
    }

    pub fn fmt_atom(&self, a: &Atom) -> String {
        let args: Vec<String> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => self.fmt(v),
                t => partition::fmt_term(t),
            })
            .collect();
        format!("{}({})", a.pred, args.join(", "))
    }

    /// `fmt_atom` with values as the program would write them (`surface`).
    pub fn surface_atom(&self, a: &Atom) -> String {
        let args: Vec<String> = a
            .args
            .iter()
            .map(|t| match t {
                Term::Val(v) => self.surface(v),
                t => partition::fmt_term(t),
            })
            .collect();
        format!("{}({})", a.pred, args.join(", "))
    }

    /// Any text that may quote a program literal (a rule's text, a flag
    /// that gave a value): each string literal in it that is a secret, as
    /// a whole, replaced, and a `--set k=v` whose value is one. The rest
    /// is untouched, an address or a word that merely holds a secret's
    /// bytes included (R-128).
    pub fn text(&self, s: &str) -> String {
        // A flag's value, in its bare spelling, runs to the end of the
        // text (`input --set k=v`).
        for flag in ["--set ", "--data "] {
            if let Some(i) = s.find(flag)
                && let Some((k, v)) = s[i + flag.len()..].split_once('=')
                && let Some(l) = self
                    .secrets
                    .iter()
                    .find(|(x, _)| partition::fmt_bare(x) == v)
                    .map(|(_, l)| l)
            {
                let head = self.tokens(&s[..i], |v| self.secret(v));
                return format!("{head}{flag}{k}={}", sensitive(l));
            }
        }
        self.tokens(s, |v| self.secret(v))
    }

    /// `s` with each string literal `secret` labels replaced.
    fn tokens(&self, s: &str, secret: impl Fn(&Value) -> Option<String>) -> String {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        while let Some(i) = rest.find('"') {
            out.push_str(&rest[..i]);
            let lit = &rest[i..];
            // The literal's end: the next quote no backslash escapes.
            let mut end = None;
            let mut escaped = false;
            for (j, c) in lit.char_indices().skip(1) {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => {
                        end = Some(j + 1);
                        break;
                    }
                    _ => {}
                }
            }
            let Some(end) = end else {
                out.push_str(lit);
                return out;
            };
            let (token, after) = lit.split_at(end);
            match crate::syntax::resolve::unescape(token)
                .ok()
                .and_then(|v| secret(&Value::Str(v)))
            {
                Some(l) => out.push_str(&sensitive(&l)),
                None => out.push_str(token),
            }
            rest = after;
        }
        out.push_str(rest);
        out
    }

    pub fn json(&self, v: &Value) -> serde_json::Value {
        if let Some(l) = self.secret(v) {
            return serde_json::json!({ "sensitive": crate::ir::label(&l) });
        }
        match v {
            Value::List(xs) => serde_json::Value::Array(xs.iter().map(|x| self.json(x)).collect()),
            Value::Obj(m) => serde_json::Value::Object(
                m.iter().map(|(k, x)| (k.clone(), self.json(x))).collect(),
            ),
            Value::Null { label, class, .. } => {
                serde_json::json!({"null": crate::ir::label(label), "class": class.name()})
            }
            v => engine::value_to_json(v),
        }
    }
}

/// The part of `v` at the dotted `path` (`""` all of it); a key may hold
/// a dot itself (`data."homeserver.yaml"`).
fn part<'v>(v: &'v Value, path: &str) -> Option<&'v Value> {
    if path.is_empty() {
        return Some(v);
    }
    let Value::Obj(m) = v else { return None };
    m.iter()
        .find_map(|(k, x)| match path.strip_prefix(k.as_str())? {
            "" => Some(x),
            rest => part(x, rest.strip_prefix('.')?),
        })
}

/// A secret in text, by its label: `(sensitive T.a.p)`.
fn sensitive(label: &str) -> String {
    format!("(sensitive {})", crate::report::attribute_label(label))
}

/// How `Redactor::spell` writes a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Spelling {
    Core,
    Surface,
    Cell,
}

/// A byte count as a person reads it: `812 B`, `5.1 KB`, `2.0 MB`.
pub fn size(n: usize) -> String {
    match n {
        n if n < 1024 => format!("{n} B"),
        n if n < 1024 * 1024 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{:.1} MB", n as f64 / (1024.0 * 1024.0)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(src: &str) -> BTreeSet<Atom> {
        let program = crate::parser::parse_program(src).unwrap();
        engine::eval(&program, &[]).unwrap().0.facts
    }

    #[test]
    fn a_pattern_binds_one_column_per_variable() {
        let f = facts("p(\"a\", 1)\np(\"b\", 2)\nq(\"b\")");
        let Query::Body { body, vars } = parse("p(X, N), q(X)").unwrap() else {
            panic!()
        };
        assert_eq!(vars, ["X", "N"]);
        let t = table(&body, &vars, &f).unwrap();
        assert_eq!(t.rows, vec![vec![Value::Str("b".into()), Value::Int(2)]]);
        assert_eq!(
            t.result(&Redactor::default()).render(&Default::default()),
            "X    N\n\"b\"  2\n"
        );
        assert!(matches!(parse("p").unwrap(), Query::Pred(p) if p == "p"));
        assert_eq!(table(&body, &[], &f).unwrap().rows, vec![Vec::new()]);
    }

    #[test]
    fn json_is_an_array_of_objects_keyed_by_variable() {
        let t = Table {
            vars: vec!["N".into(), "C".into()],
            rows: vec![vec![Value::Str("a".into()), Value::Int(1)]],
        };
        assert_eq!(
            t.result(&Redactor::default()).json(),
            serde_json::json!([{"N": "a", "C": 1}])
        );
    }

    /// A schema of `type_attr` facts.
    fn schema(src: &str) -> Schema {
        let program = crate::parser::parse_program(src).unwrap();
        let facts: Vec<Atom> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                crate::ast::Stmt::Fact(a) => Some(a.clone()),
                _ => None,
            })
            .collect();
        Schema::from_facts(&facts).unwrap()
    }

    /// `src` evaluated with what the secret pass found (`secrets::taint`),
    /// as a deployment evaluates it, and its redactor.
    fn tainted(src: &str, schema: &Schema) -> (BTreeSet<Atom>, Redactor) {
        let program = crate::parser::parse_program(src).unwrap();
        let lowered = crate::transform::lower(&program).unwrap();
        let taint = crate::secrets::taint(&lowered, schema, &Default::default());
        let facts = engine::eval(&program, &taint).unwrap().0.facts;
        let r = Redactor::new(&facts, schema);
        (facts, r)
    }

    #[test]
    fn a_secret_prints_as_its_size_wherever_it_is_forwarded() {
        let schema = schema("type_attr(\"v\", \"pw\", \"string\", [\"sensitive\"])");
        let (f, r) = tainted(
            r#"want("v", "a")
arg("v", "a", "pw", "hunter22", "normal")
               leak(s) where attr("v", "a", "pw", p), s = str.format("pw=%s", p)"#,
            &schema,
        );
        let Query::Body { body, vars } = parse("leak(S)").unwrap() else {
            panic!()
        };
        let t = table(&body, &vars, &f).unwrap().result(&r);
        let out = t.render(&Default::default());
        assert_eq!(out, "S\nsecret(11 B)\n");
        // By the column the pass found secret: the value is not the
        // secret's, it holds it.
        assert_eq!(
            t.json(),
            serde_json::json!([{"S": {"sensitive": "leak#0"}}])
        );
    }

    /// R-128: a secret object is a secret as a whole; a plain word inside
    /// it is not one elsewhere, as a value, inside a longer one, in an
    /// address, or quoted in a rule's text.
    #[test]
    fn a_word_inside_a_secret_object_is_not_a_secret_elsewhere() {
        let schema = schema(
            "type_attr(\"k.secret\", \"data\", \"map\", [\"sensitive\"])\n\
             type_attr(\"k.config\", \"name\", \"string\", [])",
        );
        let (_, r) = tainted(
            r#"arg("k.secret", "synapse_db.creds", "data", {user: "synapse", pw: "hunter22"}, "normal")
arg("k.config", "synapse", "name", "synapse-config", "normal")
arg("k.config", "other", "name", "synapse", "normal")"#,
            &schema,
        );
        let creds = Value::Obj(
            [
                ("user".to_string(), Value::Str("synapse".into())),
                ("pw".to_string(), Value::Str("hunter22".into())),
            ]
            .into(),
        );
        assert_eq!(
            r.surface(&creds),
            "(sensitive k.secret synapse_db.creds.data)"
        );
        assert_eq!(r.cell(&creds), "secret(33 B)");
        for plain in ["synapse", "synapse-config", "hunter22"] {
            let v = Value::Str(plain.into());
            assert!(!r.is_secret(&v), "{plain}");
            assert_eq!(r.surface(&v), format!("{plain:?}"));
        }
        let text =
            r#"k.secret synapse_db.creds  name = "synapse-config"  random.signing_key("synapse")"#;
        assert_eq!(r.text(text), text);
    }

    /// The last line: a secret's exact bytes, as a whole value or a whole
    /// literal in a rule's text or a flag, print as its label; text that
    /// merely holds them is untouched.
    #[test]
    fn a_secret_whole_never_prints() {
        let schema = schema("type_attr(\"v\", \"pw\", \"string\", [\"sensitive\"])");
        let (_, r) = tainted(r#"arg("v", "a", "pw", "hunter22", "normal")"#, &schema);
        let pw = Value::Str("hunter22".into());
        assert_eq!(r.surface(&pw), "(sensitive v a.pw)");
        assert_eq!(
            r.surface(&Value::List(vec![pw.clone(), Value::Str("x".into())])),
            "[(sensitive v a.pw), \"x\"]"
        );
        assert_eq!(r.json(&pw), serde_json::json!({"sensitive": "v[\"a\"].pw"}));
        assert_eq!(r.text(r#"pw = "hunter22""#), "pw = (sensitive v a.pw)");
        assert_eq!(
            r.text("input --set pw=hunter22"),
            "input --set pw=(sensitive v a.pw)"
        );
        for kept in [r#"pw = "hunter22-two""#, "hunter22", r#"v["hunter22x"]"#] {
            assert_eq!(r.text(kept), kept);
        }
    }
}
