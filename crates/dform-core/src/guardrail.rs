//! What a plan empties since the last apply (R-80), against what that
//! apply derived ([`crate::zset::Derived`]): a rule the plan deletes every
//! resource of, and resources it deletes because a relation they read has
//! no rows now. Those are the plan's `warning` headlines, and what `apply`
//! asks for once, on its own.
//!
//! A headline is always resources. A relation (a `let`, a table, a
//! program's own) that had rows then and has none now is printed only as
//! the `because` of the resources that went with it, as a chain: the
//! relation their rule reads, then the one that one reads (`pod` has none
//! because `env` has none). One no deleted resource reads is no warning:
//! a refactor empties many, and only `-v` lists them.

use crate::address::Address;
use crate::ast::{Atom, Lit, RuleStmt, Term};
use crate::engine::EvalResult;
use crate::value::Value;
use crate::zset::Derived;
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// A relation that had rows at the last apply and has none now.
#[derive(Debug, Clone, PartialEq)]
pub struct Lost {
    /// Its name in the core's spelling (`green::vpc_net`).
    pub relation: String,
    /// Its rows at the last apply.
    pub rows: usize,
}

impl Lost {
    /// How it reads in a message: `the relation vpc_net (in green), which
    /// had 1 row at the last apply, has none now`.
    fn text(&self) -> String {
        format!(
            "the relation {}, which had {} at the last apply, has none now",
            crate::report::relation_name(&self.relation),
            rows_text(self.rows)
        )
    }
}

/// `1 row`, `2 rows`.
pub fn rows_text(n: usize) -> String {
    match n {
        1 => "1 row".into(),
        n => format!("{n} rows"),
    }
}

/// Resources the plan deletes that the last apply derived: every one a
/// rule derived then, or those a relation that emptied takes with it.
#[derive(Debug, Clone, PartialEq)]
pub struct Emptied {
    /// The rule's place (`FILE:LINE`); of what a relation takes, the
    /// relation's name.
    pub name: String,
    /// The rule's statement; `None` for what a relation takes.
    pub statement: Option<String>,
    /// The resources, every one deleted by the plan.
    pub deleted: Vec<String>,
    /// The leaf that changed since the last apply (R-79's `because`), of
    /// the first deleted resource that says.
    pub because: Option<String>,
    /// The relations that emptied which the resources read, as a chain:
    /// the one their rule reads first, each next one read by the one
    /// before.
    pub lost: Vec<Lost>,
}

impl Emptied {
    /// `--allow-empty NAME` (or `[stacks.NAME] allow_empty`) names it: its
    /// place, a resource type it deletes, or, of what a relation takes,
    /// a relation of its chain.
    pub fn allowed(&self, names: &[String]) -> bool {
        let relations = match self.statement {
            Some(_) => &[][..],
            None => &self.lost[..],
        };
        names.iter().any(|n| {
            *n == self.name
                || *n == self.flag()
                || relations
                    .iter()
                    .any(|l| *n == l.relation || *n == flag_of(&l.relation))
                || self
                    .deleted
                    .iter()
                    .any(|a| crate::address::parse_resource(a).is_ok_and(|a| a.typ == *n))
        })
    }

    /// What `--allow-empty` names it by: its place, or the relation as
    /// the source reads it, a copy's own by its path (`green.vpc_net`).
    pub fn flag(&self) -> String {
        match &self.statement {
            Some(_) => self.name.clone(),
            None => flag_of(&self.name),
        }
    }

