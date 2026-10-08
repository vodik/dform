//! A relation the program answers on demand (R-187): a let with
//! parameters, `let f(a, b) = t`, is the relation `f(a, b, v)` whose
//! columns but the last are bound by the literal that reads it, as a
//! provider's table's `+` columns are (docs/grammar.md "Functions": the
//! program, the host and a provider are the three answerers). Its rule is
//! `f(A, B, V) :- f?(A, B), .., V = t`: the demand `f?` binds the
//! parameters, and the resolver puts it first in the body, so a helper of
//! the clause (`not { }`) is bound by it too.
//!
//! A literal that reads `f`, a call `f(x, y)` in a term or `f(x, y, v)`
//! after `where`, is a site, and the call is answered there: the site's
//! demand is the body before the literal, `S::f?(X, Y) :- ..`, and the
//! site reads its own copy `S` of the rule and its helpers, `S::f(A, B, V)
//! :- S::f?(A, B), ..` ([`Sites`]). One demand for every site would be one relation fed by
//! every caller's body: a caller reading an attribute (`labels(d.name)`)
//! above a caller writing it (`metadata.labels = labels("web")`) would be
//! a cycle through the attribute that the program does not have. A copy
//! per site stratifies where the site does, as the call written out would.
//! `f` itself is the union of its sites' rows, for `why` and `query`.
//!
//! What is refused, before: a let with parameters that calls itself,
//! directly or through another one, and one that reads a data source (a
//! document, a provider's table, the environment, the clock): a let is
//! the program's answer, and a program's answer is pure.

use crate::ast::{Atom, Lit, Program, RuleStmt, Span, Stmt, Term};
use crate::diag::{Diagnostic, Diagnostics};
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The demand of the let `f`: the relation of the parameters its readers
/// bind.
pub fn of(f: &str) -> String {
    format!("{f}?")
}

/// The lets with parameters of `program` (their relations as expansion
/// named them) answered at each site; the `Mode` statements gone.
/// `externs` are the data sources' relations, which a let may not read.
pub fn answer(program: Program, externs: &BTreeSet<String>) -> Result<Program> {
    let modes: BTreeMap<String, Span> = program
        .statements
        .iter()
        .filter_map(|s| match s {
            Stmt::Mode(e) => Some((e.pred.clone(), e.span)),
            _ => None,
        })
        .collect();
    if modes.is_empty() {
        return Ok(program);
    }
    let lets = Lets::of(&program, &modes);
    let mut diags = lets.impure(externs);
    diags.extend(lets.recursive());
    if !diags.is_empty() {
        return Err(Diagnostics(diags).into());
    }
    Ok(lets.specialise(program))
}

/// The lets with parameters and the rules of each: its own and its
/// clause's helpers, every rule its demand binds.
struct Lets {
    modes: BTreeMap<String, Span>,
    family: BTreeMap<String, Vec<RuleStmt>>,
}

impl Lets {
    fn of(program: &Program, modes: &BTreeMap<String, Span>) -> Lets {
        let demands: BTreeMap<String, &String> = modes.keys().map(|f| (of(f), f)).collect();
        let mut family: BTreeMap<String, Vec<RuleStmt>> = BTreeMap::new();
        for s in &program.statements {
            let Stmt::Rule(r) = s else { continue };
            let owner = r.body.iter().find_map(|l| match l {
                Lit::Pos(a) => demands.get(&a.pred).copied(),
                _ => None,
            });
            if let Some(f) = owner {
                family.entry(f.clone()).or_default().push(r.clone());
            }
        }
        Lets {
            modes: modes.clone(),
            family,
        }
    }

    /// The rules of `f` and its helpers.
    fn rules(&self, f: &str) -> &[RuleStmt] {
        self.family.get(f).map_or(&[], Vec::as_slice)
    }

    /// Whether `r` is one of a let's own rules (or a helper of one).
    fn owns(&self, r: &RuleStmt) -> bool {
        let demands: BTreeSet<String> = self.modes.keys().map(|f| of(f)).collect();
        r.body
            .iter()
            .any(|l| matches!(l, Lit::Pos(a) if demands.contains(&a.pred)))
    }

