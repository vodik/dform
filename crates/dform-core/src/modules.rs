//! Modules, instances and interfaces (E DR-3, DESIGN.org R-5).
//!
//! An `instance m i { k = V } :- B` is module `m`'s body under the scope
//! `m.i`:
//!
//! - resource names are scoped (`m.i::name`, its address `T["m.i::name"]`);
//! - every predicate the module defines is private to the instance
//!   (`m.i::p`, a name no source can spell); a value leaves the instance
//!   through an output the stack wires;
//! - `input k: T [= D] [where R]` is read inside as `k(V)`: the collapsed
//!   cell `(input, m.i, k)` of the attribute aggregate, where the instance's
//!   `k = V :- B` contributes at the normal rank and `D` at `@default`;
//! - `output k: T` declares an output and `output k = t` (or a rule for
//!   `output(k, V)`) defines it, readable anywhere as `output(m.i, k, V)`.
//!
//! A module body reads every global relation. A policy pack is a module
//! applied once, with no scope on resource names: its predicates are private
//! too. What it writes is not granted: ranks are the ownership model, and
//! the stratifier partitions a write by its head's constant type and path.

use crate::ast::{
    Atom, FieldAssign, InputDecl, Lit, OutputDecl, Program, Resource, RuleStmt, Settings, Span,
    Stmt, Term, TypeExpr,
};
use crate::diag::{self, Diagnostic, Diagnostics};
use crate::inputs::Declared;
use crate::value::Value;
use anyhow::Result;
use std::collections::{BTreeMap, BTreeSet};

/// The attribute aggregate's pseudo-type for inputs: `(input, Scope, key)`,
/// scope `""` for the stack's own.
pub const INPUT: &str = "input";

/// The attribute aggregate's pseudo-type for `let` (R-3): `(let, Scope,
/// k)`, scope `""` for the program's own, `m.i` in an instance, the pack's
/// name in a pack. The resolver writes a `let` as `let(k, t, rank)`, a
/// head no source can spell, and `lets` makes it the cell's contribution.
pub const LET: &str = "let";

fn str_term(s: &str) -> Term {
    Term::Val(Value::Str(s.to_string()))
}

fn atom(pred: &str, args: Vec<Term>, span: Span) -> Atom {
    Atom {
        pred: pred.into(),
        args,
        record: None,
        span,
    }
}

/// A head with no body and no variables is a fact; anything else a rule.
fn fact_or_rule(head: Atom, body: Vec<Lit>) -> Stmt {
    if body.is_empty() && head.args.iter().all(is_ground) {
        Stmt::Fact(head)
    } else {
        Stmt::Rule(RuleStmt { head, body })
    }
}

fn is_ground(t: &Term) -> bool {
    match t {
        Term::Var(_) | Term::Wildcard | Term::ListComp { .. } => false,
        Term::Val(_) => true,
        Term::Func { args, .. } => args.iter().all(is_ground),
        Term::List(xs) => xs.iter().all(is_ground),
        Term::Obj(m) => m.values().all(is_ground),
    }
}

/// A module's interface: what its top-level statements declare.
#[derive(Default)]
struct Interface {
    inputs: Vec<InputDecl>,
    outputs: BTreeMap<String, OutputDecl>,
    output_values: Vec<OutputDecl>,
}

/// Split a module or pack body into its interface and its statements.
fn interface(owner: &str, body: &[Stmt], diags: &mut Vec<Diagnostic>) -> (Interface, Vec<Stmt>) {
    let mut i = Interface::default();
    let mut rest = Vec::new();
    for s in body {
        match s {
            Stmt::Input(d) => {
                if i.inputs.iter().any(|x| x.name == d.name) {
                    diags.push(Diagnostic::error(
                        d.span,
                        format!("{owner} declares input {} twice", d.name),
                    ));
                }
                i.inputs.push(d.clone());
            }
            Stmt::Output(o) if o.value.is_none() => {
                if i.outputs.insert(o.name.clone(), o.clone()).is_some() {
                    diags.push(Diagnostic::error(
                        o.span,
                        format!("{owner} declares output {} twice", o.name),
                    ));
                }
            }
            Stmt::Output(o) => i.output_values.push(o.clone()),
            Stmt::Module(m) => diags.push(Diagnostic::error(
                m.span,
                format!("module {} inside {owner}: modules do not nest", m.name),
            )),
            Stmt::Instance(u) => diags.push(Diagnostic::error(
                u.span,
                format!(
                    "instance {} {} inside {owner}: instantiate modules at the top level and \
                     wire them through inputs and outputs",
                    u.module, u.name
                ),
            )),
            other => rest.push(other.clone()),
        }
    }
    (i, rest)
}

