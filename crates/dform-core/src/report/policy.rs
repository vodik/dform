//! The policy block (R-200): each policy of the program (a deny, a
//! `check` a value waits on) is a rule, so it prints as a resource does:
//! one line, a mark, its text and its site, what it is about nested
//! under it. A policy's line is a tally over what it ranges over (`3
//! hold · 1 fails`), its mark the worst of them: one failure makes a
//! failing policy. Under it only what does not hold, each with why, and
//! the rest as one `N hold` line; holding policies are the block's count
//! (`-v` lists everything). The policies a plan cannot decide yet are
//! gathered here too (`policies`, `deferred`: an undetermined deny, one that
//! may derive after a boundary, a refinement the value cannot decide), and a
//! deny over the plan with the row it matched (`denied`).

use super::errors::violation_parts;
use super::labels::{address, attribute, label};
use super::layout::{Row, layout_aligned};
use super::style::{Paint, Style};
use super::tree;
use super::tree::Site;
use super::waits::{Resolves, Until, nulls, until_text};
use super::{Input, Why};
use crate::ast::{Atom, Lit, Program, RuleStmt, Stmt, Term};
use crate::engine::EvalResult;
use crate::ir::Address;
use crate::query::Redactor;
use crate::spell;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// What one policy says of the plan, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    Fails,
    Undetermined,
    Holds,
}

impl Mark {
    /// The word in the mark's column.
    pub fn word(&self) -> &'static str {
        match self {
            Mark::Fails => "fails",
            Mark::Undetermined => "undetermined",
            Mark::Holds => "holds",
        }
    }

    fn paint(&self) -> Paint {
        match self {
            Mark::Fails => Paint::Error,
            Mark::Undetermined => Paint::Dim,
            Mark::Holds => Paint::Create,
        }
    }
}

/// One policy's line and what is under it.
#[derive(Debug, Clone)]
pub struct Line {
    /// Its text: a deny's message, a check's condition.
    pub text: String,
    /// Where it is written, `FILE:LINE`.
    pub at: String,
    /// What holds: the resources it ranges over that it holds for (their
    /// addresses, when it ranges over a type), else how many.
    pub holds: Vec<String>,
    pub hold: usize,
    /// Each failure: the resource it is of, when its context names one
    /// of those the policy ranges over, and the context (`workload =
    /// "web"`).
    pub fails: Vec<(Option<String>, String)>,
    /// Each thing it is undetermined for, and until what is known
    /// (`k8s.stateful_set db`, `until spec.template.spec.securityContext
    /// is known (tick 2)`).
    pub undetermined: Vec<(String, String)>,
}

impl Line {
    /// The worst of what it says.
    pub fn mark(&self) -> Mark {
        match (self.fails.is_empty(), self.undetermined.is_empty()) {
            (false, _) => Mark::Fails,
            (true, false) => Mark::Undetermined,
            (true, true) => Mark::Holds,
        }
    }

    /// `3 hold · 1 fails · 2 undetermined`, the parts there are.
    pub fn tally(&self) -> String {
        tally_text(
            self.hold,
            self.fails.len(),
            self.undetermined
                .iter()
                .map(|(s, _)| s)
                .collect::<BTreeSet<_>>()
                .len(),
        )
    }
}

/// `12 hold · 1 fails · 2 undetermined`: the parts that are not zero, or
/// `0 hold` for none.
pub fn tally_text(hold: usize, fails: usize, undetermined: usize) -> String {
    let parts: Vec<String> = [
        (hold, "hold"),
        (fails, "fails"),
        (undetermined, "undetermined"),
    ]
    .into_iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, w)| format!("{n} {w}"))
    .collect();
    match parts.is_empty() {
        true => "0 hold".to_string(),
        false => parts.join(" · "),
    }
}

/// The block's count: how many policies hold, fail, are undetermined.
pub fn count(lines: &[Line]) -> (usize, usize, usize) {
    let n = |m: Mark| lines.iter().filter(|l| l.mark() == m).count();
    (n(Mark::Holds), n(Mark::Fails), n(Mark::Undetermined))
}

