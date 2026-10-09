//! An attribute as the source wrote it (R-124): a resource's leaves
//! grouped by the contribution that wrote each. A subtree whose leaves
//! one contribution wrote prints once, at the path where the writers
//! diverge, as one value in the formatter's layout
//! ([`crate::fmt::value`]); a leaf another contribution wrote splits out
//! at its own line. A leaf no contribution of the program wrote (a
//! schema's default of a merge key) does not split a value: it prints on
//! its own line beside it. A create's lines are folded so (`folded`,
//! `Folding`), its leaves in the order the program gave a list's elements.

use super::deformation::{Deformation, Line, Op};
use super::explain::attr_holding;
use super::labels::path;
use super::mask::Shown;
use super::tree;
use super::tree::Site;
use super::{SCHEMA_DEFAULT, Why};
use crate::ast::{Atom, RuleStmt, Term};
use crate::fmt::value::Tree;
use crate::ir::{Address, Step, Tok, tokens};
use crate::spell;
use crate::value::Value;
use serde_json::Value as Json;
use std::collections::HashMap;
use std::collections::{BTreeMap, BTreeSet};

/// The part of `v` at steps `toks`.
pub fn reach<'v>(v: &'v Value, toks: &[Tok]) -> Option<&'v Value> {
    let Some((t, rest)) = toks.split_first() else {
        return Some(v);
    };
    let next = match (&t.step, v) {
        (Step::Key(k), Value::Obj(m)) => m.get(crate::ir::segment_key(k).as_ref())?,
        (Step::Index(i), Value::List(xs)) => xs.get(*i)?,
        (Step::Keyed(pairs), Value::List(xs)) => xs.iter().find(|x| keyed(x, pairs))?,
        _ => return None,
    };
    reach(next, rest)
}

/// [`reach`] in a contribution's value `v`, `merged` the merged value
/// at the same place: an element by position is the contribution's
/// element equal to the merged list's at that position (contributions'
/// lists are joined), else by its own position when nothing is merged.
pub fn reach_along<'v>(v: &'v Value, merged: Option<&Value>, toks: &[Tok]) -> Option<&'v Value> {
    let mut cur = (v, merged);
    for t in toks {
        cur = step_along(cur.0, cur.1, t)?;
    }
    Some(cur.0)
}

/// What a prefix of paths into one value reached ([`reach_along_memo`]),
/// by the prefix's text.
pub type Reached<'v, 'm> = HashMap<String, Option<(&'v Value, Option<&'m Value>)>>;

/// [`reach_along`] of many paths into one value from one place: what each
/// prefix reaches is kept in `memo`, so a list's element is matched
/// against the merged list's once, not once per leaf under it.
pub fn reach_along_memo<'v, 'm>(
    memo: &mut Reached<'v, 'm>,
    v: &'v Value,
    merged: Option<&'m Value>,
    toks: &[Tok],
) -> Option<&'v Value> {
    let mut cur = (v, merged);
    let mut key = String::new();
    for t in toks {
        key.push('\0');
        key.push_str(&t.text);
        let next = match memo.get(&key) {
            Some(hit) => *hit,
            None => {
                let next = step_along(cur.0, cur.1, t);
                memo.insert(key.clone(), next);
                next
            }
        };
        cur = next?;
    }
    Some(cur.0)
}

/// One step of [`reach_along`].
fn step_along<'v, 'm>(
    v: &'v Value,
    merged: Option<&'m Value>,
    t: &Tok,
) -> Option<(&'v Value, Option<&'m Value>)> {
    Some(match (&t.step, v) {
        (Step::Key(k), Value::Obj(m)) => {
            let k = crate::ir::segment_key(k);
            let merged = match merged {
                Some(Value::Obj(mm)) => mm.get(k.as_ref()),
                _ => None,
            };
            (m.get(k.as_ref())?, merged)
        }
        (Step::Index(i), Value::List(xs)) => match merged {
            Some(Value::List(ms)) => {
                let m = ms.get(*i)?;
                (xs.iter().find(|x| *x == m)?, Some(m))
            }
            _ => (xs.get(*i)?, None),
        },
        (Step::Keyed(pairs), Value::List(xs)) => {
            let merged = match merged {
                Some(Value::List(ms)) => ms.iter().find(|x| keyed(x, pairs)),
                _ => None,
            };
            (xs.iter().find(|x| keyed(x, pairs))?, merged)
        }
        _ => return None,
    })
}

/// Whether element `x` has the key fields `pairs` (`name=web`); a field
/// it leaves out is the schema's default, and does not tell.
pub fn keyed(x: &Value, pairs: &[(String, String)]) -> bool {
    let Value::Obj(m) = x else { return false };
    pairs
        .iter()
        .all(|(k, want)| m.get(k).is_none_or(|v| spell::bare(v) == *want))
}

