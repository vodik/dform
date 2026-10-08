//! Keys versus inputs (R-208, docs/grammar.md "Deployed modules"): a key
//! is a deployment's identity, an input a module's signature, and the one
//! thing shared by name is the type, `type environment = enum("lab",
//! "prod")` in config.df, read by its module's path in every type
//! position, lexically: through the file's own `use`, or by the path from
//! the root, never through what another file loads.

mod common;
use common::Scratch;

/// The type's home: a module with an input, the type in its header.
const CONFIG: &str = r#"type environment = enum("lab", "prod")
input env: environment
let domain = "${env}.example.org"
"#;

/// A module that takes its deployment by being told.
const K3S: &str = r#"input env: config.environment
use config { env }
output same: config.environment = env
output host: string = "k3s.${config.domain}"
deny "prod is not served from lab" where env == "prod", config.domain == "lab.example.org"
"#;

/// A stack keyed by the shared type, reading it in every position.
const PLATFORM: &str = r#"key env: config.environment
use fake
use config { env }
use k3s { env }
let l: config.environment = env
decl tier(e: config.environment, n: int)
tier("lab", 1)
tier("prod", 2)
resource net.vpc a { name = l, cidr = "10.0.0.0/16", size = tier[env] }
resource net.vpc b { name = k3s.same, cidr = "10.1.0.0/16" }
resource net.vpc c { name = k3s.host, cidr = "10.2.0.0/16" } where env == "lab"
output kc: string = k3s.host
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("config.df", CONFIG);
    s.write("k3s.df", K3S);
    s.write("stacks/platform.df", PLATFORM);
    s
}

fn plan(s: &Scratch, target: &[&str]) -> String {
    let mut args = vec!["plan", "--why=none"];
    args.extend(target);
    s.run(&args).success().stdout
}

/// `config.environment` in a key, an input, a typed let read as a value,
/// a `decl` column, a module output its user reads, and a comparison:
/// each the enum, none a resource type.
#[test]
fn a_type_by_its_module_path_is_the_type_in_every_position() {
    let s = project("ki-positions");
    let out = plan(&s, &["platform", "env=lab"]);
    for want in [
        "+ net.vpc[\"a\"]\n",
        "name = \"lab\"",
        "size = 1",
        "+ net.vpc[\"b\"]\n",
        "+ net.vpc[\"c\"]\n",
        "name = \"k3s.lab.example.org\"",
    ] {
        assert!(out.contains(want), "{want}\n{out}");
    }
    let out = plan(&s, &["platform", "env=prod"]);
    assert!(out.contains("size = 2"), "{out}");
    assert!(!out.contains("net.vpc[\"c\"]"), "{out}");
}