/// The program's policies over the evaluation `res`: each deny statement
/// with a message, and each check a value waits on (`undetermined`, the
/// report's [`Policy`] rows, a deny's own and a refinement's).
pub fn lines(
    program: &Program,
    res: &EvalResult,
    undetermined: &[Policy],
    r: &Redactor,
) -> Vec<Line> {
    let wanted = Wanted::new(res);
    // Each deny by its message: where it is first written, and what it
    // ranges over. A module's the stack uses and a component's it copies
    // are the stack's own: the plan carries what they deny.
    let mut out: Vec<Line> = Vec::new();
    let mut ranges: Vec<BTreeSet<Range>> = Vec::new();
    for (rule, component) in written(program) {
        let [Term::Val(Value::Str(message)), ..] = rule.head.args.as_slice() else {
            continue;
        };
        if rule.head.span.is_none() || conflict(message) {
            continue;
        }
        let i = match out.iter().position(|l| l.text == *message) {
            Some(i) => i,
            None => {
                out.push(Line {
                    text: message.clone(),
                    at: crate::diag::location(rule.head.span)
                        .map(|(f, l, _)| format!("{f}:{l}"))
                        .unwrap_or_default(),
                    holds: Vec::new(),
                    hold: 0,
                    fails: Vec::new(),
                    undetermined: Vec::new(),
                });
                ranges.push(BTreeSet::new());
                out.len() - 1
            }
        };
        if let Some((typ, var)) = ranged(&rule.body) {
            ranges[i].insert(Range {
                typ,
                var,
                component: component.map(str::to_string),
            });
        }
    }
    // A check a value waits on is a policy of its own, written where its
    // type's check is.
    let checks = check_sites(program, res);
    for p in undetermined.iter().filter(|p| p.refinement) {
        if !out.iter().any(|l| l.text == p.message) {
            let at = p.on.iter().find_map(|n| {
                let (typ, _, path) = crate::value::null_parts(n)?;
                checks.get(&(typ, path)).cloned()
            });
            out.push(Line {
                text: p.message.clone(),
                at: at
                    .or_else(|| p.site.as_ref().map(|s| s.at.clone()))
                    .unwrap_or_default(),
                holds: Vec::new(),
                hold: 0,
                fails: Vec::new(),
                undetermined: Vec::new(),
            });
            ranges.push(BTreeSet::new());
        }
    }
    for a in res.facts.iter().filter(|a| a.pred == "deny") {
        let Some(Term::Val(Value::Str(message))) = a.args.first() else {
            continue;
        };
        let Some(i) = out.iter().position(|l| l.text == *message) else {
            continue;
        };
        let l = &mut out[i];
        let text = match a.args.get(1) {
            Some(Term::Val(ctx)) => format!("{message} ctx={}", crate::spell::value_to_json(ctx)),
            _ => message.clone(),
        };
        let (_, bindings) = violation_parts(&text, r);
        let of = match a.args.get(1) {
            Some(Term::Val(ctx)) => subject_in(ctx, &wanted.subjects(&ranges[i])),
            _ => None,
        };
        l.fails.push((of, bindings.join(", ")));
    }
    for p in undetermined {
        let Some(i) = out.iter().position(|l| l.text == p.message) else {
            continue;
        };
        let l = &mut out[i];
        if l.at.is_empty()
            && let Some(s) = &p.site
        {
            l.at = s.at.clone();
        }
        let subjects = wanted.subjects(&ranges[i]);
        let bound = p.bound_in(&ranges[i], &subjects);
        l.undetermined.extend(waits(p, &subjects, &bound, &wanted));
    }
    for (l, types) in out.iter_mut().zip(&ranges) {
        l.fails.sort();
        l.fails.dedup();
        l.undetermined = by_subject(std::mem::take(&mut l.undetermined));
        let subjects: Vec<String> = wanted.subjects(types).iter().map(address).collect();
        let undetermined: BTreeSet<&String> = l.undetermined.iter().map(|(s, _)| s).collect();
        let failed: BTreeSet<&String> = l.fails.iter().filter_map(|(s, _)| s.as_ref()).collect();
        let open = l.fails.len() + undetermined.len();
        match types.is_empty() {
            // About the deployment as a whole: it holds, or it does not.
            true => l.hold = usize::from(open == 0),
            false => {
                l.holds = subjects
                    .into_iter()
                    .filter(|s| !undetermined.contains(s) && !failed.contains(s))
                    .collect();
                // A failure of no resource it ranges over is one of them.
                let unnamed = l.fails.iter().filter(|(s, _)| s.is_none()).count();
                l.hold = l.holds.len().saturating_sub(unnamed);
            }
        }
    }
    out.sort_by(|a, b| (a.mark(), &a.text).cmp(&(b.mark(), &b.text)));
    out
}

