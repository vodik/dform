//! Modules, components and instances (DESIGN.org R-65,
//! proposals/I-modules.md).
//!
//! A `use m [as n] { k = V } where B` imports module `m`, a file, and an
//! `instance c n { k = V } where B` copies component `c`, an item of one:
//! one mechanism, the body under the scope `n`:
//!
//! - resource names are scoped (`n::name`, its address `T["n::name"]`); in
//!   a component a resource written as a variable is the copy's own, in a
//!   module any its user sees (the module's rules merge into the user's
//!   scope);
//! - every predicate the body defines is `n`'s (`n::p`, a name no source
//!   can spell): a module's read by its user as `n.p`, a component's
//!   private to the copy, a value leaving it through an output;
//! - `input k: T [= D] [where R]` is read inside as `k(V)`: the collapsed
//!   cell `(input, n, k)` of the attribute aggregate, where the block's
//!   `k = V :- B` contributes at the normal rank and `D` at `@default`;
//! - `output k: T` declares an output and `output k = t` (or a rule for
//!   `output(k, V)`) defines it, readable anywhere as `output(n, k, V)`;
//! - a name the body does not define reads outward, its user's;
//! - for a copy, the fact `instance_of(c, user, n)`, which `c[t]`
//!   enumerates; with a clause `B`, the body's own gate `n::__instance(c)
//!   :- B` holds every rule and resource of it.
//!
//! A body is expanded inside out: its own `use`s and `instance`s are
//! expanded first, their names relative to it, and the body's scope is put
//! in front of them (`n.inner::x`, `output("n.inner", k, V)`); a name the
//! resolver wrote as its user's, `__scope(t)`, is left as it is. What a
//! module or a copy writes is not granted: ranks are the ownership model,
//! and the stratifier partitions a write by its head's constant type and
//! path.

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
/// k)`, scope `""` for the program's own, `n` in the instance `n`, `m` in
/// the activation `use m`. The resolver writes a `let` as `let(k, t, rank)`, a
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

/// A component's interface: what its top-level statements declare.
#[derive(Default)]
struct Interface {
    inputs: Vec<InputDecl>,
    outputs: BTreeMap<String, OutputDecl>,
    output_values: Vec<OutputDecl>,
}

