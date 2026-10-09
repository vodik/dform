//! A change the plan makes to one resource (`Deformation`) and its lines: an
//! action's changes as `+`/`~`/`-` lines, an element of a set or a keyed list
//! as one line with its leaves, a resolved reference as its resource, the paths
//! that force a replace.

use super::fold::schema_path;
use super::mask::{Shown, masked, shown};
use super::tree;
use super::tree::Site;
use crate::ast::{Atom, Term};
use crate::ir::Address;
use crate::provider::{Action, ActionKind, Change};
use crate::query::Redactor;
use crate::schema::Schema;
use crate::value::Value;
use serde_json::Value as Json;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// A leaf whose value changes (or is set, or removed, per the action).
    Leaf,
    /// An element added to a set or a keyed list.
    Add,
    /// An element removed from a set or a keyed list.
    Remove,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub op: Op,
    pub path: String,
    pub before: Shown,
    pub after: Shown,
    /// An element's leaves, paths relative to the element.
    pub leaves: Vec<Line>,
    /// Where its value was written (R-79, [`Report::explain`](crate::report::Report::explain)).
    pub site: Option<Site>,
    /// At `-vv`, how its value was made: each expression it passed
    /// through, then what it beat (R-122, [`tree::Printer::attr_chain`]).
    pub chain: Vec<tree::Step>,
    /// A fold (R-124): the value its leaves make, which one contribution
    /// wrote, printed in the formatter's layout.
    pub value: Option<crate::fmt::value::Tree>,
    /// A document value (R-131): the row a loader read it from and its
    /// size, `vendor/crds.yml:412  (24.0 KB)`, said in place of the value;
    /// at the empty path, a value body's (the resource's whole body).
    pub row: Option<String>,
}

impl Line {
    /// A line at a path its schema marks, or may mark, sensitive says no
    /// literal where it was written (R-124 amendment 2, R-215): its site,
    /// the statement's bound variables and each step of its chain are
    /// [`masked`], in every printer. A secret with a label needs none: the
    /// redactor says it wherever it is written.
    pub(super) fn mask(&mut self) {
        let path = |s: &Shown| matches!(s, Shown::Sensitive(None));
        if path(&self.before) || path(&self.after) {
            if let Some(s) = &mut self.site {
                s.statement = masked(&s.statement);
                s.entry = s.entry.as_deref().map(masked);
                s.with.iter_mut().for_each(|w| *w = masked(w));
            }
            for step in &mut self.chain {
                step.expr = masked(&step.expr);
                step.with.iter_mut().for_each(|w| *w = masked(w));
            }
        }
        self.leaves.iter_mut().for_each(Line::mask);
    }
}

#[derive(Debug, Clone)]
pub struct Deformation {
    pub kind: ActionKind,
    pub addr: Address,
    pub lines: Vec<Line>,
    /// Where it is derived (R-79): its `want`'s site; a delete's, where
    /// the last apply derived it.
    pub site: Option<Site>,
    /// The leaf that changed since the last apply ([`Report::because`](crate::report::Report::because)).
    pub because: Option<String>,
    /// Of a run that does not hold the deployment's master (R-164):
    /// `secrets unchanged` (each secret leaf it derives proven so), or
    /// `secret changed, needs the key` (an apply without it does not make
    /// it), said in the site column.
    pub custody: Option<String>,
    /// A replace: the changed paths the schema declares immutable.
    pub forces: Vec<String>,
    /// The lines as the plan prints them below `-vv` (R-124): each value
    /// one contribution wrote folded back to where the writers diverge
    /// ([`fold`](crate::report::fold)). Empty: [`Deformation::lines`] as they are.
    pub folded: Vec<Line>,
    /// A plan's delete: why the program no longer derives it, for the
    /// site column of its change line ([`Report::explain`](crate::report::Report::explain), After R-149).
    /// Where the rule that would derive it is written, and why it does
    /// not.
    pub gone: Option<(Option<String>, String)>,
    /// Each attribute given at creation only whose value differs from
    /// what the object was made with: kept, and said ([`KEPT`](crate::report::KEPT), R-198).
    pub kept: Vec<Line>,
}

