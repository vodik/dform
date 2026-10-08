//! Entrypoints (R-208, docs/grammar.md "Deployed modules"): a file under
//! stacks/, a root file a `[stacks.NAME]` names, or the one file the tool
//! is pointed at. Only an entrypoint is deployed, so only one declares a
//! `key`; any other file is a module, used, not deployed.

mod common;
use common::Scratch;

const NET: &str = "use fake\nresource net.vpc shared { cidr = \"10.9.0.0/16\" }\n";

/// A project with a stack and a root module.
fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/net.df", NET);
    s.write("types.df", "type environment = enum(\"lab\", \"prod\")\n");
    s
}

/// A root file is a module, not a stack: `stack list` and a target by
/// name do not see it (`types` was listed as a stack with no stacks/).
#[test]
fn a_root_file_is_no_stack() {
    let s = project("ep-root");
    let r = s.run(&["stack", "list"]).success();
    assert!(r.stdout.contains("net "), "{}", r.stdout);
    assert!(!r.stdout.contains("types"), "{}", r.stdout);
    let r = s.run(&["plan", "types"]).failure();
    assert!(
        r.stderr.contains("no stack types in the project"),
        "{}",
        r.stderr
    );
    // With no stacks/ either.
    std::fs::remove_dir_all(s.path("stacks")).unwrap();
    s.write("net.df", NET);
    let r = s.run(&["stack", "list"]).success();
    assert!(r.stdout.starts_with("no stacks in "), "{}", r.stdout);
}

/// A module the tool is pointed at directly is an entrypoint: it acts as
/// a stack, named by its path, its keys its deployment's (it was refused:
/// "a file that is not a stack has a `key`").
#[test]
fn a_module_run_directly_is_a_stack_with_its_keys() {
    let s = project("ep-direct");
    s.write(
        "envs/one.df",
        "key env: types.environment\nuse fake\nresource net.vpc v { name = env, cidr = \"10.0.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "envs/one.df", "env=lab"]).success();
    assert!(
        r.stdout.starts_with("deployment: envs.one[env=lab]\n"),
        "{}",
        r.stdout
    );
    // The project's own stacks are unaffected by it.
    s.run(&["plan", "net"]).success();
}

/// `key` in a file the program uses as a module is the error at its line,
/// its help the input to declare and the `use` to give it in; a stack
/// that does not load the file plans (the project-wide lint failed every
/// command, with no line).
#[test]
fn a_key_in_a_module_is_an_error_at_its_line() {
    let s = project("ep-key");
    s.write("k3s.df", "key env: types.environment\noutput e = env\n");
    s.write(
        "stacks/platform.df",
        "key env: types.environment\nuse fake\nuse k3s { env }\n",
    );
    let r = s.run(&["plan", "platform", "env=lab"]).failure();
    assert!(
        r.stderr.contains(
            "k3s.df:1:1: `key env` in a file that is not an entrypoint: a key is a \
             deployment's identity; a module takes `input env` instead"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("k3s is used here, not deployed"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "declare `input env: types.environment` here, and give it in the use: `use k3s { \
             env }`"
        ),
        "{}",
        r.stderr
    );
    s.run(&["plan", "net"]).success();
    // Run directly, the same file is an entrypoint, and its key is its own.
    s.write(
        "k3s.df",
        "key env: types.environment\nuse fake\noutput e = env\n",
    );
    let r = s.run(&["plan", "k3s.df", "env=lab"]).success();
    assert!(
        r.stdout.starts_with("deployment: k3s[env=lab]\n"),
        "{}",
        r.stdout
    );
}
