//! A module reads only what it declares (R-205): scope is lexical
//! everywhere, so a module's body resolves a name in its own declarations
//! and never through the `use` that brings it in. A read of a name its
//! user declares is an error at the site naming the input to declare and
//! the `use` to give it in; the user gives it by name (`use m { env }`)
//! or `env = env`. On the mock.

mod common;
use common::Scratch;

/// A policy pack that reads the stack's `env` and `region` bare, each
/// twice.
const PACK: &str = r#"warn "prod" where env == "prod"
warn "prod again" where env == "prod", region == "eu"
warn "region" where region == "eu"
"#;

const STACK: &str = r#"key env: enum("lab", "prod") = "lab"
input region: string = "eu"
use fake
use baseline
"#;

fn project(name: &str, pack: &str, stack: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("baseline.df", pack);
    s.write("main.df", stack);
    s
}

/// Each undeclared read is an error at its first site, once per name and
/// file, with the input to declare and the `use` to give it.
#[test]
fn an_undeclared_read_is_an_error_naming_the_input() {
    let s = project("scope-undeclared", PACK, STACK);
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    let env = "`env` is not declared in module baseline: a module reads only what it declares";
    let region =
        "`region` is not declared in module baseline: a module reads only what it declares";
    // Two errors, one per name, though each is read twice; a message
    // prints in its header and under its site.
    assert_eq!(
        r.stderr.matches("Error: baseline.df").count(),
        2,
        "{}",
        r.stderr
    );
    for msg in [env, region] {
        assert_eq!(r.stderr.matches(msg).count(), 2, "{}", r.stderr);
    }
    assert!(r.stderr.contains("baseline.df:1:"), "{}", r.stderr);
    assert!(r.stderr.contains("baseline.df:2:"), "{}", r.stderr);
    assert!(
        r.stderr.contains(
            "Help: take it as an input: `input env: enum(\"lab\", \"prod\")` in baseline.df, and \
             give it in the use: `use baseline { env }`"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("`input region: string` in baseline.df"),
        "{}",
        r.stderr
    );
}

/// The declared form: the module takes `env` and `region` as inputs, the
/// stack gives them by name (the pun) or as `k = k`.
#[test]
fn the_user_gives_the_input_by_name_or_by_assignment() {
    let pack = format!("input env: enum(\"lab\", \"prod\")\ninput region: string\n{PACK}");
    for using in [
        "use baseline { env, region }",
        "use baseline { env = env, region = region }",
    ] {
        let s = project(
            "scope-declared",
            &pack,
            &STACK.replace("use baseline", using),
        );
        let r = s
            .run(&["plan", "--why=none", "main.df", "env=prod"])
            .success();
        for w in ["prod", "prod again", "region"] {
            assert!(
                r.stderr.contains(&format!("warning: {w}\n")),
                "{using}\n{}",
                r.stderr
            );
        }
        let r = s
            .run(&["plan", "--why=none", "main.df", "env=lab"])
            .success();
        assert!(!r.stderr.contains("warning: prod"), "{using}\n{}", r.stderr);
    }
}

/// `why` on the module's value shows its chain into the user: the `use`
/// that gives it.
#[test]
fn why_follows_the_input_into_the_use() {
    let pack = format!("input env: enum(\"lab\", \"prod\")\ninput region: string = \"eu\"\n{PACK}");
    let s = project(
        "scope-why",
        &pack,
        &STACK.replace("use baseline", "use baseline { env }"),
    );
    let r = s
        .run(&["dev", "why", "baseline.env", "main.df", "env=prod"])
        .success();
    assert!(
        r.stdout
            .contains("input baseline.env = \"prod\"\n  = env  main.df:4\n"),
        "{}",
        r.stdout
    );
}

/// The module's own name wins: its `let env` is what it reads, whatever
/// the stack's `env` is.
#[test]
fn a_modules_own_name_shadows_its_users() {
    let pack = "let env = \"prod\"\nwarn \"prod\" where env == \"prod\"\n";
    let s = project("scope-shadow", pack, STACK);
    let r = s
        .run(&["plan", "--why=none", "main.df", "env=lab"])
        .success();
    assert!(r.stderr.contains("warning: prod\n"), "{}", r.stderr);
}

/// A component of a module's file reads its module, never the stack: its
/// error names the component's input and the copy.
#[test]
fn a_modules_component_reads_no_further_than_its_module() {
    let pack =
        "component vpc {\n  resource net.vpc v { cidr = \"10.0.0.0/16\", tags = { env } }\n}\n";
    let stack = format!("{STACK}resource baseline.vpc main {{}}\n");
    let s = project("scope-component", pack, &stack);
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "`env` is not declared in component baseline.vpc: a component reads only what it \
             and its module declare"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("give it in each copy: `resource baseline.vpc NAME { env }`"),
        "{}",
        r.stderr
    );
    let pack = pack.replace("{\n  resource", "{\n  input env: string\n  resource");
    let s = project(
        "scope-component",
        &pack,
        &stack.replace("main {}", "main { env }"),
    );
    let r = s
        .run(&["plan", "--why=none", "main.df", "env=prod"])
        .success();
    assert!(r.stdout.contains("tags.env = \"prod\""), "{}", r.stdout);
}