/// Every predicate the statements define (heads and facts), by name, with
/// the arity and span of its first definition.
fn defined_preds(stmts: &[Stmt], out: &mut BTreeMap<String, (usize, Span)>) {
    for s in stmts {
        match s {
            Stmt::Fact(a) | Stmt::Rule(RuleStmt { head: a, .. }) => {
                let (pred, arity) = match let_key(a) {
                    Some(k) => (k.to_string(), 1),
                    None => (a.pred.clone(), a.args.len()),
                };
                out.entry(pred).or_insert((arity, a.span));
            }
            _ => {}
        }
    }
}

/// The key `k` of a `let(k, t, rank)` head.
fn let_key(a: &Atom) -> Option<&str> {
    match (a.pred.as_str(), a.args.as_slice()) {
        (LET, [Term::Val(Value::Str(k)), _, _]) => Some(k),
        _ => None,
    }
}

/// Each `let(k, t, rank) :- B` of `stmts` as its contribution to the cell
/// `(let, scope, k)`, `arg("let", scope, "k", t, rank) :- B`, and one
/// reader per key, `k(V) :- attr("let", scope, "k", V)` (`k` renamed by
/// `names` in an instance or a pack): a read of `k` is the collapsed cell,
/// so two rows that agree are one value and two that disagree a conflict.
fn lets(stmts: Vec<Stmt>, scope: &str, names: Option<&Names>) -> Vec<Stmt> {
    let mut out = Vec::with_capacity(stmts.len());
    let mut keys: BTreeMap<String, Span> = BTreeMap::new();
    for s in stmts {
        let (head, body) = match s {
            Stmt::Fact(a) if let_key(&a).is_some() => (a, Vec::new()),
            Stmt::Rule(r) if let_key(&r.head).is_some() => (r.head, r.body),
            other => {
                out.push(other);
                continue;
            }
        };
        let [k, v, rank]: [Term; 3] = head.args.try_into().expect("let(k, t, rank)");
        let Term::Val(Value::Str(key)) = &k else {
            unreachable!("let_key")
        };
        keys.entry(key.clone()).or_insert(head.span);
        let arg = Atom {
            pred: "arg".into(),
            args: vec![str_term(LET), str_term(scope), k, v, rank],
            ..head
        };
        out.push(fact_or_rule(arg, body));
    }
    for (k, span) in keys {
        let pred = names
            .and_then(|n| n.get(&k))
            .cloned()
            .unwrap_or_else(|| k.clone());
        let v = Term::Var("V".into());
        out.push(Stmt::Rule(RuleStmt {
            head: atom(&pred, vec![v.clone()], span),
            body: vec![Lit::Pos(atom(
                "attr",
                vec![str_term(LET), str_term(scope), str_term(&k), v],
                span,
            ))],
        }));
    }
    out
}

/// Predicates a module may never make private: the compiler's, the
/// provider's and the policy heads.
fn is_shared(pred: &str) -> bool {
    crate::loader::is_core_pred(pred)
}

/// How a module's predicate names are renamed in one instance.
struct Names {
    /// Predicate name -> its private name (`m.i::p`, `pack::p`).
    map: BTreeMap<String, String>,
}

impl Names {
    fn get(&self, pred: &str) -> Option<&String> {
        self.map.get(pred)
    }
}

/// Expand `module`/`instance` and `policy`/`apply`: the program with every
/// instance's body scoped and renamed and every applied pack renamed.
/// A program with its modules and packs expanded, and the interface the
/// later passes check: every typed input, and every output declared
/// `secret(T)` (scope, key).
pub struct Expanded {
    pub program: Program,
    pub inputs: Vec<Declared>,
    pub secret_outputs: Vec<(String, String)>,
}

fn is_secret_type(t: &Option<TypeExpr>) -> bool {
    matches!(t, Some(TypeExpr::Apply(n, _)) if n == "secret")
}

