//! A value's provenance chain (R-122): how the value was made, each step
//! `= EXPR   SITE` from where it was written through the cells that passed it on,
//! and the writes it beat; a contribution's part followed the same way.

use super::compress::Compress;
use super::printer::{Focus, Printer};
use super::sites::Site;
use super::statement::is_check;
use super::surface::Surface;
use crate::ast::{Atom, RuleStmt};
use crate::circuit::{Circuit, Fact, Leaf, NodeId, View};
use crate::engine;
use crate::spell;
use crate::syntax::SyntaxKind;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

impl Surface<'_, '_> {
    /// [`winner_site`], each step kept: the winning contribution's chain,
    /// and each one it beat into `lost`.
    fn winner_chain(
        &mut self,
        c: &mut Compress,
        id: NodeId,
        focus: Option<&Focus>,
        depth: usize,
        w: &mut Chain,
    ) {
        let circuit = self.p.circuit;
        let View::Fact { alts, .. } = circuit.view(id) else {
            return;
        };
        let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a)) else {
            return;
        };
        let View::Times { children, .. } = circuit.view(alt) else {
            return;
        };
        let aggregate = children.iter().any(
            |ch| matches!(circuit.view(*ch), View::Leaf(Leaf::Rule { id }) if id.starts_with('Σ')),
        );
        if !aggregate {
            return self.value_chain(c, id, focus, None, depth, w);
        }
        let part = |v: &Value| -> Option<Value> {
            let mut at = v;
            for k in focus.map(|f| f.keys.as_slice()).unwrap_or_default() {
                let Value::Obj(m) = at else { return None };
                at = m.get(k)?;
            }
            Some(at.clone())
        };
        let mut contributions: Vec<(NodeId, &Fact)> = children
            .iter()
            .filter_map(|ch| match circuit.view(*ch) {
                View::Fact { fact, .. } if fact.pred == "arg" && !is_check(fact) => {
                    Some((*ch, fact))
                }
                _ => None,
            })
            .filter(|(_, f)| f.args.get(3).and_then(part).is_some())
            .collect();
        contributions.sort_by_key(|(_, f)| std::cmp::Reverse(rank_of(f).0));
        let Some(&(win, fact)) = contributions.first() else {
            return;
        };
        let (top, rank) = rank_of(fact);
        let won = fact.args.get(3).and_then(part);
        // What it beat: a lower rank's value, where it differs. An object's
        // contributions at the winning rank merge; none of them lost.
        for &(l, f) in &contributions[1..] {
            let v = f.args.get(3).and_then(part);
            if rank_of(f).0 == top
                || v == won
                || matches!(v, Some(Value::Obj(_)))
                || placeholder(f, v.as_ref())
            {
                continue;
            }
            if let (Some(site), Some(v)) = (self.site_of(c, l, depth + 1), v) {
                let r = rank_of(f).1;
                w.lost.push(Step {
                    expr: match f.args.get(3) {
                        Some(whole) => crate::report::surface_in(
                            self.p.redact,
                            whole,
                            focus.map(|f| f.keys.as_slice()).unwrap_or_default(),
                            &v,
                        ),
                        None => self.p.redact.surface(&v),
                    },
                    at: place(&site),
                    with: Vec::new(),
                    rank: (r != "normal").then(|| r.to_string()),
                    lost: true,
                });
            }
        }
        let rank = (rank != "normal").then_some(rank);
        self.value_chain(c, win, focus, rank, depth, w);
    }

    /// [`value_site`], each step kept: the expression that wrote fact `id`,
    /// then the chain of the input or `let` it reads.
    fn value_chain(
        &mut self,
        c: &mut Compress,
        id: NodeId,
        focus: Option<&Focus>,
        rank: Option<&str>,
        depth: usize,
        w: &mut Chain,
    ) {
        let circuit = self.p.circuit;
        let View::Fact { fact, alts, .. } = circuit.view(id) else {
            return;
        };
        // A name dform gave a replacement (R-189): state's, not a line of
        // the program's.
        if let Some((program, name)) = generated_name(circuit, alts) {
            w.out.push(Step {
                expr: self.p.redact.surface(&name),
                at: format!(
                    "dform's name for a replacement of {}",
                    spell::value(&program)
                ),
                with: Vec::new(),
                rank: rank.map(str::to_string),
                lost: false,
            });
            return;
        }
        let Some(own) = self.site_of(c, id, depth) else {
            return;
        };
        let written = self.written_step(fact, &own, focus);
        w.out.push(Step {
            expr: written.expr,
            at: place(&own),
            with: own.with.clone(),
            rank: rank.map(str::to_string),
            lost: false,
        });
        if depth >= FOLLOW {
            return;
        }
        let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a)) else {
            return;
        };
        let View::Times { children, .. } = circuit.view(alt) else {
            return;
        };
        let next = passed_on(c, circuit, children, written.value, written.rhs.as_deref());
        // A stack key ends it: the deployment line says its value.
        let key = |p: NodeId| match circuit.view(p) {
            View::Fact { fact, .. } => {
                fact.args.first().and_then(Value::as_str) == Some("input")
                    && fact.args.get(1).and_then(Value::as_str) == Some("")
                    && fact
                        .args
                        .get(2)
                        .and_then(Value::as_str)
                        .is_some_and(|n| w.keys.contains(n))
            }
            _ => false,
        };
        if let Some((p, keys)) = next.filter(|(p, _)| !key(*p)) {
            let focus = (!keys.is_empty()).then_some(Focus::from(keys));
            self.winner_chain(c, p, focus.as_ref(), depth + 1, w);
        }
    }

    /// What wrote fact `fact` at its site `own`, as a step says it: the
    /// entry's right-hand side (a call's let's value), the focused part
    /// of it, masked where it is a secret's, else the value.
    fn written_step<'f>(&self, fact: &'f Fact, own: &Site, focus: Option<&Focus>) -> Written<'f> {
        let keys = focus.map(|f| f.keys.as_slice()).unwrap_or_default();
        let mut value = match fact.pred.name() {
            "arg" | "attr" => fact.args.get(3),
            _ => None,
        };
        let mut rhs = own
            .entry
            .as_deref()
            .map(|e| e.split_once(" = ").map_or(e, |(_, r)| r).to_string());
        // A call's row is the let's value, as it is written.
        if rhs.is_none() && is_call(fact) {
            rhs = let_value(&own.statement);
        }
        if let (false, Some(e)) = (keys.is_empty(), rhs.as_deref()) {
            rhs = field_of(e, keys).or_else(|| shorthand(e, keys));
        }
        if !keys.is_empty() {
            value = keys
                .iter()
                .try_fold(value, |v, k| match v {
                    Some(Value::Obj(m)) => Some(m.get(k)),
                    _ => None,
                })
                .flatten();
        }
        let expr = self.step_expr(fact, own, keys, value, rhs.as_deref());
        Written { expr, value, rhs }
    }

    /// A step's expression: the right-hand side, masked when the value is
    /// a secret's or a part of one (R-124 amendment 2); else the value (a
    /// plain leaf of a secret object `(sensitive)` too), else the
    /// statement.
    fn step_expr(
        &self,
        fact: &Fact,
        own: &Site,
        keys: &[String],
        value: Option<&Value>,
        rhs: Option<&str>,
    ) -> String {
        let redact = self.p.redact;
        let whole = match fact.pred.name() {
            "arg" | "attr" => fact.args.get(3),
            _ => None,
        };
        let secret = match (value, whole) {
            (Some(v), Some(whole)) => {
                redact.is_secret(v)
                    || crate::report::surface_in(redact, whole, keys, v) == "(sensitive)"
            }
            (Some(v), None) => redact.is_secret(v),
            _ => false,
        };
        match (rhs, value) {
            (Some(r), _) if secret => crate::report::masked(r),
            (Some(r), _) => r.to_string(),
            (None, Some(v)) => match whole {
                Some(whole) => crate::report::surface_in(redact, whole, keys, v),
                None => redact.surface(v),
            },
            (None, None) => own.statement.clone(),
        }
    }
}

