//! Scenarios (DESIGN.org L12): `scenario name { facts; deny rules }.` A
//! test is policy over hypothetical inputs: the scenario's statements join
//! the program, and any deny (the scenario's or the program's) fails it.
//! `dform test` runs every scenario against an empty mock world;
//! `dform plan --scenario NAME` is the same program as a what-if plan of
//! the stack. Otherwise scenarios are not part of the program.

use crate::ast::{Program, Stmt};
use crate::diag::{Diagnostic, Diagnostics};
use anyhow::Result;
use std::collections::BTreeMap;

/// The program's scenarios, in order; a name used twice is an error.
pub fn names(program: &Program) -> Result<Vec<String>> {
    let mut seen = BTreeMap::new();
    let mut diags = Vec::new();
    let mut out = Vec::new();
    for s in &program.statements {
        if let Stmt::Scenario(sc) = s {
            if let Some(first) = seen.insert(sc.name.clone(), sc.span) {
                diags.push(
                    Diagnostic::error(sc.span, format!("scenario {} is defined twice", sc.name))
                        .with_label(first, "first defined here"),
                );
            } else {
                out.push(sc.name.clone());
            }
        }
    }
    if diags.is_empty() {
        Ok(out)
    } else {
        Err(Diagnostics(diags).into())
    }
}

/// The program with scenario `name`'s statements in it.
pub fn select(program: &Program, name: &str) -> Result<Program> {
    let all = names(program)?;
    let Some(body) = program.statements.iter().find_map(|s| match s {
        Stmt::Scenario(sc) if sc.name == name => Some(sc.body.clone()),
        _ => None,
    }) else {
        if all.is_empty() {
            anyhow::bail!("no scenario {name}: the program has no scenarios");
        }
        anyhow::bail!(
            "no scenario {name}: the program's scenarios are {}",
            all.join(", ")
        );
    };
    let mut out = program.clone();
    out.statements.extend(body);
    Ok(out)
}
