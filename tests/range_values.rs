//! A range in a `check` refines a value of every ordered type, as it does
//! an int: a computed value outside it is a conflict in the value's cell,
//! printed with its bounds as values (`range(10Gi, 4Ti)`), and a bound
//! of another type than the value's is an error at the check.

mod common;
use common::{Run, Scratch};

fn plan(s: &Scratch, src: &str, extra: &[&str]) -> Run {
    s.write("p.df", src);
    let mut args = vec!["dev", "--world", "w.json", "plan", "--why=none", "p.df"];
    args.extend(extra);
    s.run(&args)
}

/// Input `decl`, given `value` by a `let` (a computed value, which no
/// check refuses before evaluation), and a resource that reads it.
fn computed(decl: &str, name: &str, value: &str) -> String {
    format!(
        "\ninput {decl}\non(1)\nlet x = {value}\nset {name} = x where on(1)\n\
         resource compute.vm a {{ cpus = 1 }}\nuse fake\n"
    )
}

/// A computed value outside its range is the cell's conflict, either form,
/// with no changes for the resource that reads it.
#[test]
fn a_computed_quantity_outside_its_range_is_a_conflict() {
    let s = Scratch::new("range-bytes-conflict");
    for check in ["disk in 10Gi..=4Ti", "10Gi <= disk <= 4Ti"] {
        let src = format!(
            "\ninput disk: bytes = 50Gi check {check}\non(1)\nlet big = 5Ti\n\
             set disk = big where on(1)\nresource compute.vm a {{ disk }}\nuse fake\n"
        );
        let r = plan(&s, &src, &[]).failure();
        assert!(
            r.stdout.contains("plan: 0 changes, 1 conflict"),
            "{check}: {}",
            r.stdout
        );
        assert!(
            r.stdout.contains("5Ti violates range(10Gi, 4Ti)"),
            "{check}: {}",
            r.stdout
        );
    }
}

/// A duration's range: the conflict, and a `--set` refused at the flag.
#[test]
fn a_duration_range_refines_its_value() {
    let s = Scratch::new("range-duration");
    for check in ["ttl in 1s..=2h", "1s <= ttl <= 2h"] {
        let src = computed(&format!("ttl: duration = 1h check {check}"), "ttl", "3h");
        let r = plan(&s, &src, &[]).failure();
        assert!(
            r.stdout.contains("3h violates range(1s, 2h)"),
            "{check}: {}",
            r.stdout
        );
        let src = format!(
            "\ninput ttl: duration = 1h check {check}\nresource compute.vm a {{ cpus = 1 }}\n\
             use fake\n"
        );
        let r = plan(&s, &src, &["--set", "ttl=3h"]).failure();
        assert!(
            r.stderr
                .contains("error  --set ttl=3h is outside the check on ttl\n"),
            "{check}: {}",
            r.stderr
        );
        plan(&s, &src, &["--set", "ttl=2h"]).success();
    }
}

/// A float's range without its end holds what is below the end, not the
/// end itself.
#[test]
fn a_float_range_without_its_end_refines_its_value() {
    let s = Scratch::new("range-float");
    for check in ["r in 0.0..1.0", "0.0 <= r < 1.0"] {
        let src = computed(&format!("r: float = 0.5 check {check}"), "r", "1.0");
        let r = plan(&s, &src, &[]).failure();
        assert!(
            r.stdout.contains("1.0 violates range(0.0, 1.0, open)"),
            "{check}: {}",
            r.stdout
        );
        plan(
            &s,
            &computed(&format!("r: float = 0.5 check {check}"), "r", "0.99"),
            &[],
        )
        .success();
    }
}

/// A bound of another dimension than the value's is an error at the
/// check, in an input and in a `type` block, before any value is checked.
#[test]
fn a_bound_of_another_unit_is_an_error_at_the_check() {
    let s = Scratch::new("range-mixed");
    let r = plan(
        &s,
        "\ninput disk: bytes = 50Gi check disk in 1s..=2s\nresource compute.vm a { disk }\n\
         use fake\n",
        &[],
    )
    .failure();
    assert!(
        r.stderr.contains(
            "error  input disk is bytes, and its bound 1s is duration: a check bounds a value \
             by values of its type"
        ),
        "{}",
        r.stderr
    );
    assert!(
        r.stderr.contains("help: write the bound as bytes"),
        "{}",
        r.stderr
    );
    let r = plan(
        &s,
        "\ntype compute.vm {\n  disk: bytes check disk in 1s..=2s\n}\n\
         resource compute.vm a { disk = 1Gi }\nuse fake\n",
        &[],
    )
    .failure();
    assert!(
        r.stderr
            .contains("error  compute.vm .disk is bytes, and its bound 1s is duration"),
        "{}",
        r.stderr
    );
}

/// Not yet a refinement: a strict start of a type with no next value
/// (`0.0 < r`), which a range cannot hold (its start is in it), stays a
/// check after evaluation (a deny), so a computed value outside it plans
/// its changes and is refused by the policy, not by the cell.
#[test]
#[ignore = "a range holds its start: `lo < x` of a dense type is a deny, not a refinement"]
fn a_strict_start_of_a_float_is_a_refinement() {
    let s = Scratch::new("range-float-strict");
    let src = computed("r: float = 0.5 check 0.0 < r <= 1.0", "r", "0.0");
    let r = plan(&s, &src, &[]).failure();
    assert!(r.stdout.contains("violates range("), "{}", r.stdout);
}

/// Not yet a refinement: a bound whose literal its position reads (`1m`,
/// minutes in a duration's check, millicores in a cpu's) is not read as
/// the value's type when the check is split, so it stays a deny.
#[test]
#[ignore = "an ambiguous quantity literal in a check is not read by the value's type"]
fn an_ambiguous_bound_is_read_by_the_values_type() {
    let s = Scratch::new("range-ambiguous");
    let src = computed("ttl: duration = 1h check 1m <= ttl <= 2h", "ttl", "3h");
    let r = plan(&s, &src, &[]).failure();
    assert!(
        r.stdout.contains("3h violates range(1m, 2h)"),
        "{}",
        r.stdout
    );
}
