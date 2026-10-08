//! A component's body reads its module's items bare (R-186): its own
//! names first, then the module's (its lets, inputs, resources and
//! relations), of the instance the copy was taken from, then the
//! stack's; never the copy's user's. `super.x` reads the scope around
//! the component where its own `x` shadows it; a module's body has no
//! scope around it. On the mock.

mod common;
use common::Scratch;

/// A module whose input, let, resource and relation its component reads
/// bare at `READ`, a cidr of the copy's network.
const MODULE: &str = r#"input tag: string
let label = "l-${tag}"
resource db.postgres repo { name = "repo-${tag}" }
zone("z-${tag}") where tag != ""
component volume {
  resource net.vpc net { cidr = READ }
}
"#;

fn project(name: &str, read: &str, main: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("backups.df", &MODULE.replace("READ", read));
    s.write("main.df", main);
    s
}

/// The plan's cidr lines, in order.
fn cidrs(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("cidr = "))
        .collect()
}

const TWO: &str = r#"use fake
use backups as a { tag = "a" }
use backups as b { tag = "b" }
let label = "the user's"
resource a.volume x {}
resource b.volume y {}
"#;

/// Under two `use`s of one module each copy reads the instance it was
/// taken from: a let, an input, a resource and a relation, bare and by
/// the module's own name; never its user's `label`.
#[test]
fn a_copy_reads_the_module_instance_it_was_taken_from() {
    for (read, x, y) in [
        ("label", "\"l-a\"", "\"l-b\""),
        ("\"${tag}\"", "\"a\"", "\"b\""),
        ("repo.name", "\"repo-a\"", "\"repo-b\""),
        ("backups.repo.name", "\"repo-a\"", "\"repo-b\""),
        ("backups.label", "\"l-a\"", "\"l-b\""),
        ("z } where zone(z) #", "\"z-a\"", "\"z-b\""),
        ("z } where backups.zone(z) #", "\"z-a\"", "\"z-b\""),
    ] {
        let s = project("scope-two", read, TWO);
        let r = s.run(&["plan", "--why=none", "main.df"]).success();
        assert_eq!(cidrs(&r.stdout), [x, y], "{read}\n{}", r.stdout);
    }
}

/// The reviewer's shape on the mock: the module's resource read bare in
/// its component, the copy made in the stack through `use backups`.
#[test]
fn a_modules_resource_reads_bare_in_its_component() {
    let s = project(
        "scope-bare",
        "repo.name",
        "use fake\nuse backups { tag = \"t\" }\nresource backups.volume x {}\n",
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(cidrs(&r.stdout), ["\"repo-t\""], "{}", r.stdout);
}

/// A copy made in the module's own file reads the instance it stands in.
#[test]
fn a_copy_in_the_modules_file_reads_its_instance() {
    let s = project("scope-own", "label", TWO);
    s.write(
        "backups.df",
        &format!(
            "{}resource volume inside {{}}\n",
            MODULE.replace("READ", "label")
        ),
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        cidrs(&r.stdout),
        ["\"l-a\"", "\"l-b\"", "\"l-a\"", "\"l-b\""],
        "{}",
        r.stdout
    );
}

/// A copy by the component's path with no `use` of its module reads no
/// instance: the read is the error, naming the `use`.
#[test]
fn a_copy_by_its_path_without_a_use_reads_no_instance() {
    let s = project(
        "scope-path",
        "label",
        "use fake\nresource backups.volume x {}\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "Error: backups.df:6:33: `label` is an item of module backups, and the copy x of \
             backups.volume is made outside every instance of backups: a component reads the \
             items of the instance it is taken from"
        ) && r.stderr.contains(
            "Help: `use backups` beside the copy: `resource backups.volume x` then reads that \
             instance's `label`"
        ),
        "{}",
        r.stderr
    );
}