/// A key's value outside the shared type is refused at the key's line,
/// with the deployments the type's members name.
#[test]
fn a_key_value_outside_its_type_names_the_key_and_its_members() {
    let s = project("ki-key-value");
    let r = s.run(&["plan", "platform", "env=dev"]).failure();
    assert!(
        r.stderr.contains(
            "stacks/platform.df:1:1: `env=dev` in the target: key env is enum(lab, prod), and \
             `dev` is not one of its members"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr
            .contains("name a deployment of one of its members: `env=lab`, `env=prod`"),
        "{}",
        r.stderr
    );
}

/// By its path from the root, with no `use` in the file and none in any
/// other: the loader loads the module, so the type is the enum (it was a
/// resource type, and `dev` passed).
#[test]
fn a_type_by_path_needs_no_use_and_no_other_file() {
    let s = Scratch::project("ki-by-path");
    s.write("config.df", CONFIG);
    s.write(
        "stacks/lone.df",
        "key env: config.environment\nuse fake\nresource net.vpc a { name = env, cidr = \"10.0.0.0/16\" }\n",
    );
    plan(&s, &["lone", "env=lab"]);
    let r = s.run(&["plan", "lone", "env=dev"]).failure();
    assert!(
        r.stderr.contains("key env is enum(lab, prod)"),
        "{}",
        r.stderr
    );
    // A type read through a module needs no instance of it: config's
    // required `env` is no one's to give here.
    assert!(!r.stderr.contains("required"), "{}", r.stderr);
}

/// project.df ranges over the type by its path, with no `use` (which would
/// need config's `env`): one deployment per member (it listed none).
#[test]
fn the_project_module_ranges_over_a_type_by_path() {
    let s = project("ki-project");
    s.write(
        "project.df",
        "resource stacks.platform \"p-${e}\" { env = e } where e in config.environment\n",
    );
    let r = s.run(&["plan"]).success();
    assert!(
        r.stdout.contains("+ stacks.platform[env=lab]")
            && r.stdout.contains("+ stacks.platform[env=prod]"),
        "{}",
        r.stdout
    );
}

/// The project module gives a module it uses its inputs as a stack does:
/// one not given is the error (it listed no deployment, silently).
#[test]
fn the_project_module_checks_a_used_modules_inputs() {
    let s = project("ki-project-required");
    s.write(
        "project.df",
        "use config\nresource stacks.platform lab { env = \"lab\" } where config.domain != \"\"\n",
    );
    let r = s.run(&["plan"]).failure();
    assert!(
        r.stderr
            .contains("project.df:1:1: input config.env is required and has no value"),
        "{}",
        r.stderr
    );
}

/// One module serves a stack keyed by env, one keyed by region and one
/// with no key: it does not care how its user is deployed.
#[test]
fn two_stacks_keyed_differently_use_one_module() {
    let s = project("ki-two-keys");
    s.write(
        "stacks/edge.df",
        "key region: enum(\"eu\", \"us\")\nuse fake\nuse k3s { env = \"prod\" }\n\
         resource net.vpc e { name = \"${region}-${k3s.same}\", cidr = \"10.3.0.0/16\" }\n",
    );
    s.write(
        "stacks/plain.df",
        "use fake\nuse k3s { env = \"lab\" }\n\
         resource net.vpc p { name = k3s.host, cidr = \"10.4.0.0/16\" }\n",
    );
    assert!(plan(&s, &["platform", "env=prod"]).contains("name = \"prod\""));
    assert!(plan(&s, &["edge", "region=us"]).contains("name = \"us-prod\""));
    assert!(plan(&s, &["plain"]).contains("name = \"k3s.lab.example.org\""));
}

/// A key's enum given to a module input whose enum lacks one of its
/// members is a compile error naming both types (only the lab deployment
/// failed, at run time; prod planned).
#[test]
fn a_key_given_to_a_narrower_enum_is_an_error_naming_both() {
    let s = Scratch::project("ki-give-enum");
    s.write(
        "mod.df",
        "input env: enum(\"dev\", \"prod\")\noutput e = env\n",
    );
    s.write(
        "stacks/s.df",
        "key env: enum(\"lab\", \"prod\")\nuse fake\nuse mod { env }\n",
    );
    let r = s.run(&["plan", "s", "env=prod"]).failure();
    assert!(
        r.stderr.contains(
            "stacks/s.df:3:11: key env, enum(lab, prod), is given to input mod.env, enum(dev, \
             prod): `lab` is not one of its members"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "declare the input with the key's type: `input env: enum(\"lab\", \"prod\")` in \
             module mod"
        ),
        "{}",
        r.stderr
    );
    // One shared type: nothing to check.
    s.write("types.df", "type environment = enum(\"lab\", \"prod\")\n");
    s.write("mod.df", "input env: types.environment\noutput e = env\n");
    s.write(
        "stacks/s.df",
        "key env: types.environment\nuse fake\nuse mod { env }\n",
    );
    s.run(&["plan", "s", "env=lab"]).success();
}

/// A module input nothing gives: the help is the pun when the user's scope
/// declares the name, else the entry and its type.
#[test]
fn an_ungiven_input_suggests_the_pun() {
    let s = Scratch::project("ki-pun-help");
    s.write(
        "mod.df",
        "input env: enum(\"lab\", \"prod\")\noutput e = env\n",
    );
    s.write(
        "stacks/s.df",
        "key env: enum(\"lab\", \"prod\")\nuse fake\nuse mod\n",
    );
    let r = s.run(&["plan", "s", "env=lab"]).failure();
    assert!(
        r.stderr
            .contains("stacks/s.df:3:1: input mod.env is required and has no value"),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("give it in the use: `use mod { env }`"),
        "{}",
        r.stderr
    );
    s.write("stacks/s.df", "key region: string\nuse fake\nuse mod\n");
    let r = s.run(&["plan", "s", "region=eu"]).failure();
    assert!(
        r.stderr
            .contains("give it in the use: `use mod { env = .. }`, a value of enum(lab, prod)"),
        "{}",
        r.stderr
    );
}

/// The pun with nothing to pun says it is `env = env`, and what to
/// declare (it was "unknown name", with the help of a rule).
#[test]
fn the_pun_with_nothing_named_says_so() {
    let s = Scratch::project("ki-pun-nothing");
    s.write("mod.df", "input env: types.environment\noutput e = env\n");
    s.write("types.df", "type environment = enum(\"lab\", \"prod\")\n");
    s.write(
        "stacks/s.df",
        "key region: string\nuse fake\nuse mod { env }\n",
    );
    let r = s.run(&["plan", "s", "region=eu"]).failure();
    assert!(
        r.stderr.contains(
            "stacks/s.df:3:11: `env` in the block of `use mod` is `env = env`, and nothing \
             here declares `env`"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains(
            "declare it in this file, `key env: types.environment`, or give a value, `env = ..`"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("unknown name"), "{}", r.stderr);
}

/// A keyed read of a name the dependency does not output is an error at
/// the read, naming its outputs; a key read as an output says it names
/// the deployment (both were a run-time "not set").
#[test]
fn a_keyed_read_names_an_output_of_the_dependency() {
    let s = project("ki-keyed-read");
    s.write(
        "stacks/apps.df",
        "key env: config.environment\nuse fake\nuse stacks.platform\n\
         resource net.vpc a { name = platform[env].nosuch, cidr = \"10.0.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "apps", "env=lab"]).failure();
    assert!(
        r.stderr
            .contains("stacks/apps.df:4:29: stacks.platform has no output `nosuch`"),
        "{}",
        r.stderr
    );
    assert!(r.stderr.contains("its outputs: kc"), "{}", r.stderr);
    s.write(
        "stacks/apps.df",
        "key env: config.environment\nuse fake\nuse stacks.platform\n\
         resource net.vpc a { name = platform[env].env, cidr = \"10.0.0.0/16\" }\n",
    );
    let r = s.run(&["plan", "apps", "env=lab"]).failure();
    assert!(
        r.stderr
            .contains("`env` is a key of stacks.platform, not an output"),
        "{}",
        r.stderr
    );
    s.write(
        "stacks/apps.df",
        "key env: config.environment\nuse fake\nuse stacks.platform\n\
         resource net.vpc a { name = platform[env].kc, cidr = \"10.0.0.0/16\" }\n",
    );
    s.run(&["plan", "apps", "env=lab"]).success();
}

/// A `type` line may stand among the header's lines: the type its input
/// names comes first, and `fmt` leaves it there.
#[test]
fn a_type_line_stands_in_the_header() {
    assert_eq!(
        dform::fmt::format_source("config.df", CONFIG).unwrap(),
        CONFIG
    );
    let p = dform::syntax::parser::parse(CONFIG);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
}

/// Not yet: the label of a module input nothing gives prints the type as
/// the resolver expanded it, `enum(lab, prod)`, not as written,
/// `config.environment` (an alias is transparent by the time `inputs`
/// sees the declaration; the written text needs the declaration's span
/// read back, or the alias carried on `InputDecl`, which is ast.rs).
#[test]
#[ignore = "the required-input label prints the alias expanded (ast.rs carries no alias name)"]
fn the_required_input_label_names_the_alias() {
    let s = project("ki-label");
    s.write(
        "stacks/platform.df",
        "key env: config.environment\nuse fake\nuse k3s\n",
    );
    let r = s.run(&["plan", "platform", "env=lab"]).failure();
    assert!(
        r.stderr.contains("env: config.environment declared here"),
        "{}",
        r.stderr
    );
}