/// A relation the user defines is no more the module's than a value: the
/// module takes it, `input p` with its `decl`, and the `use` gives rows.
#[test]
fn a_users_relation_is_taken_as_an_input() {
    let stack = format!("{STACK}peer(\"a\")\n");
    let pack = "warn \"peered\" where peer(_)\n";
    let s = project("scope-relation", pack, &stack);
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(
        r.stderr
            .contains("`peer` is not declared in module baseline"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "`decl peer(c1: T)` and `input peer` in baseline.df, and give it in the use: `use \
             baseline { peer(x1) where peer(x1) }`"
        ),
        "{}",
        r.stderr
    );
    let pack = format!("input peer\ndecl peer(name: string)\n{pack}");
    let stack = stack.replace("use baseline", "use baseline {\n  peer(p) where peer(p)\n}");
    let s = project("scope-relation", &pack, &stack);
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert!(r.stderr.contains("warning: peered\n"), "{}", r.stderr);
}

/// A module used by another reads neither that module's names nor the
/// stack's.
#[test]
fn a_module_used_by_a_module_reads_neither_user() {
    let s = Scratch::project("scope-nested");
    s.write("inner.df", "warn \"tier\" where tier == \"gold\"\n");
    s.write("outer.df", "let tier = \"gold\"\nuse inner\n");
    s.write("main.df", "use fake\nuse outer\n");
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "`tier` is not declared in module inner: a module reads only what it declares"
        ),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("`use inner { tier }`"), "{}", r.stderr);
}

/// A resource of the user's is no more the module's than a value: the
/// module takes a reference, `input main: ref(net.vpc)`.
#[test]
fn a_users_resource_is_taken_as_a_reference() {
    let stack = format!("{STACK}resource net.vpc main {{ cidr = \"10.0.0.0/16\" }}\n");
    let pack = "warn \"wide\" where main.cidr == \"10.0.0.0/16\"\n";
    let s = project("scope-resource", pack, &stack);
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "Help: take it as an input: `input main: ref(net.vpc)` in baseline.df, and give it \
             in the use: `use baseline { main }`"
        ),
        "{}",
        r.stderr
    );
    let pack = format!("input main: ref(net.vpc)\n{pack}");
    let stack = stack.replace("use baseline", "use baseline { main }");
    let s = project("scope-resource", &pack, &stack);
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert!(r.stderr.contains("warning: wide\n"), "{}", r.stderr);
}

/// Not reached by R-205: a provider's `use` is the stack's, and a
/// module's resource of its type is made by the provider its user
/// configures (`use fake` in the stack, none in the module). Lexically the
/// module would name the provider it makes resources with, or take it as
/// an input; that is a design of its own (a provider is configured per
/// deployment, one account per stack).
#[test]
#[ignore = "R-205 leaves providers: a module's resources use its user's provider `use`"]
fn a_modules_provider_is_its_own() {
    let s = Scratch::project("scope-provider");
    s.write("net.df", "resource net.vpc v { cidr = \"10.0.0.0/16\" }\n");
    s.write("main.df", "use fake\nuse net\n");
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(r.stderr.contains("net.vpc"), "{}", r.stderr);
}

/// A copy of the user's is read through what the module takes of it.
#[test]
fn a_users_copy_is_read_through_an_input() {
    let s = Scratch::project("scope-copy");
    s.write(
        "lib.df",
        "component c {\n  resource net.vpc v { cidr = \"10.0.0.0/16\" }\n  output vpc: net.vpc = v\n}\n",
    );
    s.write(
        "pack.df",
        "warn \"w\" where blue.vpc.cidr == \"10.0.0.0/16\"\n",
    );
    s.write("main.df", "use fake\nresource lib.c blue {}\nuse pack\n");
    let r = s.run(&["plan", "--why=none", "main.df"]).failure();
    assert!(
        r.stderr.contains(
            "`blue` is not declared in module pack: a module reads only what it declares"
        ),
        "{}",
        r.stderr
    );
    s.write(
        "pack.df",
        "input vpc: ref(net.vpc)\nwarn \"w\" where vpc.cidr == \"10.0.0.0/16\"\n",
    );
    s.write(
        "main.df",
        "use fake\nresource lib.c blue {}\nuse pack { vpc = blue.vpc }\n",
    );
    let r = s.run(&["plan", "--why=none", "main.df"]).success();
    assert!(r.stderr.contains("warning: w\n"), "{}", r.stderr);
}
