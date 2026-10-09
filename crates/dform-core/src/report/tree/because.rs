//! A deformation's explanation, `plan --why` and `diff`: the statement that derived
//! it and its leaves, one `Because` each (`Printer::because`, `want`, `attr`), and
//! what the program states (`Printer::stated`).

use super::compress::Compress;
use super::printer::{Focus, Printer, find};
use super::sites::{base_parts, table_row};
use crate::ast::{Atom, RuleStmt, Term};
use crate::circuit::{Fact, Leaf, NodeId, View};
use crate::engine;
use crate::ir::Address;
use crate::value::Value;
use std::collections::{BTreeMap, BTreeSet};

/// One line of a deformation's explanation: the statement that derived it
/// (`kind` "rule"), or one leaf under it: a fact the program or a table
/// states ("fact"), a `--set` or `--data` ("input"), an extern's answer
/// ("answered"), a world fact ("world"), a fact of the plan ("plan"), a
/// fact found absent ("absent"), or what state alone says ("state").
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Because {
    pub kind: String,
    /// `file:line` (a table's row, `path:line`), when it has a place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<String>,
    pub text: String,
}

impl Because {
    pub(super) fn new(kind: &str, at: Option<String>, text: String) -> Because {
        Because {
            kind: kind.into(),
            at,
            text,
        }
    }

    /// `by FILE:LINE  STATEMENT`, or `because [PLACE  ]FACT`.
    pub fn line(&self) -> String {
        let word = if self.kind == "rule" { "by" } else { "because" };
        match &self.at {
            Some(at) => format!("{word} {at}  {}", self.text),
            None => format!("{word} {}", self.text),
        }
    }
}

impl Printer<'_> {
    /// Why fact node `root` holds, compressed: the statement that derived
    /// it, then one line per leaf of its shortest derivation (the facts in
    /// between are dropped). An attribute (`attr`) is explained by its
    /// winning contributions, each its statement and leaves; with a focus,
    /// only the contributions that hold the focused part.
    pub fn because(&self, rules: &[RuleStmt], root: NodeId, focus: Option<&Focus>) -> Vec<Because> {
        let mut s = self.surface(rules);
        let mut c = Compress {
            out: Vec::new(),
            sizes: BTreeMap::new(),
        };
        c.fact(&mut s, root, None, true, focus);
        c.out
    }

    /// Why resource `addr` is wanted ([`because`] of its `want`); `None`
    /// when the program does not want it.
    ///
    /// [`because`]: Printer::because
    pub fn want(&self, rules: &[RuleStmt], addr: &Address) -> Option<Vec<Because>> {
        let f = Fact::new(
            "want",
            vec![Value::Str(addr.typ.clone()), Value::Str(addr.name.clone())],
        );
        let id = self.circuit.fact_id(&f)?;
        Some(self.because(rules, id, None))
    }

    /// Why attribute `path` of `addr` has its value: its winning
    /// contributions; a dotted path below an object attribute
    /// (`tags.team`) only the contributions that set that part, and a
    /// list element (`rules[0]`) the list's. `None` when the program sets
    /// no such attribute.
    pub fn attr(
        &self,
        rules: &[RuleStmt],
        facts: &BTreeSet<Atom>,
        addr: &Address,
        path: &str,
    ) -> Option<Vec<Because>> {
        let path = path.split('[').next().unwrap_or(path);
        let s = |x: &str| Term::Val(Value::Str(x.to_string()));
        let pattern = Atom {
            pred: "attr".into(),
            args: vec![s(&addr.typ), s(&addr.name), s(path), Term::Wildcard],
            record: None,
            span: Default::default(),
        };
        let found = find(&pattern, facts).ok()?;
        let (a, focus) = found.first()?;
        let id = self.circuit.fact_id(&engine::circuit_fact(a))?;
        Some(self.because(rules, id, focus.as_ref()))
    }
}

impl Printer<'_> {
    /// Every fact the program or a table states, as the program names it,
    /// with its place: the rows `diff` compares between two evaluations.
    /// Resources, attributes and contributions are the plan's, not rows.
    pub fn stated(&self, rules: &[RuleStmt]) -> Vec<Because> {
        let s = self.surface(rules);
        let c = self.circuit;
        let mut out = Vec::new();
        for f in c.facts() {
            if matches!(f.pred.as_str(), "want" | "attr" | "arg") || f.pred.starts_with("table.") {
                continue;
            }
            let Some(View::Fact { fact, alts, .. }) = c.fact_id(&f).map(|id| c.view(id)) else {
                continue;
            };
            let at = match alts {
                [a] => match c.view(*a) {
                    View::Times { children: [l], .. } => match c.view(*l) {
                        View::Leaf(Leaf::Base { span }) => Some(base_parts(span).0.to_string()),
                        _ => None,
                    },
                    _ => table_row(c, alts),
                },
                _ => None,
            };
            // A fact the compiler states has no place.
            let placed = |at: &str| {
                at.rsplit_once(':')
                    .is_some_and(|(_, l)| !l.is_empty() && l.bytes().all(|b| b.is_ascii_digit()))
            };
            if let Some(at) = at.filter(|at| placed(at)) {
                out.push(Because::new("fact", Some(at), s.fact_text(fact)));
            }
        }
        out
    }
}
