//! R-79: `plan --why=none|line|full` on the demo and crud-api. `line`, the
//! default, is the plan grouped by tick with each change's site; `full` is
//! `line` with each change's derivation under it (`--why` alone); `none`
//! is the bare diff as it was laid out before (tests/golden/*.plan-bare.txt
//! pins it byte for byte).

mod common;
use common::{Scratch, repo};

fn plan(s: &Scratch, example: &str, why: &[&str]) -> String {
    let dir = repo().join("examples").join(example);
    let mut args = vec!["-C", dir.to_str().unwrap(), "plan"];
    args.extend_from_slice(why);
    s.run(&args).success().stdout
}

fn levels(example: &str) {
    let s = Scratch::new(&format!("why-levels-{example}"));
    let line = plan(&s, example, &[]);
    assert_eq!(plan(&s, example, &["--why=line"]), line);
    assert!(line.contains("\ntick 1  "), "{line}");
    assert!(line.contains("\napply: tick 1 now"), "{line}");

    // `full` adds the derivation's lines under each change, nothing else.
    let full = plan(&s, example, &["--why=full"]);
    assert_eq!(plan(&s, example, &["--why"]), full);
    let derivation = |l: &str| {
        let t = l.trim_start();
        t.starts_with("by ") || t.starts_with("because ")
    };
    let kept: Vec<&str> = full.lines().filter(|l| !derivation(l)).collect();
    assert_eq!(kept, line.lines().collect::<Vec<_>>(), "{full}");
    assert!(
        full.lines().any(|l| l.trim_start().starts_with("by ")),
        "{full}"
    );
    assert!(full.lines().count() > line.lines().count(), "{full}");

    // `none`: the bare diff, no tick, no site.
    let none = plan(&s, example, &["--why=none"]);
    assert!(none.contains("\ndefinite:\n+ "), "{none}");
    assert!(
        !none.contains("\ntick 1  ") && !none.contains(".df:") && !none.contains("\napply: "),
        "{none}"
    );
}

#[test]
fn the_demo_plans_at_each_level() {
    levels("demo");
}

#[test]
fn crud_api_plans_at_each_level() {
    levels("crud-api");
}
