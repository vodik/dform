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
        "examples",
        "crates/dform-mock/schemas",
        "tests/fixtures",
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
    s.write("ok.df", "edition 2026\n\np(\"a\")\nprovider fake\n");
    s.write(
        "messy.df",
        "edition 2026\nresource net.vpc main {\n    cidr = \"10.0.0.0/16\",\n    tags = {team:\"x\"},\n}\nprovider fake\n",
    );
    let r = s.run(&["fmt", "--check", "ok.df", "messy.df"]).failure();
    assert_eq!(r.stdout, "messy.df\n");
    assert!(r.stderr.contains("1 file(s) not formatted"), "{}", r.stderr);
    assert!(s.read("messy.df").contains("    cidr"));

    s.run(&["fmt", "ok.df", "messy.df"]).success();
    assert_eq!(
        s.read("messy.df"),
        "edition 2026\nresource net.vpc main {\n  cidr = \"10.0.0.0/16\"\n  tags = { team: \"x\" }\n}\nprovider fake\n"
    );
    s.run(&["fmt", "--check", "ok.df", "messy.df"]).success();
}

/// A file with a syntax error is reported, not rewritten.
#[test]
fn fmt_refuses_a_file_that_does_not_parse() {
    let s = Scratch::new("fmt-error");
    let bad = "edition 2026\np(\"a\") where q(]\nprovider fake\n";
    s.write("bad.df", bad);
    let r = s.run(&["fmt", "bad.df"]).failure();
    assert!(r.stderr.contains("bad.df:2:"), "{}", r.stderr);
    assert_eq!(s.read("bad.df"), bad);
}

/// A file's header is `edition`, then `key` and `input` lines (value
/// inputs, then relation inputs), then the body (R-27); `use` is a body
/// statement (R-65): `fmt`
/// moves a header statement written below the body, or out of its kind's
/// order, with the comments above it and on its line, and keeps the
/// author's order within a kind.
#[test]
fn fmt_puts_the_header_in_order() {
    let src = "# A program.\n\nedition 2026\n\ninput b: int\n# The key.\nkey env: string\n\n\
               provider fake\n\n#| The relation.\ninput p from csv(\"p.csv\")\ndecl p(a)\n\
               p2(x) where p(x)\nuse m # its modules\ninput a: int\n";
    let want = "# A program.\n\nedition 2026\n\n# The key.\n\
                key env: string\ninput b: int\ninput a: int\n#| The relation.\n\
                input p from csv(\"p.csv\")\n\nprovider fake\n\ndecl p(a)\np2(x) where p(x)\n\
                use m # its modules\n";
    let got = dform::fmt::format_source("p.df", src).unwrap();
    assert_eq!(got, want);
    assert_eq!(dform::fmt::format_source("p.df", &got).unwrap(), got);
    // Another error is not formatted.
    let e = dform::fmt::format_source("p.df", &format!("{src}p(\n")).unwrap_err();
    assert!(format!("{e:#}").contains("p.df:"), "{e:#}");
}

/// A `provider` or `instance` with no entries is written without braces
/// (R-26): `{}` parses, and `fmt` drops it; a block with a comment stays.
#[test]
fn fmt_drops_an_empty_block() {
    let src = "edition 2026\n\nprovider fake {}\nprovider env {\n}\nprovider k8s { # later\n}\n\
               component m {\n  input n: int = 1\n}\ninstance m a {} where 1 == 1\ninstance m b {   }\n\
               resource net.vpc v {}\n";
    let want = "edition 2026\n\nprovider fake\nprovider env\nprovider k8s { # later\n}\n\
                component m {\n  input n: int = 1\n}\ninstance m a where 1 == 1\ninstance m b\n\
                resource net.vpc v {}\n";
    assert_eq!(fmt(src), want);
    assert_eq!(fmt(want), want);
}

/// An entry that is only a path is `path = SEG`, SEG its last segment
/// (R-33): `fmt` prints `k` for `k = k` and leaves `k = v`, `k += k` and a
/// provider's `source` as written; both forms print back.
#[test]
fn fmt_puns_an_entry_whose_value_is_its_name() {
    let src = "edition 2026\n\nprovider aws { region = region, source = source }\n\
               resource net.subnet s {\n  zone = zone\n  spec.selector.color = color @default\n  \
               cidr = zone\n  tags += tags\n}\n";
    let want = "edition 2026\n\nprovider aws { region, source = source }\n\
                resource net.subnet s {\n  zone\n  spec.selector.color @default\n  \
                cidr = zone\n  tags += tags\n}\n";
    assert_eq!(fmt(src), want);
    assert_eq!(fmt(want), want);
}
