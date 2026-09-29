//! What the code under the cursor derives, and why: the facts of the
//! innermost rule or stated fact written there, read up to the attributes
//! they contribute to, printed as `dform why` prints them. A cursor on an
//! `attr` read in a rule's body names the attributes that rule read.

use crate::analysis::Evaluated;
use dform_core::ast::{Atom, Lit, Span, Stmt, Term};
use dform_core::circuit::{Leaf, NodeId, View};
use dform_core::engine;
use dform_core::lattice::Rank;
use dform_core::value::Value;
use dform_core::why;

/// What is written at a place: a rule (its index), an `attr` read in a
/// rule's body, or a stated fact.
enum Anchor<'a> {
    Rule(usize),
    Read(usize, &'a Atom),
    Fact(&'a Atom),
}

fn covers(s: Span, in_file: &dyn Fn(u32) -> bool, at: usize) -> bool {
    !s.is_none() && in_file(s.file) && (s.start as usize) <= at && at <= (s.end as usize)
}

fn len(s: Span) -> u32 {
    s.end - s.start
}

/// The fact nodes the code at byte `at` of the file (`in_file` says which
/// sources are it) is about: attributes where it contributes to or reads
/// them, else what it derives.
pub fn targets(e: &Evaluated, in_file: &dyn Fn(u32) -> bool, at: usize) -> Vec<NodeId> {
    let mut anchors: Vec<(u32, Anchor)> = Vec::new();
    for (i, r) in e.res.rules.iter().enumerate() {
        if !covers(r.head.span, in_file, at) {
            continue;
        }
        anchors.push((len(r.head.span), Anchor::Rule(i)));
        for l in &r.body {
            if let Lit::Pos(a) = l
                && a.pred == "attr"
                // Spans compare equal whatever they are: by their bytes.
                && (a.span.start, a.span.end) != (r.head.span.start, r.head.span.end)
                && covers(a.span, in_file, at)
            {
                anchors.push((len(a.span), Anchor::Read(i, a)));
            }
        }
    }
    let stated = e
        .program
        .statements
        .iter()
        .chain(e.lowered.iter().flat_map(|p| &p.statements));
    for s in stated {
        if let Stmt::Fact(a) = s
            && covers(a.span, in_file, at)
        {
            anchors.push((len(a.span), Anchor::Fact(a)));
        }
    }
    let Some(least) = anchors.iter().map(|(n, _)| *n).min() else {
        return Vec::new();
    };
    let c = &e.res.circuit;
    let mut found: Vec<NodeId> = Vec::new();
    for (_, anchor) in anchors.iter().filter(|(n, _)| *n == least) {
        match anchor {
            Anchor::Rule(i) => {
                for &n in e.derived_by(&format!("r{i}")) {
                    for t in up(e, n) {
                        push(&mut found, t);
                    }
                }
            }
            Anchor::Read(i, atom) => {
                let path = match atom.args.get(2) {
                    Some(Term::Val(Value::Str(p))) => Some(p.clone()),
                    _ => None,
                };
                let id = format!("r{i}");
                let before = found.len();
                for &n in e.derived_by(&id) {
                    for ch in read_by(e, n, &id) {
                        let View::Fact { fact, .. } = c.view(ch) else {
                            continue;
                        };
                        let same_path = match (&path, fact.args.get(2)) {
                            (Some(p), Some(Value::Str(q))) => p == q,
                            (None, _) => true,
                            _ => false,
                        };
                        if fact.pred == "attr" && same_path {
                            push(&mut found, ch);
                        }
                    }
                }
                // A rule that never fired read nothing: the attributes its
                // literal matches.
                if found.len() == before
                    && let Ok(matched) = why::find(atom, &e.res.facts)
                {
                    for (a, _) in matched {
                        if let Some(n) = c.fact_id(&engine::circuit_fact(&a)) {
                            push(&mut found, n);
                        }
                    }
                }
            }
            Anchor::Fact(atom) => {
                if let Some(n) = c.fact_id(&engine::circuit_fact(atom)) {
                    for t in up(e, n) {
                        push(&mut found, t);
                    }
                }
            }
        }
    }
    found
}

fn push(found: &mut Vec<NodeId>, n: NodeId) {
    if !found.contains(&n) {
        found.push(n);
    }
}

/// The fact nodes the firings of `n` by rule `id` read.
fn read_by(e: &Evaluated, n: NodeId, id: &str) -> Vec<NodeId> {
    let c = &e.res.circuit;
    let View::Fact { alts, .. } = c.view(n) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        if !children
            .iter()
            .any(|ch| matches!(c.view(*ch), View::Leaf(Leaf::Rule { id: r }) if r == id))
        {
            continue;
        }
        out.extend(
            children
                .iter()
                .copied()
                .filter(|ch| matches!(c.view(*ch), View::Fact { .. })),
        );
    }
    out
}

