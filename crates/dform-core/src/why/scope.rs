//! What a name means where it is read (R-184): `why NAME`, NAME a bare
//! name (the stack's) or `SCOPE.NAME`, SCOPE a copy of a component
//! (`forgejo_backup.repository`, or as `c[t]` reads it,
//! `volume["forgejo_backup"].repository`) or a used module, answered
//! from what the evaluation made. A copy reads its own names, then its
//! user's; what its module declares is private to the module, read
//! through it (`backups.repository`). A module reads its own, then its
//! user's. The language server's hover on a name in a component says the
//! same, per copy ([`in_component`]).

use crate::ast::{Atom, Term};
use crate::engine::EvalResult;
use crate::report::{self, tree};
use crate::value::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// A scope a name is read in.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Scope {
    /// The stack's top level.
    Stack,
    /// A used module, by the name its `use` binds.
    Module(String),
    /// A copy of a component.
    Copy(Copy),
}

/// A copy of a component, `instance_of(path, user, name)`: its scope
/// (`forgejo_backup`), its component's path (`backups.volume`), the
/// scope that made it (`""` the stack).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Copy {
    name: String,
    path: String,
    user: String,
}

impl Copy {
    /// The component's name, its path's last segment (`volume`).
    fn component(&self) -> &str {
        self.path.rsplit('.').next().unwrap_or(&self.path)
    }

    /// The module the component is an item of, by the name its `use`
    /// binds: its path's segment before the component's (`backups`).
    fn module(&self) -> Option<&str> {
        let (m, _) = self.path.rsplit_once('.')?;
        m.rsplit('.').next()
    }

    /// Whether it is a copy of the component `c` (a path, or its last
    /// segments).
    fn of(&self, c: &str) -> bool {
        self.path == c || self.path.ends_with(&format!(".{c}"))
    }
}

/// What a name denotes: how `why` heads it (`k8s.secret
/// backups.repository`, `let backups.ns`, `key env`), its kind, the
/// name it is read by from the stack, and where it is written.
#[derive(Debug, Clone)]
struct Denoted {
    head: String,
    kind: &'static str,
    qualified: String,
    site: Option<String>,
}

impl Denoted {
    /// `HEAD  SITE`.
    fn line(&self) -> String {
        match &self.site {
            Some(s) => format!("{}  {s}", self.head),
            None => self.head.clone(),
        }
    }
}

/// The evaluation's scopes and the names each declares.
struct Names<'a> {
    res: &'a EvalResult,
    printer: tree::Printer<'a>,
    stack_keys: &'a BTreeSet<String>,
    top: Option<&'a Path>,
    copies: Vec<Copy>,
}