    /// A read of a data source in a let's rules: an error at the read.
    fn impure(&self, externs: &BTreeSet<String>) -> Vec<Diagnostic> {
        let mut out = Vec::new();
        for f in self.modes.keys() {
            for r in self.rules(f) {
                for l in &r.body {
                    let (Lit::Pos(a) | Lit::Not(a)) = l else {
                        continue;
                    };
                    if !externs.contains(&a.pred) {
                        continue;
                    }
                    let what = match crate::tables::describe(&a.pred) {
                        Some(d) => format!("a {d}"),
                        None => format!("`{}`", program_name(&a.pred)),
                    };
                    let name = program_name(f);
                    out.push(
                        Diagnostic::error(
                            a.span,
                            format!(
                                "let {name} reads {what}: a let with parameters is pure, its \
                                 value its arguments' alone"
                            ),
                        )
                        .with_help(format!(
                            "read it in a `let` of its own and pass the value to {name} as a \
                             parameter"
                        )),
                    );
                }
            }
        }
        out
    }

    /// A let that calls itself, directly or through others: an error
    /// naming the calls.
    fn recursive(&self) -> Vec<Diagnostic> {
        let calls: BTreeMap<&String, BTreeSet<&String>> = self
            .modes
            .keys()
            .map(|f| {
                let called = self
                    .rules(f)
                    .iter()
                    .flat_map(|r| &r.body)
                    .filter_map(|l| match l {
                        Lit::Pos(a) | Lit::Not(a) => self.modes.get_key_value(&a.pred),
                        _ => None,
                    })
                    .map(|(g, _)| g)
                    .collect();
                (f, called)
            })
            .collect();
        let mut out = Vec::new();
        let mut reported = BTreeSet::new();
        for f in self.modes.keys() {
            let Some(path) = cycle(f, &calls) else {
                continue;
            };
            let members: BTreeSet<&String> = path.iter().copied().collect();
            if !reported.insert(members) {
                continue;
            }
            let names: Vec<String> = path.iter().map(|g| program_name(g)).collect();
            let name = program_name(f);
            out.push(
                Diagnostic::error(
                    self.modes[f],
                    format!("let {name} calls itself: {}", names.join(" calls ")),
                )
                .with_note(
                    "a let with parameters is answered where it is called, once its \
                     arguments are known; a call of itself waits on its own answer",
                ),
            );
        }
        out
    }

    /// `program` with each site's call answered by its own copy, every
    /// let's own rules and `Mode` statements gone, and each let the union
    /// of its copies.
    fn specialise(self, program: Program) -> Program {
        let mut kept = Vec::new();
        let mut queue = std::collections::VecDeque::new();
        for s in program.statements {
            match s {
                Stmt::Mode(_) => {}
                Stmt::Rule(r) if self.owns(&r) => {}
                Stmt::Rule(r) => queue.push_back((r, None)),
                other => kept.push(other),
            }
        }
        let mut sites = Sites::default();
        let mut rules = Vec::new();
        let mut unions = Vec::new();
        // A copy's own sites are answered after it, by copies of their own.
        while let Some((mut r, within)) = queue.pop_front() {
            for j in 0..r.body.len() {
                let (Lit::Pos(a) | Lit::Not(a)) = &r.body[j] else {
                    continue;
                };
                let f = a.pred.clone();
                if !self.modes.contains_key(&f) || a.args.is_empty() {
                    continue;
                }
                let (scope, new) = sites.scope(&f, a.span, within.as_deref());
                let copied = format!("{scope}::{}", program_name(&f));
                rules.push(RuleStmt {
                    head: Atom {
                        pred: of(&copied),
                        args: a.args[..a.args.len() - 1].to_vec(),
                        record: None,
                        span: a.span,
                    },
                    body: r.body[..j].to_vec(),
                });
                if new {
                    unions.push(union(&f, &copied, a, self.columns(&f, a.args.len())));
                    let names = self.renames(&f, &scope);
                    for c in self.rules(&f) {
                        queue.push_back((rename(c, &names), Some(scope.clone())));
                    }
                }
                if let Lit::Pos(a) | Lit::Not(a) = &mut r.body[j] {
                    a.pred = copied;
                }
            }
            rules.push(r);
        }
        kept.extend(rules.into_iter().map(Stmt::Rule));
        kept.extend(unions.into_iter().map(Stmt::Rule));
        Program {
            statements: kept,
            stack: program.stack,
        }
    }

    /// The columns of `f`, `n` of them, as variables named as the let
    /// names them: its parameters, then `Value`.
    fn columns(&self, f: &str, n: usize) -> Vec<Term> {
        let own = self.rules(f).iter().find(|r| r.head.pred == f);
        let mut seen = BTreeSet::new();
        (0..n)
            .map(|k| {
                let named = own.and_then(|r| match r.head.args.get(k) {
                    Some(Term::Var(v)) if k + 1 < n => Some(v.clone()),
                    _ => None,
                });
                let mut v = named.unwrap_or_else(|| match k + 1 == n {
                    true => "Value".to_string(),
                    false => "Scope".to_string(),
                });
                while !seen.insert(v.clone()) {
                    v.push('_');
                }
                Term::Var(v)
            })
            .collect()
    }

