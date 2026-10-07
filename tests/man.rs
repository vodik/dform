//! The manual (R-148): dform(1) and a page per command, generated from the
//! command line's definitions; dform(1)'s hand-written sections are
//! docs/man's fragments, which docs/reference.md carries unchanged.

mod common;

/// `cargo xtask man --check`: docs/reference.md's copies of the fragments
/// are the fragments.
#[test]
fn reference_carries_the_fragments() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/docs/reference.md");
    let reference = std::fs::read_to_string(path).unwrap();
    let spliced = dform::man::splice(&reference).unwrap();
    assert!(
        spliced == reference,
        "docs/reference.md's copies of docs/man/*.md differ from them: run `cargo xtask man`"
    );
    for f in &dform::man::FRAGMENTS {
        assert!(reference.contains(f.markdown.trim()), "{}", f.name);
    }
}

#[test]
fn a_page_per_command() {
    let pages = dform::man::pages();
    let names: Vec<&str> = pages.iter().map(|p| p.name.as_str()).collect();
    for want in ["dform", "dform-apply", "dform-stack-list", "dform-dev-plan"] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    // The hidden commands have none, nor clap's `help`.
    assert!(!names.iter().any(|n| n.contains("__")), "{names:?}");
    assert!(!names.iter().any(|n| n.ends_with("-help")), "{names:?}");
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), names.len(), "{names:?}");
    let top = &pages[0];
    for section in ["NAME", "SYNOPSIS", "DESCRIPTION", "OPTIONS", "SUBCOMMANDS"] {
        assert!(top.roff.contains(&format!(".SH {section}\n")), "{section}");
    }
    for section in ["\"EXIT STATUS\"", "ENVIRONMENT", "FILES", "\"SEE ALSO\""] {
        assert!(top.roff.contains(&format!(".SH {section}\n")), "{section}");
    }
    assert!(
        top.roff.contains(".TH DFORM 1 \"\" \"dform "),
        "{}",
        top.roff
    );
    let list = pages.iter().find(|p| p.name == "dform-stack-list").unwrap();
    assert!(
        list.roff
            .contains(".SH \"SEE ALSO\"\n\\fBdform\\fR(1), \\fBdform\\-stack\\fR(1)\n")
    );
    assert!(!list.roff.contains("EXIT STATUS"));
}

/// `--help` points at the manual in one closing line (R-147).
#[test]
fn help_flag_points_at_the_manual() {
    let out = common::dform().arg("--help").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.ends_with("\nSee dform(1) for the full documentation.\n"),
        "{text}"
    );
}