/// What a step of a chain wrote: its expression, its value, and the
/// right-hand side it reads the next cell by.
struct Written<'f> {
    expr: String,
    value: Option<&'f Value>,
    rhs: Option<String>,
}

/// The cell that passed a step's value on among the firing's `children`:
/// one the right-hand side `rhs` reads, when it is a path (`config.region`)
/// or an expression of one cell (`str.lower(cidrs.main)`), or the row a
/// call's site answered; with the keys below it.
fn passed_on(
    c: &mut Compress,
    circuit: &Circuit,
    children: &[NodeId],
    value: Option<&Value>,
    rhs: Option<&str>,
) -> Option<(NodeId, Vec<String>)> {
    let read: Option<Vec<String>> = rhs.and_then(|rhs| {
        let plain = !rhs.is_empty()
            && !rhs.contains(|c: char| c.is_whitespace() || "()[]{}$,\"".contains(c));
        plain.then(|| crate::address::path_keys(rhs))
    });
    let next = value.and_then(|v| passed_cell(c, circuit, children, v, read.as_deref()));
    next.or_else(|| read_cell(c, circuit, children, rhs?))
        .or_else(|| called(circuit, children))
}

/// The keys of printed path `path` below contribution `fact`'s own path,
/// up to a list: the part of its value the path names.
pub(super) fn contribution_focus(fact: &Fact, path: &str) -> Option<Focus> {
    let at = fact.args.get(2).and_then(Value::as_str).unwrap_or_default();
    let skip = match at.ends_with(crate::transform::ELEM) {
        true => usize::MAX,
        false => crate::address::tokens(at).len(),
    };
    let keys: Vec<String> = crate::address::tokens(path)
        .into_iter()
        .skip(skip)
        .map_while(|t| match t.step {
            crate::address::Step::Key(k) => Some(crate::address::segment_key(&k).into_owned()),
            _ => None,
        })
        .collect();
    (!keys.is_empty()).then_some(Focus::from(keys))
}

