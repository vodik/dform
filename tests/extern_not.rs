//! A data source under `not` (R-196): `not T(..)` and `not { T(..) }` read
//! the same absence, checked and asked alike, and `dform fmt`, which writes
//! a one-goal block as the bare form, never changes what a program means.

use dform_core::ast::ExternFn;
use dform_core::externs::Externs;
use dform_core::value::Value;
use std::collections::BTreeSet;

/// Every `not` form over a data source and over a relation of the
/// program, each written both ways; the `_b` relation the block's.
const CORPUS: &str = r#"extern next.hop(+from, -to)

paths("a")
paths("b")
paths("c")
seen("b")
missing(p) where paths(p), not next.hop(p, _)
missing_b(p) where paths(p), not { next.hop(p, _) }
not_to_x(p) where paths(p), not next.hop(p, "x")
not_to_x_b(p) where paths(p), not { next.hop(p, "x") }
not_via(p) where paths(p), not { next.hop(p, t), t == "y" }
unseen(p) where paths(p), not seen(p)
unseen_b(p) where paths(p), not { seen(p) }
"#;

/// What the data source answers: `a` hops to `x`, `b` to `y`, `c` nowhere.
fn ask(f: &ExternFn, ins: &[Value]) -> anyhow::Result<Vec<Vec<Value>>> {
    assert_eq!(f.name, "next.hop");
    let to = match &ins[0] {
        Value::Str(s) if s == "a" => "x",
        Value::Str(s) if s == "b" => "y",
        _ => return Ok(Vec::new()),
    };
    Ok(vec![vec![ins[0].clone(), Value::Str(to.into())]])
}

/// The program's own facts, evaluated with the data source answered: the
/// compiler's helpers left out, since the two forms name theirs apart.
fn meaning(src: &str) -> BTreeSet<String> {
    let p = dform_core::parser::parse_file("t.df", src).unwrap_or_else(|e| panic!("{e:#}"));
    let l = dform_core::transform::lower(&p).unwrap_or_else(|e| panic!("{e:#}\n{src}"));
    let externs = Externs::new(&l.program, &l.extern_fns, ask);
    let (res, violations) = externs.eval(&p, &[]).unwrap();
    assert!(violations.is_empty(), "{violations:?}");
    res.facts
        .iter()
        .filter(|a| !a.pred.starts_with("__"))
        .map(dform_core::spell::atom)
        .collect()
}

/// The facts of `pred` in `facts`, by their arguments.
fn of(facts: &BTreeSet<String>, pred: &str) -> BTreeSet<String> {
    let open = format!("{pred}(");
    facts
        .iter()
        .filter_map(|f| f.strip_prefix(&open))
        .map(String::from)
        .collect()
}

/// The bare form reads what the block form reads, a data source's absence
/// as a relation's.
#[test]
fn a_bare_not_and_a_block_mean_the_same() {
    let m = meaning(CORPUS);
    for (bare, block, want) in [
        ("missing", "missing_b", &["\"c\")"][..]),
        ("not_to_x", "not_to_x_b", &["\"b\")", "\"c\")"][..]),
        ("unseen", "unseen_b", &["\"a\")", "\"c\")"][..]),
    ] {
        let want: BTreeSet<String> = want.iter().map(|s| s.to_string()).collect();
        assert_eq!(of(&m, bare), want, "{bare}: {m:?}");
        assert_eq!(of(&m, block), want, "{block}: {m:?}");
    }
    // The bare form asks its own call: no block beside it asks it.
    let alone = meaning(
        "extern next.hop(+from, -to)\npaths(\"a\")\npaths(\"c\")\n\
         missing(p) where paths(p), not next.hop(p, _)\n",
    );
    assert_eq!(
        of(&alone, "missing"),
        BTreeSet::from(["\"c\")".to_string()])
    );
    assert_eq!(
        of(&m, "not_via"),
        BTreeSet::from(["\"a\")".to_string(), "\"c\")".to_string()])
    );
}

/// `dform fmt` writes a one-goal block as the bare form (the data source's
/// among them), is idempotent, and the program it writes means what the
/// one it read did.
#[test]
fn formatting_every_not_form_keeps_its_meaning() {
    let once = dform_core::fmt::format_source("t.df", CORPUS).unwrap();
    assert!(
        once.contains("missing_b(p) where paths(p), not next.hop(p, _)\n"),
        "{once}"
    );
    assert!(
        once.contains("not { next.hop(p, t), t == \"y\" }"),
        "{once}"
    );
    assert_eq!(dform_core::fmt::format_source("t.df", &once).unwrap(), once);
    assert_eq!(meaning(&once), meaning(CORPUS), "{once}");
}