/// The label the plan names element `x` of a list keyed by `keys` with
/// (R-158), `port=5432,protocol=TCP`: each key field and its value bare,
/// as [`keyed`] reads it back. `None` when `x` leaves a key field out.
pub fn label(keys: &[String], x: &Value) -> Option<String> {
    let Value::Obj(m) = x else { return None };
    let fields: Option<Vec<String>> = keys
        .iter()
        .map(|k| Some(format!("{k}={}", spell::bare(m.get(k)?))))
        .collect();
    Some(fields?.join(","))
}

/// Where the leaf at `toks` sits in value `v`, to order leaves by: a
/// key by its name, an element by its position in the list (a keyed
/// one's in the order the program gave the elements).
pub fn position(v: &Value, toks: &[Tok]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut at = Some(v);
    for t in toks {
        let (pos, next) = match (&t.step, at) {
            (Step::Key(k), Some(Value::Obj(m))) => (0, m.get(crate::ir::segment_key(k).as_ref())),
            (Step::Index(i), Some(Value::List(xs))) => (*i, xs.get(*i)),
            (Step::Keyed(pairs), Some(Value::List(xs))) => {
                match xs.iter().position(|x| keyed(x, pairs)) {
                    Some(i) => (i, xs.get(i)),
                    None => (usize::MAX, None),
                }
            }
            (Step::Index(i), _) => (*i, None),
            _ => (0, None),
        };
        out.push((pos, t.text.clone()));
        at = next;
    }
    out
}

/// One printed item: the leaves it prints, at `path`. A fold of one leaf
/// is that leaf's own line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Group {
    pub path: String,
    pub depth: usize,
    pub leaves: Vec<usize>,
}

/// Fold leaves `paths`, each written by `writers[i]` (`None`: by no
/// contribution of the program), into the items to print, in the order
/// of their first leaf: each writer's leaves as one value at the deepest
/// path that holds them all, where it and the other writers diverge
/// (`spec.template.spec = { .. }`, `tags = { .. }` beside a policy's
/// `tags.team`); a writer of one leaf, and a leaf no one wrote, on its
/// own line. A leaf `noted` (no one wrote it, a note says what made it:
/// a schema's default, R-217) is inside the deepest value that holds its
/// path, else on its own line; it makes no list one other writers add to.
pub fn fold<W: PartialEq>(paths: &[String], writers: &[Option<W>], noted: &[bool]) -> Vec<Group> {
    let toks: Vec<Vec<Tok>> = paths.iter().map(|p| tokens(p)).collect();
    let alone = |i: usize| Group {
        path: paths[i].clone(),
        depth: toks[i].len(),
        leaves: vec![i],
    };
    let mut out = Vec::new();
    let mut done = vec![false; paths.len()];
    for i in 0..paths.len() {
        if done[i] || noted[i] {
            continue;
        }
        let Some(w) = &writers[i] else {
            done[i] = true;
            out.push(alone(i));
            continue;
        };
        let mine: Vec<usize> = (i..paths.len())
            .filter(|&j| !done[j] && writers[j].as_ref() == Some(w))
            .collect();
        mine.iter().for_each(|&j| done[j] = true);
        // The steps every leaf of the writer shares, short of a leaf.
        let shortest = mine.iter().map(|&j| toks[j].len()).min().unwrap_or(0);
        let mut depth = (0..shortest.saturating_sub(1))
            .take_while(|&d| mine.iter().all(|&j| toks[j][d].step == toks[i][d].step))
            .count();
        // A list's element by position is the list's: `egress = [{ .. }]`.
        while depth > 0 && matches!(toks[i][depth - 1].step, Step::Index(_) | Step::Other(_)) {
            depth -= 1;
        }
        // A list other writers add elements to as well: each of this
        // writer's elements by its position (`statements[1] = { .. }`),
        // not a list that says only part of it.
        let positional = matches!(
            toks[i].get(depth).map(|t| &t.step),
            Some(Step::Index(_) | Step::Other(_))
        );
        let shared = positional
            && (0..paths.len()).any(|j| {
                !noted[j]
                    && writers[j].as_ref() != Some(w)
                    && toks[j].len() > depth
                    && toks[j][..depth] == toks[i][..depth]
            });
        if shared && depth > 0 {
            let mut elements: Vec<Vec<usize>> = Vec::new();
            for &j in &mine {
                match elements
                    .iter_mut()
                    .find(|e| toks[e[0]][depth].step == toks[j][depth].step)
                {
                    Some(e) => e.push(j),
                    None => elements.push(vec![j]),
                }
            }
            for e in elements {
                match e.as_slice() {
                    [j] => out.push(alone(*j)),
                    _ => out.push(Group {
                        path: text(&toks[e[0]], depth + 1),
                        depth: depth + 1,
                        leaves: e,
                    }),
                }
            }
            continue;
        }
        match (mine.as_slice(), depth) {
            // A list only this writer wrote, of one element: the list
            // as it was written (`policies = [app_policy]`).
            ([j], d) if d > 0 && positional => out.push(Group {
                path: text(&toks[*j], d),
                depth: d,
                leaves: vec![*j],
            }),
            ([j], _) => out.push(alone(*j)),
            (_, 0) => out.extend(mine.iter().map(|&j| alone(j))),
            _ => out.push(Group {
                path: text(&toks[i], depth),
                depth,
                leaves: mine,
            }),
        }
    }
    // A leaf a note says is inside the deepest value that holds its path,
    // after the value's own leaves; else on its own line.
    for i in (0..paths.len()).filter(|&i| noted[i]) {
        let holds = |g: &Group| {
            g.path != paths[g.leaves[0]]
                && toks[i].len() > g.depth
                && toks[i][..g.depth] == toks[g.leaves[0]][..g.depth]
        };
        match out.iter_mut().filter(|g| holds(g)).max_by_key(|g| g.depth) {
            Some(g) => g.leaves.push(i),
            None => out.push(alone(i)),
        }
    }
    // A value where its path's leaves begin, before a leaf another
    // writer made inside it: `metadata = { .. }`, then
    // `metadata.labels.owner = ..`.
    out.sort_by_key(|g| {
        let first = g.leaves.first().copied().unwrap_or_default();
        let at = (0..paths.len())
            .find(|&j| toks[j].len() >= g.depth && toks[j][..g.depth] == toks[first][..g.depth])
            .unwrap_or(first);
        (at, g.depth, first)
    });
    out
}