/// Whether contribution `f` (an `arg/5`) holds the leaf at printed path
/// `toks`: its path is a prefix, and its value has the rest.
pub(super) fn holds<'v>(
    memo: &mut crate::report::fold::Reached<'v, 'v>,
    f: &'v Fact,
    toks: &[crate::address::Tok],
    merged: &'v Value,
    top: usize,
) -> bool {
    use crate::address::Step;
    // The merged value where the contribution's path ends: a list's
    // element is found in a contribution by its value, not its position
    // in the merged list.
    let merged_at = |n: usize| crate::report::fold::reach(merged, &toks[top.min(n)..n]);
    let (Some(at), Some(v)) = (f.args.get(2).and_then(Value::as_str), f.args.get(3)) else {
        return false;
    };
    let key = |s: &Step| match s {
        Step::Key(k) => Some(crate::address::segment_key(k).into_owned()),
        _ => None,
    };
    let prefix = |p: &str| -> Option<usize> {
        let ptoks = crate::address::tokens(p);
        let same = ptoks.len() <= toks.len()
            && ptoks
                .iter()
                .zip(toks)
                .all(|(a, b)| key(&a.step).is_some() && key(&a.step) == key(&b.step));
        same.then_some(ptoks.len())
    };
    // An element write: `[K, V]` into the element of list `L` keyed `K`.
    if let Some(list) = at.strip_suffix(crate::transform::ELEM) {
        let (Some(n), Value::List(kv)) = (prefix(list), v) else {
            return false;
        };
        let ([k, content], Some(Step::Keyed(pairs))) =
            (kv.as_slice(), toks.get(n).map(|t| &t.step))
        else {
            return false;
        };
        let matches = match k {
            Value::Obj(_) => crate::report::fold::keyed(k, pairs),
            k => matches!(pairs.as_slice(), [(_, want)] if spell::bare(k) == *want),
        };
        let elem = merged_at(n + 1);
        return matches
            && crate::report::fold::reach_along(content, elem, &toks[n + 1..]).is_some();
    }
    match prefix(at) {
        Some(n) => {
            crate::report::fold::reach_along_memo(memo, v, merged_at(n), &toks[n..]).is_some()
        }
        None => false,
    }
}

/// The `remote_name(T, A, Program, Name)` fact a contribution was derived
/// from, when it was (`transform::remote_name_prelude`): the program's
/// name and the one dform gave the object.
fn generated_name(circuit: &Circuit, alts: &[NodeId]) -> Option<(Value, Value)> {
    alts.iter().find_map(|a| {
        let View::Times { children, .. } = circuit.view(*a) else {
            return None;
        };
        children.iter().find_map(|ch| match circuit.view(*ch) {
            View::Fact { fact, .. } if fact.pred == crate::transform::REMOTE_NAME => {
                Some((fact.args.get(2)?.clone(), fact.args.get(3)?.clone()))
            }
            _ => None,
        })
    })
}

/// How deep a value is followed through cells that pass it on.
pub(super) const FOLLOW: usize = 8;

/// The rank of a contribution, `arg/5`'s last column.
pub(super) fn rank_of(f: &Fact) -> (u8, &str) {
    match f.args.get(4).and_then(Value::as_str) {
        Some("override") => (2, "override"),
        Some("default") => (0, "default"),
        _ => (1, "normal"),
    }
}