/// Where each type's check on an attribute is written, `FILE:LINE`, by
/// (type, path): the program's rules that refuse a value violating it.
fn check_sites(program: &Program, res: &EvalResult) -> BTreeMap<(String, String), String> {
    let mut out = BTreeMap::new();
    // A checkable one: `type_refine(T, Path, C)`, `attr_refine(T, A,
    // Path, C)`, where its `check` is written.
    for a in res
        .facts
        .iter()
        .filter(|a| a.pred == crate::refine::TYPE_REFINE || a.pred == crate::refine::ATTR_REFINE)
    {
        if let Some(Term::Val(Value::Str(typ))) = a.args.first()
            && let Some(Term::Val(Value::Str(path))) = a.args.get(a.args.len().saturating_sub(2))
            && let Some((f, l, _)) = crate::diag::location(a.span)
        {
            out.entry((typ.clone(), path.clone()))
                .or_insert_with(|| format!("{f}:{l}"));
        }
    }
    for s in crate::modules::reached(program) {
        let Stmt::Rule(rule) = s else { continue };
        let [Term::Val(Value::Str(m)), Term::Obj(ctx)] = rule.head.args.as_slice() else {
            continue;
        };
        if rule.head.pred != "deny" || m != crate::refine::VIOLATED {
            continue;
        }
        let field = |k: &str| match ctx.get(k) {
            Some(Term::Val(Value::Str(s))) => Some(s.clone()),
            _ => None,
        };
        let (Some(typ), Some(path), Some(at)) = (field("type"), field("path"), field("at")) else {
            continue;
        };
        // `file:line:col`, then where it was lowered from: its line.
        let at = at.split(", ").next().unwrap_or_default();
        let at = at.rsplit_once(':').map_or(at, |(fl, _)| fl);
        out.entry((typ, path)).or_insert_with(|| at.to_string());
    }
    out
}

/// A deny's own message for what is no policy of the program's: a
/// conflict, a refinement violated.
fn conflict(message: &str) -> bool {
    message == super::CONFLICT || message == crate::refine::VIOLATED
}

/// The one of `subjects` a failure's context names: a reference to it,
/// or its name.
fn subject_in(ctx: &Value, subjects: &[Address]) -> Option<String> {
    let of = |typ: Option<&str>, name: &str| {
        subjects
            .iter()
            .find(|a| a.name == name && typ.is_none_or(|t| a.typ == t))
            .map(address)
    };
    match ctx {
        Value::Ref { typ, name, .. } => of(Some(typ), name),
        Value::Str(name) => of(None, name),
        Value::Obj(m) => m.values().find_map(|v| subject_in(v, subjects)),
        Value::List(xs) => xs.iter().find_map(|v| subject_in(v, subjects)),
        _ => None,
    }
}

/// What a deny ranges over: the type of its first `x in T` and its
/// variable, and the component it is written in, if any, for a copy's `x
/// in T` is its own resources of `T` (a module's, like the stack's, is
/// every one).
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct Range {
    typ: String,
    var: String,
    component: Option<String>,
}

/// Each deny the program writes where the plan carries what it derives:
/// the stack's own, those of each module a `use` reaches and each
/// component a copy does ([`crate::modules::reached`]), each with the
/// component whose body it is in.
fn written(program: &Program) -> Vec<(&RuleStmt, Option<&str>)> {
    let components: Vec<(&str, &[Stmt])> = crate::modules::definitions(&program.statements)
        .into_iter()
        .filter(|(_, m)| m.component)
        .map(|(path, m)| (path, m.body.as_slice()))
        .collect();
    crate::modules::reached(program)
        .into_iter()
        .filter_map(|s| match s {
            Stmt::Rule(rule) if rule.head.pred == "deny" => {
                let component = components
                    .iter()
                    .find(|(_, body)| body.as_ptr_range().contains(&std::ptr::from_ref(s)))
                    .map(|(path, _)| *path);
                Some((rule, component))
            }
            _ => None,
        })
        .collect()
}

/// The resources the program wants, by type, the scope of each copy of a
/// component (`k3s.agent-0` of `k3s.node`), and each one's attributes.
struct Wanted<'a> {
    by_type: BTreeMap<&'a str, Vec<&'a str>>,
    copies: BTreeMap<&'a str, Vec<String>>,
    attrs: BTreeMap<(&'a str, &'a str), Vec<(&'a str, &'a Value)>>,
}