/// An action's change lines. An update diffs a keyless set, or a list
/// with merge keys, by element: an element that is new or gone is one
/// `+`/`-` line with its leaves. A keyless set's element is labeled by a
/// hash of its content (`[#k3j2d]`); it prints as `[]` in an update and
/// by position everywhere else.
pub(super) fn deformation(a: &Action, schema: &Schema, r: &Redactor, refs: &Refs) -> Deformation {
    let sent = a.sent();
    let paths = relabel(sent.iter().map(|c| c.path.as_str()));
    // Whether the schema types the leaf at `path` a reference: an
    // element of a `set(ref(T))`, or a `ref(T)` field.
    let is_ref = |path: &str| {
        let ty = schema
            .attr(&a.addr.typ, &schema_path(path))
            .map(|s| s.ty.replace(' ', ""))
            .unwrap_or_default();
        ty.starts_with("ref(")
            || (path.ends_with(']') && (ty.starts_with("set(ref(") || ty.starts_with("list(ref(")))
    };
    let leaf = |c: &Change, path: String| {
        let is_ref = is_ref(&c.path);
        Line {
            op: Op::Leaf,
            path,
            before: refs.shown(
                &a.addr,
                &c.path,
                is_ref,
                shown(c.before.as_ref(), c.sensitive, schema, r),
            ),
            after: refs.shown(
                &a.addr,
                &c.path,
                is_ref,
                shown(c.after.as_ref(), c.sensitive, schema, r),
            ),
            leaves: vec![],
            site: None,
            chain: Vec::new(),
            value: None,
            row: None,
        }
    };
    let by_element = matches!(
        a.kind,
        ActionKind::Update | ActionKind::Drift | ActionKind::Pending | ActionKind::Replace { .. }
    );
    let mut lines = Vec::new();
    // Element prefix -> (its display path, its changes), in first-seen order.
    let mut elements: Vec<(String, String, ElementChanges)> = Vec::new();
    for (c, shown_path) in sent.into_iter().zip(paths) {
        let Some((list, elem, rest)) = by_element
            .then(|| element_of(&a.addr.typ, &c.path, schema))
            .flatten()
        else {
            lines.push(leaf(c, shown_path));
            continue;
        };
        let prefix = format!("{list}[{elem}]");
        let display = if elem.starts_with('#') {
            format!("{list}[]")
        } else {
            prefix.clone()
        };
        // The rest of the path as relabeled with every other change's.
        let rest = match rest.is_empty() {
            true => rest,
            false => {
                let after_elem = shown_path[list.len()..]
                    .find(']')
                    .map(|j| list.len() + j + 1);
                after_elem
                    .map(|j| shown_path[j..].trim_start_matches('.').to_string())
                    .unwrap_or(rest)
            }
        };
        match elements.iter_mut().find(|(p, _, _)| *p == prefix) {
            Some((_, _, cs)) => cs.push((c, rest)),
            None => elements.push((prefix, display, vec![(c, rest)])),
        }
    }
    for (_, display, cs) in elements {
        let added = cs.iter().all(|(c, _)| c.before.is_none());
        let removed = cs.iter().all(|(c, _)| c.after.is_none());
        if !added && !removed {
            lines.extend(
                cs.iter()
                    .map(|(c, rest)| leaf(c, format!("{display}.{rest}"))),
            );
            continue;
        }
        let op = if added { Op::Add } else { Op::Remove };
        // A scalar element is its own leaf.
        if let [(c, rest)] = cs.as_slice()
            && rest.is_empty()
        {
            let l = leaf(c, display);
            lines.push(Line { op, ..l });
            continue;
        }
        let leaves = cs.iter().map(|(c, rest)| leaf(c, rest.clone())).collect();
        lines.push(Line {
            op,
            path: display,
            before: Shown::Absent,
            after: Shown::Absent,
            leaves,
            site: None,
            chain: Vec::new(),
            value: None,
            row: None,
        });
    }
    Deformation {
        kind: a.kind.clone(),
        addr: a.addr.clone(),
        lines,
        site: None,
        because: None,
        custody: None,
        forces: match a.kind {
            ActionKind::Replace { .. } => forces(a, schema),
            _ => Vec::new(),
        },
        folded: Vec::new(),
        gone: None,
        kept: a.kept().iter().map(|c| leaf(c, c.path.clone())).collect(),
    }
}