/// The printed path of the first `n` steps of `toks`.
fn text(toks: &[Tok], n: usize) -> String {
    toks[..n].iter().map(|t| t.text.as_str()).collect()
}

/// The value of group `g`: its leaves' values (`values[i]` of leaf
/// `paths[i]`) below its path, objects by key in the order they come,
/// a list's elements by position.
pub fn assemble(g: &Group, paths: &[String], values: &[Tree]) -> Tree {
    let rel: Vec<(Vec<Tok>, &Tree)> = g
        .leaves
        .iter()
        .map(|&i| (tokens(&paths[i])[g.depth..].to_vec(), &values[i]))
        .collect();
    build(&rel)
}

/// Leaves below a path: the steps left, and each leaf's value.
type Below<'a> = Vec<(Vec<Tok>, &'a Tree)>;

fn build(rel: &[(Vec<Tok>, &Tree)]) -> Tree {
    if let [(t, v)] = rel
        && t.is_empty()
    {
        return (*v).clone();
    }
    let mut children: Vec<(&Tok, Below)> = Vec::new();
    for (t, v) in rel {
        let Some((first, rest)) = t.split_first() else {
            continue;
        };
        let item = (rest.to_vec(), *v);
        match children.iter_mut().find(|(s, _)| s.step == first.step) {
            Some((_, xs)) => xs.push(item),
            None => children.push((first, vec![item])),
        }
    }
    let keyed = children.iter().all(|(t, _)| matches!(t.step, Step::Key(_)));
    if keyed {
        return Tree::Obj(
            children
                .iter()
                .map(|(t, xs)| match &t.step {
                    Step::Key(k) => (k.clone(), build(xs)),
                    _ => unreachable!("every step is a key"),
                })
                .collect(),
        );
    }
    children.sort_by_key(|(t, _)| match t.step {
        Step::Index(n) => n,
        _ => 0,
    });
    Tree::List(children.iter().map(|(_, xs)| build(xs)).collect())
}

/// Create `d`'s lines folded (R-124): the leaves each contribution wrote
/// as one value where the writers diverge, a leaf another wrote on its
/// own line; one no contribution wrote is the schema's default when the
/// schema gives one there (`type_default`, in `all`).
pub(super) fn folded(
    d: &Deformation,
    p: &tree::Printer,
    rules: &[RuleStmt],
    facts: &[&Atom],
    all: &BTreeSet<Atom>,
    why: Why,
    site: &mut dyn FnMut(&Line) -> Option<Site>,
) -> Vec<Line> {
    let lines = written_order(d, facts);
    let paths: Vec<String> = lines.iter().map(|l| l.path.clone()).collect();
    let writers = leaf_writers(p, facts, &lines, &paths);
    // A document value (R-131), at the default level: the leaves a
    // contribution read whole from a loader's document are its row, said
    // once (below), so their values are not laid out.
    let mut rows: BTreeMap<crate::circuit::NodeId, Option<tree::DocRow>> = BTreeMap::new();
    if why == Why::Line {
        for w in writers.iter().flatten() {
            rows.entry(*w).or_insert_with(|| p.document_row(rules, *w));
        }
    }
    let defaults = type_defaults(all, &d.addr.typ);
    // A leaf no contribution wrote that the schema defaults (R-217).
    let noted: Vec<bool> = paths
        .iter()
        .zip(&writers)
        .map(|(p, w)| w.is_none() && defaults.contains(&schema_path(p)))
        .collect();
    let f = Folding {
        p,
        rules,
        why,
        sets: keyless_sets(&d.addr.typ, all),
        lines,
        paths,
        writers,
        noted,
        rows,
    };
    let values: Vec<Tree> = f
        .lines
        .iter()
        .zip(&f.writers)
        .zip(&f.noted)
        .map(|((l, w), noted)| match (f.in_row(*w), noted) {
            (true, _) => Tree::Leaf(String::new()),
            (false, true) => Tree::Noted(whole(&l.after, why), SCHEMA_DEFAULT.into()),
            (false, false) => Tree::Leaf(whole(&l.after, why)),
        })
        .collect();
    let out: Vec<(Option<crate::circuit::NodeId>, Line)> = fold(&f.paths, &f.writers, &f.noted)
        .into_iter()
        .flat_map(|g| f.group(&g, &values, site))
        .collect();
    if why != Why::Line {
        return out.into_iter().map(|(_, l)| l).collect();
    }
    f.with_rows(out)
}

