//! Record atoms (`p { f: x }`, H-11) to the positional atoms the engine
//! runs: each field in its column by the `decl` of `p` in the scope the
//! atom is written in, or one around it, or the core's own. A module's or
//! a component's own `decl`s name its relations' fields inside it, over
//! any outer one of the same name (R-211: lowered by scope, where
//! `transform::apply_decls` rewrote each module's body).

use crate::ast::{Atom, Stmt, Term};
use crate::diag::Diagnostic;
use crate::program::{ItemId, ItemKind, Program};
use std::collections::BTreeMap;

/// The fields of each relation a record atom may name.
#[derive(Debug, Clone)]
pub struct Records(BTreeMap<String, Vec<String>>);

impl Records {
    /// The core's relations' fields.
    pub fn core() -> Records {
        let fields = |fs: &[&str]| fs.iter().map(|f| f.to_string()).collect::<Vec<_>>();
        let arg = fields(&["type", "name", "path", "value"]);
        Records(BTreeMap::from([
            ("want".to_string(), fields(&["type", "name"])),
            ("arg".to_string(), arg.clone()),
            ("arg_add".to_string(), arg),
            ("output".to_string(), fields(&["scope", "key", "value"])),
            ("input".to_string(), fields(&["key", "value"])),
            ("data".to_string(), fields(&["key", "value"])),
            ("merge_rule".to_string(), fields(&["type", "path", "op"])),
            ("warn".to_string(), fields(&["msg", "ctx"])),
            ("deny".to_string(), fields(&["msg", "ctx"])),
        ]))
    }

    /// These fields, and those the `decl` statements of `stmts` declare
    /// over them.
    pub fn with(&self, stmts: &[Stmt]) -> Records {
        let mut out = self.clone();
        for s in stmts {
            if let Stmt::Decl(d) = s {
                out.0.insert(d.pred.clone(), d.fields.clone());
            }
        }
        out
    }

    /// These fields, and those the items `items` of `program` declare
    /// over them: a `decl`'s columns, a typed `let`'s cell (its `decl k(k:
    /// T)`), as they lower.
    pub fn with_items(&self, program: &Program, items: &[ItemId]) -> Records {
        let mut out = self.clone();
        for &i in items {
            match &program.items[i].kind {
                ItemKind::Decl { rel, columns, .. } => {
                    let fields = columns.iter().map(|(f, _)| f.clone()).collect();
                    out.0.insert(rel.name.clone(), fields);
                }
                ItemKind::Let {
                    name,
                    ty: Some(_),
                    declares: true,
                    ..
                } => {
                    out.0.insert(name.clone(), vec![name.clone()]);
                }
                _ => {}
            }
        }
        out
    }

    /// `atom` with its record's fields in their columns; `complete`: a
    /// head's or a fact's, which gives every field a value. An atom with
    /// no record is itself.
    pub fn resolve(&self, mut atom: Atom, complete: bool) -> Result<Atom, Box<Diagnostic>> {
        let Some(fields) = atom.record.take() else {
            return Ok(atom);
        };
        let pred = &atom.pred;
        let Some(order) = self.0.get(pred) else {
            return Err(Box::new(Diagnostic::error(
                atom.span,
                format!(
                    "no record fields declared for predicate '{pred}' (declare them with decl \
                     {pred}(Field: type, ...))"
                ),
            )));
        };
        if let Some(k) = fields.keys().find(|k| !order.contains(k)) {
            return Err(Box::new(Diagnostic::error(
                atom.span,
                format!("unknown field '{k}' for predicate '{pred}'"),
            )));
        }
        let mut args = Vec::with_capacity(order.len());
        for f in order {
            match fields.get(f) {
                Some(t) => args.push(t.clone()),
                None if complete => {
                    return Err(Box::new(Diagnostic::error(
                        atom.span,
                        format!("missing field '{f}' for predicate '{pred}'"),
                    )));
                }
                None => args.push(Term::Wildcard),
            }
        }
        if complete && args.iter().any(|t| matches!(t, Term::Wildcard)) {
            return Err(Box::new(Diagnostic::error(
                atom.span,
                format!("`_` is no value: not in a fact, nor left of `where`, of '{pred}'"),
            )));
        }
        atom.args = args;
        Ok(atom)
    }
}