impl<'a> Wanted<'a> {
    fn new(res: &'a EvalResult) -> Self {
        let mut w = Wanted {
            by_type: BTreeMap::new(),
            copies: BTreeMap::new(),
            attrs: BTreeMap::new(),
        };
        for a in &res.facts {
            match (a.pred.as_str(), a.args.as_slice()) {
                ("want", [Term::Val(Value::Str(t)), Term::Val(Value::Str(n))]) => {
                    w.by_type.entry(t).or_default().push(n)
                }
                (
                    "attr",
                    [
                        Term::Val(Value::Str(t)),
                        Term::Val(Value::Str(n)),
                        Term::Val(Value::Str(path)),
                        Term::Val(v),
                    ],
                ) => w.attrs.entry((t, n)).or_default().push((path, v)),
                (
                    crate::modules::INSTANCE_OF,
                    [
                        Term::Val(Value::Str(path)),
                        Term::Val(Value::Str(user)),
                        Term::Val(Value::Str(name)),
                    ],
                ) => w
                    .copies
                    .entry(path)
                    .or_default()
                    .push(crate::types::dotted(user, name)),
                _ => {}
            }
        }
        w
    }

    /// The resources `ranges` take in: each of its type, a component's
    /// only those inside one of its copies.
    fn subjects(&self, ranges: &BTreeSet<Range>) -> Vec<Address> {
        let mut out: Vec<Address> = Vec::new();
        for Range { typ, component, .. } in ranges {
            let inside = |name: &str| match component {
                None => true,
                Some(c) => self.copies.get(c.as_str()).is_some_and(|scopes| {
                    scopes.iter().any(|s| {
                        name.strip_prefix(s.as_str())
                            .is_some_and(|rest| rest.starts_with('.'))
                    })
                }),
            };
            for name in self.by_type.get(typ.as_str()).into_iter().flatten() {
                let a = Address {
                    typ: typ.clone(),
                    name: name.to_string(),
                };
                if inside(name) && !out.contains(&a) {
                    out.push(a);
                }
            }
        }
        out
    }
}

impl Wanted<'_> {
    /// Each of `subjects` whose wanted value holds the null `n`, with the
    /// path it holds it at: what a policy over them waits on through `n`
    /// (`compute.vm app`, `db_host`, of `db.postgres db`'s endpoint).
    fn holding(&self, subjects: &[Address], n: &str) -> Vec<(Address, String)> {
        let holds =
            |v: &Value| v.any_scalar(&mut |x| matches!(x, Value::Null { label, .. } if label == n));
        let mut out = Vec::new();
        for a in subjects {
            let attrs = self.attrs.get(&(a.typ.as_str(), a.name.as_str()));
            for (path, _) in attrs.into_iter().flatten().filter(|(_, v)| holds(v)) {
                out.push((a.clone(), path.to_string()));
            }
        }
        out
    }
}

/// The type a deny ranges over and its variable: its body's first `x in
/// T`.
fn ranged(body: &[Lit]) -> Option<(String, String)> {
    body.iter().find_map(|l| match l {
        Lit::Pos(a) if a.pred == "want" => match a.args.as_slice() {
            [Term::Val(Value::Str(t)), Term::Var(v)] => Some((t.clone(), v.clone())),
            _ => None,
        },
        _ => None,
    })
}

