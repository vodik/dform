//! `dform fmt`: the repository's files are in formatted form, formatting
//! changes only whitespace and commas, and the CLI rewrites or checks.

mod common;
use common::{Scratch, repo};
use dform::syntax::SyntaxKind::{COMMA, WHITESPACE};
use dform::syntax::parser::parse;
use std::path::{Path, PathBuf};

fn df_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    entries.sort();
    for p in entries {
        if p.is_dir() {
            if dir != repo() {
                df_files(&p, out);
            }
        } else if p.extension().is_some_and(|e| e == "df") {
            out.push(p);
        }
    }
}

fn corpus() -> Vec<PathBuf> {
    let mut out = Vec::new();
    df_files(repo(), &mut out);
    for d in [
        "modules",
        "policies",
        "examples",
        "providers",
        "tests/syntax/ok",
    ] {
        df_files(&repo().join(d), &mut out);
    }
    out
}

fn fmt(src: &str) -> String {
    let p = parse(src);
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    dform::fmt::format(&p.syntax())
}

/// The tokens that carry meaning: everything but whitespace and commas
/// (comments included, so none is lost).
fn meaning(src: &str) -> Vec<String> {
    parse(src)
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !matches!(t.kind(), WHITESPACE | COMMA))
        .map(|t| t.text().trim_end().to_string())
        .collect()
}

/// Every file the repository ships prints back byte for byte (E §7's
/// programs are kept as E writes them).
#[test]
fn the_repository_is_formatted() {
    for f in corpus() {
        let name = f.strip_prefix(repo()).unwrap().display().to_string();
        if name.starts_with("tests/syntax/ok/e7") {
            continue;
        }
        let src = std::fs::read_to_string(&f).unwrap();
        assert_eq!(
            fmt(&src),
            src,
            "{name} is not formatted: run dform fmt {name}"
        );
    }
}

/// Formatting is idempotent and moves only whitespace and commas; the
/// commas it drops are the ones a newline or a closer makes redundant.
#[test]
fn formatting_keeps_every_token_but_redundant_commas() {
    for f in corpus() {
        let src = std::fs::read_to_string(&f).unwrap();
        let once = fmt(&src);
        assert_eq!(fmt(&once), once, "{} is not idempotent", f.display());
        assert_eq!(meaning(&once), meaning(&src), "{}", f.display());
        let commas = |s: &str| s.matches(',').count();
        assert!(commas(&once) <= commas(&src), "{}", f.display());
    }
}

#[test]
fn check_lists_unformatted_files_and_fmt_rewrites_them() {
    let s = Scratch::new("fmt");
    s.write("ok.df", "edition 2026.\n\np(a).\n");
    s.write(
        "messy.df",
        "edition 2026.\nresource net.vpc main {\n    cidr = \"10.0.0.0/16\",\n    tags = {team:\"x\"},\n}.\n",
    );
    let r = s.run(&["fmt", "--check", "ok.df", "messy.df"]).failure();
    assert_eq!(r.stdout, "messy.df\n");
    assert!(r.stderr.contains("1 file(s) not formatted"), "{}", r.stderr);
    assert!(s.read("messy.df").contains("    cidr"));

    s.run(&["fmt", "ok.df", "messy.df"]).success();
    assert_eq!(
        s.read("messy.df"),
        "edition 2026.\nresource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  tags = { team: \"x\" }\n}.\n"
    );
    s.run(&["fmt", "--check", "ok.df", "messy.df"]).success();
}

/// A file with a syntax error is reported, not rewritten.
#[test]
fn fmt_refuses_a_file_that_does_not_parse() {
    let s = Scratch::new("fmt-error");
    let bad = "edition 2026.\np(a :- q.\n";
    s.write("bad.df", bad);
    let r = s.run(&["fmt", "bad.df"]).failure();
    assert!(r.stderr.contains("bad.df:2:"), "{}", r.stderr);
    assert_eq!(s.read("bad.df"), bad);
}
