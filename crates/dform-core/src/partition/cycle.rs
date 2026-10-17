//! The error for a cycle through a negation or a merged value (R-107,
//! R-109): the values on it as the program names them, why none of them
//! can be computed first, and each rule on it as the program writes it,
//! at its line, with what it makes of what.

use super::{Addr, Edge, Graph, Node, scoped_cell};
use crate::ast::RuleStmt;
use crate::transform::{ATTR_BASE, ELEM};
use std::collections::{BTreeMap, BTreeSet};

/// The compile error for the negative cycle `scc`, `negative` its
/// negative edges.
pub fn cycle_error(g: &Graph, scc: &BTreeSet<Node>, negative: &[Edge]) -> String {
    let names = Names::of(&g.rules);
    let mut out = waits(&names, scc, negative);
    for (rule, lines) in on_cycle(&names, g, scc) {
        out.push_str(&format!("\n  {rule}"));
        for l in lines {
            out.push_str(&format!("\n    {l}"));
        }
    }
    if let Some(help) = unless_itself(&names, g, negative) {
        out.push_str(&format!("\n  help: {help}"));
    }
    out
}

/// What waits on what, the values on the cycle as the program names
/// them, once each, and why none of them is computed first: what each
/// kind of negative edge on it waits for.
fn waits(said: &Names, scc: &BTreeSet<Node>, negative: &[Edge]) -> String {
    let mut names: Vec<String> = Vec::new();
    for n in scc.iter().filter_map(|n| said.node(n)) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    let kinds: BTreeSet<Wait> = negative.iter().map(|e| Wait::of(&e.why)).collect();
    let why: Vec<&str> = kinds.iter().map(|k| k.clause()).collect();
    let why = why.join("; ");
    if names.len() > LISTED {
        let more = names.len() - LISTED + 1;
        names.truncate(LISTED - 1);
        names.push(format!("{more} more"));
    }
    match names.split_last() {
        Some((one, [])) => format!("{one} waits on itself: {why}, so it cannot be computed"),
        Some((last, rest)) => format!(
            "{} and {last} wait on each other: {why}, so none of them can be computed first",
            rest.join(", ")
        ),
        None => format!("these rules wait on each other: {why}, so none of them can run first"),
    }
}

/// The most values the error names; the rest are counted.
const LISTED: usize = 8;

/// What a negative edge waits for, by the graph's reason for it.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
enum Wait {
    /// A value read waits on every write to it.
    Value,
    Not,
    Fold,
    Table,
    Stuck,
}

impl Wait {
    fn of(why: &str) -> Wait {
        match why.split(" (").next().unwrap_or(why) {
            "not" => Wait::Not,
            "aggregate head" => Wait::Fold,
            "extern" => Wait::Table,
            w if w.starts_with("can stick") || w.contains("stuck/4") => Wait::Stuck,
            _ => Wait::Value,
        }
    }

    fn clause(&self) -> &'static str {
        match self {
            Wait::Value => "a value is known only once every write to it is",
            Wait::Not => "a `not` holds only once what it tests is known",
            Wait::Fold => "a fold is known only once every row it folds is",
            Wait::Table => "a provider table is read only once what it is called with is known",
            Wait::Stuck => "`stuck` is known only once every rule that can wait has run",
        }
    }
}

/// Each rule with an edge inside the cycle as written, in program order,
/// once each (the rules one statement lowers to are one), and what it
/// makes of what there.
fn on_cycle(names: &Names, g: &Graph, scc: &BTreeSet<Node>) -> Vec<(String, Vec<String>)> {
    let mut rules: BTreeMap<usize, Vec<&Edge>> = BTreeMap::new();
    let inside = g
        .edges
        .iter()
        .filter(|e| scc.contains(&e.from) && scc.contains(&e.to));
    for e in inside {
        if let Some(i) = e.rule {
            rules.entry(i).or_default().push(e);
        }
    }
    let mut out: Vec<(String, Vec<String>)> = Vec::new();
    for (i, edges) in rules {
        let rule = written(&g.rules[i]);
        let at = match out.iter().position(|(r, _)| *r == rule) {
            Some(at) => at,
            None => {
                out.push((rule, through(&g.rules[i]).into_iter().collect()));
                out.len() - 1
            }
        };
        let lines = &mut out[at].1;
        for l in edges.into_iter().filter_map(|e| makes(names, e)) {
            if !lines.contains(&l) {
                lines.push(l);
            }
        }
    }
    out
}

/// What the rule of edge `e` makes of what it reads: `x.p from y.q`;
/// none where either end is the compiler's.
fn makes(names: &Names, e: &Edge) -> Option<String> {
    let (mut from, to) = (names.node(&e.from)?, names.node(&e.to)?);
    if from == to {
        from = "itself".into();
    }
    Some(match Wait::of(&e.why) {
        Wait::Not => format!("{to} unless {from}"),
        Wait::Fold => format!("{to} folds {from}"),
        Wait::Table => format!("{to} from the provider table {from}"),
        Wait::Value | Wait::Stuck => format!("{to} from {from}"),
    })
}