/// A deformation's lines in the order the program gave a list's elements.
pub(super) fn written_order<'d>(d: &'d Deformation, facts: &[&Atom]) -> Vec<&'d Line> {
    let mut lines: Vec<&Line> = d.lines.iter().collect();
    lines.sort_by_cached_key(|l| {
        let Some((a, _, _)) = attr_holding(facts, &l.path) else {
            return Vec::new();
        };
        let (Some(Term::Val(Value::Str(top))), Some(Term::Val(v))) = (a.args.get(2), a.args.get(3))
        else {
            return Vec::new();
        };
        let toks = tokens(&l.path);
        let skip = tokens(top).len().min(toks.len());
        let mut key: Vec<(usize, String)> =
            toks[..skip].iter().map(|t| (0, t.text.clone())).collect();
        key.extend(position(v, &toks[skip..]));
        key
    });
    lines
}

/// The contribution that wrote each leaf line (`None` for another line),
/// by the fact's address: an attribute fact compared as a key is its whole
/// value compared, once per leaf.
pub(super) fn leaf_writers(
    p: &tree::Printer,
    facts: &[&Atom],
    lines: &[&Line],
    paths: &[String],
) -> Vec<Option<crate::circuit::NodeId>> {
    let mut writers: Vec<Option<crate::circuit::NodeId>> = vec![None; paths.len()];
    let mut by_attr: BTreeMap<*const Atom, (&Atom, Vec<usize>)> = BTreeMap::new();
    for (i, l) in lines.iter().enumerate() {
        if l.op == Op::Leaf
            && let Some((a, _, _)) = attr_holding(facts, &l.path)
        {
            by_attr
                .entry(a as *const Atom)
                .or_insert_with(|| (a, Vec::new()))
                .1
                .push(i);
        }
    }
    for (a, at) in by_attr.into_values() {
        let held: Vec<String> = at.iter().map(|&i| paths[i].clone()).collect();
        for (i, w) in at.into_iter().zip(p.writers(a, &held)) {
            writers[i] = w;
        }
    }
    writers
}

/// The paths of type `typ` whose value is its schema's default.
pub(crate) fn type_defaults(all: &BTreeSet<Atom>, typ: &str) -> BTreeSet<String> {
    all.iter()
        .filter(|f| f.pred == "type_default")
        .filter_map(|f| match f.args.as_slice() {
            [Term::Val(Value::Str(t)), Term::Val(Value::Str(p)), _] if *t == typ => Some(p.clone()),
            _ => None,
        })
        .collect()
}

/// What the schema says of an attribute's type that the plan's paths
/// and notes read: the paths whose value is its default (R-217), and
/// each keyed list's keys (`type_list_key`), by schema path.
pub struct Shape {
    defaults: BTreeSet<String>,
    lists: BTreeMap<String, Vec<String>>,
}

impl Shape {
    /// The shape of attribute fact `f`'s type, from the schema's facts.
    pub fn of(facts: &BTreeSet<Atom>, f: &Atom) -> Shape {
        let Some(Term::Val(Value::Str(typ))) = f.args.first() else {
            return Shape {
                defaults: BTreeSet::new(),
                lists: BTreeMap::new(),
            };
        };
        let lists = facts
            .iter()
            .filter(|a| a.pred == "type_list_key")
            .filter_map(|a| match a.args.as_slice() {
                [
                    Term::Val(Value::Str(t)),
                    Term::Val(Value::Str(l)),
                    Term::Val(ks),
                ] if t == typ => {
                    let ks = match ks {
                        Value::List(ks) => ks
                            .iter()
                            .filter_map(Value::as_str)
                            .map(String::from)
                            .collect(),
                        Value::Str(k) => vec![k.clone()],
                        _ => return None,
                    };
                    Some((l.clone(), ks))
                }
                _ => None,
            })
            .collect();
        Shape {
            defaults: type_defaults(facts, typ),
            lists,
        }
    }

    /// The leaf at printed path `path` is where the schema gives a default.
    pub fn defaulted(&self, path: &str) -> bool {
        self.defaults.contains(&schema_path(path))
    }

