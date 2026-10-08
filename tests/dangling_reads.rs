//! A read of what nothing derives is an error at the read's site in every
//! run, before any provider is asked (R-194): plan, apply and test say it
//! the same way, and nothing is applied. A read of what may still derive
//! is not: it is an unknown a later tick fills.

mod common;
use common::Scratch;

/// `other` is wanted only when `peered` is set; the subnet reads it.
const PROGRAM: &str = "\
input peered: bool = false
resource net.vpc main { cidr = \"10.0.0.0/16\" }
resource net.vpc other { cidr = \"10.1.0.0/16\" } where peered
resource net.subnet a { cidr = \"10.0.1.0/24\", vpc = other }
use fake
";

const ERROR: &str = "p.df:4:47: net.vpc other answered nothing, so net.subnet a.vpc has no \
                     value: nothing derives net.vpc other";

#[test]
fn plan_and_apply_refuse_the_read_at_its_site() {
    let s = Scratch::project("dangling-reads");
    s.write("p.df", PROGRAM);
    let r = s.run(&["plan", "p.df"]).failure();
    assert_eq!(r.stderr, format!("Error: {ERROR}\n"));
    assert!(r.stdout.is_empty(), "{}", r.stdout);
    // Nothing is applied, `main` included: the program is refused whole.
    let r = s.run(&common::yes(&["apply", "p.df"])).failure();
    assert_eq!(r.stderr, format!("Error: {ERROR}\n"));
    let r = s.run(&["plan", "p.df", "--set", "peered=true"]).success();
    assert_eq!(
        r.summary(),
        "plan: 3 changes (3 create) over 1 tick",
        "{}",
        r.stdout
    );
}

/// `dform test` fails the combination that leaves the read with no row,
/// in the plan's words, and passes the one that wants what it reads.
#[test]
fn test_fails_the_combination_with_the_read() {
    let s = Scratch::new("dangling-reads-test");
    s.write("p.df", PROGRAM);
    let r = s.run(&["test", "p.df"]).failure();
    assert!(
        r.stdout.contains("test p: 2 combinations of peered\n")
            && r.stdout.contains(&format!(
                "error  dform plan p.df --set peered=false\n  Error: {ERROR}\n"
            ))
            && r.stdout.contains("test p: 2 combinations, 1 failed\n"),
        "{}",
        r.stdout
    );
}

/// A read of a resource whose clause waits on a value a tick makes
/// (`has main`) is no such read: the plan holds the reader until then.
#[test]
fn a_read_of_what_may_still_derive_waits() {
    let s = Scratch::new("dangling-reads-unknown");
    s.write(
        "p.df",
        "resource net.vpc main { cidr = \"10.0.0.0/16\" }\n\
         resource net.vpc peer { cidr = \"10.1.0.0/16\" } where has main\n\
         resource net.subnet a { cidr = \"10.1.1.0/24\", vpc = peer }\n\
         use fake\n",
    );
    let r = s.run(&["plan", "p.df"]).success();
    assert!(
        r.stdout
            .contains("  net.vpc peer    p.df:2  waits on main\n")
            && !r.stderr.contains("answered nothing"),
        "{}\n{}",
        r.stdout,
        r.stderr
    );
    s.run(&["test", "p.df"]).success();
}