/// What an undetermined policy waits on, by what it is about: each
/// resource with the cell it waits on (`until spec.x is known (tick 2)`),
/// else the value. A policy over `subjects` (a deny's `x in T`) is about
/// them: a value of another resource it waits on is said of the subject
/// that reads it, at the path it reads it at (`compute.vm app  until
/// db_host is known`, not the database whose endpoint it is). When its
/// undetermined instances are `bound` to subjects, those are what waits:
/// one that reads the value at a path of its own says the path, the rest
/// the value (`net.vpc a  until stacks.platform[env=lab].endpoint is
/// known`, not the deployment).
fn waits(
    p: &Policy,
    subjects: &[Address],
    bound: &[Address],
    wanted: &Wanted,
) -> Vec<(String, String)> {
    let when = match p.after {
        Some(t) => format!("(tick {})", t + 1),
        None => match until_text(&p.until) {
            u if u.is_empty() => String::new(),
            u => format!("({u})"),
        },
    };
    let mut out = Vec::new();
    for n in &p.on {
        let owner = crate::value::null_owner(n);
        let of_subject = owner
            .as_ref()
            .is_some_and(|(typ, name)| subjects.iter().any(|a| a.typ == *typ && a.name == *name));
        let (readers, bound) = match of_subject {
            true => (Vec::new(), Vec::new()),
            false => {
                // What an instance is bound to is what waits, when one is.
                let mut readers = wanted.holding(subjects, n);
                readers.retain(|(a, _)| bound.is_empty() || bound.contains(a));
                let bound: Vec<&Address> = bound
                    .iter()
                    .filter(|b| !readers.iter().any(|(a, _)| a == *b))
                    .collect();
                (readers, bound)
            }
        };
        if !readers.is_empty() || !bound.is_empty() {
            out.extend(
                readers
                    .into_iter()
                    .map(|(a, path)| (address(&a), format!("{path}\t{when}"))),
            );
            out.extend(
                bound
                    .into_iter()
                    .map(|a| (address(a), format!("{}\t{when}", value_label(n)))),
            );
            continue;
        }
        out.push(match crate::value::null_parts(n) {
            Some((typ, name, path))
                if !name.is_empty()
                    && typ != crate::stack::UNAPPLIED
                    && typ != crate::transform::OUTPUT
                    && !path.is_empty() =>
            {
                (address(&Address { typ, name }), format!("{path}\t{when}"))
            }
            _ => (label(n), format!("{}\t{when}", label(n))),
        });
    }
    out
}

/// The value the null `n` stands for, as a policy waits on it: the
/// reference it is (`db.endpoint`), another deployment's output as it is
/// read (`stacks.platform[env=lab].endpoint`), not the deployment.
fn value_label(n: &str) -> String {
    match crate::value::null_parts(n) {
        Some((typ, _, _)) if typ == crate::stack::UNAPPLIED => crate::ir::label(n),
        _ => label(n),
    }
}

/// What `waits` found, one line per resource: `until A, B are known
/// (tick 2)`.
fn by_subject(waits: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut cells: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for (subject, at) in waits {
        let (cell, when) = at.split_once('\t').unwrap_or((&at, ""));
        cells
            .entry((subject, when.to_string()))
            .or_default()
            .insert(cell.to_string());
    }
    cells
        .into_iter()
        .map(|((subject, when), cells)| {
            let verb = match cells.len() {
                1 => "is",
                _ => "are",
            };
            let cells: Vec<String> = cells.into_iter().collect();
            let until = format!("until {} {verb} known {when}", cells.join(", "));
            (subject, until.trim_end().to_string())
        })
        .collect()
}

/// The block's rows: its header with the count, then each policy that
/// does not hold (each that does too, at `-v`), what does not hold under
/// it with why, and the rest as `N hold` (each, at `-v`).
pub(super) fn rows(lines: &[Line], why: Why, style: Style) -> Vec<Row> {
    let mut rows = Vec::new();
    if lines.is_empty() {
        return rows;
    }
    let (hold, fails, undetermined) = count(lines);
    let head = format!("policy  {}", tally_text(hold, fails, undetermined));
    rows.push(Row::new(&head, style.paint(Paint::Bold, &head)));
    let every = why >= Why::How;
    let width = lines
        .iter()
        .filter(|l| every || l.mark() != Mark::Holds)
        .map(|l| l.mark().word().len())
        .max()
        .unwrap_or(0);
    for l in lines {
        let mark = l.mark();
        if mark == Mark::Holds && !every {
            continue;
        }
        let word = mark.word();
        let pad = " ".repeat(width - word.len());
        let plain = format!("  {word}{pad}  {}", l.text);
        let painted = format!("  {}{pad}  {}", style.paint(mark.paint(), word), l.text);
        let right = format!("{}  {}", l.at, l.tally()).trim().to_string();
        rows.push(Row::new(&plain, painted).with(vec![right, l.at.clone()]));
        for (of, why) in &l.fails {
            let row = match (of, why.is_empty()) {
                (Some(of), true) => Row::plain(format!("    {of}")),
                (Some(of), false) => Row::plain(format!("    {of}")).with(vec![why.clone()]),
                (None, true) => Row::plain("    fails".to_string()),
                (None, false) => Row::plain(format!("    {why}")),
            };
            rows.push(row.kept());
        }
        for (subject, until) in &l.undetermined {
            rows.push(
                Row::plain(format!("    {subject}"))
                    .with(vec![until.clone()])
                    .kept(),
            );
        }
        match every {
            true => rows.extend(
                l.holds
                    .iter()
                    .map(|h| Row::plain(format!("    {h}")).with(vec!["holds".into()])),
            ),
            false if l.hold > 0 && mark != Mark::Holds => {
                rows.push(Row::plain(format!("    {} hold", l.hold)))
            }
            false => {}
        }
    }
    rows
}

