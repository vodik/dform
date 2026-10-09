//! A program named from inside a project that is outside it (`plan
//! ../p.df`) keeps the plan's site column: each site is relative to the
//! program's directory, as when the run is in it. The project the run is
//! named from was taken for the root, the sites were under no root and
//! printed absolute, too wide for the column, so none printed.

mod common;
use common::Scratch;

#[test]
fn a_program_outside_the_project_keeps_its_sites() {
    let s = Scratch::new("plan-outside");
    s.write("proj/dform.toml", "[project]\nedition = \"2026\"\n");
    s.write(
        "p.df",
        "use fake\nresource net.vpc a { cidr = \"10.0.0.0/16\" }\n",
    );
    let inside = s.run_in("proj", &["plan", "../p.df"]).success();
    let here = s.run(&["plan", "p.df"]).success();
    assert!(
        inside.stdout.contains("  + net.vpc a  p.df:2\n"),
        "{}",
        inside.stdout
    );
    assert_eq!(inside.stdout, here.stdout);
}
