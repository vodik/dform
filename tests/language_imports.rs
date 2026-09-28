//! `import` is a file include, loaded once per file however it is reached.

mod common;
use common::Scratch;

/// The same module file through two relative spellings is one file: were it
/// loaded twice, its module would be defined twice.
#[test]
fn a_file_reached_by_two_paths_loads_once() {
    let s = Scratch::new("lang-imports");
    s.write(
        "lib/net.df",
        "edition 2026.\nmodule net {\n  resource net.vpc vpc { cidr = \"10.0.0.0/16\" }.\n}.\n",
    );
    s.write("lib/more.df", "edition 2026.\nimport \"../lib/net.df\".\n");
    s.write(
        "p.df",
        "edition 2026.\nimport \"lib/net.df\".\nimport \"lib/more.df\".\ninstance net main {}.\n",
    );
    let r = s
        .run(&["--file", "p.df", "--world", "w.json", "plan"])
        .success();
    assert_eq!(
        r.summary(),
        "plan: 1 deformation (1 create)",
        "{}",
        r.stdout
    );
}