    /// Element `j` of the list at printed path `list`, `x`, as the plan
    /// prints its path: by its key in a keyed list,
    /// `spec.ports[port=5432,protocol=TCP]`, else by its position.
    pub fn element(&self, list: &str, j: usize, x: &Value) -> String {
        match self
            .lists
            .get(&schema_path(list))
            .and_then(|ks| label(ks, x))
        {
            Some(label) => format!("{list}[{label}]"),
            None => format!("{list}[{j}]"),
        }
    }

    /// Attribute fact `f`'s value below `keys`, and past a list `below`
    /// (`[0].protocol`), its path, and the path the pattern named from the
    /// resource as the plan prints it (`spec.ports[port=5432,protocol=TCP].protocol`).
    pub fn reach<'f>(
        &self,
        f: &'f Atom,
        keys: &[String],
        below: &[Tok],
    ) -> Option<(&'f Value, &'f String, String)> {
        let (Some(Term::Val(v)), Some(Term::Val(Value::Str(top)))) = (f.args.get(3), f.args.get(2))
        else {
            return None;
        };
        let mut v = keys.iter().try_fold(v, |v, k| match v {
            Value::Obj(m) => m.get(k),
            _ => None,
        })?;
        let mut path = keys
            .iter()
            .fold(top.clone(), |p, k| crate::ir::path_join(&p, k));
        for t in below {
            let next = reach(v, std::slice::from_ref(t))?;
            path = match v {
                Value::List(xs) => {
                    let j = xs.iter().position(|x| std::ptr::eq(x, next))?;
                    self.element(&path, j, next)
                }
                _ => path + &t.text,
            };
            v = next;
        }
        Some((v, top, path))
    }

    /// Value `v` at printed path `path` of attribute fact `f` as a tree to
    /// lay out, as the plan lays a value out: a list's elements at their
    /// paths as the plan prints them, each leaf as `leaf` spells it, and
    /// a leaf no write of the program made that the schema defaults
    /// followed by its note (R-217).
    pub fn laid(
        &self,
        p: &tree::Printer,
        f: &Atom,
        path: &str,
        v: &Value,
        leaf: &dyn Fn(&Value) -> String,
    ) -> Tree {
        let open =
            |v: &Value| matches!(v, Value::Obj(_) | Value::List(_)) && !p.redact.is_secret(v);
        // Each leaf of `v` at its path, as `each` takes it.
        fn walk(
            v: &Value,
            path: String,
            open: &dyn Fn(&Value) -> bool,
            shape: &Shape,
            each: &mut dyn FnMut(&Value, String) -> Tree,
        ) -> Tree {
            match v {
                Value::Obj(m) if open(v) => Tree::Obj(
                    m.iter()
                        .map(|(k, x)| {
                            let at = crate::ir::path_join(&path, k);
                            let t = walk(x, at, open, shape, each);
                            (crate::fmt::value::key_text(k), t)
                        })
                        .collect(),
                ),
                Value::List(xs) if open(v) => Tree::List(
                    xs.iter()
                        .enumerate()
                        .map(|(j, x)| walk(x, shape.element(&path, j, x), open, shape, each))
                        .collect(),
                ),
                v => each(v, path),
            }
        }
        // The leaves the schema defaults, and of them those no write made.
        let mut defaulted = Vec::new();
        if !self.defaults.is_empty() {
            walk(v, path.to_string(), &open, self, &mut |_, at| {
                if self.defaulted(&at) {
                    defaulted.push(at);
                }
                Tree::Leaf(String::new())
            });
        }
        let noted: BTreeSet<&String> = defaulted
            .iter()
            .zip(p.writers(f, &defaulted))
            .filter_map(|(at, w)| w.is_none().then_some(at))
            .collect();
        walk(
            v,
            path.to_string(),
            &open,
            self,
            &mut |v, at| match noted.contains(&at) {
                true => Tree::Noted(leaf(v), SCHEMA_DEFAULT.into()),
                false => Tree::Leaf(leaf(v)),
            },
        )
    }
}

/// A deformation's lines being folded: their paths and writers, which
/// are the schema's defaults, the type's keyless sets, and each
/// contribution's document row.
pub(super) struct Folding<'a> {
    pub(super) p: &'a tree::Printer<'a>,
    pub(super) rules: &'a [RuleStmt],
    pub(super) why: Why,
    pub(super) sets: BTreeSet<String>,
    pub(super) lines: Vec<&'a Line>,
    pub(super) paths: Vec<String>,
    pub(super) writers: Vec<Option<crate::circuit::NodeId>>,
    /// The leaf no contribution wrote and the schema defaults (R-217).
    pub(super) noted: Vec<bool>,
    pub(super) rows: BTreeMap<crate::circuit::NodeId, Option<tree::DocRow>>,
}