/// A rule as the program writes it, at its line: the statement (the
/// entry its head is lowered out of, in a block), the copy it is in, the
/// call a let's copy answers; one dform writes by what it does.
fn written(r: &RuleStmt) -> String {
    let span = match r.head.span.is_none() {
        true => r.site(),
        false => r.head.span,
    };
    let (Some((file, line, _)), Some(text)) = (
        crate::diag::location(span),
        crate::report::tree::written_at(span),
    ) else {
        let words = crate::report::tree::core_words(r).unwrap_or("a rule dform writes");
        return format!("dform  {words}");
    };
    let at = format!("{file}:{line}");
    let within = crate::diag::origin(span).map(|o| format!("   ({o})"));
    let call = r.origin.and_then(|o| o.call).and_then(crate::diag::at);
    let call = call.map(|c| format!("   (for the call at {c})"));
    format!(
        "{at}  {text}{}{}",
        within.unwrap_or_default(),
        call.unwrap_or_default()
    )
}

/// The rule of a relation a `set` writes through, as the program writes
/// it, when the rule is the `set`'s unfolded with it: `through main.df:3
/// pod(w, p) where ..`, the rule that may read what the `set` writes.
fn through(r: &RuleStmt) -> Option<String> {
    let span = r.origin?.through?;
    let (file, line, _) = crate::diag::location(span)?;
    let text = crate::report::tree::written_at(span)?;
    Some(format!("through {file}:{line}  {text}"))
}

/// A value node: `T.p`, `T a.p` at an address, a cell by its scope.
fn value(n: &Node) -> String {
    let path = n.path.as_deref().map(|p| p.strip_suffix(ELEM).unwrap_or(p));
    match (n.typ.as_deref(), path) {
        (Some(t), Some(p)) if scoped_cell(t) => match p.rsplit_once("::") {
            Some((scope, k)) => {
                let scope = scope.split('|').next().unwrap_or(scope);
                format!("{t} {scope}.{k}")
            }
            None => format!("{t} {p}"),
        },
        (Some(t), Some(p)) => match &n.addr {
            Some(a) => format!("{t} {}.{p}", address(a)),
            None => format!("{t}.{p}"),
        },
        (Some(t), None) => format!("every attribute of {t}"),
        (None, Some(p)) => format!("every resource's {p}"),
        (None, None) => "every attribute".into(),
    }
}

/// The addresses a node is about as the plan names one (`a`,
/// `k3s.server`), a pattern's gaps `*`, its alternatives `|`.
fn address(a: &Addr) -> String {
    let alts: Vec<String> =
        a.0.iter()
            .map(|g| crate::report::path(&g.join("*")))
            .collect();
    alts.join("|")
}

/// What the program names a node by: the relations the compiler writes
/// beside the program's ([`crate::ast::Helper`]) it does not.
struct Names {
    helpers: BTreeSet<crate::ast::Name>,
}

impl Names {
    fn of(rules: &[RuleStmt]) -> Names {
        let helpers = rules.iter().filter(|r| r.helper.is_some());
        Names {
            helpers: helpers.map(|r| r.head.pred.clone()).collect(),
        }
    }

    /// A node as the program names what it holds: a value by its resource
    /// type, address and path (`net.vpc.peer`, `db.postgres a.endpoint`),
    /// an input's, a `let`'s or an output's cell by its scope (`input
    /// k3s.name`), the resources of a type, a relation by its name; none
    /// for one only the compiler writes.
    fn node(&self, n: &Node) -> Option<String> {
        match n.pred.name() {
            "arg" | "attr" | ATTR_BASE => Some(value(n)),
            "want" => Some(match (&n.typ, &n.addr) {
                (Some(t), Some(a)) => format!("{t} {}", address(a)),
                (Some(t), None) => format!("the {t} resources"),
                (None, _) => "every resource".into(),
            }),
            _ => self.relation(&n.pred),
        }
    }

    /// A relation by the name the program gives it: an enclosing
    /// definition's by its own, a module's after its scope, a let's copy
    /// for a call by the let's; none for a helper, or one of the core's
    /// own relations (`__known`), which no program names.
    fn relation(&self, pred: &crate::ast::Name) -> Option<String> {
        let crate::ast::Name::User(text) = pred else {
            return None;
        };
        if self.helpers.contains(pred) {
            return None;
        }
        let pred = text;
        let (scope, name) = match pred.rsplit_once("::") {
            Some((s, n)) => (Some(s), n),
            None => (None, pred.as_str()),
        };
        Some(match scope {
            Some(s) if s.starts_with("call at ") => name.trim_end_matches('?').to_string(),
            Some(s) => format!("{name} (in {s})"),
            None => name.to_string(),
        })
    }
}

/// The help for a rule that writes a value unless the value is written
/// (`set r.tags = .. where not r.tags == ..`): the default it means is a
/// write at `@default` rank, which any other write overrides.
fn unless_itself(names: &Names, g: &Graph, negative: &[Edge]) -> Option<String> {
    let e = negative.iter().find(|e| {
        e.rule.is_some() && Wait::of(&e.why) == Wait::Not && e.from.unifies(&value_of(&e.to))
    })?;
    let at = crate::diag::at(g.rules[e.rule?].head.span)?;
    Some(format!(
        "at {at}, write the default at `@default` rank: any other write of {} overrides it, \
         with no test for one",
        names.node(&e.to)?
    ))
}

/// The value a write's node writes, as a read of it reads it.
fn value_of(n: &Node) -> Node {
    match n.pred.name() {
        "arg" => Node {
            pred: crate::ast::Name::user("attr"),
            ..n.clone()
        },
        _ => n.clone(),
    }
}
