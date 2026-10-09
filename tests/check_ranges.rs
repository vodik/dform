//! A range in a `check` is a check like its bounds written out: `x in
//! lo..=hi` is `lo <= x <= hi`, and `x in lo..hi` is `lo <= x < hi`, in
//! every position a `check` is written (an input, an object input's
//! field, a `type` block's attribute). Each pair plans the same.

mod common;
use common::{Run, Scratch};

fn plan(s: &Scratch, src: &str, extra: &[&str]) -> Run {
    s.write("p.df", src);
    let mut args = vec!["dev", "--world", "w.json", "plan", "--why=none", "p.df"];
    args.extend(extra);
    s.run(&args)
}

/// The program with each form of the check, planned the same way: both
/// refused with the same plan, and the same first line of the error.
fn same(name: &str, template: &str, forms: [&str; 2], extra: &[&str]) -> Run {
    let s = Scratch::new(name);
    let [a, b] = forms.map(|f| plan(&s, &template.replace("CHECK", f), extra).failure());
    assert_eq!(a.stdout, b.stdout, "{}\n---\n{}", a.stdout, b.stdout);
    let first = |r: &Run| r.stderr.lines().next().unwrap_or_default().to_string();
    assert_eq!(first(&a), first(&b), "{}\n---\n{}", a.stderr, b.stderr);
    a
}

/// `set agents = 4` against `check agents in 0..=3` is the error
/// `check 0 <= agents <= 3` gives (a literal outside the cell's range),
/// not a deny after a plan of four copies.
#[test]
fn an_inputs_range_check_is_its_bounds() {
    let src = "\ninput agents: int = 0 check CHECK\non(1)\nset agents = 4 where on(1)\n\
               resource compute.vm \"agent-${i}\" { cpus = 1 } where i in 0..agents\nuse fake\n";
    let r = same(
        "range-input",
        src,
        ["agents in 0..=3", "0 <= agents <= 3"],
        &[],
    );
    assert!(r.stderr.contains("range(0, 3)"), "{}", r.stderr);
    same(
        "range-input-open",
        src,
        ["agents in 0..4", "0 <= agents < 4"],
        &[],
    );
    let s = Scratch::new("range-input-ok");
    let ok = src
        .replace("CHECK", "agents in 0..=3")
        .replace("agents = 4", "agents = 3");
    plan(&s, &ok, &[]).success();
}

/// An object input's field: its check is its own.
#[test]
fn an_object_fields_range_check_is_its_bounds() {
    same(
        "range-field",
        "\ninput pool { size: int = 1 check CHECK }\non(1)\nset pool.size = 4 where on(1)\n\
         resource compute.vm a { cpus = pool.size }\nuse fake\n",
        ["size in 1..=3", "1 <= size <= 3"],
        &[],
    );
}

/// A `type` block's attribute: a literal outside the range is a compile
/// error either way.
#[test]
fn a_type_attributes_range_check_is_its_bounds() {
    let r = same(
        "range-type",
        "\ntype compute.vm {\n  cpus: int check CHECK\n}\n\
         resource compute.vm a { cpus = 40 }\nuse fake\n",
        ["cpus in 1..=8", "1 <= cpus <= 8"],
        &[],
    );
    assert!(r.stderr.contains("range(1, 8)"), "{}", r.stderr);
}

/// A range of quantities is checked over the value as its bounds are:
/// `--set disk=5Ti` is refused by either form.
#[test]
fn a_quantity_range_check_refuses_as_its_bounds_do() {
    let s = Scratch::new("range-bytes");
    for check in ["disk in 10Gi..=4Ti", "10Gi <= disk <= 4Ti"] {
        let src = format!(
            "\ninput disk: bytes = 50Gi check {check}\nresource compute.vm a {{ disk }}\nuse fake\n"
        );
        let r = plan(&s, &src, &["--set", "disk=5Ti"]).failure();
        assert!(
            r.stderr.contains("input disk fails"),
            "{check}: {}",
            r.stderr
        );
        plan(&s, &src, &["--set", "disk=4Ti"]).success();
    }
}
