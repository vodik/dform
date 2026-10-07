//! The manual (R-148): dform(1) and a page per command, generated from the
//! command line's definitions; dform(1)'s hand-written sections are
//! docs/man's fragments, which docs/reference.md carries unchanged; `dform
//! help CMD` renders a page.

mod common;

use common::Scratch;
use expectrl::{Eof, Expect, Session};

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
    for want in [
        "dform",
        "dform-apply",
        "dform-stack-list",
        "dform-dev-plan",
        "dform-help",
    ] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    // The hidden commands have none.
    assert!(!names.iter().any(|n| n.contains("__")), "{names:?}");
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

#[test]
fn help_renders_dform_1() {
    let out = common::dform().arg("help").output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("NAME\n       dform - "), "{text}");
    for heading in [
        "SYNOPSIS",
        "OPTIONS",
        "SUBCOMMANDS",
        "EXIT STATUS",
        "ENVIRONMENT",
        "FILES",
    ] {
        assert!(
            text.contains(&format!("\n{heading}\n")),
            "{heading} in {text}"
        );
    }
    assert!(
        text.contains("\n       6      locked: another run holds the stack"),
        "{text}"
    );
    assert!(
        text.contains("\n       128 + N\n              stopped by signal N"),
        "{text}"
    );
    assert!(
        text.contains("\n       DFORM_LOG\n              debug: a line on stderr"),
        "{text}"
    );
    // Plain text: no roff, no overstrikes, no Markdown's backticks.
    assert!(
        !text.contains('\\') && !text.contains('\u{8}') && !text.contains('`'),
        "{text}"
    );
}

#[test]
fn help_renders_a_command() {
    let out = common::dform().args(["help", "apply"]).output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("NAME\n       dform-apply - A report: apply"),
        "{text}"
    );
    assert!(text.contains("\nSYNOPSIS\n       dform apply "), "{text}");
    assert!(
        text.contains("\n       -y, --yes\n              Apply without asking"),
        "{text}"
    );
    assert!(text.ends_with("SEE ALSO\n       dform(1)\n"), "{text}");
    assert!(!text.contains("EXIT STATUS"), "{text}");

    let out = common::dform()
        .args(["help", "stack", "list"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with("NAME\n       dform-stack-list - "),
        "{text}"
    );
    assert!(
        text.ends_with("SEE ALSO\n       dform(1), dform-stack(1)\n"),
        "{text}"
    );
}

#[test]
fn help_of_no_command_fails_naming_it() {
    let out = common::dform()
        .args(["help", "stack", "nosuch"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8(out.stderr).unwrap();
    assert!(err.contains("no command `dform stack nosuch`"), "{err}");
    // A hidden command has no page.
    let out = common::dform()
        .args(["help", "__complete"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
}

/// The pager is the terminal's: with stdout not one, the page is printed
/// even when `MANPAGER` is set.
#[test]
fn help_pages_only_on_a_terminal() {
    let out = common::dform()
        .args(["help", "apply"])
        .env("MANPAGER", "echo paged")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("NAME\n"), "{text}");
}

/// On a terminal `MANPAGER` (before `PAGER`) is given the page as man(1)
/// gives it, bold and italic overstruck.
#[test]
fn help_pages_through_manpager() {
    let s = Scratch::new("man-pager");
    let file = s.path("paged");
    let mut cmd = common::dform();
    cmd.args(["help", "apply"])
        .env("MANPAGER", format!("cat > '{}'", file.display()))
        .env("PAGER", "false");
    let mut p = Session::spawn(cmd).unwrap();
    p.set_expect_timeout(Some(std::time::Duration::from_secs(60)));
    p.expect(Eof).unwrap();
    match p.get_process().wait().unwrap() {
        expectrl::process::unix::WaitStatus::Exited(_, code) => assert_eq!(code, 0),
        other => panic!("{other:?}"),
    }
    let paged = std::fs::read_to_string(&file).unwrap();
    assert!(
        paged.starts_with("N\u{8}NA\u{8}AM\u{8}ME\u{8}E\n"),
        "{paged:?}"
    );
    assert!(paged.contains("-\u{8}-y\u{8}y"), "{paged:?}");
}

/// `--help` points at the manual in one closing line (R-147).
#[test]
fn help_flag_points_at_the_manual() {
    let out = common::dform().arg("--help").output().unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.ends_with("\nSee dform(1), or `dform help CMD`, for the full documentation.\n"),
        "{text}"
    );
}