impl<'a> Names<'a> {
    fn of(
        res: &'a EvalResult,
        redact: &'a crate::query::Redactor,
        stack_keys: &'a BTreeSet<String>,
        top: Option<&'a Path>,
    ) -> Names<'a> {
        let copies = res
            .facts
            .iter()
            .filter(|a| a.pred == crate::modules::INSTANCE_OF)
            .filter_map(|a| match a.args.as_slice() {
                [
                    Term::Val(Value::Str(path)),
                    Term::Val(Value::Str(user)),
                    Term::Val(Value::Str(name)),
                ] => Some(Copy {
                    name: join(user, name),
                    path: path.clone(),
                    user: user.clone(),
                }),
                _ => None,
            })
            .collect();
        Names {
            res,
            printer: tree::Printer {
                circuit: &res.circuit,
                redact,
                all: false,
            },
            stack_keys,
            top,
            copies,
        }
    }

    /// The scope a name of `user`'s scope is: a copy, a used module, or
    /// the stack (`""`).
    fn scope(&self, s: &str) -> Option<Scope> {
        if s.is_empty() {
            return Some(Scope::Stack);
        }
        if let Some(c) = self.copy(s) {
            return Some(Scope::Copy(c.clone()));
        }
        let cells = [crate::modules::INPUT, crate::modules::LET];
        let used = self.res.facts.iter().any(|a| {
            a.pred == "attr"
                && matches!(a.args.as_slice(), [Term::Val(Value::Str(k)), Term::Val(Value::Str(n)), ..]
                    if cells.contains(&k.as_str()) && n == s)
        }) || self.resources().any(|(_, n)| n.starts_with(&format!("{s}.")));
        used.then(|| Scope::Module(s.to_string()))
    }

    /// The copy whose scope is `s`.
    fn copy(&self, s: &str) -> Option<&Copy> {
        self.copies.iter().find(|c| c.name == s)
    }

    /// The resources the program wants: `(type, name)`.
    fn resources(&self) -> impl Iterator<Item = (&str, &str)> {
        self.res
            .facts
            .iter()
            .filter(|a| a.pred == "want")
            .filter_map(|a| match a.args.as_slice() {
                [Term::Val(Value::Str(t)), Term::Val(Value::Str(n))] => {
                    Some((t.as_str(), n.as_str()))
                }
                _ => None,
            })
    }

    /// What scope `s` (`""` the stack) itself declares named `n`: a
    /// resource, an input or a key, a `let`, an output, a copy.
    fn own(&self, s: &str, n: &str) -> Option<Denoted> {
        let qualified = join(s, n);
        if let Some((t, name)) = self.resources().find(|(_, name)| *name == qualified) {
            let addr = crate::ir::Address {
                typ: t.to_string(),
                name: name.to_string(),
            };
            let site = self
                .printer
                .want_site(&self.res.rules, &addr)
                .map(|w| self.relative(&w.at));
            return Some(Denoted {
                head: report::address(&addr),
                kind: "resource",
                qualified,
                site,
            });
        }
        let kinds = [
            (crate::modules::INPUT, "input"),
            (crate::modules::LET, "let"),
            (crate::transform::OUTPUT, "output"),
        ];
        for (k, kind) in kinds {
            let Some(f) = self.res.facts.iter().find(|a| cell(a, k, s, n)) else {
                continue;
            };
            let kind = match kind {
                "input" if s.is_empty() && self.stack_keys.contains(n) => "key",
                kind => kind,
            };
            let site = self
                .printer
                .attr_chain(&self.res.rules, f, &[], self.stack_keys)
                .into_iter()
                .map(|step| step.at)
                .find(|at| !at.is_empty())
                .map(|at| self.relative(&at));
            return Some(Denoted {
                head: format!("{kind} {qualified}"),
                kind,
                qualified,
                site,
            });
        }
        let c = self.copy(&qualified)?;
        Some(Denoted {
            head: format!("{} {qualified}", c.path),
            kind: "copy",
            qualified,
            site: None,
        })
    }

    fn relative(&self, at: &str) -> String {
        self.top
            .and_then(|t| report::relative_place(at, t))
            .unwrap_or_else(|| at.to_string())
    }

    /// `n` read in `scope`: what it denotes there, else why nothing.
    fn resolve(&self, scope: &Scope, n: &str) -> Resolved {
        match scope {
            Scope::Stack => match self.own("", n) {
                Some(d) => Resolved::Own(d),
                None => Resolved::Nothing(self.elsewhere(n)),
            },
            Scope::Module(m) => match self.own(m, n) {
                Some(d) => Resolved::Own(d),
                None => match self.own("", n) {
                    Some(d) => Resolved::Outward("the stack's".into(), d),
                    None => Resolved::Nothing(None),
                },
            },
            Scope::Copy(c) => {
                if let Some(d) = self.own(&c.name, n) {
                    return Resolved::Own(d);
                }
                // What the component's module declares is private to it
                // (R-183).
                if let Some(d) = c.module().and_then(|m| self.own(m, n)) {
                    return Resolved::Private(d);
                }
                let outer = self.scope(&c.user).unwrap_or(Scope::Stack);
                match self.resolve(&outer, n) {
                    Resolved::Own(d) | Resolved::Outward(_, d) => {
                        Resolved::Outward(format!("{}'s", describe(&outer)), d)
                    }
                    _ => Resolved::Nothing(None),
                }
            }
        }
    }

    /// The name `n` declared in another scope than the stack's, the first
    /// a used module's (a scope of cells or resources that is no copy),
    /// else a copy's: what reads it from the stack.
    fn elsewhere(&self, n: &str) -> Option<(String, Denoted)> {
        let cells = [crate::modules::INPUT, crate::modules::LET];
        let modules: BTreeSet<&str> = self
            .res
            .facts
            .iter()
            .filter_map(|a| match (a.pred.as_str(), a.args.as_slice()) {
                ("attr", [Term::Val(Value::Str(k)), Term::Val(Value::Str(s)), ..])
                    if cells.contains(&k.as_str()) =>
                {
                    Some(s.as_str())
                }
                _ => None,
            })
            .chain(
                self.resources()
                    .filter_map(|(_, r)| Some(r.split_once('.')?.0)),
            )
            .filter(|s| !s.is_empty() && self.copy(s).is_none())
            .collect();
        let copies = self.copies.iter().map(|c| c.name.as_str());
        modules
            .into_iter()
            .chain(copies)
            .filter_map(|s| Some((self.scope(s)?, s)))
            .find_map(|(scope, s)| self.own(s, n).map(|d| (describe(&scope), d)))
    }
}