    /// The question `apply` asks of it, and the line it refuses with.
    pub fn what(&self) -> String {
        match (&self.statement, self.lost.first()) {
            (Some(_), _) | (None, None) => format!(
                "the plan deletes all {} resources the rule at {} derived at the last apply",
                self.deleted.len(),
                self.name
            ),
            (None, Some(l)) => {
                let shown: Vec<String> = self
                    .deleted
                    .iter()
                    .map(|a| crate::report::address_text(a))
                    .collect();
                format!("the plan deletes {} because {}", and_list(&shown), l.text())
            }
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A relation as `--allow-empty` names it: a copy's own by its path.
fn flag_of(relation: &str) -> String {
    relation.replace("::", ".")
}

/// What a plan empties since the last apply: the headlines, and the
/// relations that emptied which none of them reads (`-v` lists them).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Emptying {
    pub headlines: Vec<Emptied>,
    pub unread: Vec<Lost>,
}

/// How many rows each of `relations` has in `res`.
pub(crate) fn rows_of(res: &EvalResult, relations: &BTreeSet<String>) -> BTreeMap<String, usize> {
    let mut rows: BTreeMap<String, usize> = relations.iter().map(|r| (r.clone(), 0)).collect();
    for f in &res.facts {
        if let Some(n) = rows.get_mut(&f.pred) {
            *n += 1;
        }
    }
    rows
}

/// The rows of each of `relations` (the lowered program's) in `res` that
/// may be compared with the last apply's: `None` for one whose rows wait
/// on a value the plan does not know yet (a stuck instance of it, or one
/// that may derive after a boundary). A name the program has no relation
/// of now (renamed, moved into a copy, removed) is no key: neither is one
/// that emptied.
pub fn rows_now(res: &EvalResult, relations: &BTreeSet<String>) -> BTreeMap<String, Option<usize>> {
    let mut rows: BTreeMap<String, Option<usize>> = rows_of(res, relations)
        .into_iter()
        .map(|(r, n)| (r, Some(n)))
        .collect();
    let waiting = res.stuck.iter().map(|s| &s.head.pred);
    for pred in waiting.chain(res.may_derive.iter().map(|m| &m.head.pred)) {
        if let Some(n) = rows.get_mut(pred) {
            *n = None;
        }
    }
    rows
}

/// What the plan empties since the last apply `then`: each rule all of
/// whose resources are in `deleted`, then the other resources of
/// `deleted` a relation that emptied takes (one headline a relation),
/// each with the chain of relations that emptied which it reads, through
/// `rules`, the evaluated program's; `now` is [`rows_now`]. `because` is
/// each address's leaf, when R-79 found one.
pub fn emptied(
    then: &Derived,
    rules: &[RuleStmt],
    now: &BTreeMap<String, Option<usize>>,
    deleted: &BTreeSet<Address>,
    because: &dyn Fn(&str) -> Option<String>,
) -> Emptying {
    let reads = Reads::new(rules, then, now);
    let text: BTreeMap<String, &Address> = deleted.iter().map(|a| (a.to_string(), a)).collect();
    let mut headlines = Vec::new();
    let mut covered = BTreeSet::new();
    for (at, r) in &then.rules {
        if r.addresses.is_empty() || !r.addresses.iter().all(|a| text.contains_key(a)) {
            continue;
        }
        covered.extend(r.addresses.iter().cloned());
        let lost = r
            .addresses
            .iter()
            .map(|a| reads.chain(text[a]))
            .find(|c| !c.is_empty())
            .unwrap_or_default();
        headlines.push(Emptied {
            name: at.clone(),
            statement: Some(r.statement.clone()),
            deleted: r.addresses.clone(),
            because: r.addresses.iter().find_map(|a| because(a)),
            lost,
        });
    }
    let mut taken: BTreeMap<String, Emptied> = BTreeMap::new();
    for (a, addr) in text.iter().filter(|(a, _)| !covered.contains(*a)) {
        let lost = reads.chain(addr);
        let Some(first) = lost.first() else {
            continue;
        };
        let e = taken
            .entry(first.relation.clone())
            .or_insert_with(|| Emptied {
                name: first.relation.clone(),
                statement: None,
                deleted: Vec::new(),
                because: None,
                lost: lost.clone(),
            });
        e.deleted.push(a.clone());
        e.because = e.because.take().or_else(|| because(a));
    }
    headlines.extend(taken.into_values());
    let read: BTreeSet<&str> = headlines
        .iter()
        .flat_map(|e| e.lost.iter().map(|l| l.relation.as_str()))
        .collect();
    let unread = reads
        .lost
        .iter()
        .filter(|(r, _)| !read.contains(**r))
        .map(|(r, n)| Lost {
            relation: r.to_string(),
            rows: *n,
        })
        .collect();
    Emptying { headlines, unread }
}

/// What the rules of a program read, toward the relations that emptied.
struct Reads<'a> {
    rules: &'a [RuleStmt],
    /// The rules by the relation of their head.
    by_head: BTreeMap<&'a str, Vec<&'a RuleStmt>>,
    /// The program's relations now: a read stops at one that did not
    /// empty. The compiler's own are read through.
    relations: BTreeSet<&'a str>,
    /// The relations that emptied, with their rows at the last apply.
    lost: BTreeMap<&'a str, usize>,
}

impl<'a> Reads<'a> {
    fn new(
        rules: &'a [RuleStmt],
        then: &'a Derived,
        now: &'a BTreeMap<String, Option<usize>>,
    ) -> Reads<'a> {
        let mut by_head: BTreeMap<&str, Vec<&RuleStmt>> = BTreeMap::new();
        for r in rules {
            by_head.entry(r.head.pred.as_str()).or_default().push(r);
        }
        let lost = then
            .rows
            .iter()
            .filter(|(rel, n)| **n > 0 && now.get(*rel) == Some(&Some(0)))
            .map(|(rel, n)| (rel.as_str(), *n))
            .collect();
        Reads {
            rules,
            by_head,
            relations: now.keys().map(String::as_str).collect(),
            lost,
        }
    }

