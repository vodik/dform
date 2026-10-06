//! R-79: under each change the plan names the leaf that changed since the
//! last apply, from the derivation that apply recorded: a row gone or new,
//! an input that changed, a guard that stopped holding. A delete says
//! where the last apply derived it. No apply, no `because`.

mod common;
use common::Scratch;

const NET: &str = r#"

input zone from csv("data/zones.csv")
input size: int = 1
input big: bool = false

decl zone(name: string, n: int)

provider fake

resource net.vpc main { cidr = "10.0.0.0/16", size }

resource net.subnet "private-${z}" {
  vpc = main
  cidr = inet.subnet(main.cidr, 8, n)
  zone = z
} where zone(z, n)

resource net.subnet extra { vpc = main, cidr = "10.0.200.0/24", zone = "x" } where big
"#;

fn project(name: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write(".gitignore", "dform.state/\n");
    s.write("data/zones.csv", "name,n\nus-test-1a,1\nus-test-1b,2\n");
    s.write("stacks/net.df", NET);
    s
}

/// `git ARGS` in the scratch project, hermetic (as tests/diff.rs).
fn git(s: &Scratch, args: &[&str]) {
    let out = std::process::Command::new("git")
        .args([
            "-c",
            "user.name=dform",
            "-c",
            "user.email=dform@example.com",
        ])
        .args(args)
        .current_dir(&s.dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A row gone from the table deletes its subnet, which says where the last
/// apply derived it and which row is gone; a new row's subnet says the row
/// it gained. The last apply's program is read at the commit it recorded.
#[test]
fn a_removed_row_and_a_new_row_are_the_because() {
    let s = project("because-rows");
    git(&s, &["init", "-q", "."]);
    git(&s, &["add", "-A"]);
    git(&s, &["commit", "-qm", "one"]);
    s.run(&["apply", "net", "--yes"]).success();
    s.write("data/zones.csv", "name,n\nus-test-1a,1\nus-test-1c,3\n");
    git(&s, &["commit", "-qam", "two"]);
    let r = s.run(&["plan", "net"]).success();
    for want in [
        "  + net.subnet[\"private-us-test-1c\"]  stacks/net.df:13  with z = \"us-test-1c\", n = 3\n",
        "      cidr = \"10.0.3.0/24\"            inet.subnet(main.cidr, 8, n)\n",
        "      because data/zones.csv:3 gained the row zone(\"us-test-1c\", 3)\n",
        "  - net.subnet[\"private-us-test-1b\"]  was stacks/net.df:13  with z = \"us-test-1b\", n = 2\n",
        "      because data/zones.csv no longer has the row zone(\"us-test-1b\", 2)\n",
    ] {
        assert!(r.stdout.contains(want), "{want}\n---\n{}", r.stdout);
    }
    // The bare diff says none of it.
    let r = s.run(&["plan", "net", "--why=none"]).success();
    assert!(
        !r.stdout.contains("because") && !r.stdout.contains("stacks/net.df"),
        "{}",
        r.stdout
    );
}

/// An input changed since the last apply is the because of the update it
/// makes; a guard that no longer holds (`where big`) is the because of the
/// delete.
#[test]
fn a_changed_input_and_a_guard_now_false_are_the_because() {
    let s = project("because-input");
    s.run(&["apply", "net", "--yes", "--set", "big=true"])
        .success();
    let r = s.run(&["plan", "net", "--set", "size=2"]).success();
    assert!(
        r.stdout.contains(
            "  ~ net.vpc[\"main\"]\n      size: 1 → 2        --set size=2  (over @default)\n      \
             because input size is now 2 (was 1)\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stdout.contains(
            "  - net.subnet[\"extra\"]  was stacks/net.df:19\n      cidr was \"10.0.200.0/24\"\n"
        ) && r
            .stdout
            .contains("      because input big is now false (was true)\n"),
        "{}",
        r.stdout
    );
}

/// With no apply there is nothing to compare: no `because`, the sites
/// still printed.
#[test]
fn no_apply_no_because() {
    let s = project("because-none");
    let r = s.run(&["plan", "net"]).success();
    assert!(
        r.stdout
            .contains("  + net.vpc[\"main\"]                   stacks/net.df:11\n"),
        "{}",
        r.stdout
    );
    assert!(!r.stdout.contains("because"), "{}", r.stdout);
}
