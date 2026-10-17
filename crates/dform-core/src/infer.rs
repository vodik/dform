//! Column types (R-34): every relation's columns are typed, declared by a
//! `decl` or inferred from their uses. They settle once, with the
//! readings, per definition ([`crate::edges`]: the columns, the checks);
//! this is what they settle as, a relation's signature (`az(name:
//! string, index: int)`, what the language server hovers and `why`
//! prints), the program's own relations, and an undeclared table's
//! columns read from its first document.

use crate::ast::{Atom, Lit, Program, Stmt, TypeExpr};
use crate::edges::lattice::{function, untyped};
use crate::program::Scopes;
use crate::types::Ty;
use anyhow::Result;
use std::collections::BTreeMap;

/// A relation's columns as declared or inferred.
#[derive(Debug, Clone, PartialEq)]
pub struct Signature {
    pub pred: String,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// The `decl`'s name for it, or the variable a rule's head writes.
    pub name: Option<String>,
    /// `None`: nothing gives it a type (`any`).
    pub ty: Option<Ty>,
}

impl std::fmt::Display for Signature {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let cols: Vec<String> = self
            .columns
            .iter()
            .map(|c| {
                let ty = c.ty.as_ref().map_or("any".to_string(), Ty::to_string);
                match &c.name {
                    Some(n) => format!("{n}: {ty}"),
                    None => ty,
                }
            })
            .collect();
        write!(f, "{}({})", self.pred, cols.join(", "))
    }
}

/// Every relation's signature, by the definition that declares it (its
/// path, `""` the program's), its name and its arity: settled once, at
/// the readings' stage (R-192), per definition, every copy's private
/// relation its definition's (R-211).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Signatures(BTreeMap<(String, String, usize), Signature>);

impl Signatures {
    /// The relation `pred` of `arity` the definition at `def` declares
    /// has the columns `sig`.
    pub fn insert(&mut self, def: String, arity: usize, sig: Signature) {
        self.0.insert((def, sig.pred.clone(), arity), sig);
    }

    /// Every signature, by definition, name and arity.
    pub fn values(&self) -> impl Iterator<Item = &Signature> {
        self.0.values()
    }

    /// The signature of the relation an evaluated fact names, `pred` of
    /// `arity`: the program's own, or a copy's private relation (`n::p`,
    /// `n.inner::p`) its definition's, found by the copies' names in the
    /// program's `scopes`.
    pub fn of(&self, pred: &str, arity: usize, scopes: &Scopes) -> Option<&Signature> {
        let (def, name) = match pred.rsplit_once("::") {
            Some((copies, name)) => {
                let body = scopes.copied(copies)?;
                (scopes.definition(body)?.0.clone(), name)
            }
            None => (String::new(), pred),
        };
        self.0.get(&(def, name.to_string(), arity))
    }
}

/// The relations of `program` (lowered) that are the program's: what its
/// facts, its rules' heads and their bodies name, but a function's call,
/// a table's read and what the compiler writes with values of any type.
pub fn relations(program: &Program) -> std::collections::BTreeSet<String> {
    arities(program)
        .into_keys()
        .filter(|p| !untyped(p))
        .map(|p| p.name().to_string())
        .filter(|p| function(p).is_none() && !p.starts_with("table."))
        .collect()
}

/// The arities of every relation `program` has: its facts', its rules'
/// heads' and their bodies'.
fn arities(program: &Program) -> BTreeMap<crate::ast::Name, std::collections::BTreeSet<usize>> {
    let mut arities: BTreeMap<crate::ast::Name, std::collections::BTreeSet<usize>> =
        BTreeMap::new();
    let mut add = |a: &Atom| {
        arities
            .entry(a.pred.clone())
            .or_default()
            .insert(a.args.len());
    };
    for s in &program.statements {
        match s {
            Stmt::Fact(a) => add(a),
            Stmt::Rule(r) => {
                add(&r.head);
                for l in &r.body {
                    if let Lit::Pos(a) | Lit::Not(a) = l {
                        add(a);
                    }
                }
            }
            _ => {}
        }
    }
    arities
}

/// A table's columns read from its first document (R-34, R-55: a
/// relation with no `decl` takes its first source's): the first row's
/// keys in the order the document writes them (a CSV's header), each typed
/// by its value where the value says (`int`, `bool`); a string, and every
/// CSV cell, is unknown until the program's uses settle it.
pub fn document_columns(
    format: &str,
    table: &str,
    text: &str,
) -> Result<Vec<(String, Option<TypeExpr>)>> {
    use anyhow::{Context, anyhow};
    let first = match format {
        "csv" => {
            let mut r = csv::Reader::from_reader(text.as_bytes());
            let header = r
                .headers()
                .context("a CSV table's first line names its columns")?;
            return Ok(header.iter().map(|h| (h.to_string(), None)).collect());
        }
        "json" => serde_json::from_str::<Vec<Row>>(text)
            .context("a JSON table is a list of objects, one per row")?
            .into_iter()
            .next(),
        "yaml" => match crate::tables::yaml_stream(text)? {
            crate::tables::Stream::One(doc) => serde_yaml::from_value::<Vec<Row>>(doc),
            crate::tables::Stream::Many(docs, _) => {
                serde_yaml::from_value::<Vec<Row>>(serde_yaml::Value::Sequence(docs))
            }
        }
        .context("a YAML table is a list of mappings, one per row")?
        .into_iter()
        .next(),
        "toml" => toml::from_str::<BTreeMap<String, Vec<Row>>>(text)
            .with_context(|| format!("a TOML table is its rows as `[[{table}]]`"))?
            .remove(table)
            .and_then(|rows| rows.into_iter().next()),
        f => return Err(anyhow!("unknown format {f}")),
    };
    let row = first.ok_or_else(|| anyhow!("it has no rows to read the columns from"))?;
    Ok(row
        .0
        .into_iter()
        .map(|(k, t)| (k, t.0.map(|n| TypeExpr::Name(n.to_string()))))
        .collect())
}

/// A document's row: its keys in order, each with its value's type.
struct Row(Vec<(String, Cell)>);

/// A cell's type where its value says: `None` for a string (unknown) and
/// for anything else the pass does not judge.
struct Cell(Option<&'static str>);

impl<'de> serde::Deserialize<'de> for Row {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Row;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a row: a mapping of its columns")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Row, A::Error> {
                let mut out = Vec::new();
                while let Some((k, v)) = m.next_entry::<String, Cell>()? {
                    out.push((k, v));
                }
                Ok(Row(out))
            }
        }
        d.deserialize_map(V)
    }
}

impl<'de> serde::Deserialize<'de> for Cell {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Cell;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a value")
            }
            fn visit_bool<E>(self, _: bool) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("bool")))
            }
            fn visit_i64<E>(self, _: i64) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("int")))
            }
            fn visit_u64<E>(self, _: u64) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("int")))
            }
            fn visit_f64<E>(self, _: f64) -> std::result::Result<Cell, E> {
                Ok(Cell(Some("float")))
            }
            fn visit_str<E>(self, _: &str) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_unit<E>(self) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_none<E>(self) -> std::result::Result<Cell, E> {
                Ok(Cell(None))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut s: A,
            ) -> std::result::Result<Cell, A::Error> {
                while s.next_element::<serde::de::IgnoredAny>()?.is_some() {}
                Ok(Cell(None))
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut m: A,
            ) -> std::result::Result<Cell, A::Error> {
                while m
                    .next_entry::<serde::de::IgnoredAny, serde::de::IgnoredAny>()?
                    .is_some()
                {}
                Ok(Cell(None))
            }
        }
        d.deserialize_any(V)
    }
}