pub fn expand(program: &Program) -> Result<Expanded> {
    let mut diags = Vec::new();
    let mut declared = Vec::new();
    let mut secret_outputs = Vec::new();
    let check_types = |who: &str, inputs: &[InputDecl], diags: &mut Vec<Diagnostic>| {
        for i in inputs {
            if let Err(e) = crate::inputs::check_type(&i.ty) {
                diags.push(Diagnostic::error(
                    i.span,
                    format!("{who} input {}: {e}", i.name),
                ));
            }
            let (_, rest) = crate::refine::split_input(i);
            diags.extend(crate::refine::check_rest(&rest, i.span));
        }
    };
    let mut modules: BTreeMap<String, (&crate::ast::Module, Interface, Vec<Stmt>)> =
        BTreeMap::new();
    let mut packs: BTreeMap<String, &crate::ast::PolicyPack> = BTreeMap::new();
    for s in &program.statements {
        match s {
            Stmt::Module(m) => {
                let owner = format!("module {}", m.name);
                let (i, rest) = interface(&owner, &m.body, &mut diags);
                check_module(m, &i, &rest, &mut diags);
                check_types(&format!("module {}", m.name), &i.inputs, &mut diags);
                if modules.insert(m.name.clone(), (m, i, rest)).is_some() {
                    diags.push(Diagnostic::error(
                        m.span,
                        format!("module {} is defined twice", m.name),
                    ));
                }
            }
            Stmt::PolicyPack(p) => {
                packs.insert(p.name.clone(), p);
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    // Private names by plain name, for the error when the program reads one.
    let mut private: BTreeMap<String, (String, Option<String>)> = BTreeMap::new();
    let mut instances: BTreeSet<String> = BTreeSet::new();
    for s in &program.statements {
        match s {
            Stmt::Module(_) | Stmt::PolicyPack(_) => {}
            // The stack's own input: read as `k(V)`, given by `--set` (an
            // `input(k, V)` fact) at the normal rank.
            Stmt::Input(i) => {
                check_types("stack", std::slice::from_ref(i), &mut diags);
                out.extend(input_reader("", i, &i.name));
                let v = Term::Var("V".into());
                out.push(Stmt::Rule(RuleStmt {
                    head: atom(
                        "arg",
                        vec![
                            str_term(INPUT),
                            str_term(""),
                            str_term(&i.name),
                            v.clone(),
                            str_term(crate::transform::NORMAL),
                        ],
                        i.span,
                    ),
                    body: vec![Lit::Pos(atom("input", vec![str_term(&i.name), v], i.span))],
                }));
                out.extend(refinement(i, ""));
                declared.push(Declared {
                    scope: String::new(),
                    decl: i.clone(),
                });
            }
            // The stack's own output: `output(k, V)` in the root scope.
            Stmt::Output(o) if o.value.is_none() => {
                if is_secret_type(&o.ty) {
                    secret_outputs.push((String::new(), o.name.clone()));
                }
            }
            Stmt::Output(o) => out.push(fact_or_rule(
                atom(
                    "output",
                    vec![str_term(&o.name), o.value.clone().unwrap()],
                    o.span,
                ),
                Vec::new(),
            )),
            Stmt::Instance(u) => {
                let scope = format!("{}.{}", u.module, u.name);
                if !instances.insert(scope.clone()) {
                    diags.push(Diagnostic::error(
                        u.span,
                        format!("instance {} {} is declared twice", u.module, u.name),
                    ));
                    continue;
                }
                let Some((m, iface, body)) = modules.get(&u.module) else {
                    diags.push(Diagnostic::error(
                        u.span,
                        format!(
                            "instance {} {} names an unknown module '{}'",
                            u.module, u.name, u.module
                        ),
                    ));
                    continue;
                };
                let origin = diag::origin_id(&format!("module {} instance {}", u.module, u.name));
                instance_inputs(u, &scope, iface, &mut out, &mut diags);
                let names = module_names(&scope, iface, body);
                for p in names.map.keys() {
                    let help = format!(
                        "pass the value through an output of module {}, \
                         `output {p} = ...`, and read it as {}.INSTANCE.{p}",
                        m.name, m.name
                    );
                    private.insert(p.clone(), (format!("module {}", m.name), Some(help)));
                }
                let mut stmts = module_stmts(&scope, iface, body);
                set_origin(&mut stmts, origin);
                let stmts = stmts
                    .into_iter()
                    .map(|st| rewrite_stmt(rename_stmt(st, &names), &scope))
                    .collect();
                out.extend(lets(stmts, &scope, Some(&names)));
                out.extend(input_readers(&scope, &iface.inputs, &names));
                secret_outputs.extend(
                    iface
                        .outputs
                        .values()
                        .filter(|o| is_secret_type(&o.ty))
                        .map(|o| (scope.clone(), o.name.clone())),
                );
                declared.extend(iface.inputs.iter().map(|i| Declared {
                    scope: scope.clone(),
                    decl: i.clone(),
                }));
            }
            other => out.push(other.clone()),
        }
    }

    // Packs: applied where `apply` is, in `apply` order.
    let mut applied = BTreeSet::new();
    let mut expanded = Vec::new();
    for s in out {
        let Stmt::ApplyPolicy(a) = &s else {
            expanded.push(s);
            continue;
        };
        let Some(p) = packs.get(&a.name) else {
            diags.push(Diagnostic::error(
                a.span,
                format!("apply {} names an unknown policy '{}'", a.name, a.name),
            ));
            continue;
        };
        if !applied.insert(a.name.clone()) {
            continue;
        }
        let owner = format!("policy {}", p.name);
        let (iface, body) = interface(&owner, &p.body, &mut diags);
        if let Some(i) = iface.inputs.first() {
            diags.push(Diagnostic::error(
                i.span,
                format!("{owner} declares an input: a policy pack has none"),
            ));
        }
        if let Some(o) = iface.outputs.values().chain(&iface.output_values).next() {
            diags.push(Diagnostic::error(
                o.span,
                format!("{owner} declares an output: a policy pack has none"),
            ));
        }
        let names = pack_names(&p.name, &body);
        for pr in names.map.keys() {
            private.insert(pr.clone(), (owner.clone(), None));
        }
        let mut body = body;
        set_origin(&mut body, diag::origin_id(&owner));
        let body = body.into_iter().map(|s| rename_stmt(s, &names)).collect();
        expanded.extend(lets(body, &p.name, Some(&names)));
    }
    // The program's own `let`s (a scenario's joined it).
    let expanded = lets(expanded, "", None);

    // A read of a name only a module defines: say it is private.
    let mut defined = BTreeMap::new();
    defined_preds(&expanded, &mut defined);
    let externs: BTreeSet<&str> = expanded
        .iter()
        .filter_map(|s| match s {
            Stmt::Extern(e) => Some(e.pred.as_str()),
            _ => None,
        })
        .collect();
    let mut reads = Vec::new();
    for s in &expanded {
        body_atoms(s, &mut reads);
    }
    for a in reads {
        if let Some((owner, help)) = private.get(&a.pred)
            && !defined.contains_key(&a.pred)
            && !externs.contains(a.pred.as_str())
        {
            let d = Diagnostic::error(
                a.span,
                format!("{}/{} is private to {owner}", a.pred, a.args.len()),
            );
            diags.push(match help {
                Some(h) => d.with_help(h.clone()),
                None => d,
            });
        }
    }

    if diags.is_empty() {
        Ok(Expanded {
            program: Program {
                statements: expanded,
            },
            inputs: declared,
            secret_outputs,
        })
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// The instance's `k = V :- B`, each a normal-rank contribution to its
/// input cell, with the checks against the module's declared inputs.
fn instance_inputs(
    u: &crate::ast::Instance,
    scope: &str,
    iface: &Interface,
    out: &mut Vec<Stmt>,
    diags: &mut Vec<Diagnostic>,
) {
    let declared: Vec<&str> = iface.inputs.iter().map(|i| i.name.as_str()).collect();
    for (k, v, span) in &u.inputs {
        if !declared.contains(&k.as_str()) {
            let d = Diagnostic::error(*span, format!("module {} has no input {k}", u.module));
            diags.push(if declared.is_empty() {
                d.with_note(format!("module {} declares no inputs", u.module))
            } else {
                d.with_note(format!("its inputs: {}", declared.join(", ")))
            });
            continue;
        }
        let head = atom(
            "arg",
            vec![
                str_term(INPUT),
                str_term(scope),
                str_term(k),
                v.clone(),
                str_term(crate::transform::NORMAL),
            ],
            *span,
        );
        out.push(fact_or_rule(head, u.body.clone().unwrap_or_default()));
    }
    for i in &iface.inputs {
        if i.default.is_none() && !u.inputs.iter().any(|(k, _, _)| *k == i.name) {
            diags.push(
                Diagnostic::error(
                    u.span,
                    format!(
                        "instance {} {} does not set required input {}",
                        u.module, u.name, i.name
                    ),
                )
                .with_label(
                    i.span,
                    format!(
                        "{}: {} declared here",
                        i.name,
                        crate::inputs::type_text(&i.ty)
                    ),
                )
                .with_help(format!("add `{} = ...` to the instance block", i.name)),
            );
        }
    }
}

/// Every predicate a module defines, its inputs (and their refinement
/// helpers) included.
fn module_defined(iface: &Interface, body: &[Stmt]) -> BTreeMap<String, (usize, Span)> {
    let mut defined = BTreeMap::new();
    defined_preds(body, &mut defined);
    for i in &iface.inputs {
        defined.insert(i.name.clone(), (1, i.span));
        defined.insert(refine_pred(&i.name), (1, i.span));
    }
    defined
}

/// The checks on a module's interface, once per module: an input is not
/// also defined, and every output it gives a value is declared.
fn check_module(
    m: &crate::ast::Module,
    iface: &Interface,
    body: &[Stmt],
    diags: &mut Vec<Diagnostic>,
) {
    let mut own = BTreeMap::new();
    defined_preds(body, &mut own);
    for i in &iface.inputs {
        if let Some((_, span)) = own.get(&i.name) {
            diags.push(Diagnostic::error(
                *span,
                format!(
                    "module {} defines {}, which is its input: an input is set by the instance",
                    m.name, i.name
                ),
            ));
        }
    }
    let undeclared = |k: &str, span: Span| {
        Diagnostic::error(span, format!("module {} has no output {k}", m.name))
            .with_help(format!("declare it: `output {k}: TYPE`"))
    };
    for o in &iface.output_values {
        if !iface.outputs.contains_key(&o.name) {
            diags.push(undeclared(&o.name, o.span));
        }
    }
    // `output(k, V)` written as a rule must name a declared output too.
    let mut heads = Vec::new();
    for s in body {
        head_atoms(s, &mut heads);
    }
    for h in heads {
        if h.pred == "output"
            && h.args.len() == 2
            && let Term::Val(Value::Str(k)) = &h.args[0]
            && !iface.outputs.contains_key(k)
        {
            diags.push(undeclared(k, h.span));
        }
    }
}

/// The instance names of a module's predicates: every one private.
fn module_names(scope: &str, iface: &Interface, body: &[Stmt]) -> Names {
    let map = module_defined(iface, body)
        .into_keys()
        .filter(|p| !is_shared(p))
        .map(|p| {
            let n = format!("{scope}::{p}");
            (p, n)
        })
        .collect();
    Names { map }
}

/// A pack's predicates: every one private.
fn pack_names(name: &str, body: &[Stmt]) -> Names {
    let mut defined = BTreeMap::new();
    defined_preds(body, &mut defined);
    let map = defined
        .into_keys()
        .filter(|p| !is_shared(p))
        .map(|p| {
            let n = format!("{name}::{p}");
            (p, n)
        })
        .collect();
    Names { map }
}

/// The module's statements for one instance, before scoping: its body, the
/// output values and each input's refinement.
fn module_stmts(scope: &str, iface: &Interface, body: &[Stmt]) -> Vec<Stmt> {
    let mut out: Vec<Stmt> = body.to_vec();
    for o in &iface.output_values {
        // An undeclared output is reported once, by `check_module`.
        let Some(decl) = iface.outputs.get(&o.name) else {
            continue;
        };
        let mut value = o.value.clone().expect("an output value");
        // An `addr` output names one of this instance's resources.
        if matches!(&decl.ty, Some(TypeExpr::Name(t)) if t == "addr") {
            value = Term::Func {
                name: "scoped".into(),
                args: vec![str_term(scope), value],
            };
        }
        out.push(fact_or_rule(
            atom("output", vec![str_term(&o.name), value], o.span),
            Vec::new(),
        ));
    }
    for i in &iface.inputs {
        out.extend(refinement(i, scope));
    }
    out
}

fn refine_pred(input: &str) -> String {
    format!("__refine_{input}")
}

/// `input k: T where R`: what of `R` fits the checkable table is a
/// refinement of the input's cell (`input_reader`, `crate::refine`); the
/// rest is a deny unless it holds of the input's value: `__refine_k(V) :-
/// k(V), R[k := V]` and `deny(...) :- k(V), not __refine_k(V)`. The input
/// is written by its name in `R`.
pub fn refinement(i: &InputDecl, scope: &str) -> Vec<Stmt> {
    let (_, rest) = crate::refine::split_input(i);
    if rest.is_empty() {
        return Vec::new();
    }
    let v = Term::Var("__Input".into());
    let read = Lit::Pos(atom(&i.name, vec![v.clone()], i.span));
    let mut body = vec![read.clone()];
    body.extend(rest.iter().map(|l| subst_lit(l, &i.name, &v)));
    let ok = atom(&refine_pred(&i.name), vec![v.clone()], i.span);
    let named = Term::Var(i.name.clone());
    let text = rest
        .iter()
        .map(|l| crate::partition::fmt_lit(&subst_lit(l, &i.name, &named)))
        .collect::<Vec<_>>()
        .join(", ");
    let who = if scope.is_empty() {
        format!("input {}", i.name)
    } else {
        format!("input {} of {scope}", i.name)
    };
    // A secret input's value is not printed.
    let ctx = if matches!(&i.ty, TypeExpr::Apply(n, _) if n == "secret") {
        BTreeMap::new()
    } else {
        BTreeMap::from([("value".to_string(), v.clone())])
    };
    let deny = atom(
        "deny",
        vec![
            str_term(&format!("{who} fails its refinement: {text}")),
            Term::Obj(ctx),
        ],
        i.span,
    );
    vec![
        Stmt::Rule(RuleStmt {
            head: ok.clone(),
            body,
        }),
        Stmt::Rule(RuleStmt {
            head: deny,
            body: vec![read, Lit::Not(ok)],
        }),
    ]
}

fn subst_lit(l: &Lit, name: &str, v: &Term) -> Lit {
    let t = |x: &Term| subst_term(x, name, v);
    let a = |x: &Atom| Atom {
        args: x.args.iter().map(t).collect(),
        ..x.clone()
    };
    match l {
        Lit::Pos(x) => Lit::Pos(a(x)),
        Lit::Not(x) => Lit::Not(a(x)),
        Lit::Eq(x, y) => Lit::Eq(t(x), t(y)),
        Lit::Neq(x, y) => Lit::Neq(t(x), t(y)),
        Lit::Gt(x, y) => Lit::Gt(t(x), t(y)),
        Lit::Ge(x, y) => Lit::Ge(t(x), t(y)),
        Lit::Lt(x, y) => Lit::Lt(t(x), t(y)),
        Lit::Le(x, y) => Lit::Le(t(x), t(y)),
    }
}

fn subst_term(t: &Term, name: &str, v: &Term) -> Term {
    match t {
        Term::Val(Value::Str(s)) if s == name => v.clone(),
        Term::Func { name: f, args } => Term::Func {
            name: f.clone(),
            args: args.iter().map(|a| subst_term(a, name, v)).collect(),
        },
        Term::List(xs) => Term::List(xs.iter().map(|a| subst_term(a, name, v)).collect()),
        Term::Obj(m) => Term::Obj(
            m.iter()
                .map(|(k, a)| (k.clone(), subst_term(a, name, v)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// `k(V) :- attr(input, Scope, k, V)` per input, and its `@default`
/// contribution. Written after scoping: the scope is already in the terms.
fn input_readers(scope: &str, inputs: &[InputDecl], names: &Names) -> Vec<Stmt> {
    let mut out = Vec::new();
    for i in inputs {
        let pred = names
            .get(&i.name)
            .cloned()
            .unwrap_or_else(|| i.name.clone());
        out.extend(input_reader(scope, i, &pred));
    }
    out
}

/// One input's reader rule and its default, with the reader named `pred`.
pub fn input_reader(scope: &str, i: &InputDecl, pred: &str) -> Vec<Stmt> {
    let v = Term::Var("V".into());
    let mut out = vec![Stmt::Rule(RuleStmt {
        head: atom(pred, vec![v.clone()], i.span),
        body: vec![Lit::Pos(atom(
            "attr",
            vec![str_term(INPUT), str_term(scope), str_term(&i.name), v],
            i.span,
        ))],
    })];
    let (checkable, _) = crate::refine::split_input(i);
    for c in checkable {
        out.push(crate::refine::refine_fact(
            INPUT,
            Some(scope),
            &i.name,
            &c,
            i.span,
        ));
    }
    if let Some(d) = &i.default {
        out.push(fact_or_rule(
            atom(
                "arg",
                vec![
                    str_term(INPUT),
                    str_term(scope),
                    str_term(&i.name),
                    d.clone(),
                    str_term(crate::ast::Rank::Default.name()),
                ],
                i.span,
            ),
            Vec::new(),
        ));
    }
    out
}

/// Every head atom of a statement (a resource's are its `want` and `arg`).
fn head_atoms(s: &Stmt, out: &mut Vec<Atom>) {
    match s {
        Stmt::Fact(a) => out.push(a.clone()),
        Stmt::Rule(r) => out.push(r.head.clone()),
        _ => {}
    }
}

/// Every body atom of a statement, comprehensions included.
fn body_atoms(s: &Stmt, out: &mut Vec<Atom>) {
    fn lits(ls: &[Lit], out: &mut Vec<Atom>) {
        for l in ls {
            match l {
                Lit::Pos(a) | Lit::Not(a) => {
                    out.push(a.clone());
                    a.args.iter().for_each(|t| term(t, out));
                }
                Lit::Eq(x, y)
                | Lit::Neq(x, y)
                | Lit::Gt(x, y)
                | Lit::Ge(x, y)
                | Lit::Lt(x, y)
                | Lit::Le(x, y) => {
                    term(x, out);
                    term(y, out);
                }
            }
        }
    }
    fn term(t: &Term, out: &mut Vec<Atom>) {
        match t {
            Term::ListComp { item, body } => {
                term(item, out);
                lits(body, out);
            }
            Term::Func { args, .. } | Term::List(args) => args.iter().for_each(|t| term(t, out)),
            Term::Obj(m) => m.values().for_each(|t| term(t, out)),
            _ => {}
        }
    }
    match s {
        Stmt::Fact(a) => a.args.iter().for_each(|t| term(t, out)),
        Stmt::Rule(r) => {
            r.head.args.iter().for_each(|t| term(t, out));
            lits(&r.body, out);
        }
        Stmt::Resource(r) => {
            r.fields.iter().for_each(|f| term(&f.value, out));
            lits(r.body.as_deref().unwrap_or_default(), out);
        }
        Stmt::Settings(st) => {
            st.fields.iter().for_each(|f| term(&f.value, out));
            lits(st.body.as_deref().unwrap_or_default(), out);
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Renaming predicates and scoping resource names.
// ---------------------------------------------------------------------------

fn rename_stmt(stmt: Stmt, names: &Names) -> Stmt {
    let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| rename_lit(l, names)).collect();
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rename_atom(a, names)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rename_atom(r.head, names),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            fields: rename_fields(r.fields, names),
            body: r.body.map(lits),
            ..r
        }),
        Stmt::Settings(s) => Stmt::Settings(Settings {
            fields: rename_fields(s.fields, names),
            body: s.body.map(lits),
            ..s
        }),
        Stmt::Mixed(mut e) => {
            if let Some(n) = names.get(&e.pred) {
                e.pred = n.clone();
            }
            Stmt::Mixed(e)
        }
        other => other,
    }
}

fn rename_fields(fs: Vec<FieldAssign>, names: &Names) -> Vec<FieldAssign> {
    fs.into_iter()
        .map(|f| FieldAssign {
            value: rename_term(f.value, names),
            ..f
        })
        .collect()
}

fn rename_lit(l: Lit, names: &Names) -> Lit {
    let t = |x| rename_term(x, names);
    match l {
        Lit::Pos(a) => Lit::Pos(rename_atom(a, names)),
        Lit::Not(a) => Lit::Not(rename_atom(a, names)),
        Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
        Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
        Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
        Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
        Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
        Lit::Le(a, b) => Lit::Le(t(a), t(b)),
    }
}

fn rename_atom(mut a: Atom, names: &Names) -> Atom {
    if let Some(n) = names.get(&a.pred) {
        a.pred = n.clone();
    }
    a.args = a.args.into_iter().map(|t| rename_term(t, names)).collect();
    a
}

fn rename_term(t: Term, names: &Names) -> Term {
    match t {
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rename_term(*item, names)),
            body: body.into_iter().map(|l| rename_lit(l, names)).collect(),
        },
        Term::Func { name, args } => Term::Func {
            name,
            args: args.into_iter().map(|t| rename_term(t, names)).collect(),
        },
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rename_term(t, names)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rename_term(v, names)))
                .collect(),
        ),
        other => other,
    }
}

/// Scope one statement of an instance: resource names in `want`, `arg`,
/// `attr`, `adopt` and `ref` become `scoped(Scope, Name)`, and `output(k, V)` is
/// `output(Scope, k, V)`.
fn rewrite_stmt(stmt: Stmt, scope: &str) -> Stmt {
    let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| rewrite_lit(l, scope)).collect();
    let fields = |fs: Vec<FieldAssign>| {
        fs.into_iter()
            .map(|f| FieldAssign {
                value: rewrite_term(f.value, scope),
                ..f
            })
            .collect()
    };
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom(a, scope)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rewrite_atom(r.head, scope),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: rewrite_term(r.typ, scope),
            name: scoped_term(scope, rewrite_term(r.name, scope)),
            fields: fields(r.fields),
            body: r.body.map(lits),
            ..r
        }),
        // Settings are addressed by environment, not by scope: only the
        // values and the body are rewritten.
        Stmt::Settings(s) => Stmt::Settings(Settings {
            fields: fields(s.fields),
            body: s.body.map(lits),
            ..s
        }),
        other => other,
    }
}