/// One step of a value's provenance chain (R-122), `= EXPR   SITE`: the
/// expression that wrote the value as the source writes it, where, the
/// clause's bindings and the rank it won at; or a contribution it beat
/// (`lost`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Step {
    pub expr: String,
    /// `file:line`, or the flag that gave the value (`--set env=prod`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub at: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub with: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rank: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub lost: bool,
}

/// A chain being walked: its steps, the writes they beat, and the stack
/// keys that end it.
struct Chain<'a> {
    pub(super) out: &'a mut Vec<Step>,
    lost: &'a mut Vec<Step>,
    pub(super) keys: &'a BTreeSet<String>,
}

/// Whether `v`, contribution `f`'s value (or the part of it asked
/// about), is the engine's own open null for an attribute of the resource
/// it contributes to: a placeholder the program's write replaces, never a
/// write it beat (R-124).
pub(super) fn placeholder(f: &Fact, v: Option<&Value>) -> bool {
    let own = |label: &str| {
        crate::value::null_owner(label).is_some_and(|(t, a)| {
            f.args.first().and_then(Value::as_str) == Some(t.as_str())
                && f.args.get(1).and_then(Value::as_str) == Some(a.as_str())
        })
    };
    fn nulls<'v>(v: &'v Value, out: &mut Vec<&'v str>) -> bool {
        match v {
            Value::Null { label, .. } => {
                out.push(label);
                true
            }
            Value::Obj(m) => !m.is_empty() && m.values().all(|x| nulls(x, out)),
            _ => false,
        }
    }
    let mut labels = Vec::new();
    v.is_some_and(|v| nulls(v, &mut labels)) && labels.iter().all(|l| own(l))
}

/// A site's place: `file:line`, or the flag that gave the value.
pub(super) fn place(site: &Site) -> String {
    match site.at.is_empty() {
        true => site.statement.clone(),
        false => site.at.clone(),
    }
}

/// The one input or `let` cell among a firing's `children` that
/// expression `e` reads, with the keys below it; `None` when it reads
/// none, or several.
fn read_cell(
    c: &mut Compress,
    circuit: &Circuit,
    children: &[NodeId],
    e: &str,
) -> Option<(NodeId, Vec<String>)> {
    let paths: Vec<Vec<String>> = code_of(e)
        .split(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.'))
        .filter(|t| t.starts_with(|ch: char| ch.is_alphabetic() || ch == '_'))
        .map(crate::address::path_keys)
        .collect();
    let mut cells: Vec<NodeId> = children.to_vec();
    for ch in children {
        let View::Fact { fact, alts, .. } = circuit.view(*ch) else {
            continue;
        };
        if matches!(fact.pred.name(), "attr" | "arg" | "want") {
            continue;
        }
        if let Some(&alt) = alts.iter().min_by_key(|a| c.size(circuit, **a))
            && let View::Times { children, .. } = circuit.view(alt)
        {
            cells.extend(children.iter().copied());
        }
    }
    let mut found: Vec<(NodeId, Vec<String>)> = Vec::new();
    for id in cells {
        for p in &paths {
            if let Some(rest) = cell_reads(circuit, id, p)
                && !found.iter().any(|(x, _)| *x == id)
            {
                found.push((id, rest));
            }
        }
    }
    match found.as_slice() {
        [one] => Some(one.clone()),
        _ => None,
    }
}

/// The row a call's site answered among `children`, when the firing read
/// one ([`is_call`]).
fn called(circuit: &Circuit, children: &[NodeId]) -> Option<(NodeId, Vec<String>)> {
    children.iter().find_map(|ch| match circuit.view(*ch) {
        View::Fact { fact, .. } if is_call(fact) => Some((*ch, Vec::new())),
        _ => None,
    })
}

/// Whether `fact` is a row a call answered: a let with parameters is
/// answered by its copy for the call's site (`crate::demand`), whose
/// relation is named for the call; not the call's demand (`f?`).
fn is_call(fact: &Fact) -> bool {
    fact.pred
        .name()
        .rsplit_once("::")
        .is_some_and(|(scope, f)| scope.starts_with("call at ") && !f.ends_with('?'))
}

/// The value a let with parameters is written as, `"10.${a}.${b}.0/24"`
/// of `let cidr(a, b) = "10.${a}.${b}.0/24" where ..`: what a call of it
/// makes.
fn let_value(statement: &str) -> Option<String> {
    let root = crate::syntax::parser::parse(statement).syntax();
    let l = root.descendants().find(|n| n.kind() == SyntaxKind::LET)?;
    let value = l
        .children_with_tokens()
        .skip_while(|e| e.kind() != SyntaxKind::EQ)
        .skip(1)
        .find(|e| !e.kind().is_trivia())?;
    Some(value.to_string().trim().to_string())
}

/// Expression `e` without its string literals' text, their
/// interpolations kept: what it reads (`name` of `"${name}-gke"`).
fn code_of(e: &str) -> String {
    let mut out = String::new();
    let (mut quoted, mut depth, mut escaped) = (false, 0usize, false);
    let mut chars = e.chars().peekable();
    while let Some(ch) = chars.next() {
        if quoted && depth == 0 {
            match ch {
                _ if escaped => escaped = false,
                '\\' => escaped = true,
                '"' => {
                    quoted = false;
                    out.push(' ');
                }
                '$' if chars.peek() == Some(&'{') => {
                    chars.next();
                    depth = 1;
                    out.push(' ');
                }
                _ => {}
            }
            continue;
        }
        match ch {
            '"' if depth == 0 => quoted = true,
            '{' if depth > 0 => depth += 1,
            '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(' ');
                    continue;
                }
            }
            _ => {}
        }
        out.push(ch);
    }
    out
}

