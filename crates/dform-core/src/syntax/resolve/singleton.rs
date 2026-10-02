//! A variable written once in its rule is an error (DESIGN.org R-2): with
//! one occurrence it joins nothing, so it is a typo (`link(bb, c)` beside
//! `reaches(a, b)` is a cross product) or a placeholder that should say
//! so. Its occurrences are counted in the source, across the whole rule: a
//! block's header, clause, entries and interpolated names together, a
//! `not { }` body once. `_` and a name starting with `_` are exempt, and so
//! is a variable an enclosing statement binds.

use super::*;

/// Where each source variable of a rule is written: distinct places, so a
/// name lowered twice from one place (a probe, then the lowering) counts
/// once.
#[derive(Default, Clone)]
pub(super) struct Uses(BTreeMap<String, BTreeMap<(u32, u32, u32), Span>>);

impl Uses {
    pub(super) fn add(&mut self, name: &str, span: Span) {
        self.0
            .entry(name.to_string())
            .or_default()
            .insert((span.file, span.start, span.end), span);
    }
}

impl Lowerer<'_> {
    /// Every variable of the rule `rc` is written at least twice.
    pub(super) fn singletons(&mut self, rc: &Rc) -> L<()> {
        // A query pattern names what it does not join; text the compiler
        // printed has no variables.
        if self.any_type || self.text {
            return Ok(());
        }
        let mut failed = false;
        for (src, places) in &rc.uses.0 {
            if src.starts_with('_') || places.len() != 1 {
                continue;
            }
            // Bound by an enclosing statement: it has no first place here.
            if !rc.first.contains_key(src) {
                continue;
            }
            let span = *places.values().next().expect("one place");
            failed = true;
            let d = Diagnostic::error(
                span,
                format!(
                    "variable `{src}` is used once; a typo, or `_{src}` if it is meant to be \
                     unbound"
                ),
            )
            .with_help(
                "a variable joins where it is written twice; written once it matches anything",
            )
            .with_fix(
                format!("rename it `_{src}`"),
                vec![(span, format!("_{src}"))],
            );
            self.diags.push(d);
        }
        if failed { Err(Skip) } else { Ok(()) }
    }
}