/// Split a module's or component's body into its interface and its
/// statements; the definitions nested in it are left out.
fn interface(body: &[Stmt]) -> (Interface, Vec<Stmt>) {
    let mut i = Interface::default();
    let mut rest = Vec::new();
    for s in body {
        match s {
            Stmt::Input(d) => i.inputs.push(d.clone()),
            Stmt::Output(o) if o.value.is_none() => {
                i.outputs.entry(o.name.clone()).or_insert_with(|| o.clone());
            }
            Stmt::Output(o) => i.output_values.push(o.clone()),
            Stmt::Module(_) => {}
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
/// `names` in an instance or an activation): a read of `k` is the
/// collapsed cell, so two rows that agree are one value and two that
/// disagree a conflict.
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

/// The relation a copy is recorded in (R-65): `instance_of(Path, User,
/// Name)`, the component's path, the scope that made the copy (`""` the
/// program) and the copy's name there, which `c[t]` enumerates: `c["n"]`
/// is the copy `n`.
pub const INSTANCE_OF: &str = "instance_of";

/// A scope the resolver wrote as its user's, `__scope(t)`: a read inside a
/// component of a copy the component does not make (`blue.vpc` of the
/// stack's `blue`). Expansion puts no copy's scope in front of it, and
/// takes the mark off once done.
pub const ABSOLUTE: &str = "__scope";

/// How predicate names are renamed in one copy or activation.
struct Names {
    /// Predicate name -> its private name (`n::p`; `n.inner::p` for the
    /// private `inner::p` of a copy inside it).
    map: BTreeMap<String, String>,
}

impl Names {
    fn get(&self, pred: &str) -> Option<&String> {
        self.map.get(pred)
    }

    /// Every predicate `defined` names but the shared ones, private to
    /// `scope`. `instance_of` is the program's: a copy's copies are told
    /// apart by their user's scope, its second column.
    fn private(scope: &str, defined: BTreeMap<String, (usize, Span)>) -> Names {
        let map = defined
            .into_keys()
            .filter(|p| !is_shared(p) && p != INSTANCE_OF)
            .map(|p| {
                let n = private_name(scope, &p);
                (p, n)
            })
            .collect();
        Names { map }
    }
}

/// `p` private to `scope`: `scope::p`, or `scope.inner::p` for a name
/// already private to a copy inside it.
fn private_name(scope: &str, p: &str) -> String {
    match p.contains("::") {
        true => format!("{scope}.{p}"),
        false => format!("{scope}::{p}"),
    }
}

/// A program with its modules and components expanded, and the interface
/// the later passes check: every typed input, and every output declared
/// `secret(T)` (scope, key).
pub struct Expanded {
    pub program: Program,
    pub inputs: Vec<Declared>,
    pub secret_outputs: Vec<(String, String)>,
}

fn is_secret_type(t: &Option<TypeExpr>) -> bool {
    matches!(t, Some(TypeExpr::Apply(n, _)) if n == "secret")
}

/// Every definition of the program, by path, wherever it stands.
fn definitions<'a>(stmts: &'a [Stmt], out: &mut BTreeMap<String, &'a crate::ast::Module>) {
    for s in stmts {
        if let Stmt::Module(m) = s {
            out.insert(m.name.clone(), m);
            definitions(&m.body, out);
        }
    }
}

/// The expansion's state.
struct Cx<'a> {
    defs: BTreeMap<String, &'a crate::ast::Module>,
    diags: Vec<Diagnostic>,
    declared: Vec<Declared>,
    secret_outputs: Vec<(String, String)>,
    /// Private names by plain name, for the error when the program reads
    /// one.
    private: BTreeMap<String, (String, Option<String>)>,
    /// The definitions being expanded, outermost first: a component that
    /// reaches itself is an error, not a loop.
    expanding: Vec<String>,
    /// The definitions whose interface was checked.
    checked: BTreeSet<String>,
}

fn check_types(who: &str, inputs: &[InputDecl], diags: &mut Vec<Diagnostic>) {
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
}

/// `scope.rest`, the scope `""` being the program's.
fn join_scope(scope: &str, rest: &str) -> String {
    match (scope.is_empty(), rest.is_empty()) {
        (true, _) => rest.to_string(),
        (_, true) => scope.to_string(),
        _ => format!("{scope}.{rest}"),
    }
}

pub fn expand(program: &Program) -> Result<Expanded> {
    let mut defs = BTreeMap::new();
    definitions(&program.statements, &mut defs);
    let mut cx = Cx {
        defs,
        diags: Vec::new(),
        declared: Vec::new(),
        secret_outputs: Vec::new(),
        private: BTreeMap::new(),
        expanding: Vec::new(),
        checked: BTreeSet::new(),
    };
    let mut out = Vec::new();
    for s in &program.statements {
        match s {
            // The stack's own input: read as `k(V)`, given by `--set` (an
            // `input(k, V)` fact) at the normal rank.
            Stmt::Input(i) => {
                check_types("stack", std::slice::from_ref(i), &mut cx.diags);
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
                cx.declared.push(Declared {
                    scope: String::new(),
                    decl: i.clone(),
                });
            }
            // The stack's own output: `output(k, V)` in the root scope.
            Stmt::Output(o) if o.value.is_none() => {
                if is_secret_type(&o.ty) {
                    cx.secret_outputs.push((String::new(), o.name.clone()));
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
            Stmt::Use(u) => out.extend(cx.instance(u, "", true)),
            Stmt::Instance(u) => out.extend(cx.instance(u, "", false)),
            Stmt::Module(_) => {}
            other => out.push(other.clone()),
        }
    }
    // The program's own `let`s.
    let mut expanded = lets(out, "", None);
    // `instance_of` (and a copy's private one) is a fact for a copy with
    // no clause and a rule for one with: both, by design.
    let mut mixed = BTreeMap::new();
    for s in &expanded {
        if let Stmt::Fact(a) | Stmt::Rule(RuleStmt { head: a, .. }) = s
            && a.pred == INSTANCE_OF
        {
            mixed.entry(a.pred.clone()).or_insert(a.span);
        }
    }
    expanded.extend(mixed.into_iter().map(|(pred, span)| {
        Stmt::Mixed(crate::ast::Extern {
            pred,
            arity: 3,
            span,
        })
    }));

    // A read of a name only a module or a component defines: say it is
    // private.
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
        if let Some((owner, help)) = cx.private.get(&a.pred)
            && !defined.contains_key(&a.pred)
            && !externs.contains(a.pred.as_str())
        {
            let d = Diagnostic::error(
                a.span,
                format!("{}/{} is private to {owner}", a.pred, a.args.len()),
            );
            cx.diags.push(match help {
                Some(h) => d.with_help(h.clone()),
                None => d,
            });
        }
    }

    let expanded = expanded.into_iter().map(unmark_stmt).collect::<Vec<_>>();
    if cx.diags.is_empty() {
        Ok(Expanded {
            program: Program {
                statements: expanded,
            },
            inputs: cx.declared,
            secret_outputs: cx.secret_outputs,
        })
    } else {
        Err(Diagnostics(cx.diags).into())
    }
}

impl Cx<'_> {
    /// The `use`s and `instance`s of a body expanded, every name relative
    /// to the body's scope; `at` is that scope's absolute name.
    fn body(&mut self, stmts: &[Stmt], at: &str) -> Vec<Stmt> {
        let mut out = Vec::new();
        for s in stmts {
            match s {
                Stmt::Use(u) => out.extend(self.instance(u, at, true)),
                Stmt::Instance(u) => out.extend(self.instance(u, at, false)),
                other => out.push(other.clone()),
            }
        }
        out
    }

    /// `def`, or `None` (reported) when it is being expanded already.
    fn enter(&mut self, path: &str, span: Span) -> Option<&crate::ast::Module> {
        let def = *self.defs.get(path)?;
        if self.expanding.iter().any(|p| p == path) {
            let mut cycle = self.expanding.clone();
            cycle.push(path.to_string());
            self.diags.push(Diagnostic::error(
                span,
                format!("{path} reaches itself: {}", cycle.join(" -> ")),
            ));
            return None;
        }
        self.expanding.push(path.to_string());
        Some(def)
    }

    /// `instance c n { k = V } where B`, a component's body under the
    /// scope `n`, recorded in `instance_of`; or `use m [as n] { k = V }
    /// where B`, a module's (`used`): one mechanism (R-65).
    fn instance(&mut self, u: &crate::ast::Instance, at: &str, used: bool) -> Vec<Stmt> {
        let Some(def) = self.enter(&u.module, u.span) else {
            return Vec::new();
        };
        let kind = if used { "module" } else { "component" };
        let (iface, body) = interface(&def.body);
        if self.checked.insert(u.module.clone()) {
            check_component(kind, &u.module, &iface, &body, &mut self.diags);
            check_types(
                &format!("{kind} {}", u.module),
                &iface.inputs,
                &mut self.diags,
            );
        }
        let scope = u.name.as_str();
        let abs = join_scope(at, scope);
        let mut out = Vec::new();
        instance_inputs(kind, u, scope, &iface, &mut out, &mut self.diags);
        let body = self.body(&body, &abs);
        self.expanding.pop();
        let stmts = module_stmts(scope, &iface, body);
        let mut defined = BTreeMap::new();
        defined_preds(&stmts, &mut defined);
        for i in &iface.inputs {
            defined.insert(i.name.clone(), (1, i.span));
            defined.insert(refine_pred(&i.name), (1, i.span));
        }
        let names = Names::private(scope, defined);
        for p in names.map.keys() {
            let help = match used {
                true => format!("read it as {scope}.{p}, after `use {}`", u.module),
                false => format!(
                    "pass the value through an output of component {}, `output {p} = ...`, \
                     and read it as INSTANCE.{p}",
                    u.module
                ),
            };
            self.private
                .insert(p.clone(), (format!("{kind} {}", u.module), Some(help)));
        }
        let mut stmts = stmts;
        let origin = match used {
            true if u.module.rsplit('.').next() == Some(scope) => format!("use {}", u.module),
            true => format!("use {} as {scope}", u.module),
            false => format!("instance {} {}", u.module, u.name),
        };
        set_origin(&mut stmts, diag::origin_id(&origin));
        let stmts = stmts
            .into_iter()
            .map(|st| {
                let sc = Sc {
                    name: scope,
                    vars: !used,
                };
                rewrite_stmt(rename_stmt(st, &names), sc)
            })
            .collect();
        let mut copy = lets(stmts, scope, Some(&names));
        copy.extend(input_readers(scope, &iface.inputs, &names));
        self.secret_outputs.extend(
            iface
                .outputs
                .values()
                .filter(|o| is_secret_type(&o.ty))
                .map(|o| (abs.clone(), o.name.clone())),
        );
        self.declared.extend(iface.inputs.iter().map(|i| Declared {
            scope: abs.clone(),
            decl: i.clone(),
        }));
        out.extend(gate(
            copy,
            &u.module,
            scope,
            u.clause.as_deref(),
            !used,
            u.span,
        ));
        out
    }
}

/// The statements of the copy or activation `scope`, and for a copy
/// (`record`) the fact `instance_of(path, scope)`. With a clause `B` they
/// exist only while it holds: the copy's own `scope::__instance(path) :-
/// B` gates each of its rules and resources, and `instance_of(path,
/// scope)` is derived from it. The gate is the copy's own relation, so
/// that one copy's clause reading another's outputs is no cycle through
/// every copy.
fn gate(
    stmts: Vec<Stmt>,
    path: &str,
    scope: &str,
    clause: Option<&[Lit]>,
    record: bool,
    span: Span,
) -> Vec<Stmt> {
    let fact = atom(
        INSTANCE_OF,
        vec![str_term(path), str_term(""), str_term(scope)],
        span,
    );
    let Some(b) = clause.filter(|b| !b.is_empty()) else {
        let mut out = stmts;
        if record {
            out.push(Stmt::Fact(fact));
        }
        return out;
    };
    let own = atom(&format!("{scope}::{GATE}"), vec![str_term(path)], span);
    let on = Lit::Pos(own.clone());
    let mut out: Vec<Stmt> = stmts
        .into_iter()
        .map(|s| match s {
            Stmt::Fact(a) => Stmt::Rule(RuleStmt {
                head: a,
                body: vec![on.clone()],
            }),
            Stmt::Rule(mut r) => {
                r.body.insert(0, on.clone());
                Stmt::Rule(r)
            }
            Stmt::Resource(mut r) => {
                r.body.get_or_insert_with(Vec::new).insert(0, on.clone());
                Stmt::Resource(r)
            }
            Stmt::Settings(mut st) => {
                st.body.get_or_insert_with(Vec::new).insert(0, on.clone());
                Stmt::Settings(st)
            }
            other => other,
        })
        .collect();
    out.push(Stmt::Rule(RuleStmt {
        head: own,
        body: b.to_vec(),
    }));
    if record {
        out.push(Stmt::Rule(RuleStmt {
            head: fact,
            body: vec![on],
        }));
    }
    out
}

/// A gated copy's own relation (`gate`).
const GATE: &str = "__instance";

/// The instance's `k = V :- B`, each a normal-rank contribution to its
/// input cell, with the checks against the component's declared inputs.
fn instance_inputs(
    kind: &str,
    u: &crate::ast::Instance,
    scope: &str,
    iface: &Interface,
    out: &mut Vec<Stmt>,
    diags: &mut Vec<Diagnostic>,
) {
    let declared: Vec<&str> = iface.inputs.iter().map(|i| i.name.as_str()).collect();
    for (k, v, span) in &u.inputs {
        if !declared.contains(&k.as_str()) {
            let d = Diagnostic::error(*span, format!("{kind} {} has no input {k}", u.module));
            diags.push(if declared.is_empty() {
                d.with_note(format!("{kind} {} declares no inputs", u.module))
            } else {
                d.with_note(format!("its inputs: {}", declared.join(", ")))
            });
            continue;
        }
        // A literal is read as the input's declared type (R-31).
        let ty = iface.inputs.iter().find(|i| i.name == *k).map(|i| &i.ty);
        let v = match ty.map(|t| crate::types::literal(&crate::types::of_expr(t), v.clone())) {
            Some(Ok(v)) => v,
            Some(Err(why)) => {
                diags.push(Diagnostic::error(
                    *span,
                    format!("input {k} of {kind} {} {why}", u.module),
                ));
                continue;
            }
            None => v.clone(),
        };
        let head = atom(
            "arg",
            vec![
                str_term(INPUT),
                str_term(scope),
                str_term(k),
                v,
                str_term(crate::transform::NORMAL),
            ],
            *span,
        );
        out.push(fact_or_rule(head, u.body.clone().unwrap_or_default()));
    }
    // An input nothing gives a value is a stack input's error (R-65),
    // fixed the same ways: a default, or a value given.
    for i in &iface.inputs {
        if i.default.is_none() && !u.inputs.iter().any(|(k, _, _)| *k == i.name) {
            diags.push(
                Diagnostic::error(
                    u.span,
                    format!(
                        "input {} is required and has no value",
                        join_scope(scope, &i.name)
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
                .with_help(format!(
                    "give it in the block, `{{ {} = ... }}`, or give it a default",
                    i.name
                )),
            );
        }
    }
}

/// The checks on a component's interface, once per component: an input
/// or an output is declared once, an input is not also defined, and every
/// output it gives a value is declared.
fn check_component(
    kind: &str,
    path: &str,
    iface: &Interface,
    body: &[Stmt],
    diags: &mut Vec<Diagnostic>,
) {
    let mut seen = BTreeSet::new();
    for i in &iface.inputs {
        if !seen.insert(&i.name) {
            diags.push(Diagnostic::error(
                i.span,
                format!("{kind} {path} declares input {} twice", i.name),
            ));
        }
    }
    let mut own = BTreeMap::new();
    defined_preds(body, &mut own);
    for i in &iface.inputs {
        if let Some((_, span)) = own.get(&i.name) {
            diags.push(Diagnostic::error(
                *span,
                format!(
                    "{kind} {path} defines {}, which is its input: an input is set where it \
                     is used",
                    i.name
                ),
            ));
        }
    }
    let undeclared = |k: &str, span: Span| {
        Diagnostic::error(span, format!("{kind} {path} has no output {k}"))
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

/// The component's statements for one copy, before scoping: its body,
/// the output values and each input's refinement.
fn module_stmts(scope: &str, iface: &Interface, body: Vec<Stmt>) -> Vec<Stmt> {
    let mut out = body;
    for o in &iface.output_values {
        // An undeclared output is reported once, by `check_component`.
        let Some(decl) = iface.outputs.get(&o.name) else {
            continue;
        };
        let mut value = o.value.clone().expect("an output value");
        // An `addr` output names one of this copy's resources: scoped by
        // the copy itself (`""`), which scoping makes its own name.
        if matches!(&decl.ty, Some(TypeExpr::Name(t)) if t == "addr") {
            value = Term::Func {
                name: "scoped".into(),
                args: vec![str_term(""), value],
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

/// How a copy's statements are scoped: its name, and whether a resource
/// written as a variable is its own (a component's copy) or any its user
/// sees (a module's, whose rules merge into the user's scope).
#[derive(Clone, Copy)]
struct Sc<'a> {
    name: &'a str,
    vars: bool,
}

/// Scope one statement of an instance: resource names in `want`, `arg`,
/// `attr`, `adopt` and `ref` become `scoped(Scope, Name)`, and `output(k, V)` is
/// `output(Scope, k, V)`.
fn rewrite_stmt(stmt: Stmt, sc: Sc) -> Stmt {
    let lits = |ls: Vec<Lit>| ls.into_iter().map(|l| rewrite_lit(l, sc)).collect();
    let fields = |fs: Vec<FieldAssign>| {
        fs.into_iter()
            .map(|f| FieldAssign {
                value: rewrite_term(f.value, sc),
                ..f
            })
            .collect()
    };
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom(a, sc)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rewrite_atom(r.head, sc),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: rewrite_term(r.typ, sc),
            name: scoped_term(Sc { vars: true, ..sc }, r.name),
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

fn rewrite_lit(lit: Lit, sc: Sc) -> Lit {
    let t = |x| rewrite_term(x, sc);
    match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom(a, sc)),
        Lit::Not(a) => Lit::Not(rewrite_atom(a, sc)),
        Lit::Eq(a, b) => Lit::Eq(t(a), t(b)),
        Lit::Neq(a, b) => Lit::Neq(t(a), t(b)),
        Lit::Gt(a, b) => Lit::Gt(t(a), t(b)),
        Lit::Ge(a, b) => Lit::Ge(t(a), t(b)),
        Lit::Lt(a, b) => Lit::Lt(t(a), t(b)),
        Lit::Le(a, b) => Lit::Le(t(a), t(b)),
    }
}

fn rewrite_atom(mut atom: Atom, sc: Sc) -> Atom {
    let t = |x: Term| rewrite_term(x, sc);
    let pseudo =
        |a: &Term| matches!(a, Term::Val(Value::Str(s)) if s == INPUT || s == LET || s == "output");
    match (atom.pred.as_str(), atom.args.len()) {
        // A cell of a copy inside this one, `(input, inner, k)`, or a read
        // of its output: its scope is relative.
        ("arg" | "attr", 4 | 5) | (crate::refine::ATTR_REFINE, 4) if pseudo(&atom.args[0]) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[1] = prefix_scope(sc.name, args[1].clone());
            atom.args = args;
        }
        // A settings row is addressed by its environment, never a scope.
        ("arg" | "attr", 4) | ("arg", 5) | ("arg_add", 4) if matches!(&atom.args[0], Term::Val(Value::Str(s)) if s == crate::transform::SETTINGS) =>
        {
            atom.args = atom.args.into_iter().map(t).collect();
        }
        ("want", 2) | ("arg" | "attr", 4) | ("arg", 5) | ("arg_add", 4) => {
            atom.args = atom
                .args
                .into_iter()
                .enumerate()
                .map(|(i, a)| match i {
                    1 => scoped_term(sc, a),
                    _ => t(a),
                })
                .collect();
        }
        ("output", 2) => {
            let mut args = atom.args.into_iter().map(t);
            let (k, v) = (args.next().unwrap(), args.next().unwrap());
            atom.args = vec![str_term(sc.name), k, v];
        }
        // A copy inside this one's record: its user's scope is relative.
        (INSTANCE_OF, 3) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[1] = prefix_scope(sc.name, args[1].clone());
            atom.args = args;
        }
        // A copy inside this one's output.
        ("output", 3) => {
            let mut args: Vec<Term> = atom.args.into_iter().map(t).collect();
            args[0] = prefix_scope(sc.name, args[0].clone());
            atom.args = args;
        }
        _ => atom.args = atom.args.into_iter().map(t).collect(),
    }
    atom
}

/// A scope relative to this copy, made absolute: `n.inner`, `n` for the
/// copy's own `""`.
fn prefix_scope(scope: &str, inner: Term) -> Term {
    match inner {
        Term::Val(Value::Str(s)) => str_term(&join_scope(scope, &s)),
        Term::Func { ref name, .. } if name == ABSOLUTE => inner,
        t => Term::Func {
            name: "format".into(),
            args: vec![str_term(&format!("{scope}.%s")), t],
        },
    }
}

fn rewrite_term(term: Term, sc: Sc) -> Term {
    match term {
        Term::Func { ref name, .. } if name == ABSOLUTE => term,
        Term::List(xs) => Term::List(xs.into_iter().map(|t| rewrite_term(t, sc)).collect()),
        Term::Obj(m) => Term::Obj(
            m.into_iter()
                .map(|(k, v)| (k, rewrite_term(v, sc)))
                .collect::<BTreeMap<_, _>>(),
        ),
        Term::ListComp { item, body } => Term::ListComp {
            item: Box::new(rewrite_term(*item, sc)),
            body: body.into_iter().map(|l| rewrite_lit(l, sc)).collect(),
        },
        // A name a copy inside this one scoped: its scope is relative.
        Term::Func { name, mut args } if name == "scoped" && args.len() == 2 => {
            args[0] = prefix_scope(sc.name, args[0].clone());
            args[1] = rewrite_term(args[1].clone(), sc);
            Term::Func { name, args }
        }
        Term::Func { name, args } => {
            let is_ref = name == "ref" && args.len() == 3;
            let args = args
                .into_iter()
                .enumerate()
                .map(|(i, a)| match i {
                    1 if is_ref => scoped_term(sc, a),
                    _ => rewrite_term(a, sc),
                })
                .collect();
            Term::Func { name, args }
        }
        other => other,
    }
}

/// `scoped(Scope, Name)`; a name a copy inside this one scoped,
/// `scoped(inner, Name)`, is `scoped(Scope.inner, Name)`; and a name that
/// is already an address (`"blue::vpc"` names another copy's resource) is
/// itself. In a module's (`!sc.vars`), only a name the module writes out is
/// its own: a variable ranges over every resource its user sees.
fn scoped_term(sc: Sc, name: Term) -> Term {
    match name {
        Term::Func { name: ref f, .. } if f == "scoped" => rewrite_term(name, sc),
        Term::Func { name: ref f, .. } if f == ABSOLUTE => name,
        Term::Val(Value::Str(s)) if s.contains("::") => Term::Val(Value::Str(s)),
        name if !sc.vars && !matches!(name, Term::Val(_)) => rewrite_term(name, sc),
        name => Term::Func {
            name: "scoped".to_string(),
            args: vec![str_term(sc.name), rewrite_term(name, sc)],
        },
    }
}

/// A statement with the marks of `ABSOLUTE` taken off: `__scope(t)` is
/// `t`.
fn unmark_stmt(s: Stmt) -> Stmt {
    fn term(t: Term) -> Term {
        match t {
            Term::Func { name, args } if name == ABSOLUTE && args.len() == 1 => {
                term(args.into_iter().next().unwrap())
            }
            Term::Func { name, args } => Term::Func {
                name,
                args: args.into_iter().map(term).collect(),
            },
            Term::List(xs) => Term::List(xs.into_iter().map(term).collect()),
            Term::Obj(m) => Term::Obj(m.into_iter().map(|(k, v)| (k, term(v))).collect()),
            Term::ListComp { item, body } => Term::ListComp {
                item: Box::new(term(*item)),
                body: lits(body),
            },
            t => t,
        }
    }
    fn atom(mut a: Atom) -> Atom {
        a.args = a.args.into_iter().map(term).collect();
        a
    }
    fn lits(ls: Vec<Lit>) -> Vec<Lit> {
        ls.into_iter()
            .map(|l| match l {
                Lit::Pos(a) => Lit::Pos(atom(a)),
                Lit::Not(a) => Lit::Not(atom(a)),
                Lit::Eq(a, b) => Lit::Eq(term(a), term(b)),
                Lit::Neq(a, b) => Lit::Neq(term(a), term(b)),
                Lit::Gt(a, b) => Lit::Gt(term(a), term(b)),
                Lit::Ge(a, b) => Lit::Ge(term(a), term(b)),
                Lit::Lt(a, b) => Lit::Lt(term(a), term(b)),
                Lit::Le(a, b) => Lit::Le(term(a), term(b)),
            })
            .collect()
    }
    let fields = |fs: Vec<FieldAssign>| {
        fs.into_iter()
            .map(|f| FieldAssign {
                value: term(f.value),
                ..f
            })
            .collect()
    };
    match s {
        Stmt::Fact(a) => Stmt::Fact(atom(a)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: atom(r.head),
            body: lits(r.body),
        }),
        Stmt::Resource(r) => Stmt::Resource(Resource {
            typ: term(r.typ),
            name: term(r.name),
            fields: fields(r.fields),
            body: r.body.map(lits),
            ..r
        }),
        Stmt::Settings(st) => Stmt::Settings(Settings {
            fields: fields(st.fields),
            body: st.body.map(lits),
            ..st
        }),
        other => other,
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