fn rewrite_lit(lit: Lit, scope: &str) -> Lit {
    let t = |x| rewrite_term(x, scope);
    match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom(a, scope)),
        Lit::Not(a) => Lit::Not(rewrite_atom(a, scope)),
        Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
        Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
        Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
        Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
        Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
        Lit::Le(a, b) => Lit::Le(t(a), t(b)),
    }
}

fn rewrite_atom(mut atom: Atom, scope: &str) -> Atom {
    let t = |x: Term| rewrite_term(x, scope);
    match (atom.pred.as_str(), atom.args.len()) {
        ("want", 2) | ("arg" | "attr", 4) | ("arg", 5) | ("arg_add", 4) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[1] = scoped_term(scope, args[1].clone());
            atom.args = args;
        }
        ("output", 2) => {
            let mut args = atom.args.into_iter().map(t);
            let (k, v) = (args.next().unwrap(), args.next().unwrap());
            atom.args = vec![str_term(scope), k, v];
        }
        _ => atom.args = atom.args.into_iter().map(t).collect(),
    }
    atom
}

fn rewrite_term(term: Term, scope: &str) -> Term {
    match term {
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rewrite_term(t, scope)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rewrite_term(v, scope)))
                .collect::<BTreeMap<_, _>>(),
        ),
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rewrite_term(*item, scope)),
            body: body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        },
        Term::Func { name, args } => {
            let mut args: Vec<Term> = args.into_iter().map(|t| rewrite_term(t, scope)).collect();
            if name == "ref" && args.len() == 3 {
                args[1] = scoped_term(scope, args[1].clone());
            }
            Term::Func { name, args }
        }
        other => other,
    }
}

