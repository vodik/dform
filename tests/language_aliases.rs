//! Type aliases (docs/grammar.md "Type aliases"): `type NAME = TYPE` is
//! transparent, in scope in its file or component, and public: another
//! module reads it through the module's name, `types.environment` (R-65).

mod common;
use common::Scratch;
use dform::ast::Stmt;

/// Each input's type, as the program lowered it: `name: type`.
fn input_types(s: &Scratch, entry: &str) -> Vec<String> {
    let program = dform::loader::load_program(&[s.path(entry)]).unwrap_or_else(|e| panic!("{e:#}"));
    let mut out = Vec::new();
    fn walk(stmts: &[Stmt], out: &mut Vec<String>) {
        for st in stmts {
            match st {
                Stmt::Input(i) => {
                    out.push(format!("{}: {}", i.name, dform::inputs::type_text(&i.ty)))
                }
                Stmt::Module(m) => walk(&m.body, out),
                _ => {}
            }
        }
    }
    walk(&program.statements, &mut out);
    out
}

fn load_error(s: &Scratch, entry: &str) -> String {
    match dform::loader::load_program(&[s.path(entry)]) {
        Ok(_) => panic!("{entry} loaded"),
        Err(e) => format!("{e:#}"),
    }
}

/// An input in the header reads an alias the body declares below it:
/// resolution is program-wide (R-27).
#[test]
fn an_alias_is_its_type() {
    let s = Scratch::new("alias-transparent");
    s.write(
        "p.df",
        "edition 2026\n\
         input env: environment = \"dev\"\n\
         input all: envs = [\"dev\"]\n\
         input rec: { e: environment } = { e: \"dev\" }\n\
         input peering(env: environment, name: string) from csv(\"p.csv\")\n\
         type environment = enum(\"dev\", \"prod\")\n\
         type envs = list(environment)\n\
         ",
    );
    assert_eq!(
        input_types(&s, "p.df"),
        vec![
            "env: enum(dev, prod)",
            "all: list(enum(dev, prod))",
            "rec: { e: enum(dev, prod) }",
        ]
    );
    // The table's column is the enum too: a row outside it is an error.
    let program = dform::loader::load_program(&[s.path("p.df")]).unwrap();
    let text = format!("{:?}", program.statements);
    assert!(text.contains("Apply(\"enum\""), "{text}");
    assert!(!text.contains("environment"), "{text}");
}

#[test]
fn an_enum_member_is_a_value_not_an_alias() {
    let s = Scratch::new("alias-enum");
    s.write(
        "p.df",
        "edition 2026\ninput env: enum(dev, prod) = \"dev\"\ntype dev = enum(\"a\")\n",
    );
    assert_eq!(input_types(&s, "p.df"), vec!["env: enum(dev, prod)"]);
}

#[test]
fn a_cycle_is_an_error_naming_both() {
    let s = Scratch::new("alias-cycle");
    s.write(
        "p.df",
        "edition 2026\ninput x: a\ntype a = list(b)\ntype b = set(a)\n",
    );
    let e = load_error(&s, "p.df");
    assert!(e.contains("type alias cycle: a -> b -> a"), "{e}");
    assert!(e.contains("p.df:3") && e.contains("p.df:4"), "{e}");
    let s = Scratch::new("alias-self");
    s.write("p.df", "edition 2026\ninput x: a\ntype a = list(a)\n");
    let e = load_error(&s, "p.df");
    assert!(e.contains("type alias cycle: a -> a"), "{e}");
}

#[test]
fn an_alias_may_not_take_a_builtin_name() {
    let s = Scratch::new("alias-builtin");
    s.write("p.df", "edition 2026\ninput x: int\ntype int = string\n");
    let e = load_error(&s, "p.df");
    assert!(
        e.contains("type alias `int` takes the name of a built-in type"),
        "{e}"
    );
}

/// Another module's alias is its own, read by the name a `use` or an
/// `instance` gives the module (R-65): `types.environment`, never bare.
#[test]
fn another_modules_alias_is_read_through_its_name() {
    let s = Scratch::new("alias-paths");
    s.write(
        "types.df",
        "edition 2026\ntype environment = enum(\"dev\", \"prod\")\n",
    );
    s.write(
        "p.df",
        "edition 2026\ninput env: types.environment = \"dev\"\ninput bare: environment\nuse types\n",
    );
    assert_eq!(
        input_types(&s, "p.df"),
        vec!["env: enum(dev, prod)", "bare: environment"]
    );
    // `use .. as` names it otherwise.
    s.write(
        "q.df",
        "edition 2026\ninput env: t.environment = \"dev\"\nuse types as t\n",
    );
    assert_eq!(input_types(&s, "q.df"), vec!["env: enum(dev, prod)"]);
}

/// A used module's aliases are public: `lib.tier` where it is used, its
/// own inputs typed by them inside.
#[test]
fn a_used_modules_alias_is_public() {
    let s = Scratch::new("alias-component");
    s.write(
        "lib.df",
        "edition 2026\n\
         input t: tier\n\
         type tier = enum(\"gold\", \"silver\")\n\
         ",
    );
    s.write(
        "p.df",
        "edition 2026\ninput t: lib.tier = \"gold\"\nuse lib { t }\n",
    );
    assert_eq!(
        input_types(&s, "p.df"),
        vec!["t: enum(gold, silver)", "t: enum(gold, silver)"]
    );
}

#[test]
fn two_aliases_of_one_name_are_an_error_listing_both() {
    // Two modules' aliases of one name are two names: each is read
    // through its module.
    let s = Scratch::new("alias-twice");
    s.write("a.df", "edition 2026\ntype environment = enum(\"dev\")\n");
    s.write("b.df", "edition 2026\ntype environment = enum(\"prod\")\n");
    s.write(
        "p.df",
        "edition 2026\ninput x: a.environment\ninput y: b.environment\nuse a\nuse b\n",
    );
    assert_eq!(
        input_types(&s, "p.df"),
        vec!["x: enum(dev)", "y: enum(prod)"]
    );
    // A component's alias and its file's, of one name, are two in its scope.
    s.write(
        "m.df",
        "edition 2026\ntype k = int\ncomponent m {\n  type k = string\n  input x: k\n}\n",
    );
    let e = load_error(&s, "m.df");
    assert!(e.contains("2 type aliases named `k` are in scope"), "{e}");
}
