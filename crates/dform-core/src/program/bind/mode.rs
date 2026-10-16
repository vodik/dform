//! A relation's mode (R-187): which of its columns a reference reads, so
//! the clause must bind them first, and which it binds. A let with
//! parameters, a provider's table and a std function written as a
//! relation are one kind of relation with a mode: a let reads its
//! parameters' columns, a table its `+` columns, a function every column;
//! any other relation binds every column.

use crate::program::node::RelRef;
use crate::program::{ScopeId, Scopes};

/// How a reference reads a relation's columns: each column it reads, by
/// position, and whether it reads the columns past those.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Mode {
    pub reads: Vec<bool>,
    pub rest: bool,
}

impl Mode {
    /// The mode of the relation `rel` a read in `scope` names.
    pub fn of(scopes: &Scopes, scope: ScopeId, rel: &RelRef) -> Mode {
        let pred = rel.pred();
        if crate::functions::callable(&pred) {
            return Mode {
                reads: Vec::new(),
                rest: true,
            };
        }
        let params = scopes
            .function_columns(scope, &rel.name)
            .map_or(0, |n| n - 1);
        let table: Vec<bool> = scopes
            .externs
            .get(&pred)
            .map(|cols| cols.iter().map(|(read, _)| *read).collect())
            .unwrap_or_default();
        let reads = (0..params.max(table.len()))
            .map(|i| i < params || table.get(i).copied().unwrap_or(false))
            .collect();
        Mode { reads, rest: false }
    }

    /// Whether column `i` is read.
    pub fn reads(&self, i: usize) -> bool {
        self.reads.get(i).copied().unwrap_or(self.rest)
    }
}
