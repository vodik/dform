//! A copy's line names the bindings of the clause that made it, those
//! its address does not show (After R-149 amendment 5): `i` of `i in
//! 0..agents`, never `agents`, an input the clause reads and the same on
//! every row. `-v` says how, the values read included.

mod common;
use common::Scratch;

const COPIES: &str = r#"input agents: int = 2

let base = 3

use fake

resource net.vpc "agent-${i}" { cidr = "10.${i}.0.0/16" } where i in 0..agents
resource net.subnet "s-${n}" { cidr = "10.${i}.1.0/24" } where i in 0..agents, n = i * 10 + base
"#;

/// `agent-1` shows `i = 1`, so its line says no binding; `s-13` does not
/// show `i = 1`, so its line says it; neither says `agents` or `base`.
#[test]
fn a_copys_line_names_what_its_clause_ranges_over() {
    let s = Scratch::project("copies-with");
    s.write("main.df", COPIES);
    let r = s.run(&["plan", "main.df"]).success();
    let line = |addr: &str| {
        r.stdout
            .lines()
            .find(|l| l.starts_with(&format!("  + {addr} ")))
            .unwrap_or_else(|| panic!("no {addr}:\n{}", r.stdout))
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    assert_eq!(line("net.vpc agent-1"), "+ net.vpc agent-1 main.df:7");
    assert_eq!(
        line("net.subnet s-13"),
        "+ net.subnet s-13 main.df:8 with i = 1"
    );
    let r = s.run(&["plan", "-v", "main.df"]).success();
    assert!(
        r.stdout
            .contains("main.df:8  with i = 1, agents = 2, n = 13, base = 3"),
        "{}",
        r.stdout
    );
}
