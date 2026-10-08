//! The environment matrix is code (R-114): a project module, project.df
//! at the root, makes each deployment a resource of its stack's type;
//! `plan` with no target says each one's state and then plans each,
//! `apply` applies them in dependency order, each confirmed on its own,
//! `test` tests each, `destroy` takes a target always. A deployment an
//! apply of the module made that it lists no more is destroyed by the
//! next; one it never listed is a target of its own.

mod common;
use common::Scratch;

const PLATFORM: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
resource net.vpc edge { cidr = "10.0.0.0/16" }
output ingress_ip = edge.cidr
"#;

const APPS: &str = r#"
key env: enum("lab", "prod") = "lab"
use fake
use stacks.platform
resource net.vpc rec {
  cidr = "10.1.0.0/16"
  name = platform[env].ingress_ip
}
"#;

/// The readers first: the order is the matrix's dependencies', not the
/// module's.
const MATRIX: &str = r#"
resource stacks.apps lab { env = "lab" }
resource stacks.apps prod { env = "prod" }
resource stacks.platform lab { env = "lab" }
resource stacks.platform prod { env = "prod" }
"#;

fn project(name: &str, matrix: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/platform.df", PLATFORM);
    s.write("stacks/apps.df", APPS);
    s.write("project.df", matrix);
    s
}

/// The plan tree's header lines (R-200): each deployment by its full
/// name, where it is listed and its state, in apply order.
fn states(stdout: &str) -> Vec<String> {
    stdout
        .lines()
        .filter(|l| ["+ ", "~ ", "= ", "- "].iter().any(|m| l.starts_with(m)))
        .map(|l| l.split_whitespace().collect::<Vec<_>>().join(" "))
        .collect()
}

#[test]
fn plan_with_no_target_is_one_tree_of_the_deployments_in_apply_order() {
    let s = project("matrix-plan", MATRIX);
    let r = s.run(&["plan"]).success();
    // One headline across the tree; each deployment a header line, its
    // plan nested under it; a reader planned against what its dependency
    // will publish (R-200).
    assert!(
        r.stdout.starts_with("plan: 4 changes (4 create)\n\n"),
        "{}",
        r.stdout
    );
    assert_eq!(
        states(&r.stdout),
        [
            "+ stacks.platform[env=lab] project.df:4 never applied, 1 change (1 create) over 1 tick",
            "+ stacks.apps[env=lab] project.df:2 never applied, 1 change (1 create) over 1 tick after stacks.platform[env=lab]",
            "+ stacks.platform[env=prod] project.df:5 never applied, 1 change (1 create) over 1 tick",
            "+ stacks.apps[env=prod] project.df:3 never applied, 1 change (1 create) over 1 tick after stacks.platform[env=prod]",
        ],
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  tick 1  1 change\n    + net.vpc rec  stacks/apps.df:5\n        cidr = \"10.1.0.0/16\"\n        name = \"10.0.0.0/16\""
        ),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("== "), "{}", r.stdout);

    s.run(&["apply", "platform", "env=lab"]).success();
    let r = s.run(&["plan"]).success();
    assert_eq!(
        states(&r.stdout)[..2],
        [
            "= stacks.platform[env=lab] project.df:4 up to date",
            "+ stacks.apps[env=lab] project.df:2 never applied, 1 change (1 create) over 1 tick after stacks.platform[env=lab]",
        ],
        "{}",
        r.stdout
    );
}

#[test]
fn apply_with_no_target_applies_each_in_dependency_order() {
    let s = project("matrix-apply", MATRIX);
    let r = s.run(&["apply"]).success();
    assert!(
        r.stdout.starts_with(
            "stacks: project.df's deployments, in apply order: platform[env=lab], then \
             apps[env=lab], then platform[env=prod], then apps[env=prod]; each is planned, \
             confirmed and applied in turn\n== stacks.platform[env=lab]\n"
        ),
        "{}",
        r.stdout
    );
    assert!(r.stdout.contains("name = \"10.0.0.0/16\""), "{}", r.stdout);
    let r = s.run(&["plan"]).success();
    assert!(
        states(&r.stdout).iter().all(|l| l.ends_with(" up to date")),
        "{}",
        r.stdout
    );
    let list = s.run(&["stack", "list"]).success();
    for d in ["platform[env=lab]", "apps[env=prod]"] {
        assert!(
            list.stdout
                .lines()
                .any(|l| l.contains(d) && l.contains("project.df")),
            "{}",
            list.stdout
        );
    }
}

/// `dform apply`, answering each question in turn: what it said up to
/// each and after the last, and its exit code.
fn answers(s: &Scratch, answers: &[&str]) -> (Vec<String>, i32) {
    let mut cmd = common::dform();
    cmd.args(["apply"]).current_dir(s.path(""));
    common::answering(&s.dir, cmd, answers)
}