/// A contribution read up to the attribute it is part of; a resource's
/// `want` to the resource's attributes; anything else is itself.
fn up(e: &Evaluated, n: NodeId) -> Vec<NodeId> {
    let c = &e.res.circuit;
    let View::Fact { fact, .. } = c.view(n) else {
        return Vec::new();
    };
    match fact.pred.as_str() {
        "arg" => {
            let attrs: Vec<NodeId> = e
                .parents(n)
                .iter()
                .copied()
                .filter(|p| matches!(c.view(*p), View::Fact { fact, .. } if fact.pred == "attr"))
                .collect();
            if attrs.is_empty() { vec![n] } else { attrs }
        }
        "want" => {
            let (Some(t), Some(name)) = (fact.args.first(), fact.args.get(1)) else {
                return vec![n];
            };
            let attrs: Vec<NodeId> = e
                .res
                .facts
                .iter()
                .filter(|a| a.pred == "attr")
                .filter(|a| {
                    matches!((a.args.first(), a.args.get(1)),
                        (Some(Term::Val(x)), Some(Term::Val(y))) if x == t && y == name)
                })
                .filter_map(|a| c.fact_id(&engine::circuit_fact(a)))
                .collect();
            if attrs.is_empty() { vec![n] } else { attrs }
        }
        _ => vec![n],
    }
}

/// `dform why`'s text for each fact node, one after another.
pub fn why_text(e: &Evaluated, facts: &[NodeId]) -> String {
    let printer = why::Printer {
        circuit: &e.res.circuit,
        redact: &e.redact,
        all: false,
    };
    facts
        .iter()
        .map(|n| printer.tree(*n, None))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The hover: per attribute its collapsed value, the winning rank and
/// every contribution with its rank and owner; then the derivations.
pub fn hover(e: &Evaluated, facts: &[NodeId]) -> String {
    let c = &e.res.circuit;
    let mut out = format!("*{}*\n\n", e.deployment);
    for n in facts.iter().take(8) {
        let View::Fact { fact, alts, .. } = c.view(*n) else {
            continue;
        };
        if fact.pred != "attr" || fact.args.len() < 4 {
            out.push_str(&format!("`{}`\n\n", e.redact.fmt_atom(&atom_of(fact))));
            continue;
        }
        let s = |v: &Value| match v {
            Value::Str(s) => s.clone(),
            v => e.redact.fmt(v),
        };
        out.push_str(&format!(
            "**{} {} .{}** = `{}`\n\n",
            s(&fact.args[0]),
            s(&fact.args[1]),
            s(&fact.args[2]),
            e.redact.fmt(&fact.args[3])
        ));
        let mut contributions = Vec::new();
        for a in alts.iter().take(1) {
            let View::Times { children, .. } = c.view(*a) else {
                continue;
            };
            for ch in children {
                let View::Fact { fact: f, .. } = c.view(*ch) else {
                    continue;
                };
                if f.pred != "arg" {
                    continue;
                }
                let rank = match f.args.get(4) {
                    Some(Value::Str(r)) => r.clone(),
                    _ => continue,
                };
                let value = f.args.get(3).map(|v| e.redact.fmt(v)).unwrap_or_default();
                contributions.push((rank, value, owner(e, *ch)));
            }
        }
        if let Some(best) = contributions
            .iter()
            .filter_map(|(r, _, _)| Rank::parse(r))
            .max()
        {
            out.push_str(&format!("winning rank: {}\n\n", best.name()));
        }
        for (rank, value, owner) in &contributions {
            out.push_str(&format!("- rank {rank}: `{value}` by {owner}\n"));
        }
        out.push('\n');
    }
    if facts.len() > 8 {
        out.push_str(&format!("... and {} more\n\n", facts.len() - 8));
    }
    out.push_str("```text\n");
    out.push_str(&why_text(e, &facts[..facts.len().min(8)]));
    out.push_str("```\n");
    out
}

/// Who wrote contribution `n`: its rule and where it is written, with the
/// pack or module instance, or the place of a stated fact.
fn owner(e: &Evaluated, n: NodeId) -> String {
    let c = &e.res.circuit;
    let View::Fact { alts, .. } = c.view(n) else {
        return "?".into();
    };
    let mut owners = Vec::new();
    for a in alts {
        let View::Times { children, .. } = c.view(*a) else {
            continue;
        };
        for ch in children {
            let o = match c.view(*ch) {
                View::Leaf(Leaf::Rule { id }) => match c.rule_at(id) {
                    Some(at) => format!("{id} ({at})"),
                    None => id.clone(),
                },
                View::Leaf(Leaf::Base { span }) => span.clone(),
                View::Leaf(Leaf::Input { source }) => source.clone(),
                _ => continue,
            };
            if !owners.contains(&o) {
                owners.push(o);
            }
        }
    }
    if owners.is_empty() {
        "?".into()
    } else {
        owners.join(", ")
    }
}

fn atom_of(f: &dform_core::circuit::Fact) -> Atom {
    Atom {
        pred: f.pred.clone(),
        args: f.args.iter().cloned().map(Term::Val).collect(),
        record: None,
        span: Default::default(),
    }
}