    /// The names of `f`'s rules in its copy `scope`: the relation, its
    /// demand, and its helpers.
    fn renames(&self, f: &str, scope: &str) -> BTreeMap<String, String> {
        let copy = |p: &str| format!("{scope}::{}", program_name(p));
        let mut out: BTreeMap<String, String> = self
            .rules(f)
            .iter()
            .map(|r| (r.head.pred.clone(), copy(&r.head.pred)))
            .collect();
        out.insert(f.to_string(), copy(f));
        out.insert(of(f), of(&copy(f)));
        out
    }
}

/// The copies made so far, one per site: a call where it is written, in
/// the copy around it. A site's copy is named for where it is, `call at
/// a.df:3:9` (`call at b.df:2:5 from a.df:3:9` inside another's), so
/// `why` prints its rows as the call's (`f(1, 2, v)   (in call at
/// a.df:3:9)`); the rules lowered from one statement are one site.
#[derive(Default)]
struct Sites {
    by: BTreeMap<(String, u32, u32, u32, u32, Option<String>), String>,
    used: BTreeSet<String>,
}

impl Sites {
    /// The scope of `f`'s copy for the literal at `span` in the copy
    /// `within`, and whether it is new.
    fn scope(&mut self, f: &str, span: Span, within: Option<&str>) -> (String, bool) {
        let key = (
            f.to_string(),
            span.file,
            span.start,
            span.end,
            span.origin,
            within.map(str::to_string),
        );
        if let Some(s) = self.by.get(&key) {
            return (s.clone(), false);
        }
        let at = crate::diag::at(span).unwrap_or_else(|| "the compiler's".to_string());
        let base = match within.and_then(|w| w.strip_prefix("call at ")) {
            Some(w) => format!("call at {at} from {w}"),
            None => format!("call at {at}"),
        };
        let mut scope = base.clone();
        let mut n = 1;
        while self.used.contains(&scope) {
            n += 1;
            scope = format!("{base} #{n}");
        }
        self.used.insert(scope.clone());
        self.by.insert(key, scope.clone());
        (scope, true)
    }
}

/// `f(A, .., V) :- copy(A, .., V)`: the copy's rows are `f`'s, its
/// columns named as the let's.
fn union(f: &str, copy: &str, site: &Atom, columns: Vec<Term>) -> RuleStmt {
    RuleStmt {
        head: Atom {
            pred: f.to_string(),
            args: columns.clone(),
            record: None,
            span: site.span,
        },
        body: vec![Lit::Pos(Atom {
            pred: copy.to_string(),
            args: columns,
            record: None,
            span: site.span,
        })],
    }
}

/// `r` with the relations `names` renames renamed, its head and body.
fn rename(r: &RuleStmt, names: &BTreeMap<String, String>) -> RuleStmt {
    let atom = |mut a: Atom| {
        if let Some(n) = names.get(&a.pred) {
            a.pred = n.clone();
        }
        a
    };
    RuleStmt {
        head: atom(r.head.clone()),
        body: r.body.iter().cloned().map(|l| l.map(atom, |t| t)).collect(),
    }
}

/// The calls from `f` back to `f`, in order, `f` first and last.
fn cycle<'a>(
    f: &'a String,
    calls: &BTreeMap<&'a String, BTreeSet<&'a String>>,
) -> Option<Vec<&'a String>> {
    fn walk<'a>(
        at: &'a String,
        goal: &'a String,
        calls: &BTreeMap<&'a String, BTreeSet<&'a String>>,
        path: &mut Vec<&'a String>,
        seen: &mut BTreeSet<&'a String>,
    ) -> bool {
        for g in calls.get(at).into_iter().flatten() {
            if *g == goal {
                path.push(g);
                return true;
            }
            if seen.insert(g) {
                path.push(g);
                if walk(g, goal, calls, path, seen) {
                    return true;
                }
                path.pop();
            }
        }
        false
    }
    let mut path = vec![f];
    walk(f, f, calls, &mut path, &mut BTreeSet::new()).then_some(path)
}

/// A relation as the program names it: a module's by its module and name.
fn program_name(pred: &str) -> String {
    pred.replace("::", ".")
}