#[test]
fn each_deployment_is_confirmed_on_its_own() {
    let one = "resource stacks.apps lab { env = \"lab\" }\n\
               resource stacks.platform lab { env = \"lab\" }\n";
    let s = project("matrix-confirm", one);
    let (said, code) = answers(&s, &["y", "y"]);
    assert_eq!(code, 0, "{said:?}");
    assert!(
        said[0].ends_with("Apply this change to platform[env=lab]? [y/N] "),
        "{}",
        said[0]
    );
    assert!(
        said[1].contains("== stacks.apps[env=lab]")
            && said[1].ends_with("Apply this change to apps[env=lab]? [y/N] "),
        "{}",
        said[1]
    );

    // A no to the second ends the run there: the first is applied, the
    // project made it, and the second is not.
    let s = project("matrix-decline", one);
    let (said, code) = answers(&s, &["y", "n"]);
    assert_eq!(code, 3, "{said:?}");
    let r = s.run(&["plan"]).success();
    assert_eq!(
        states(&r.stdout),
        [
            "= stacks.platform[env=lab] project.df:2 up to date",
            "+ stacks.apps[env=lab] project.df:1 never applied, 1 change (1 create) over 1 tick after stacks.platform[env=lab]"
        ],
        "{}",
        r.stdout
    );
    let made = s.json("dform.state/project.json");
    assert!(
        made["project.df"]["platform[env=lab]"].is_object(),
        "{made}"
    );
    assert!(made["project.df"]["apps[env=lab]"].is_null(), "{made}");
}

#[test]
fn a_deployment_removed_from_the_matrix_is_destroyed_by_the_next_apply() {
    let s = project("matrix-removed", MATRIX);
    s.run(&["apply"]).success();
    let without_prod: String = MATRIX
        .lines()
        .filter(|l| !l.contains("prod"))
        .map(|l| format!("{l}\n"))
        .collect();
    s.write("project.df", &without_prod);
    let r = s.run(&["plan"]).success();
    assert_eq!(
        states(&r.stdout),
        [
            "= stacks.platform[env=lab] project.df:3 up to date",
            "= stacks.apps[env=lab] project.df:2 up to date",
            "- stacks.apps[env=prod] stacks/apps.df removed from project.df: the next apply destroys it, 1 change (1 delete) over 1 tick",
            "- stacks.platform[env=prod] stacks/platform.df removed from project.df: the next apply destroys it, 1 change (1 delete) over 1 tick",
        ],
        "{}",
        r.stdout
    );
    let r = s.run(&["apply"]).success();
    assert!(
        r.stdout.contains(
            "; then, removed from it, destroyed: apps[env=prod], then platform[env=prod]; each"
        ) && r
            .stdout
            .contains("== stacks.apps[env=prod]  removed from project.df\n"),
        "{}",
        r.stdout
    );
    let list = s.run(&["stack", "list"]).success();
    assert!(!list.stdout.contains("env=prod"), "{}", list.stdout);
    let made = s.json("dform.state/project.json");
    assert!(made["project.df"]["platform[env=prod]"].is_null(), "{made}");
    // Nothing is left to remove.
    let r = s.run(&["plan"]).success();
    assert_eq!(states(&r.stdout).len(), 2, "{}", r.stdout);
}

#[test]
fn a_removed_deployment_s_destroy_honours_prevent_destroy() {
    let s = project("matrix-prevent", MATRIX);
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}lifecycle(edge, \"prevent_destroy\")\n"),
    );
    s.run(&["apply"]).success();
    let only_lab: String = MATRIX
        .lines()
        .filter(|l| !l.contains("prod"))
        .map(|l| format!("{l}\n"))
        .collect();
    s.write("project.df", &only_lab);
    let r = s.run(&["apply"]);
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(
        r.stdout
            .contains("lifecycle prevent_destroy: the plan would delete net.vpc[\"edge\"]"),
        "{}",
        r.stdout
    );
    // Its reader went first; it is still the project's, and the next
    // apply asks again.
    let made = s.json("dform.state/project.json");
    assert!(made["project.df"]["apps[env=prod]"].is_null(), "{made}");
    // Still the project's: the next apply asks again.
    let made = s.json("dform.state/project.json");
    assert!(
        made["project.df"]["platform[env=prod]"].is_object(),
        "{made}"
    );
}

#[test]
fn a_deployment_the_matrix_does_not_list_is_a_target_of_its_own() {
    let s = project(
        "matrix-unlisted",
        "resource stacks.apps lab { env = \"lab\" }\n",
    );
    // Read by a listed one, it is applied first, said so.
    let r = s.run(&["plan"]).success();
    assert_eq!(
        states(&r.stdout),
        [
            "+ stacks.platform[env=lab] stacks/platform.df never applied, 1 change (1 create) over 1 tick (not listed: a listed deployment reads it)",
            "+ stacks.apps[env=lab] project.df:1 never applied, 1 change (1 create) over 1 tick after stacks.platform[env=lab]",
        ],
        "{}",
        r.stdout
    );
    s.run(&["apply"]).success();
    // Applied by its target, an unlisted deployment is not the project's:
    // its apply never destroys it.
    s.run(&["apply", "platform", "env=prod"]).success();
    let r = s.run(&["plan"]).success();
    assert!(!r.stdout.contains("env=prod"), "{}", r.stdout);
    s.run(&["apply"]).success();
    let r = s.run(&["plan", "platform", "env=prod"]).success();
    assert!(r.stdout.contains("is up to date"), "{}", r.stdout);
    let made = s.json("dform.state/project.json");
    assert!(made["project.df"]["platform[env=lab]"].is_null(), "{made}");
}