/// A component's own name shadows the module's: a bare read is the
/// component's, `super.x` the module's, and nothing warns (R-209): the
/// inner is the predictable answer, as in Rust.
#[test]
fn super_reads_what_the_components_own_name_shadows() {
    let s = Scratch::project("scope-shadow");
    s.write(
        "backups.df",
        r#"input tag: string
resource db.postgres repo { name = "repo-${tag}" }
component volume {
  input tag: string = "own"
  resource db.postgres repo { name = "own" }
  resource net.vpc net { cidr = "${tag} ${super.tag} ${repo.name} ${super.repo.name}" }
}
"#,
    );
    s.write("main.df", TWO);
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        cidrs(&r.stdout),
        ["\"own a own repo-a\"", "\"own b own repo-b\""],
        "{}",
        r.stdout
    );
    assert!(!r.stderr.contains("warning"), "{}", r.stderr);
}

/// A component inside another reads the enclosing component's names,
/// its copy's: bare, and by `super` where its own shadows them; two
/// `super`s are the stack's.
#[test]
fn a_nested_components_super_is_the_enclosing_copy() {
    let s = Scratch::project("scope-nested");
    s.write(
        "main.df",
        r#"use fake
let tag = "stack"
component outer {
  input tag: string
  resource db.postgres d { name = "d-${tag}" }
  component inner {
    input tag: string = "inner"
    resource net.vpc net { cidr = "${tag} ${super.tag} ${super.super.tag} ${d.name}" }
  }
  resource inner i {}
}
resource outer x { tag = "x" }
resource outer y { tag = "y" }
"#,
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(
        cidrs(&r.stdout),
        ["\"inner x stack d-x\"", "\"inner y stack d-y\""],
        "{}",
        r.stdout
    );
}

/// `super` in a module's body is an error: a module never reaches its
/// user, and the help is the input that would; past a component's
/// module it is the same error, and at the stack's top level its own.
#[test]
fn super_in_a_modules_body_is_an_error() {
    let s = Scratch::project("scope-super-module");
    s.write(
        "backups.df",
        "resource db.postgres repo { name = super.region }\n",
    );
    s.write("main.df", "use fake\nuse backups\n");
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "Error: backups.df:1:36: `super` in module backups: a module has no scope \
             around it"
        ) && r.stderr.contains(
            "Help: a module never reaches its user: declare `input region: TYPE` in it and give \
             it in the `use`, `use backups { region = .. }`"
        ),
        "{}",
        r.stderr
    );
    s.write(
        "backups.df",
        "input region: string = \"r\"\ncomponent volume {\n  resource db.postgres repo { name = \
         super.super.region }\n}\n",
    );
    s.write(
        "main.df",
        "use fake\nuse backups\nresource backups.volume x {}\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "Error: backups.df:3:38: `super` in module backups: a module has no scope \
             around it"
        ) && r
            .stderr
            .contains("Help: read module backups's own as `super.region`"),
        "{}",
        r.stderr
    );
    s.write(
        "main.df",
        "use fake\nlet region = \"r\"\nresource db.postgres repo { name = super.region }\n",
    );
    let r = s.run(&["plan", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "Error: main.df:3:36: `super` at the stack's top level: it names the scope around a \
             component"
        ) && r.stderr.contains("Help: read `region` bare"),
        "{}",
        r.stderr
    );
}

/// A component in the stack's file reads the stack's names whatever copy
/// is around its copy: a component that declares the name and makes the
/// copy does not take the read.
#[test]
fn a_component_reads_the_stack_not_the_copy_around_it() {
    let s = Scratch::project("scope-stack");
    s.write(
        "main.df",
        r#"use fake
let tag = "stack"
component leaf {
  resource net.vpc net { cidr = tag }
}
component around {
  input tag: string = "around"
  resource leaf l {}
}
resource around x {}
"#,
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert_eq!(cidrs(&r.stdout), ["\"stack\""], "{}", r.stdout);
}
