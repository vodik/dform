//! Instancing (R-211): each copy of a component is its definition's
//! items lowered again under it, from the program's nodes. A definition's
//! records are placed by its own `decl`s in every copy; a record its
//! scope cannot place fails the evaluation, never the load (what
//! `deployment::load` and the language server read), whether or not a
//! copy of its definition is made.

use crate::common;
use common::{Scratch, facts};
use dform_core::engine;
use dform_core::parser::parse_program;

/// A component whose own `decl` names its relation's fields.
const COMPONENT: &str = r#"component c {
  decl zone(name, index)
  zone("a", 1)
  zone("b", 2)
  seen(n) where zone({ name = n, index = 2 })
}
"#;

#[test]
fn a_definitions_record_is_placed_by_its_own_decl_in_each_copy() {
    let src = format!("{COMPONENT}resource c x {{}}\nresource c y {{}}\n");
    for copy in ["x", "y"] {
        let seen = format!("{copy}::seen");
        assert_eq!(facts(&src, &seen), [format!("{seen}(\"b\")")], "{seen}");
    }
}

#[test]
fn a_record_its_scope_cannot_place_fails_the_evaluation_copied_or_not() {
    let bad = COMPONENT.replace("index = 2", "size = 2");
    for src in [bad.clone(), format!("{bad}resource c x {{}}\n")] {
        let program = parse_program(&src).unwrap_or_else(|e| panic!("{e:#}\n{src}"));
        let e = format!("{:#}", engine::eval(&program, &[]).unwrap_err());
        assert!(
            e.contains("unknown field 'size' for predicate 'zone'"),
            "{e}\n{src}"
        );
    }
}

/// [`COMPONENT`]'s own rule, its record wrapped in a list comprehension:
/// the record is placed by `zone`'s `decl` before the engine sees it,
/// not left for it to match structurally.
const COMPREHENSION: &str = r#"component c {
  decl zone(name, index)
  zone("a", 1)
  zone("b", 2)
  seen(l) where l = [n | zone({ name = n, index = 2 })]
}
"#;

#[test]
fn a_record_inside_a_comprehension_is_resolved_before_the_engine_sees_it() {
    let src = format!("{COMPREHENSION}resource c x {{}}\n");
    assert_eq!(facts(&src, "x::seen"), [r#"x::seen(["b"])"#]);
}

/// A relation outside [`COMPONENT`], matched by a record in the `where`
/// of the copy's own row (`resource c NAME {} where ..`): the row that
/// names the copy is placed by the `decl` too, before the copy is made.
const ROWS: &str = r#"decl candidate(name, index)
candidate("a", 1)
candidate("b", 2)
"#;

#[test]
fn a_record_in_a_copy_blocks_row_is_resolved_before_the_engine_sees_it() {
    let src = format!(
        "{ROWS}{COMPONENT}resource c \"z-${{n}}\" {{}} where candidate({{ name = n, index = 2 }})\n"
    );
    assert_eq!(facts(&src, "z-${n}::seen"), [r#"z-${n}::seen("z-b", "b")"#]);
}

/// A component nested inside another, inside a module used twice under
/// different instances: the inner component's bare read of the outer's
/// resource goes through the outer copy's scope and the module
/// instance's own, once each, never the module instance's scope twice.
const NESTED: &str = r#"input tag: int
component outer {
  resource net.vpc v { cidr = "10.${tag}.0.0/16" }
  component inner {
    resource net.vpc net { cidr = v.cidr }
  }
  resource inner i {}
}
"#;

fn cidrs(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("cidr = "))
        .collect()
}

#[test]
fn a_scope_read_through_a_module_instance_in_a_scope_column_is_prefixed_once() {
    let s = Scratch::project("instancing-scope-column");
    s.write("mod.df", NESTED);
    s.write(
        "main.df",
        "use fake\nuse mod as a { tag = 1 }\nuse mod as b { tag = 2 }\n\
         resource a.outer x {}\nresource b.outer y {}\n",
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        cidrs(&r.stdout),
        [
            "\"10.1.0.0/16\"",
            "\"10.1.0.0/16\"",
            "\"10.2.0.0/16\"",
            "\"10.2.0.0/16\"",
        ],
        "{}",
        r.stdout
    );
}
