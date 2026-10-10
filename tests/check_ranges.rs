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
    // The check is spelled back from its parts until R-211 step 10 keeps
    // the source text, so the two forms agree up to the word `check`.
    let first = |r: &Run| {
        let line = r.stderr.lines().next().unwrap_or_default();
        line.split(" check ").next().unwrap_or_default().to_string()
    };
    assert_eq!(first(&a), first(&b), "{}\n---\n{}", a.stderr, b.stderr);
    a
}

/// `set agents = 4` against `check agents in 0..=3` is the error
/// `check 0 <= agents <= 3` gives (a literal the input's check refuses
/// before evaluation), not a deny after a plan of four copies.
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
    assert!(
        r.stderr.contains("input agents is int check"),
        "{}",
        r.stderr
    );
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

/// A range of quantities is a refinement as an int range is: a literal
/// outside it is refused before evaluation, in an input and in a `type`
/// block, and a `--set` outside it at the flag.
#[test]
fn a_quantity_range_check_is_its_bounds() {
    let forms = ["disk in 10Gi..=4Ti", "10Gi <= disk <= 4Ti"];
    let r = same(
        "range-bytes",
        "\ninput disk: bytes = 50Gi check CHECK\non(1)\nset disk = 5Ti where on(1)\n\
         resource compute.vm a { disk }\nuse fake\n",
        forms,
        &[],
    );
    assert!(
        r.stderr.contains("disk = 5Ti: input disk is bytes check"),
        "{}",
        r.stderr
    );
    let r = same(
        "range-bytes-type",
        "\ntype compute.vm {\n  disk: bytes check CHECK\n}\n\
         resource compute.vm a { disk = 5Ti }\nuse fake\n",
        forms,
        &[],
    );
    assert!(r.stderr.contains("range(10Gi, 4Ti)"), "{}", r.stderr);
    let s = Scratch::new("range-bytes-set");
    for check in forms {
        let src = format!(
            "\ninput disk: bytes = 50Gi check {check}\nresource compute.vm a {{ disk }}\nuse fake\n"
        );
        let r = plan(&s, &src, &["--set", "disk=5Ti"]).failure();
        assert!(
            r.stderr
                .contains("--set disk=5Ti: input disk is bytes check"),
            "{check}: {}",
            r.stderr
        );
        plan(&s, &src, &["--set", "disk=4Ti"]).success();
    }
}