#[test]
fn test_with_no_target_tests_each_deployment() {
    let s = project("matrix-test", MATRIX);
    s.write(
        "stacks/platform.df",
        &format!("{PLATFORM}deny \"prod needs a bigger network\" where env == \"prod\"\n"),
    );
    let r = s.run(&["test"]).failure();
    let heads: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("== ")).collect();
    assert_eq!(
        heads,
        [
            "== stacks.platform[env=lab]",
            "== stacks.apps[env=lab]",
            "== stacks.platform[env=prod]",
            "== stacks.apps[env=prod]"
        ],
        "{}",
        r.stdout
    );
    assert!(
        r.stderr
            .contains("test: 1 of project.df's 4 deployments failed: platform[env=prod]"),
        "{}",
        r.stderr
    );
}

/// `dev effects` with no target prints each stack the module lists, once
/// (its effects are its program's, whatever its key); `--json` needs one.
#[test]
fn dev_effects_with_no_target_prints_each_stack_once() {
    let s = project("matrix-effects", MATRIX);
    let r = s.run(&["dev", "effects"]).success();
    let heads: Vec<&str> = r.stdout.lines().filter(|l| l.starts_with("== ")).collect();
    assert_eq!(heads, ["== platform", "== apps"], "{}", r.stdout);
    assert!(
        r.stdout.contains("stack  offers  ingress_ip: any\n"),
        "{}",
        r.stdout
    );
    let r = s.run(&["dev", "effects", "--json"]).failure();
    assert!(
        r.stderr.contains(
            "dev effects project.df: --json says one stack's effects; name it (`dform dev \
             effects STACK --json`)"
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn destroy_takes_a_target() {
    let s = project("matrix-destroy", MATRIX);
    let r = s.run(&["destroy", "--yes"]).failure();
    assert!(
        r.stderr.contains(
            "destroy needs a target: it removes one deployment, named (`dform destroy STACK \
             K=V`); the project lists apps[env=lab], apps[env=prod], platform[env=lab], \
             platform[env=prod]"
        ),
        "{}",
        r.stderr
    );
}

#[test]
fn a_range_lists_a_deployment_per_value() {
    let s = project(
        "matrix-range",
        r#"
type environment = enum("lab", "prod")
resource stacks.platform "${e}" { env = e } where e in environment
"#,
    );
    let r = s.run(&["plan"]).success();
    assert_eq!(
        states(&r.stdout),
        [
            "+ stacks.platform[env=lab] project.df:3 never applied, 1 change (1 create) over 1 tick",
            "+ stacks.platform[env=prod] project.df:3 never applied, 1 change (1 create) over 1 tick",
        ],
        "{}",
        r.stdout
    );
}

#[test]
fn the_module_makes_deployments_and_nothing_else() {
    let s = project(
        "matrix-errors",
        "resource stacks.platform lab { env = \"lab\", region = \"eu\" }\n",
    );
    let r = s.run(&["plan"]).failure();
    assert!(
        r.stderr.contains(
            "resource stacks.platform lab: `region` is not a key of the stack platform (its key: \
             env)"
        ),
        "{}",
        r.stderr
    );
    s.write("project.df", "use fake\nresource net.vpc x { cidr = \"10.0.0.0/16\" }\nresource stacks.platform lab { env = \"lab\" }\n");
    let r = s.run(&["plan"]).failure();
    assert!(
        r.stderr.contains(
            "net.vpc is no stack: a project module's resources are deployments of stacks"
        ),
        "{}",
        r.stderr
    );
    s.write(
        "project.df",
        "resource stacks.platform a { env = \"lab\" }\nresource stacks.platform b { env = \"lab\" }\n",
    );
    let r = s.run(&["plan"]).failure();
    assert!(
        r.stderr.contains(
            "resource stacks.platform b lists platform[env=lab] again: a is the same deployment"
        ),
        "{}",
        r.stderr
    );
    // In a stack, a resource of a stack is still an error.
    s.write("project.df", MATRIX);
    s.write(
        "stacks/apps.df",
        &format!("{APPS}resource stacks.platform lab {{ env = \"lab\" }}\n"),
    );
    let r = s.run(&["plan", "apps"]).failure();
    assert!(
        r.stderr
            .contains("stacks.platform is deployed by the tool; `use` it"),
        "{}",
        r.stderr
    );
}

/// A module named as the target is a matrix of its own.
#[test]
fn a_project_module_named_as_the_target() {
    let s = project("matrix-named", MATRIX);
    s.write(
        "envs/lab.df",
        "resource stacks.platform lab { env = \"lab\" }\n",
    );
    let r = s.run(&["plan", "envs/lab.df"]).success();
    assert_eq!(
        states(&r.stdout),
        ["+ stacks.platform[env=lab] envs/lab.df:1 never applied, 1 change (1 create) over 1 tick"],
        "{}",
        r.stdout
    );
}
