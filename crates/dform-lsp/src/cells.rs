//! The values the selected deployment gives what a name denotes (R-20):
//! the cell of an input, a `let` or an output (`attr("input", S, k, V)`,
//! one per copy or per name a module is used by), a field of an object
//! input, a resource's attribute. The hover shows them with their
//! contributions; the inlay hints show the value where it is read.

use crate::analysis::Evaluated;
use dform_core::circuit::NodeId;
use dform_core::names::{Decls, Parsed, Site, Symbol, What};
use dform_core::syntax::{SyntaxKind, SyntaxNode, SyntaxToken};
use dform_core::value::Value;

/// One cell's value in one evaluation.
pub struct Cell<'e> {
    pub e: &'e Evaluated,
    /// The fact node of the cell (the attribute aggregate's `attr`).
    pub node: NodeId,
    /// Which copy or binding it is the cell of (`""` for the program's).
    pub scope: String,
    /// The value at the path read (the whole cell's at none).
    pub value: Value,
}

/// `v` read along a dotted `path` of object fields.
fn walk(v: &Value, path: &str) -> Option<Value> {
    let mut v = v.clone();
    for f in path.split('.').filter(|f| !f.is_empty()) {
        v = match v {
            Value::Obj(m) => m.get(f)?.clone(),
            _ => return None,
        };
    }
    Some(v)
}

/// The evaluations that hold `sym`'s cells: a stack's own names only its
/// own evaluation's, anything else every evaluation's.
fn holding<'e>(
    evaluated: &[&'e Evaluated],
    d: &Decls,
    files: &[Parsed],
    sym: &Symbol,
) -> Vec<&'e Evaluated> {
    let scope = match sym {
        Symbol::Value(s, _) | Symbol::Let(s, _) | Symbol::Output(s, _) | Symbol::Field(s, ..) => s,
        Symbol::Resource(s, ..) => s,
        _ => return Vec::new(),
    };
    let declared_in = d.stack_file(scope).cloned().or_else(|| {
        match d.definition(files, &What::Name(sym.clone(), false)).first() {
            Some(Site::Text(f, _)) => Some(f.path.clone()),
            _ => None,
        }
    });
    let own: Vec<&Evaluated> = evaluated
        .iter()
        .copied()
        .filter(|e| declared_in.as_ref() == Some(&e.file))
        .collect();
    if own.is_empty() {
        evaluated.to_vec()
    } else {
        own
    }
}

/// The cells `sym` has, read along `path` (an attribute's or a field's
/// path after it), in the evaluations.
pub fn cells<'e>(
    evaluated: &[&'e Evaluated],
    d: &Decls,
    files: &[Parsed],
    sym: &Symbol,
    path: &str,
) -> Vec<Cell<'e>> {
    // Which attribute facts: (type, scopes or addresses, key, the path
    // into the value).
    let (typ, names, key, inner): (String, Vec<String>, String, String) = match sym {
        Symbol::Value(s, k) => ("input".into(), d.cell_scopes(s), k.clone(), path.into()),
        Symbol::Let(s, k) => ("let".into(), d.cell_scopes(s), k.clone(), path.into()),
        Symbol::Output(s, k) => ("output".into(), d.cell_scopes(s), k.clone(), path.into()),
        Symbol::Field(s, i, f) => (
            "input".into(),
            d.cell_scopes(s),
            i.clone(),
            dform_core::types::dotted(f, path),
        ),
        Symbol::Resource(..) if !path.is_empty() => {
            let addrs = d.addresses_of(sym);
            let Some((t, _)) = addrs.first() else {
                return Vec::new();
            };
            (
                t.clone(),
                addrs.iter().map(|(_, a)| a.clone()).collect(),
                path.into(),
                String::new(),
            )
        }
        _ => return Vec::new(),
    };
    let mut out = Vec::new();
    for e in holding(evaluated, d, files, sym) {
        let c = &e.res.circuit;
        for a in e.res.facts.iter().filter(|a| a.pred == "attr") {
            let s = |i: usize| match a.args.get(i) {
                Some(dform_core::ast::Term::Val(Value::Str(x))) => Some(x.as_str()),
                _ => None,
            };
            if s(0) != Some(typ.as_str()) || s(2) != Some(key.as_str()) {
                continue;
            }
            let Some(scope) = s(1).filter(|x| names.iter().any(|n| n == x)) else {
                continue;
            };
            let Some(dform_core::ast::Term::Val(v)) = a.args.get(3) else {
                continue;
            };
            let (Some(value), Some(node)) = (
                walk(v, &inner),
                c.fact_id(&dform_core::engine::circuit_fact(a)),
            ) else {
                continue;
            };
            out.push(Cell {
                e,
                node,
                scope: scope.to_string(),
                value,
            });
        }
    }
    out
}

/// What a chain reads, read at its name `t`: the symbol of the last
/// segment up to `t` that has a value, and the path of the segments after
/// it up to `t` (`cidrs.main`: the input `cidrs`, `main`; `vpc.cidr`: the
/// resource, `cidr`). Through an index, nothing.
pub fn read_at(d: &Decls, chain: &SyntaxNode, t: &SyntaxToken) -> Option<(Symbol, String)> {
    let mut found: Option<(Symbol, Vec<String>)> = None;
    for x in chain.children_with_tokens() {
        match x {
            rowan::NodeOrToken::Node(n) if n.kind() == SyntaxKind::INDEX => {
                if found.is_some() {
                    return None;
                }
            }
            rowan::NodeOrToken::Token(x) if x.kind() == SyntaxKind::IDENT => {
                match d.classify(&x) {
                    What::Name(
                        s @ (Symbol::Value(..)
                        | Symbol::Let(..)
                        | Symbol::Output(..)
                        | Symbol::Field(..)
                        | Symbol::Resource(..)),
                        false,
                    ) => found = Some((s, Vec::new())),
                    _ => {
                        if let Some((_, path)) = &mut found {
                            path.push(x.text().to_string());
                        }
                    }
                }
                if &x == t {
                    break;
                }
            }
            _ => {}
        }
    }
    let (sym, path) = found?;
    Some((sym, path.join(".")))
}

/// The cells' values as one label: the value; several copies' values
/// each with its copy (`main: 10.50.0.0/16, peer: 10.60.0.0/16`).
pub fn label(cells: &[Cell]) -> Option<String> {
    let shown = |c: &Cell| dform_core::report::shown_value(&c.value, &c.e.redact).text();
    let mut distinct: Vec<String> = Vec::new();
    for c in cells {
        let v = shown(c);
        if !distinct.contains(&v) {
            distinct.push(v);
        }
    }
    match distinct.len() {
        0 => None,
        1 => distinct.pop(),
        _ => Some(
            cells
                .iter()
                .map(|c| match c.scope.as_str() {
                    "" => shown(c),
                    s => format!("{s}: {}", shown(c)),
                })
                .collect::<Vec<_>>()
                .join(", "),
        ),
    }
}
