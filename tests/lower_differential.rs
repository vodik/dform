//! R-211's gate: every `.df` of the corpus (examples/, tests/fixtures,
//! tests/syntax/ok and err, the mock's schemas) is lowered by the resolver
//! and through the program, and the two compared statement by statement,
//! every span with its origin and every diagnostic included
//! (`program::check::dump`), and every term, written literal and clause
//! in them as well, built as nodes and lowered back
//! (`program::check::{term, literal, clause}`), and the items counted by
//! kind. The same comparison runs in every
//! `lower_stack` of the suite under `DFORM_CHECK_LOWER=1`; both are deleted
//! with the old path at the migration's end.

mod common;
use common::{corpus, df_files, rel, repo};
use dform::program::check;
use std::path::Path;

/// A provider's schema and the mock's are core text, read as a test of
/// the core is; every other file is a program loaded as its stack would
/// be, modules and all.
fn lower(f: &Path) {
    let core = f
        .components()
        .any(|c| c.as_os_str() == "providers" || c.as_os_str() == "schemas");
    let _ = match core {
        true => dform::parser::parse_program(&std::fs::read_to_string(f).unwrap()),
        false => dform::loader::load_program(&[f.to_path_buf()]),
    };
}

#[test]
fn the_program_lowers_as_the_resolver_does() {
    let mut files = corpus();
    df_files(&repo().join("tests/syntax/err"), true, &mut files);
    assert!(files.len() > 60, "{files:?}");
    let mut failures = Vec::new();
    let mut total = check::Collected::default();
    for f in &files {
        let ((), mut seen) = check::collect(|| lower(f));
        let differences = std::mem::take(&mut seen.differences);
        failures.extend(differences.into_iter().map(|d| format!("{}: {d}", rel(f))));
        total.add(seen);
    }
    // A query's pattern and the text a refinement prints are read by the
    // same resolver, in modes of their own.
    let modes: [(&str, fn()); 2] = [
        ("a pattern", || {
            let _ = dform::parser::parse_pattern("attr(leaky.vault, a, .password, v)");
        }),
        ("a refinement's text", || {
            let _ = dform::parser::parse_literal_text("p(regex(\"^[a-z]{3}$\"), enum([\"a\"]))");
        }),
    ];
    for (what, read) in modes {
        let ((), mut seen) = check::collect(read);
        let differences = std::mem::take(&mut seen.differences);
        failures.extend(differences.into_iter().map(|d| format!("{what}: {d}")));
        total.add(seen);
    }
    let check::Collected {
        compared,
        terms,
        literals,
        clauses,
        folds,
        built,
        items,
        reads,
        ..
    } = total;
    assert!(
        failures.is_empty(),
        "{} of {compared} lowerings differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(compared >= files.len() / 2, "only {compared} compared");
    // Every term, written literal and clause the resolver lowers is built
    // as nodes and lowered back.
    eprintln!(
        "{compared} lowerings, {terms} terms, {literals} literals, {clauses} clauses, {folds} folds"
    );
    assert!(terms >= 10 * compared, "only {terms} terms compared");
    assert!(
        literals >= 4 * compared,
        "only {literals} literals compared"
    );
    assert!(clauses >= 2 * compared, "only {clauses} clauses compared");
    assert!(folds > 0, "no rule folded over its aggregates");
    eprintln!("goals built: {built:?}");
    // How far the port has come: the items by kind (`a statement`, one
    // not yet ported).
    eprintln!("items: {items:?}");
    assert!(items.get("a let").is_some_and(|n| *n > 0), "{items:?}");
    // Reads built as the read's own node, not the goal it lowered to.
    eprintln!("reads: {reads:?}");
    for kind in ["value", "attribute", "output", "lookup"] {
        assert!(reads.get(kind).is_some_and(|n| *n > 0), "{kind}: {reads:?}");
    }
}