/// What a name read in a scope resolved to.
enum Resolved {
    /// What the scope itself declares.
    Own(Denoted),
    /// What its user's scope declares, read outward, by whose.
    Outward(String, Denoted),
    /// What the copy's module declares, private to it.
    Private(Denoted),
    /// Nothing; the name another scope declares, by whose.
    Nothing(Option<(String, Denoted)>),
}

/// A scope as an answer names it: `volume forgejo_backup`, `module
/// backups`, `the stack`.
fn describe(s: &Scope) -> String {
    match s {
        Scope::Stack => "the stack".into(),
        Scope::Module(m) => format!("module {m}"),
        Scope::Copy(c) => format!("{} {}", c.component(), c.name),
    }
}

/// The answer to `n` read in `scope`, `resolved` so: one line (R-109's
/// form: what, then the fix); for what it denotes outside the scope, the
/// name `why` explains it by.
fn answer(scope: &Scope, n: &str, resolved: Resolved) -> (String, Option<String>) {
    let lead = match scope {
        Scope::Stack => n.to_string(),
        s => format!("{n} in {}", describe(s)),
    };
    let here = match scope {
        Scope::Stack => "in the stack",
        Scope::Module(_) => "in this module",
        Scope::Copy(_) => "in this copy",
    };
    let read = |whose: &str, d: &Denoted| {
        let at = d
            .site
            .as_ref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        format!(
            "{whose} {} is {}{at}, read it as {}",
            d.kind, d.qualified, d.qualified
        )
    };
    match resolved {
        Resolved::Own(d) => (format!("{lead}: {}", d.line()), None),
        Resolved::Outward(whose, d) => (format!("{lead}: {whose} {}", d.line()), Some(d.qualified)),
        Resolved::Private(d) => (
            format!("{lead}: no such name {here}; {}", read("the module's", &d)),
            None,
        ),
        Resolved::Nothing(Some((whose, d))) => (
            format!(
                "{lead}: no such name {here}; {}",
                read(&format!("{whose}'s"), &d)
            ),
            None,
        ),
        Resolved::Nothing(None) => (format!("{lead}: no such name {here}"), None),
    }
}

/// A cell fact `attr(kind, scope, n[.leaf], _)`.
fn cell(a: &Atom, kind: &str, scope: &str, n: &str) -> bool {
    a.pred == "attr"
        && matches!(a.args.as_slice(), [
            Term::Val(Value::Str(k)),
            Term::Val(Value::Str(s)),
            Term::Val(Value::Str(p)),
            _,
        ] if k == kind && s == scope
            && (p == n || p.strip_prefix(n).is_some_and(|r| r.starts_with('.'))))
}