/// The paths of replace `a`'s changes the schema declares `force_new`,
/// dotted, without indices.
pub(super) fn forces(a: &Action, schema: &Schema) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for c in &a.changes {
        let p = crate::ir::path_keys(&c.path)
            .iter()
            .map(|k| k.split('[').next().unwrap_or(k).to_string())
            .collect::<Vec<_>>()
            .join(".");
        if schema.forces_new(&a.addr.typ, &p) && !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// What prints a resolved reference as its resource (R-43): the world's
/// ids, `identity(T, A, R)` joined with `world_attr(T, R, IDENTITY, V)`, by
/// value, and the desired documents' attributes that hold a reference.
pub(super) struct Refs<'a> {
    ids: BTreeMap<&'a str, Address>,
    desired: BTreeMap<(&'a str, &'a str, &'a str), &'a Value>,
}

impl<'a> Refs<'a> {
    pub(super) fn new(facts: &'a BTreeSet<Atom>) -> Refs<'a> {
        let s = |t: &'a Term| match t {
            Term::Val(Value::Str(s)) => Some(s.as_str()),
            _ => None,
        };
        let mut names: BTreeMap<(&str, &str), &str> = BTreeMap::new();
        let mut ids: Vec<(&str, &str, &str)> = Vec::new();
        let mut desired = BTreeMap::new();
        for f in facts {
            match (f.pred.as_str(), f.args.as_slice()) {
                ("identity", [t, a, r]) => {
                    if let (Some(t), Some(a), Some(r)) = (s(t), s(a), s(r)) {
                        names.insert((t, r), a);
                    }
                }
                ("world_attr", [t, r, p, v]) if s(p) == Some(crate::schema::IDENTITY) => {
                    if let (Some(t), Some(r), Some(v)) = (s(t), s(r), s(v)) {
                        ids.push((t, r, v));
                    }
                }
                ("attr", [t, a, p, Term::Val(v)]) if holds_ref(v) || holds_uri(v) => {
                    if let (Some(t), Some(a), Some(p)) = (s(t), s(a), s(p)) {
                        desired.insert((t, a, p), v);
                    }
                }
                _ => {}
            }
        }
        let ids = ids
            .into_iter()
            .filter_map(|(t, r, v)| {
                let name = names.get(&(t, r))?;
                Some((
                    v,
                    Address {
                        typ: t.to_string(),
                        name: name.to_string(),
                    },
                ))
            })
            .collect();
        Refs { ids, desired }
    }

    /// `v`, a side of the change at `path` of `addr`: an id where the
    /// program's document holds a reference prints as that resource; a
    /// uri the provider holds in its A-labels prints as the program wrote
    /// it when it is the program's, else as read, never decoded (R-134).
    pub(super) fn shown(&self, addr: &Address, path: &str, is_ref: bool, v: Shown) -> Shown {
        let Shown::Value(Json::String(id)) = &v else {
            return v;
        };
        let top = path.split(['.', '[']).next().unwrap_or(path);
        let at = self
            .desired
            .get(&(addr.typ.as_str(), addr.name.as_str(), top))
            .and_then(|d| walk(d, &path[top.len()..]));
        match (at, self.ids.get(id.as_str())) {
            (Some(Value::Ref { attr, .. }), Some(to)) if attr.is_empty() => Shown::Ref {
                addr: to.clone(),
                value: Json::String(id.clone()),
            },
            // A set's element the program no longer holds (R-158): the
            // schema says it is a reference.
            (None, Some(to)) if is_ref => Shown::Ref {
                addr: to.clone(),
                value: Json::String(id.clone()),
            },
            (Some(Value::Uri(u)), _) if u.ascii() == *id => {
                Shown::Value(Json::String(u.to_string()))
            }
            _ => v,
        }
    }
}

/// Whether a value holds a uri: its host is the program's spelling,
/// the provider's its A-labels ([`Refs::shown`]).
fn holds_uri(v: &Value) -> bool {
    v.any_scalar(&mut |x| matches!(x, Value::Uri(u) if u.unicode_host()))
}

fn holds_ref(v: &Value) -> bool {
    v.any_scalar(&mut |x| matches!(x, Value::Ref { attr, .. } if attr.is_empty()))
}

/// The value at `rest` (`.a.b`, `[2]`, as a change's path goes on) of `v`.
pub(super) fn walk<'v>(v: &'v Value, rest: &str) -> Option<&'v Value> {
    if rest.is_empty() {
        return Some(v);
    }
    if let Some(r) = rest.strip_prefix('[') {
        let (i, r) = r.split_once(']')?;
        let Value::List(xs) = v else { return None };
        return walk(xs.get(i.parse::<usize>().ok()?)?, r);
    }
    let r = rest.strip_prefix('.')?;
    let end = r.find(['.', '[']).unwrap_or(r.len());
    let Value::Obj(m) = v else { return None };
    walk(m.get(&r[..end])?, &r[end..])
}

/// An element's changes, each with its path relative to the element.
type ElementChanges<'a> = Vec<(&'a Change, String)>;

/// Replace every content label `[#hash]` by the element's position among
/// the labels under the same list, in order of appearance.
fn relabel<'a>(paths: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut out = Vec::new();
    for p in paths {
        let mut s = String::new();
        let mut rest = p;
        while let Some(i) = rest.find("[#") {
            let Some(j) = rest[i..].find(']') else { break };
            s.push_str(&rest[..i]);
            let (list, label) = (s.clone(), rest[i + 1..i + j].to_string());
            let labels = seen.entry(list).or_default();
            let n = match labels.iter().position(|l| *l == label) {
                Some(n) => n,
                None => {
                    labels.push(label);
                    labels.len() - 1
                }
            };
            s.push_str(&format!("[{n}]"));
            rest = &rest[i + j + 1..];
        }
        s.push_str(rest);
        out.push(s);
    }
    out
}

/// `(list path, element label, rest)` when `path`'s first list segment is
/// a keyless set or a list with merge keys.
fn element_of(typ: &str, path: &str, schema: &Schema) -> Option<(String, String, String)> {
    let open = path.find('[')?;
    let close = open + path[open..].find(']')?;
    let list = &path[..open];
    let keyed = schema.list_key(typ, list).is_some();
    let set = schema.attr(typ, list).is_some_and(|a| a.kind() == "set");
    if !keyed && !set {
        return None;
    }
    Some((
        list.to_string(),
        path[open + 1..close].to_string(),
        path[close + 1..].trim_start_matches('.').to_string(),
    ))
}