/// `scoped(Scope, Name)`, unless `Name` is already an address
/// (`"network.main::vpc"` names another instance's resource).
fn scoped_term(scope: &str, name: Term) -> Term {
    if matches!(&name, Term::Func { name, .. } if name == "scoped")
        || matches!(&name, Term::Val(Value::Str(s)) if s.contains("::"))
    {
        return name;
    }
    Term::Func {
        name: "scoped".to_string(),
        args: vec![str_term(scope), name],
    }
}

/// Mark every statement (and each head, field and nested statement) as
/// lowered out of `origin` (`diag::origin_id`): a pack or a module
/// instance. What already has an origin keeps it.
pub fn set_origin(stmts: &mut [Stmt], origin: u32) {
    let fields = |fs: &mut [FieldAssign]| {
        for f in fs {
            f.span = f.span.within(origin);
        }
    };
    for s in stmts {
        match s {
            Stmt::Fact(a) => a.span = a.span.within(origin),
            Stmt::Rule(r) => r.head.span = r.head.span.within(origin),
            Stmt::Resource(r) => {
                r.span = r.span.within(origin);
                fields(&mut r.fields);
            }
            Stmt::Settings(st) => {
                st.span = st.span.within(origin);
                fields(&mut st.fields);
            }
            _ => {}
        }
    }
}
