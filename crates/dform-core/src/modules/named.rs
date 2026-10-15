//! A copy named by its clause (R-191): `resource node "agent-${i}" { .. }
//! where i in 0..agents` makes one copy of `node` per row of the clause,
//! named by the header's term as a provider type's resource is. The
//! component is expanded once, under the scope its header writes
//! (`agent-${i}`), and then given a column: every relation private to it
//! takes the copy's name first, and every scope it writes (`agent-${i}`,
//! `agent-${i}.inner`, `format("agent-${i}.%s", ..)` for a copy inside
//! it named by its own clause) is that name's. A copy with a static name
//! is the case where the name is a constant and the column is folded
//! away: one mechanism.

use crate::ast::Term;
use crate::value::Value;

/// Whether a scope's segment is a header with holes, a copy named by its
/// clause: each such segment of a private relation's scope is a column
/// before the relation's own.
fn by_clause(segment: &str) -> bool {
    segment.contains("${")
}

/// A private relation's scope with each segment named by a clause read
/// off the fact's first columns, `agent-${i}.inner` with `"agent-0"` as
/// `agent-0.inner`, and the columns that are the relation's own.
pub fn runtime_scope<'a>(scope: &str, args: &'a [Term]) -> (String, &'a [Term]) {
    let mut used = 0;
    let segments: Vec<String> = crate::address::path_segments(scope)
        .into_iter()
        .map(|seg| match (by_clause(seg), args.get(used)) {
            (true, Some(Term::Val(Value::Str(name)))) => {
                used += 1;
                name.clone()
            }
            _ => seg.to_string(),
        })
        .collect();
    (segments.join("."), &args[used..])
}
