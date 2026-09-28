//! Semi-naive evaluation with hash indexes does work proportional to what
//! it derives: a recursive walk down a chain of n edges reads O(n) tuples
//! (a naive loop, or a join without an index, reads O(n²)).

fn run(src: &str) -> dform::engine::EvalResult {
    let program = dform::parser::parse_program(src).unwrap();
    dform::engine::eval(&program, &[]).unwrap().0
}

#[test]
fn a_recursive_walk_reads_linearly() {
    let n = 2000;
    let mut src = String::from("start(0).\n");
    for i in 0..n {
        src.push_str(&format!("edge({i}, {}).\n", i + 1));
    }
    src.push_str("reach(X) :- start(X).\nreach(Y) :- reach(X), edge(X, Y).\n");
    let r = run(&src);
    assert_eq!(r.facts.iter().filter(|a| a.pred == "reach").count(), n + 1);
    assert!(
        r.reads < 20 * n as u64,
        "{} reads for a chain of {n}",
        r.reads
    );
}
