//! One path for every input check (After the printing-bugs landing): a
//! value given to an input (`--set`, an `--input-file`, a literal the
//! program writes) is checked before evaluation against the input's whole
//! check, a range, a bound, `!=` or any condition over the value alone;
//! a check the value cannot decide alone (it reads a relation, here one
//! another input's value makes) is a deny after evaluation, which prints
//! in the policy block like any deny, with the word `check`.

mod common;
use common::Scratch;

const CHECKED: &str = "key env: enum(dev, prod)\n\
                       input agents: int = 2 check agents < 4, agents != 7\n\
                       use fake\n\
                       resource net.vpc \"agent-${i}\" { cidr = \"10.0.0.0/16\" } where i in 0..agents\n";

fn project(name: &str, stack: &str) -> Scratch {
    let s = Scratch::project(name);
    s.write("stacks/app.df", stack);
    s
}

#[test]
fn a_set_value_is_checked_against_the_whole_check() {
    let s = project("input-check-set", CHECKED);
    for v in ["5", "7"] {
        let r = s
            .run(&["plan", "app", "env=dev", "--set", &format!("agents={v}")])
            .failure();
        assert_eq!(r.code, Some(1), "{}", r.stderr);
        assert!(
            r.stderr.contains(&format!(
                "error  --set agents={v} is outside the check on agents\n  \
                 ├─ stacks/app.df:2  input agents: int = 2 check agents < 4, agents != 7\n  \
                 │                                         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ checked here\n  \
                 └─ --set agents={v}   given here\n"
            )),
            "{}",
            r.stderr
        );
        assert!(r.stdout.is_empty(), "{}", r.stdout);
    }
    s.run(&["plan", "app", "env=dev", "--set", "agents=3"])
        .success();
}

#[test]
fn an_input_files_value_is_checked_at_its_line() {
    let s = project("input-check-file", CHECKED);
    s.write("in.df", "agents(7)\n");
    let r = s
        .run(&["plan", "app", "env=dev", "--input-file", "in.df"])
        .failure();
    assert!(
        r.stderr.contains(
            "error  agents = 7 is outside the check on input agents\n  \
             ├─ in.df:1          agents(7)  given here\n"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stdout.contains("conflicts"), "{}", r.stdout);
}

#[test]
fn a_literal_set_is_checked_where_it_is_written() {
    let s = project(
        "input-check-literal",
        &format!("{CHECKED}set agents = 7 where env == \"prod\"\n"),
    );
    let r = s.run(&["plan", "app", "env=prod"]).failure();
    assert!(
        r.stderr.contains(&format!(
            "error  agents = 7 is outside the check on input agents\n  \
             ├─ stacks/app.df:5  set agents = 7 where env == \"prod\"  given here\n  \
             └─ stacks/app.df:2  input agents: int = 2 check agents < 4, agents != 7\n\
             {}^^^^^^^^^^^^^^^^^^^^^^^^^^^^^ checked here\n",
            " ".repeat(44)
        )),
        "{}",
        r.stderr
    );
}

/// A check that reads what the program derives from another input is
/// the evaluation's: a deny, listed in the policy block and the headline.
#[test]
fn a_check_over_another_input_is_a_policy() {
    let s = project(
        "input-check-multi",
        "key env: enum(dev, prod)\n\
         input max: int = 3\n\
         input agents: int = 2 check fits(agents)\n\
         use fake\n\
         fits(n) where max(m), n in 0..=m\n\
         resource net.vpc \"agent-${i}\" { cidr = \"10.0.0.0/16\" } where i in 0..agents\n",
    );
    let r = s
        .run(&["plan", "app", "env=dev", "--set", "agents=5"])
        .failure();
    assert_eq!(r.code, Some(4), "{}\n{}", r.stdout, r.stderr);
    assert!(r.summary().ends_with("policy: 1 fails"), "{}", r.stdout);
    assert!(
        r.stdout.contains(
            "  fails  input agents check fits(agents)  stacks/app.df:3  1 fails\n    value = 5\n"
        ),
        "{}",
        r.stdout
    );
    assert!(
        r.stderr.contains(
            "refused  input agents check fits(agents)  stacks/app.df:3\n  └─ value = 5\n"
        ),
        "{}",
        r.stderr
    );
    assert!(!r.stderr.contains("refinement"), "{}", r.stderr);
    s.run(&[
        "plan", "app", "env=dev", "--set", "agents=5", "--set", "max=9",
    ])
    .success();
}