/// `name` in `scope` (`""` the stack's).
fn join(scope: &str, name: &str) -> String {
    match scope.is_empty() {
        true => name.to_string(),
        false => format!("{scope}.{name}"),
    }
}

/// `C["copy"].rest`, a copy as `c[t]` names it, as `copy.rest`; `None`
/// when `pattern` is not that.
pub(super) fn normal(pattern: &str, res: &EvalResult) -> Option<String> {
    let (c, rest) = pattern.split_once("[\"")?;
    let (copy, rest) = rest.split_once("\"]")?;
    let rest = match rest {
        "" => "",
        r => r.strip_prefix('.')?,
    };
    let is_copy = res
        .facts
        .iter()
        .filter(|a| a.pred == crate::modules::INSTANCE_OF)
        .any(|a| match a.args.as_slice() {
            [Term::Val(Value::Str(path)), _, Term::Val(Value::Str(name))] => {
                name == copy && (path == c || path.ends_with(&format!(".{c}")))
            }
            _ => false,
        });
    is_copy.then(|| join(copy, rest).trim_end_matches('.').to_string())
}

/// `why NAME` of a name nothing the pattern names derives (R-184): the
/// name read in its scope, what it denotes there with its site, else
/// that nothing does and what reads it. `None` when `pattern` is no such
/// name (a relation, an attribute of what the scope declares): `why`'s
/// why-not answers it.
pub(super) fn why_name(pattern: &str, cx: &super::Context) -> Option<(String, Option<String>)> {
    let res = cx.res;
    // A segment is a name, or a copy's name its clause gave (`agent-0`,
    // R-191).
    let plain = !pattern.is_empty()
        && pattern.split('.').all(|s| {
            !s.is_empty()
                && s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        });
    if !plain
        || res.facts.iter().any(|a| a.pred == pattern)
        || res.rules.iter().any(|r| r.head.pred == pattern)
    {
        return None;
    }
    let names = Names::of(res, cx.redact, cx.stack_keys, cx.top);
    let segs: Vec<&str> = pattern.split('.').collect();
    // The longest prefix that is a scope, else the stack.
    let (scope, at) = (1..segs.len())
        .rev()
        .find_map(|i| match names.scope(&segs[..i].join(".")) {
            Some(Scope::Stack) | None => None,
            Some(s) => Some((s, i)),
        })
        .unwrap_or((Scope::Stack, 0));
    let n = segs[at];
    let rest = &segs[at + 1..];
    let resolved = names.resolve(&scope, n);
    match (&scope, &resolved) {
        // What the scope declares: `why` has answered it, or its why-not
        // will (an attribute nothing sets).
        (_, Resolved::Own(_)) => return None,
        // A dotted name of no scope that names nothing anywhere: a type,
        // an address, the why-not's.
        (Scope::Stack, Resolved::Nothing(None)) if !rest.is_empty() => return None,
        _ => {}
    }
    let (line, outward) = answer(&scope, n, resolved);
    let outward = outward.map(|q| rest.iter().fold(q, |p, s| format!("{p}.{s}")));
    Some((format!("{line}\n"), outward))
}

/// What the name `n` read bare in component `path`'s body denotes in
/// each copy of it the evaluation made (the language server's hover,
/// R-184): one line each, as `why COPY.n` says it.
pub fn in_component(
    path: &str,
    n: &str,
    res: &EvalResult,
    redact: &crate::query::Redactor,
    stack_keys: &BTreeSet<String>,
    top: Option<&Path>,
) -> Vec<String> {
    let names = Names::of(res, redact, stack_keys, top);
    names
        .copies
        .iter()
        .filter(|c| c.of(path))
        .map(|c| {
            let scope = Scope::Copy(c.clone());
            answer(&scope, n, names.resolve(&scope, n)).0
        })
        .collect()
}