    /// The chain of relations that emptied which the rules that may
    /// derive `addr` read: the nearest one, then the nearest one that one
    /// reads, until one reads none.
    fn chain(&self, addr: &Address) -> Vec<Lost> {
        let want = Atom {
            pred: "want".into(),
            args: vec![
                Term::Val(Value::Str(addr.typ.clone())),
                Term::Val(Value::Str(addr.name.clone())),
            ],
            record: None,
            span: Default::default(),
        };
        let deriving = |exact: bool| -> Vec<&RuleStmt> {
            self.rules
                .iter()
                .filter(|r| crate::zset::heads(&r.head, &want, exact))
                .collect()
        };
        let mut from = deriving(true);
        if from.is_empty() {
            from = deriving(false);
        }
        let mut chain: Vec<Lost> = Vec::new();
        while let Some((rel, rows)) = self.nearest(&from) {
            if chain.iter().any(|l| l.relation == rel) {
                break;
            }
            chain.push(Lost {
                relation: rel.to_string(),
                rows,
            });
            from = self.by_head.get(rel).cloned().unwrap_or_default();
        }
        chain
    }

    /// The rules whose heads the read `a` may read: a value's
    /// (`attr(T, A, P, V)`) its contributions (`arg(T, A, P, V, MERGE)`
    /// of the same `T`, `A` and `P`), else those of its relation.
    fn read_by(&self, a: &'a Atom) -> impl Iterator<Item = &'a RuleStmt> + '_ {
        let (pred, n) = match a.pred.as_str() {
            "attr" => ("arg", 3),
            p => (p, a.args.len()),
        };
        self.by_head
            .get(pred)
            .into_iter()
            .flatten()
            .copied()
            .filter(move |r| {
                a.args
                    .iter()
                    .zip(&r.head.args)
                    .take(n)
                    .all(|(v, h)| matches!(v, Term::Var(_)) || crate::zset::derives(h, v, false))
            })
    }

    /// The first relation that emptied the bodies of `from` read
    /// positively, breadth first through the compiler's relations, never
    /// through one of the program's that has rows now.
    fn nearest(&self, from: &[&'a RuleStmt]) -> Option<(&'a str, usize)> {
        let mut queue: VecDeque<&'a RuleStmt> = from.iter().copied().collect();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        while let Some(r) = queue.pop_front() {
            for l in &r.body {
                let Lit::Pos(a) = l else {
                    continue;
                };
                let pred = a.pred.as_str();
                if !seen.insert(pred) {
                    continue;
                }
                if let Some(n) = self.lost.get(pred) {
                    return Some((pred, *n));
                }
                if !self.relations.contains(pred) {
                    queue.extend(self.read_by(a));
                }
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Stmt;

    /// `a` reads `pod` through the compiler's `mid`, `pod` reads `env`:
    /// both emptied, a chain. `b` reads `kept`, which still has rows: the
    /// walk stops there, though `kept` reads `env`.
    #[test]
    fn a_chain_walks_the_compilers_relations_and_stops_at_rows() {
        let program = crate::parser::parse_program(
            r#"
            want("t", "a") where mid(_)
            mid(x) where pod(x)
            pod(x) where env(x)
            want("t", "b") where kept(_)
            kept(x) where env(x)
            "#,
        )
        .unwrap();
        let rules: Vec<RuleStmt> = program
            .statements
            .iter()
            .filter_map(|s| match s {
                Stmt::Rule(r) => Some(r.clone()),
                _ => None,
            })
            .collect();
        let then = Derived {
            rules: BTreeMap::new(),
            rows: [("pod", 2), ("env", 1), ("kept", 1)]
                .map(|(r, n)| (r.to_string(), n))
                .into(),
        };
        let now = [("pod", Some(0)), ("env", Some(0)), ("kept", Some(1))]
            .map(|(r, n)| (r.to_string(), n))
            .into();
        let at = |name: &str| Address {
            typ: "t".into(),
            name: name.into(),
        };
        let deleted = [at("a"), at("b")].into();
        let e = emptied(&then, &rules, &now, &deleted, &|_| None);
        let lost = |r: &str, rows| Lost {
            relation: r.into(),
            rows,
        };
        assert_eq!(
            e.headlines,
            vec![Emptied {
                name: "pod".into(),
                statement: None,
                deleted: vec![at("a").to_string()],
                because: None,
                lost: vec![lost("pod", 2), lost("env", 1)],
            }]
        );
        assert!(e.unread.is_empty(), "{:?}", e.unread);
    }
}
