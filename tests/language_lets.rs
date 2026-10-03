//! `let` is a cell of the attribute aggregate (DESIGN.org R-3), like an
//! input: rows that agree are one value, rows that disagree at the winning
//! rank are a conflict naming both, a `@default` row gives way.

mod common;
use common::Scratch;

fn plan(src: &str) -> common::Run {
    let s = Scratch::new("lang-lets");
    s.write("p.df", src);
    s.run(&["dev", "--world", "w.json", "plan", "p.df"])
}

/// Two rows that disagree: one conflict naming both rows, not two of
/// everything that reads `n`.
#[test]
fn two_rows_that_disagree_are_a_conflict_naming_both() {
    let r = plan(
        r#"
ok(1)
let n = "a"
let n = "b" where ok(1)
resource net.vpc v { cidr = n }
provider fake
"#,
    )
    .failure();
    assert!(r.stdout.contains("conflicts:\n! "), "{}", r.stdout);
    for at in ["(at p.df:3:1)", "(at p.df:4:1)"] {
        assert!(r.stdout.contains(at), "{at}\n---\n{}", r.stdout);
    }
    assert!(!r.stdout.contains("cidr = \"a\""), "{}", r.stdout);
    assert!(!r.stdout.contains("cidr = \"b\""), "{}", r.stdout);
}

/// Rows that agree are one value: one resource.
#[test]
fn two_rows_that_agree_are_one_value() {
    let r = plan(
        r#"
ok(1)
let n = "a"
let n = "a" where ok(1)
resource net.vpc "v-${n}" { cidr = n }
provider fake
"#,
    )
    .success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 create)",
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("cidr = \"a\""), "{}", r.stdout);
}

/// A `@default` row gives way to a conditional row when it holds, and is
/// the value when it does not.
#[test]
fn a_default_row_gives_way_to_a_conditional_one() {
    let src = |flag: &str| {
        format!(
            "\n\
             flag(\"{flag}\")\n\
             let size = \"small\" @default\n\
             let size = \"large\" where flag(\"on\")\n\
             resource net.vpc v {{ size = size }}\nprovider fake\n"
        )
    };
    let r = plan(&src("on")).success();
    assert!(r.stdout.contains("size = \"large\""), "{}", r.stdout);
    assert!(!r.stdout.contains("conflicts:"), "{}", r.stdout);
    let r = plan(&src("off")).success();
    assert!(r.stdout.contains("size = \"small\""), "{}", r.stdout);
}

/// A component's `let` is a cell per copy: two copies that compute two
/// values do not conflict.
#[test]
fn a_component_let_is_scoped_to_its_copy() {
    let r = plan(
        r#"
component m {
  input n: int
  let size = n
  resource net.vpc vpc { size = size }
}
instance m a { n = 1 }
instance m b { n = 2 }
provider fake
"#,
    )
    .success();
    assert!(
        r.stdout.contains("+ net.vpc[\"a::vpc\"]\n  size = 1\n"),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains("+ net.vpc[\"b::vpc\"]\n  size = 2\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("conflicts:"), "{}", r.stdout);
}