/// The block under a tick an apply ran (R-206), the policies as the
/// boundary after it re-checked them: `policy after tick 1   14 hold · 1
/// undetermined`, then each that does not hold as the plan's block says
/// it (a policy that fails there its line with its mark red). `was`, the
/// count before the tick, follows when the count moved, each word the
/// line says already left out: `(was 12 · 1 fails · 2)`.
pub fn after(
    tick: usize,
    lines: &[Line],
    was: Option<(usize, usize, usize)>,
    why: Why,
    style: Style,
) -> String {
    let mut rows = rows(lines, why, style);
    if rows.is_empty() {
        return String::new();
    }
    let now = count(lines);
    let head = format!(
        "policy after tick {tick}   {}",
        tally_text(now.0, now.1, now.2)
    );
    let mut header = Row::new(&head, style.paint(Paint::Bold, &head));
    if let Some(was) = was.filter(|w| *w != now) {
        header = header.with(vec![format!("(was {})", was_text(now, was))]);
    }
    rows[0] = header;
    layout_aligned(&rows, style)
}

/// The count before, each part's word left out where the count now says
/// it: `12 · 1 fails · 2` against `14 hold · 1 undetermined`.
fn was_text(now: (usize, usize, usize), was: (usize, usize, usize)) -> String {
    let parts: Vec<String> = [
        (was.0, now.0, "hold"),
        (was.1, now.1, "fails"),
        (was.2, now.2, "undetermined"),
    ]
    .into_iter()
    .filter(|(n, _, _)| *n > 0)
    .map(|(n, said, w)| match said > 0 {
        true => n.to_string(),
        false => format!("{n} {w}"),
    })
    .collect();
    match parts.is_empty() {
        true => "0 hold".to_string(),
        false => parts.join(" · "),
    }
}

impl Policy {
    /// The ones of `subjects` an undetermined instance of it is bound to
    /// by the variable a range of its deny ranges over (`V: "a"` of `v in
    /// net.vpc`).
    fn bound_in(&self, ranges: &BTreeSet<Range>, subjects: &[Address]) -> Vec<Address> {
        let mut out: Vec<Address> = Vec::new();
        for (b, r) in self
            .bound
            .iter()
            .flat_map(|b| ranges.iter().map(move |r| (b, r)))
        {
            let name = match b.get(&r.var) {
                Some(Value::Str(name)) => name,
                Some(Value::Ref { typ, name, .. }) if *typ == r.typ => name,
                _ => continue,
            };
            let a = Address {
                typ: r.typ.clone(),
                name: name.clone(),
            };
            if subjects.contains(&a) && !out.contains(&a) {
                out.push(a);
            }
        }
        out
    }
}

