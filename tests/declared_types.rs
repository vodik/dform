//! A declared type is a declaration (R-213): a typed let, input or
//! output has the type it is written with, whatever flows into it. A
//! value of a narrower type given to it (an enum where a string is
//! declared) is a widening, checked assignable and never a constraint on
//! the let's other rules: `let apex: string = env where not is_prod` does
//! not make `""` in its other rule a member of `env`'s enum. An untyped
//! let fed by an enum and a string is a string, the wider. A value that is
//! not assignable is an error naming the let once, at its declaration.

mod common;
use common::{Run, Scratch};

/// A project whose one stack `s` is `src`, after the key `env`.
fn stack(src: &str) -> Scratch {
    let s = Scratch::project("declared");
    s.write(
        "stacks/s.df",
        &format!("key env: enum(\"lab\", \"prod\") = \"lab\"\nuse fake\n{src}"),
    );
    s
}

/// The plan of `s` at `env=value`.
fn plan(s: &Scratch, value: &str) -> Run {
    s.run(&["plan", "--why=none", "s", &format!("env={value}")])
}

/// A vpc named `name`.
fn vpc(name: &str) -> String {
    format!("resource net.vpc v {{ cidr = \"10.0.0.0/16\", name = {name} }}\n")
}

/// The private project's shape: a string let given `""` by one rule and
/// the enum key by the other plans at both of the key's values, and
/// `dform test` runs both.
#[test]
fn a_typed_let_takes_an_enum_and_a_string() {
    let s = stack(&format!(
        "let is_prod: bool = true where env == \"prod\"\n\
         let is_prod: bool = false where env != \"prod\"\n\
         let apex: string = \"\" where is_prod\n\
         let apex: string = env where not is_prod\n{}",
        vpc("apex")
    ));
    let r = plan(&s, "lab").success();
    assert!(r.stdout.contains("name = \"lab\""), "{}", r.stdout);
    let r = plan(&s, "prod").success();
    assert!(r.stdout.contains("name = \"\""), "{}", r.stdout);
    s.run(&["test", "s"]).success();
}

/// A typed input given the enum key, by its instance and by `set`.
#[test]
fn a_typed_input_takes_an_enum() {
    let s = stack(&format!(
        "component box {{\n  input name: string = \"\"\n  {}}}\nresource box b {{ name = env }}\n",
        vpc("name")
    ));
    let r = plan(&s, "prod").success();
    assert!(r.stdout.contains("name = \"prod\""), "{}", r.stdout);
    let s = Scratch::project("declared");
    s.write(
        "stacks/s.df",
        &format!(
            "key env: enum(\"lab\", \"prod\") = \"lab\"\ninput apex: string = \"\"\nuse fake\n\
             set apex = env where env == \"lab\"\n{}",
            vpc("apex")
        ),
    );
    let r = plan(&s, "lab").success();
    assert!(r.stdout.contains("name = \"lab\""), "{}", r.stdout);
}

/// An output declared a string, given `""` by one rule and the enum key
/// by the other.
#[test]
fn a_string_output_takes_an_enum() {
    let s = stack(
        "output o: string = \"\" where env == \"prod\"\n\
         output o: string = env where env != \"prod\"\n",
    );
    plan(&s, "lab").success();
    plan(&s, "prod").success();
}

/// An untyped let given `""` and the enum key is a string, the wider of
/// the two, here and where a component's input reads it.
#[test]
fn an_untyped_let_of_an_enum_and_a_string_is_a_string() {
    let s = stack(&format!(
        "let apex = \"\" where env == \"prod\"\nlet apex = env where env != \"prod\"\n{}",
        vpc("apex")
    ));
    let r = plan(&s, "lab").success();
    assert!(r.stdout.contains("name = \"lab\""), "{}", r.stdout);
    let r = plan(&s, "prod").success();
    assert!(r.stdout.contains("name = \"\""), "{}", r.stdout);
    let s = stack(&format!(
        "component box {{\n  input name: string\n  {}}}\n\
         let given = \"\" where env == \"prod\"\nlet given = env where env != \"prod\"\n\
         resource box b {{ name = given }}\n",
        vpc("name")
    ));
    plan(&s, "prod").success();
}

/// An untyped let still narrows where its literal flows into an enum: a
/// literal that is no member is an error.
#[test]
fn a_literal_given_where_an_enum_is_read_is_checked() {
    let s = stack("let e = \"stage\"\ndeny \"no\" where env == e\n");
    let r = plan(&s, "lab").failure();
    assert!(r.stderr.contains("\"stage\""), "{}", r.stderr);
}

/// A value that is not assignable to the let's type: the let is named
/// once, at its declaration, with the rule that gives it.
#[test]
fn a_value_not_of_the_declared_type_names_the_let() {
    let s = Scratch::project("declared");
    s.write(
        "stacks/s.df",
        "key env: enum(\"lab\", \"prod\") = \"lab\"\ninput count: int = 3\nuse fake\n\
         let apex: string = \"\" where env == \"prod\"\n\
         let apex: string = count where env != \"prod\"\n",
    );
    let r = plan(&s, "lab").failure();
    assert!(
        r.stderr
            .contains("s.df:4:1: let apex: string takes count: an int is not a string"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("count is an int here (input count)"),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("column"), "{}", r.stderr);
}
