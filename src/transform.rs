use crate::ast::{Atom, Constraint, Lit, Program, RuleStmt, Stmt};
use crate::value::{Term, Value};
use anyhow::{bail, Result};

pub fn expand_components(program: &Program) -> Result<Program> {
    let mut out = Vec::new();
    for stmt in &program.statements {
        expand_stmt(stmt, &mut out)?;
    }
    Ok(Program { statements: out })
}

fn expand_stmt(stmt: &Stmt, out: &mut Vec<Stmt>) -> Result<()> {
    match stmt {
        Stmt::Component(c) => {
            // Scope string is stable identity for component instance.
            // This becomes part of resource addresses via scoped(scope, name).
            let scope = format!("{}.{}", c.comp, c.inst);

            out.push(Stmt::Fact(Atom {
                pred: "component_scope".to_string(),
                args: vec![
                    Term::Val(Value::Str(c.comp.clone())),
                    Term::Val(Value::Str(c.inst.clone())),
                    Term::Val(Value::Str(scope.clone())),
                ],
            }));

            for inner in &c.body {
                // No nested component blocks for now (keeps mental model simple).
                if matches!(inner, Stmt::Component(_)) {
                    bail!("nested components are not supported yet");
                }
                out.push(rewrite_stmt(inner.clone(), &scope));
            }
        }
        _ => out.push(stmt.clone()),
    }
    Ok(())
}

fn rewrite_stmt(stmt: Stmt, scope: &str) -> Stmt {
    match stmt {
        Stmt::Fact(a) => Stmt::Fact(rewrite_atom(a, scope)),
        Stmt::Rule(r) => Stmt::Rule(RuleStmt {
            head: rewrite_atom(r.head, scope),
            body: r.body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        }),
        Stmt::Constraint(c) => Stmt::Constraint(Constraint {
            message: c.message,
            body: c.body.into_iter().map(|l| rewrite_lit(l, scope)).collect(),
        }),
        Stmt::Component(c) => Stmt::Component(c),
    }
}

fn rewrite_lit(lit: Lit, scope: &str) -> Lit {
    match lit {
        Lit::Pos(a) => Lit::Pos(rewrite_atom(a, scope)),
        Lit::Not(a) => Lit::Not(rewrite_atom(a, scope)),
        Lit::Eq(a, b) => Lit::Eq(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Neq(a, b) => Lit::Neq(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Gt(a, b) => Lit::Gt(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Ge(a, b) => Lit::Ge(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Lt(a, b) => Lit::Lt(rewrite_term(a, scope), rewrite_term(b, scope)),
        Lit::Le(a, b) => Lit::Le(rewrite_term(a, scope), rewrite_term(b, scope)),
    }
}

fn rewrite_atom(mut atom: Atom, scope: &str) -> Atom {
    // Apply scoping to resources inside components.
    match atom.pred.as_str() {
        "want" if atom.args.len() == 2 => {
            atom.args[1] = scoped_term(scope, atom.args[1].clone());
        }
        "arg" if atom.args.len() == 4 => {
            atom.args[1] = scoped_term(scope, atom.args[1].clone());
            atom.args[3] = rewrite_term(atom.args[3].clone(), scope);
        }
        // Sugar: inside a component, allow output(Key, Value)
        // which becomes output(Scope, Key, Value).
        "output" if atom.args.len() == 2 => {
            let key = atom.args[0].clone();
            let val = rewrite_term(atom.args[1].clone(), scope);
            atom.args = vec![Term::Val(Value::Str(scope.to_string())), key, val];
        }
        "output" if atom.args.len() == 3 => {
            // output/3 is the fully-qualified form; do not rewrite the scope.
            atom.args = atom
                .args
                .into_iter()
                .map(|t| rewrite_term(t, scope))
                .collect();
        }
        _ => {
            atom.args = atom
                .args
                .into_iter()
                .map(|t| rewrite_term(t, scope))
                .collect();
        }
    }
    atom
}

fn rewrite_term(term: Term, scope: &str) -> Term {
    match term {
        Term::Val(v) => Term::Val(v),
        Term::Var(v) => Term::Var(v),
        Term::Func { name, args } => {
            // Inside a component, `ref(Type, Name, Attr)` defaults to local.
            // Use `gref(Type, Name, Attr)` to reference global/other components.
            if name == "ref" && args.len() == 3 {
                let mut out = args;
                out[1] = scoped_term(scope, rewrite_term(out[1].clone(), scope));
                return Term::Func { name, args: out };
            }
            Term::Func {
                name,
                args: args.into_iter().map(|t| rewrite_term(t, scope)).collect(),
            }
        }
    }
}

fn scoped_term(scope: &str, name_term: Term) -> Term {
    Term::Func {
        name: "scoped".to_string(),
        args: vec![Term::Val(Value::Str(scope.to_string())), name_term],
    }
}