/// When fact `id` is an input or `let` cell and `read` names it (or a
/// path into it), the keys of `read` below the cell.
fn cell_reads(circuit: &Circuit, id: NodeId, read: &[String]) -> Option<Vec<String>> {
    let View::Fact { fact: f, .. } = circuit.view(id) else {
        return None;
    };
    if f.pred != "attr"
        || !matches!(
            f.args.first().and_then(Value::as_str),
            Some("input" | "let")
        )
    {
        return None;
    }
    let name = crate::address::path_keys(f.args.get(2)?.as_str()?);
    let scoped: Vec<String> = match f.args.get(1)?.as_str()? {
        "" => Vec::new(),
        scope => scope
            .split('.')
            .map(str::to_string)
            .chain(name.clone())
            .collect(),
    };
    read.strip_prefix(name.as_slice())
        .or_else(|| {
            read.strip_prefix(scoped.as_slice())
                .filter(|_| !scoped.is_empty())
        })
        .map(<[String]>::to_vec)
}

/// The variable a shorthand field of object literal `e` reads at `key`,
/// `env` of `{ env, component: "network" }`.
pub(super) fn shorthand(e: &str, keys: &[String]) -> Option<String> {
    let [k] = keys else { return None };
    let parse = crate::syntax::parser::parse_term(e.trim());
    let object = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::OBJECT)?;
    object
        .children()
        .filter(|n| n.kind() == SyntaxKind::OBJECT_FIELD)
        .map(|f| f.text().to_string().trim().to_string())
        .find(|t| t == k)
}

/// The expression object literal `e` gives at `keys` (`config.domain` of
/// `{ d: config.domain }` at `d`); `e` itself with no keys.
pub(super) fn field_of(e: &str, keys: &[String]) -> Option<String> {
    let Some((k, rest)) = keys.split_first() else {
        return Some(e.to_string());
    };
    let parse = crate::syntax::parser::parse_term(e.trim());
    let object = parse
        .syntax()
        .descendants()
        .find(|n| n.kind() == SyntaxKind::OBJECT)?;
    let value = object
        .children()
        .filter(|n| n.kind() == SyntaxKind::OBJECT_FIELD)
        .find_map(|f| {
            let key = f
                .children_with_tokens()
                .filter_map(|e| e.into_token())
                .find(|t| !t.kind().is_trivia())?;
            let key = key.text();
            let key = key
                .strip_prefix('"')
                .and_then(|k| k.strip_suffix('"'))
                .unwrap_or(key);
            let value = f.children().next()?;
            (key == k).then(|| value.text().to_string())
        })?;
    field_of(&value, rest)
}