impl Line {
    /// The line as `--json` says it: its mark, text, site and tally, and
    /// what is under it.
    pub fn json(&self) -> serde_json::Value {
        serde_json::json!({
            "mark": self.mark().word(),
            "text": self.text,
            "at": self.at,
            "hold": self.hold,
            "holds": self.holds,
            "fails": self.fails.iter().map(|(of, why)| serde_json::json!({
                "resource": of,
                "context": why,
            })).collect::<Vec<_>>(),
            "undetermined": self.undetermined.iter().map(|(of, until)| serde_json::json!({
                "of": of,
                "until": until,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Policy {
    pub message: String,
    pub on: Vec<String>,
    pub reason: String,
    pub after: Option<usize>,
    /// Undetermined (Rule 3), or may derive after a boundary (a positive
    /// read of a predicate with a stuck instance).
    pub may_derive: bool,
    /// A deferred refinement check, not a deny (`message` names it).
    pub refinement: bool,
    /// The deny's rule (`r12`), and where it is written.
    pub rule: Option<String>,
    pub site: Option<Site>,
    /// What it waits on outside this plan ([`Until`]), when no tick of
    /// it decides it.
    pub until: BTreeSet<Until>,
    /// Each undetermined instance's bindings (`{V: "a"}`): which of what
    /// it ranges over waits.
    pub bound: Vec<BTreeMap<String, Value>>,
}

/// A deny over the plan, the row of the plan it matched and where it is
/// written ([`Report::explain`](crate::report::Report::explain)).
#[derive(Debug, Clone)]
pub struct Denied {
    pub message: String,
    /// The resource of the `deformation` row it read, when it read one.
    pub addr: String,
    pub site: Option<Site>,
}

pub(super) fn deny_message(head: &Atom) -> String {
    match head.args.first() {
        Some(Term::Val(Value::Str(m))) => m.clone(),
        _ => spell::atom(head),
    }
}

/// Undetermined denies (Rule 3), then denies that may derive after a
/// boundary: a body that positively reads a predicate with a stuck
/// instance in its partition (F DR-2 revised, last clause).
pub(super) fn policies(
    i: &Input,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Policy> {
    let mut out: Vec<Policy> = Vec::new();
    for s in i.res.stuck.iter().filter(|s| s.head.pred == "deny") {
        let on = nulls(s);
        let p = Policy {
            message: deny_message(&s.head),
            after: resolves(&on, tick_of),
            on,
            reason: s.reason.clone(),
            may_derive: false,
            refinement: false,
            rule: s.rule.map(|r| format!("r{r}")),
            site: None,
            until: BTreeSet::new(),
            bound: vec![s.bindings.clone()],
        };
        match out
            .iter_mut()
            .find(|x| x.message == p.message && x.on == p.on && x.reason == p.reason)
        {
            Some(x) => x.bound.extend(p.bound),
            None => out.push(p),
        }
    }
    out.sort_by(|a, b| (&a.message, &a.on).cmp(&(&b.message, &b.on)));

    type Found = (BTreeSet<String>, Vec<String>, Option<usize>);
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for m in i.res.may_derive.iter().filter(|m| m.head.pred == "deny") {
        let message = deny_message(&m.head);
        if out.iter().any(|p| p.message == message) {
            continue;
        }
        let (on, reads, rule) = found.entry(message).or_default();
        on.extend(m.nulls.iter().cloned());
        // A reference column's row by its address (R-185).
        reads.push(crate::query::Redactor::default().surface_atom(&m.reads));
        rule.get_or_insert(m.rule);
    }
    let mut may: Vec<Policy> = Vec::new();
    for (message, (on, mut reads, rule)) in found {
        if on.is_empty() {
            continue;
        }
        reads.sort();
        reads.dedup();
        let on: Vec<String> = on.into_iter().collect();
        may.push(Policy {
            message,
            after: resolves(&on, tick_of),
            on,
            reason: format!("reads {} with a stuck instance", reads.join(", ")),
            may_derive: true,
            refinement: false,
            rule: rule.map(|r| format!("r{r}")),
            site: None,
            until: BTreeSet::new(),
            bound: Vec::new(),
        });
    }
    may.sort_by(|a, b| a.message.cmp(&b.message));
    may.dedup_by(|a, b| a.message == b.message);
    out.extend(may);
    out
}

/// Refinements the winning value could not decide yet (E §2.4 step 4):
/// `refinement_deferred(T, A, Path, C, Nulls)`, re-checked at the boundary
/// that resolves `Nulls`.
pub(super) fn deferred(
    res: &EvalResult,
    tick_of: &BTreeMap<(String, String), usize>,
    resolves: &Resolves,
) -> Vec<Policy> {
    let mut out = Vec::new();
    for a in res
        .facts
        .iter()
        .filter(|a| a.pred == crate::refine::DEFERRED)
    {
        let [
            Term::Val(Value::Str(t)),
            Term::Val(addr),
            Term::Val(Value::Str(path)),
            Term::Val(Value::Str(c)),
            Term::Val(Value::List(nulls)),
        ] = a.args.as_slice()
        else {
            continue;
        };
        let on: Vec<String> = nulls
            .iter()
            .filter_map(|n| n.as_str().map(str::to_string))
            .collect();
        let addr = spell::bare(addr);
        out.push(Policy {
            message: format!(
                "{c} of {}",
                attribute(
                    &Address {
                        typ: t.to_string(),
                        name: addr,
                    },
                    path
                )
            ),
            after: resolves(&on, tick_of),
            on,
            reason: "the value carries a null".into(),
            may_derive: false,
            refinement: true,
            rule: None,
            site: None,
            until: BTreeSet::new(),
            bound: Vec::new(),
        });
    }
    out
}

/// Deny `text` over the plan (`MESSAGE`, or `MESSAGE ctx={..}`) as the
/// plan's rows say it: its message, the resource of the `deformation` row
/// its firing read, and where it is written.
pub(super) fn denied(p: &tree::Printer, res: &EvalResult, text: &str) -> Denied {
    let message = |a: &Atom| match a.args.first() {
        Some(Term::Val(Value::Str(m))) => Some(m.clone()),
        _ => None,
    };
    let fact =
        res.facts.iter().filter(|a| a.pred == "deny").find(|a| {
            message(a).is_some_and(|m| text == m || text.starts_with(&format!("{m} ctx=")))
        });
    let Some(fact) = fact else {
        return Denied {
            message: text.to_string(),
            addr: String::new(),
            site: None,
        };
    };
    let id = res.circuit.fact_id(&crate::engine::circuit_fact(fact));
    let addr = id
        .and_then(|id| match res.circuit.view(id) {
            crate::circuit::View::Fact { alts, .. } => alts.first().copied(),
            _ => None,
        })
        .and_then(|alt| match res.circuit.view(alt) {
            crate::circuit::View::Times { children, .. } => {
                children.iter().find_map(|c| match res.circuit.view(*c) {
                    crate::circuit::View::Fact { fact, .. } if fact.pred == "deformation" => {
                        match fact.args.get(1) {
                            Some(Value::Ref { typ, name, .. }) => Some(
                                Address {
                                    typ: typ.clone(),
                                    name: name.clone(),
                                }
                                .to_string(),
                            ),
                            Some(v) => Some(spell::bare(v)),
                            None => None,
                        }
                    }
                    _ => None,
                })
            }
            _ => None,
        })
        .unwrap_or_default();
    Denied {
        message: message(fact).unwrap_or_else(|| text.to_string()),
        addr,
        site: id.and_then(|id| p.site(&res.rules, id)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_count_before_leaves_out_the_words_the_line_says() {
        assert_eq!(was_text((14, 0, 1), (12, 1, 2)), "12 · 1 fails · 2");
        assert_eq!(was_text((2, 0, 0), (1, 0, 1)), "1 · 1 undetermined");
    }

    #[test]
    fn the_block_after_a_tick_says_the_count_and_what_it_was() {
        let line = |fails: usize, undetermined: usize| Line {
            text: "the vm reads an endpoint".into(),
            at: "p.df:7".into(),
            holds: vec![],
            hold: 1,
            fails: (0..fails)
                .map(|_| (Some("compute.vm app".into()), "db_host = \"x\"".into()))
                .collect(),
            undetermined: (0..undetermined)
                .map(|_| {
                    (
                        "compute.vm app".into(),
                        "until db_host is known (tick 2)".into(),
                    )
                })
                .collect(),
        };
        let held = Line {
            undetermined: vec![],
            ..line(0, 0)
        };
        let text = after(
            1,
            std::slice::from_ref(&held),
            Some((0, 0, 1)),
            Why::Line,
            Style::default(),
        );
        assert_eq!(text, "policy after tick 1   1 hold  (was 1 undetermined)\n");
        // Unmoved, no `was`.
        let text = after(2, &[held], Some((1, 0, 0)), Why::Line, Style::default());
        assert_eq!(text, "policy after tick 2   1 hold\n");
        // A policy that fails at the boundary: its line, its mark red.
        let text = after(
            1,
            &[line(1, 0)],
            Some((0, 0, 1)),
            Why::Line,
            Style { color: true },
        );
        assert!(
            text.contains("\x1b[31mfails\x1b[0m  the vm reads an endpoint"),
            "{text}"
        );
    }

    /// A policy's text wider than the page's column: what is under it
    /// says why in the same column as the policy's site, as long as the
    /// page holds it.
    #[test]
    fn a_wide_policy_keeps_one_column_with_what_is_under_it() {
        let line = Line {
            text: "the vm reads no endpoint that is nowhere at all".into(),
            at: "p.df:4".into(),
            holds: vec![],
            hold: 0,
            fails: vec![],
            undetermined: vec![(
                "compute.vm app".into(),
                "until db_host is known (tick 2)".into(),
            )],
        };
        let text = after(1, &[line], None, Why::Line, Style::default());
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(
            lines[1..],
            [
                "  undetermined  the vm reads no endpoint that is nowhere at all  p.df:4  1 undetermined",
                "    compute.vm app                                               until db_host is known (tick 2)",
            ],
            "{text}"
        );
    }
}