impl Folding<'_> {
    /// The contribution `w` is said as its document's row.
    pub(super) fn in_row(&self, w: Option<crate::circuit::NodeId>) -> bool {
        w.and_then(|w| self.rows.get(&w))
            .is_some_and(Option::is_some)
    }

    /// A fold group's lines: a leaf of its own with its site found (a
    /// host with a label that is not ASCII keeps its own line, so its
    /// A-labels print beside it, R-134), a schema default with its note,
    /// an element of a set several writers add to named by itself (R-158),
    /// or the group's value laid out under its path, the defaults inside
    /// it with theirs (R-217).
    pub(super) fn group(
        &self,
        g: &Group,
        values: &[Tree],
        site: &mut dyn FnMut(&Line) -> Option<Site>,
    ) -> Vec<(Option<crate::circuit::NodeId>, Line)> {
        let (p, rules, why) = (self.p, self.rules, self.why);
        let (lines, paths, writers) = (&self.lines, &self.paths, &self.writers);
        let sets = &self.sets;
        let mut own = |i: usize| match self.noted[i] {
            true => self.default_line(i, values),
            false => Line {
                site: site(lines[i]),
                ..lines[i].clone()
            },
        };
        let host = |l: &Line| matches!(&l.after, Shown::Value(Json::String(s)) if crate::uri::ascii_form(s).is_some());
        if g.leaves.len() > 1 && g.leaves.iter().any(|&i| host(lines[i])) {
            return g
                .leaves
                .iter()
                .map(|&i| (writers[i], own(i)))
                .collect::<Vec<_>>();
        }
        let w = writers[g.leaves[0]];
        let first = lines[g.leaves[0]];
        // A value said as its document's row says no default inside it:
        // each is its own line after it.
        if g.leaves.len() > 1 && self.in_row(w) && g.leaves.iter().any(|&i| self.noted[i]) {
            let mine = Group {
                leaves: g
                    .leaves
                    .iter()
                    .copied()
                    .filter(|&i| !self.noted[i])
                    .collect(),
                ..g.clone()
            };
            let mut out = self.group(&mine, values, site);
            out.extend(
                g.leaves
                    .iter()
                    .filter(|&&i| self.noted[i])
                    .map(|&i| (None, self.default_line(i, values))),
            );
            return out;
        }
        let line = match (g.leaves.as_slice(), w) {
            ([i], None) if self.noted[*i] => self.default_line(*i, values),
            // A scalar element of a set several writers add to is
            // named by itself (R-158): `policies[app_policy]`.
            // An element several writers add to says its own writer's
            // site, which the attribute's winner does not.
            ([i], Some(w)) if g.path == paths[*i] && paths[*i].ends_with(']') => {
                let first = own(*i);
                let site = match first.site {
                    Some(s) => Some(s),
                    None => p.contribution_site(rules, w, &paths[*i]),
                };
                let path = match set_element(&paths[*i], sets) {
                    Some(list) if scalar(&first.after) => {
                        format!("{list}[{}]", first.after.said(why))
                    }
                    _ => first.path.clone(),
                };
                Line {
                    path,
                    site,
                    ..first
                }
            }
            ([i], _) if g.path == paths[*i] => match set_element(&paths[*i], sets) {
                Some(list) if scalar(&first.after) => Line {
                    path: format!("{list}[{}]", first.after.said(why)),
                    ..own(*i)
                },
                _ => own(*i),
            },
            (_, w) => Line {
                op: Op::Leaf,
                path: g.path.clone(),
                before: Shown::Absent,
                after: Shown::Absent,
                leaves: Vec::new(),
                site: w.and_then(|w| p.contribution_site(rules, w, &g.path)),
                chain: Vec::new(),
                value: (!self.in_row(w)).then(|| assemble(g, paths, values)),
                row: None,
            },
        };
        vec![(w, line)]
    }

    /// Leaf `i`, a schema's default, on its own line: its value and the
    /// note (R-217), `protocol = "TCP" (schema default)`.
    fn default_line(&self, i: usize, values: &[Tree]) -> Line {
        Line {
            site: None,
            value: Some(values[i].clone()),
            ..self.lines[i].clone()
        }
    }

    /// A document value (R-131): the leaves a contribution read whole from
    /// a loader's document are its row, said once; a value body's first,
    /// as the resource's. A leaf another write made stays its own line.
    pub(super) fn with_rows(&self, out: Vec<(Option<crate::circuit::NodeId>, Line)>) -> Vec<Line> {
        let rows = &self.rows;
        let mut said = BTreeSet::new();
        let mut body = Vec::new();
        let mut rest = Vec::new();
        for (w, l) in out {
            let row = w.and_then(|w| rows.get(&w).cloned().flatten());
            let Some(row) = row else {
                rest.push(l);
                continue;
            };
            let at = match row.body {
                true => String::new(),
                false => path(&row.path),
            };
            if !said.insert((at.clone(), row.at.clone())) {
                continue;
            }
            let line = Line {
                op: Op::Leaf,
                path: at,
                before: Shown::Absent,
                after: Shown::Absent,
                leaves: Vec::new(),
                site: l.site,
                chain: Vec::new(),
                value: None,
                row: Some(row.text()),
            };
            if row.body {
                body.push(line);
                continue;
            }
            // Before a leaf another write made inside it.
            let under = |x: &Line| {
                x.path
                    .strip_prefix(line.path.as_str())
                    .is_some_and(|r| r.starts_with(['.', '[']))
            };
            match rest.iter().position(under) {
                Some(i) => rest.insert(i, line),
                None => rest.push(line),
            }
        }
        body.extend(rest);
        body
    }
}