/// The input or `let` cell among a firing's `children` that passes on
/// `v`: read directly (`attr("input", .., v)`), or through the relation
/// the compiler reads a cell by (`kubernetes::nodepool_max(3)`); an
/// object cell when the entry reads `v` at a path of it (`read`:
/// `gcp.project_id`), with the keys below the cell.
pub(super) fn passed_cell(
    c: &mut Compress,
    circuit: &Circuit,
    children: &[NodeId],
    v: &Value,
    read: Option<&[String]>,
) -> Option<(NodeId, Vec<String>)> {
    let cell = |id: NodeId| -> Option<Vec<String>> {
        let View::Fact { fact: f, .. } = circuit.view(id) else {
            return None;
        };
        if f.pred != "attr"
            || !matches!(
                f.args.first().and_then(Value::as_str),
                Some("input" | "let")
            )
        {
            return None;
        }
        // Only the cell the entry reads: another one the firing reads may
        // hold the same value by chance (`public = false` beside a
        // `multi_az` that is false).
        let at = f.args.get(3)?;
        // `gcp.project_id` of the cell `gcp`: the keys after its name;
        // a used module's item by its scope too, `config.zone` (R-111).
        let name = crate::address::path_keys(f.args.get(2)?.as_str()?);
        let scoped: Vec<String> = match f.args.get(1)?.as_str()? {
            "" => Vec::new(),
            scope => scope
                .split('.')
                .map(str::to_string)
                .chain(name.clone())
                .collect(),
        };
        let read = read?;
        let rest = read.strip_prefix(name.as_slice()).or_else(|| {
            read.strip_prefix(scoped.as_slice())
                .filter(|_| !scoped.is_empty())
        })?;
        if rest.is_empty() {
            return (at == v).then(Vec::new);
        }
        let mut x = at;
        for k in rest {
            let Value::Obj(m) = x else { return None };
            x = m.get(k)?;
        }
        (x == v).then(|| rest.to_vec())
    };
    if let Some(found) = children.iter().find_map(|ch| cell(*ch).map(|k| (*ch, k))) {
        return Some(found);
    }
    children.iter().find_map(|ch| {
        let View::Fact { fact, alts, .. } = circuit.view(*ch) else {
            return None;
        };
        if matches!(fact.pred.name(), "attr" | "arg" | "want") {
            return None;
        }
        let alt = *alts.iter().min_by_key(|a| c.size(circuit, **a))?;
        let View::Times { children, .. } = circuit.view(alt) else {
            return None;
        };
        children.iter().find_map(|x| cell(*x).map(|k| (*x, k)))
    })
}

impl Printer<'_> {
    /// The chain of the part at printed path `path` of contribution `id`
    /// ([`Printer::writers`]), as [`Printer::attr_chain`] says a value's.
    pub fn contribution_chain(
        &self,
        rules: &[RuleStmt],
        id: NodeId,
        path: &str,
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let View::Fact { fact, .. } = self.circuit.view(id) else {
            return Vec::new();
        };
        let focus = contribution_focus(fact, path);
        self.chain(rules, id, focus.as_ref(), stack_keys)
    }
}

impl Printer<'_> {
    /// The chain of the winning value of attribute fact `attr` (an
    /// `attr/4`), at `keys` below it when they name part of an object:
    /// one step per expression it passed through, following only what
    /// each reads, until a literal, a key or a provider's value; then one
    /// step per contribution it beat. Empty when no statement wrote it.
    /// A stack key in `stack_keys` ends it: the deployment line has its
    /// value.
    pub fn attr_chain(
        &self,
        rules: &[RuleStmt],
        attr: &Atom,
        keys: &[String],
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let Some(id) = self.circuit.fact_id(&engine::circuit_fact(attr)) else {
            return Vec::new();
        };
        let focus = (!keys.is_empty()).then(|| Focus::from(keys.to_vec()));
        self.chain(rules, id, focus.as_ref(), stack_keys)
    }

    /// The chain of fact node `id`'s value ([`Printer::attr_chain`]).
    pub fn chain(
        &self,
        rules: &[RuleStmt],
        id: NodeId,
        focus: Option<&Focus>,
        stack_keys: &BTreeSet<String>,
    ) -> Vec<Step> {
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        let (mut out, mut lost) = (Vec::new(), Vec::new());
        let mut w = Chain {
            out: &mut out,
            lost: &mut lost,
            keys: stack_keys,
        };
        s.winner_chain(&mut c, id, focus, 0, &mut w);
        // A binding is a step's when its expression reads it and no next
        // step names its value (`= zone_index[z]  with z = "a"`); a key's
        // value is the deployment line's.
        let last = out.len().saturating_sub(1);
        for (i, step) in out.iter_mut().enumerate() {
            let reads: BTreeSet<&str> = step
                .expr
                .split(|ch: char| !(ch.is_alphanumeric() || ch == '_'))
                .collect();
            step.with.retain(|w| {
                let name = w.split(" = ").next().unwrap_or_default();
                i == last
                    && reads.contains(name)
                    && step.expr.trim() != name
                    && !stack_keys.contains(name)
            });
        }
        out.extend(lost);
        out
    }
}
