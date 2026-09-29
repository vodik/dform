//! Type aliases (docs/grammar.md "Type aliases"): `type NAME = TYPE` is
//! transparent, in scope in its file and wherever the file is imported, a
//! module's once it says `export type NAME`.

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

#[test]
fn an_alias_is_its_type() {
    let s = Scratch::new("alias-transparent");
    s.write(
        "p.df",
        "edition 2027\n\
         type environment = enum(\"dev\", \"prod\")\n\
         type envs = list(environment)\n\
         input env: environment = \"dev\"\n\
         input all: envs = [\"dev\"]\n\
         input rec: { e: environment } = { e: \"dev\" }\n\
         input peering(env: environment, name: string) from csv(\"p.csv\")\n\
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
        "edition 2027\ntype dev = enum(\"a\")\ninput env: enum(dev, prod) = \"dev\"\n",
    );
    assert_eq!(input_types(&s, "p.df"), vec!["env: enum(dev, prod)"]);
}

#[test]
fn a_cycle_is_an_error_naming_both() {
    let s = Scratch::new("alias-cycle");
    s.write(
        "p.df",
        "edition 2027\ntype a = list(b)\ntype b = set(a)\ninput x: a\n",
    );
    let e = load_error(&s, "p.df");
    assert!(e.contains("type alias cycle: a -> b -> a"), "{e}");
    assert!(e.contains("p.df:2") && e.contains("p.df:3"), "{e}");
    let s = Scratch::new("alias-self");
    s.write("p.df", "edition 2027\ntype a = list(a)\ninput x: a\n");
    let e = load_error(&s, "p.df");
    assert!(e.contains("type alias cycle: a -> a"), "{e}");
}

#[test]
fn an_alias_may_not_take_a_builtin_name() {
    let s = Scratch::new("alias-builtin");
    s.write("p.df", "edition 2027\ntype int = string\ninput x: int\n");
    let e = load_error(&s, "p.df");
    assert!(
        e.contains("type alias `int` takes the name of a built-in type"),
        "{e}"
    );
}

#[test]
fn an_import_brings_a_files_aliases() {
    let s = Scratch::new("alias-import");
    s.write(
        "types.df",
        "edition 2027\ntype environment = enum(\"dev\", \"prod\")\n",
    );
    // Through a file that imports it, too: an import inlines the file.
    s.write("more.df", "edition 2027\nimport \"types.df\"\n");
    s.write(
        "p.df",
        "edition 2027\nimport \"more.df\"\ninput env: environment = \"dev\"\n",
    );
    assert_eq!(input_types(&s, "p.df"), vec!["env: enum(dev, prod)"]);
    // A file that does not import it does not see it.
    s.write(
        "q.df",
        "edition 2027\nimport \"lib.df\"\ninput env: string = \"dev\"\n",
    );
    s.write(
        "lib.df",
        "edition 2027\nimport \"types.df\"\nmodule m {\n  input e: environment\n}\n",
    );
    s.write(
        "r.df",
        "edition 2027\nimport \"q.df\"\ninput x: environment\n",
    );
    assert_eq!(
        input_types(&s, "r.df"),
        vec!["e: enum(dev, prod)", "env: string", "x: enum(dev, prod)"]
    );
    s.write("alone.df", "edition 2027\ninput x: environment\n");
    assert_eq!(input_types(&s, "alone.df"), vec!["x: environment"]);
}

#[test]
fn a_module_exports_an_alias() {
    let s = Scratch::new("alias-export");
    s.write(
        "lib.df",
        "edition 2027\n\
         module platform {\n\
           type tier = enum(\"gold\", \"silver\")\n\
           type hidden = int\n\
           export type tier\n\
           input t: tier\n\
           input h: hidden\n\
         }\n\
         ",
    );
    s.write(
        "p.df",
        "edition 2027\nimport \"lib.df\"\ninput t: tier\ninput h: hidden\n",
    );
    assert_eq!(
        input_types(&s, "p.df"),
        vec![
            "t: enum(gold, silver)",
            "h: int",
            "t: enum(gold, silver)",
            "h: hidden"
        ]
    );
    s.write(
        "bad.df",
        "edition 2027\nmodule m {\n  export type nope\n}\nexport type top\n",
    );
    let e = load_error(&s, "bad.df");
    assert!(
        e.contains("`export type nope`: the module declares no alias nope"),
        "{e}"
    );
    assert!(e.contains("`export type` is a module's"), "{e}");
}

#[test]
fn two_aliases_of_one_name_are_an_error_listing_both() {
    let s = Scratch::new("alias-twice");
    s.write("a.df", "edition 2027\ntype environment = enum(\"dev\")\n");
    s.write("b.df", "edition 2027\ntype environment = enum(\"prod\")\n");
    s.write(
        "p.df",
        "edition 2027\nimport \"a.df\"\nimport \"b.df\"\ninput env: environment\n",
    );
    let e = load_error(&s, "p.df");
    assert!(
        e.contains("2 type aliases named `environment` are in scope"),
        "{e}"
    );
    assert!(e.contains("a.df:2") && e.contains("b.df:2"), "{e}");
    // The same file reached twice is one alias.
    s.write("c.df", "edition 2027\nimport \"a.df\"\n");
    s.write(
        "q.df",
        "edition 2027\nimport \"a.df\"\nimport \"c.df\"\ninput env: environment\n",
    );
    assert_eq!(input_types(&s, "q.df"), vec!["env: enum(dev)"]);
    // A module's alias and its file's, of one name, are two in its scope.
    s.write(
        "m.df",
        "edition 2027\ntype k = int\nmodule m {\n  type k = string\n  input x: k\n}\n",
    );
    let e = load_error(&s, "m.df");
    assert!(e.contains("2 type aliases named `k` are in scope"), "{e}");
}