/// A leaf's value inside a folded one: as the line says it, a string
/// whole (no elision inside a laid-out value).
pub(super) fn whole(v: &Shown, why: Why) -> String {
    match v {
        Shown::Value(Json::String(s)) => spell::quote(s),
        v => v.said(why),
    }
}

/// The keyless sets of type `typ`, by schema path: each attribute
/// `type_attr` types `set(..)` or `type_lattice` declares a set, but a
/// list keyed by `type_list_key`.
pub(super) fn keyless_sets(typ: &str, all: &BTreeSet<Atom>) -> BTreeSet<String> {
    let mut keyed = BTreeSet::new();
    let mut sets = BTreeSet::new();
    for f in all {
        let (Some(Term::Val(Value::Str(t))), Some(Term::Val(Value::Str(p))), Some(Term::Val(k))) =
            (f.args.first(), f.args.get(1), f.args.get(2))
        else {
            continue;
        };
        if t != typ {
            continue;
        }
        match (f.pred.as_str(), k) {
            ("type_list_key", _) => {
                keyed.insert(p.clone());
            }
            ("type_lattice", Value::Str(l)) if l == "set" => {
                sets.insert(p.clone());
            }
            ("type_attr", Value::Str(ty)) if ty.split('(').next().map(str::trim) == Some("set") => {
                sets.insert(p.clone());
            }
            _ => {}
        }
    }
    &sets - &keyed
}

/// Where the element a set's update line adds was written (R-158), when
/// several writers add to the set: its writer's site. The line's path is
/// the set's, `policies[]`; the element is found in the program's value
/// by what it prints as.
pub(super) fn element_site(
    p: &tree::Printer,
    rules: &[RuleStmt],
    facts: &[&Atom],
    l: &Line,
) -> Option<Site> {
    let list = l.path.strip_suffix("[]")?;
    if l.op != Op::Add || !scalar(&l.after) {
        return None;
    }
    let (a, _, _) = attr_holding(facts, list)?;
    let Some(Term::Val(Value::List(xs))) = a.args.get(3) else {
        return None;
    };
    let want = l.after.said(Why::Line);
    let printed = |v: &Value| match v {
        Value::Ref { typ, name, attr } if attr.is_empty() => Shown::Ref {
            addr: Address {
                typ: typ.clone(),
                name: name.clone(),
            },
            value: Json::Null,
        }
        .said(Why::Line),
        Value::Str(s) => Shown::Value(Json::String(s.clone())).said(Why::Line),
        _ => String::new(),
    };
    let i = xs.iter().position(|x| printed(x) == want)?;
    let path = format!("{list}[{i}]");
    let w = p.writers(a, std::slice::from_ref(&path)).pop().flatten()?;
    p.contribution_site(rules, w, &path)
}

/// A value a set's element is named by (R-158): a string, a reference,
/// an unknown; a number would read as a position.
pub(super) fn scalar(v: &Shown) -> bool {
    matches!(
        v,
        Shown::Value(Json::String(_)) | Shown::Ref { .. } | Shown::Null { .. }
    )
}

/// The list of printed path `path` when it is an element of one of
/// `sets` and nothing below it: `policies` of `policies[2]`.
pub(super) fn set_element(path: &str, sets: &BTreeSet<String>) -> Option<String> {
    let list = path.strip_suffix(']')?.rsplit_once('[')?.0;
    (!list.contains('[') && sets.contains(&schema_path(list))).then(|| list.to_string())
}

/// A printed path's schema path: its keys, no selectors
/// (`spec.ports[name=web].protocol` is `spec.ports.protocol`).
pub(crate) fn schema_path(path: &str) -> String {
    tokens(path)
        .into_iter()
        .filter_map(|t| match t.step {
            Step::Key(k) => Some(crate::ir::segment_key(&k).into_owned()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(".")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(ps: &[&str]) -> Vec<String> {
        ps.iter().map(|p| p.to_string()).collect()
    }

    fn unnoted<W: PartialEq>(ps: &[String], w: &[Option<W>]) -> Vec<Group> {
        fold(ps, w, &vec![false; ps.len()])
    }

    #[test]
    fn one_writer_folds_where_the_writers_diverge() {
        let ps = paths(&[
            "metadata.labels.owner",
            "metadata.name",
            "spec.c[name=t].args[0]",
            "spec.c[name=t].args[1]",
            "spec.c[name=t].name",
            "spec.c[name=t].ports[name=web].name",
            "spec.c[name=t].ports[name=web].protocol",
        ]);
        let w = [Some(2), Some(3), Some(1), Some(1), Some(1), Some(1), None];
        let g = unnoted(&ps, &w);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(
            printed,
            [
                "metadata.labels.owner",
                "metadata.name",
                "spec.c[name=t]",
                "spec.c[name=t].ports[name=web].protocol",
            ]
        );
        let values: Vec<Tree> = (0..ps.len()).map(|i| Tree::Leaf(format!("v{i}"))).collect();
        assert_eq!(
            assemble(&g[2], &ps, &values),
            Tree::Obj(vec![
                (
                    "args".into(),
                    Tree::List(vec![Tree::Leaf("v2".into()), Tree::Leaf("v3".into())])
                ),
                ("name".into(), Tree::Leaf("v4".into())),
                (
                    "ports".into(),
                    Tree::List(vec![Tree::Obj(vec![(
                        "name".into(),
                        Tree::Leaf("v5".into())
                    )])])
                ),
            ])
        );
    }

    #[test]
    fn a_writer_folds_beside_another_in_the_same_object() {
        let ps = paths(&["tags.component", "tags.env", "tags.team"]);
        let g = unnoted(&ps, &[Some(1), Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["tags", "tags.team"]);
        // A value comes before a leaf another writer made inside it.
        let ps = paths(&[
            "metadata.labels.owner",
            "metadata.name",
            "metadata.namespace",
        ]);
        let g = unnoted(&ps, &[Some(2), Some(1), Some(1)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["metadata", "metadata.labels.owner"]);
    }

    #[test]
    fn a_list_several_writers_add_to_is_said_by_element() {
        let ps = paths(&[
            "statements[0].action",
            "statements[0].resource",
            "statements[1].action",
            "statements[1].resource",
            "statements[2].action",
        ]);
        let g = unnoted(&ps, &[Some(2), Some(2), Some(1), Some(1), Some(1)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(
            printed,
            ["statements[0]", "statements[1]", "statements[2].action"]
        );
    }

    #[test]
    fn a_list_of_one_element_one_writer_wrote_is_the_list() {
        let ps = paths(&["policies[0]", "tags.team"]);
        let g = unnoted(&ps, &[Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["policies", "tags.team"]);
        let values = [Tree::Leaf("p".into()), Tree::Leaf("t".into())];
        assert_eq!(
            assemble(&g[0], &ps, &values),
            Tree::List(vec![Tree::Leaf("p".into())])
        );
        // Another writer's element beside it: each by its own line.
        let g = unnoted(&paths(&["policies[0]", "policies[1]"]), &[Some(1), Some(2)]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["policies[0]", "policies[1]"]);
    }

    #[test]
    fn positions_order_numerically() {
        let ps = paths(&["a[10]", "a[2]"]);
        let g = unnoted(&ps, &[Some(1), Some(1)]);
        let values = [Tree::Leaf("ten".into()), Tree::Leaf("two".into())];
        assert_eq!(g[0].path, "a");
        assert_eq!(
            assemble(&g[0], &ps, &values),
            Tree::List(vec![Tree::Leaf("two".into()), Tree::Leaf("ten".into())])
        );
    }

    /// R-217: a schema's default is inside the value that holds it, after
    /// its own leaves, and makes no list one other writers add to; one no
    /// value holds is its own line.
    #[test]
    fn a_default_folds_into_the_value_that_holds_it() {
        let ps = paths(&[
            "metadata.name",
            "spec.ports[port=80,protocol=TCP].port",
            "spec.ports[port=80,protocol=TCP].protocol",
            "spec.ports[port=80,protocol=TCP].targetPort",
            "spec.selector.app",
            "spec.type",
        ]);
        let w = [Some(1), Some(2), None, Some(2), Some(2), None];
        let g = fold(&ps, &w, &[false, false, true, false, false, true]);
        let printed: Vec<(&str, &[usize])> = g
            .iter()
            .map(|g| (g.path.as_str(), g.leaves.as_slice()))
            .collect();
        assert_eq!(
            printed,
            [("metadata.name", &[0][..]), ("spec", &[1, 3, 4, 2, 5][..]),]
        );
        // Under a list by position, it is the list's still.
        let ps = paths(&["ports[0].port", "ports[0].protocol", "ports[1].port"]);
        let g = fold(&ps, &[Some(1), None, Some(1)], &[false, true, false]);
        assert_eq!(g.len(), 1);
        assert_eq!(
            (g[0].path.as_str(), g[0].leaves.as_slice()),
            ("ports", &[0, 2, 1][..])
        );
        // No value holds it: its own line.
        let ps = paths(&["name", "protocol"]);
        let g = fold(&ps, &[Some(1), None], &[false, true]);
        let printed: Vec<&str> = g.iter().map(|g| g.path.as_str()).collect();
        assert_eq!(printed, ["name", "protocol"]);
    }

    #[test]
    fn leaves_no_contribution_wrote_stay_their_own() {
        let ps = paths(&["a.b", "a.c"]);
        let g = unnoted::<u32>(&ps, &[None, None]);
        assert_eq!(g.len(), 2);
    }
}
