//! `dform fmt`: the repository's files are in formatted form, formatting
//! changes only whitespace and commas, no line break of the author's
//! survives it (R-52), and the CLI rewrites or checks.

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

/// The tokens that carry meaning: everything but whitespace, commas and a
/// `where` body's braces (comments included, so none is lost).
fn meaning(src: &str) -> Vec<String> {
    use dform::syntax::SyntaxKind::{BODY, L_BRACE, LIT_NOT_BLOCK, R_BRACE};
    let where_brace = |t: &dform::syntax::SyntaxToken| {
        matches!(t.kind(), L_BRACE | R_BRACE)
            && t.parent().is_some_and(|b| {
                b.kind() == BODY && b.parent().is_some_and(|p| p.kind() != LIT_NOT_BLOCK)
            })
    };
    parse(src)
        .syntax()
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|t| !matches!(t.kind(), WHITESPACE | COMMA) && !where_brace(t))
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

/// Formatting is idempotent and moves only whitespace and commas (a
/// broken list's trailing comma, a broken block's separators, R-52), but
/// for a header statement out of order (R-27, R-11a), which it reorders
/// instead: every file of the corpus is already in order, so this still
/// holds for all of them, E §7's included (their `decl`/`output` already
/// sit in R-11a's order).
#[test]
fn formatting_keeps_every_token_but_commas() {
    for f in corpus() {
        let src = std::fs::read_to_string(&f).unwrap();
        let once = fmt(&src);
        assert_eq!(fmt(&once), once, "{} is not idempotent", f.display());
        assert_eq!(meaning(&once), meaning(&src), "{}", f.display());
    }
}

/// No line break of the author's survives (R-52): every file of the
/// repository (its comments and the blank lines inside its statements
/// taken out, which do survive), with each of its terms, blocks and bodies
/// joined onto one line, formats as it did.
#[test]
fn the_authors_line_breaks_do_not_survive() {
    let mut joins = 0;
    for f in corpus() {
        let name = f.strip_prefix(repo()).unwrap().display().to_string();
        let src = plain(&std::fs::read_to_string(&f).unwrap());
        let joined = join_lines(&src);
        joins += usize::from(joined != src);
        assert_eq!(fmt(&joined), fmt(&src), "{name}");
    }
    assert!(joins > 20, "only {joins} files had a line to join");
}

/// `src` without its comments, and without blank lines but between
/// statements.
fn plain(src: &str) -> String {
    let tree = parse(src).syntax();
    let mut out = String::new();
    for t in tree
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        match t.kind() {
            dform::syntax::SyntaxKind::COMMENT => {}
            WHITESPACE if t.text().contains('\n') && !top(&t) => {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
            }
            _ => out.push_str(t.text()),
        }
    }
    out
}

/// A token between two statements.
fn top(t: &dform::syntax::SyntaxToken) -> bool {
    t.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            dform::syntax::SyntaxKind::SOURCE_FILE | dform::syntax::SyntaxKind::STMT_BLOCK
        )
    })
}

/// `src` with every newline inside brackets replaced by a space, and every
/// block and braced body written on one line (its entries joined by `, `).
fn join_lines(src: &str) -> String {
    let tree = parse(src).syntax();
    let mut out = String::new();
    for t in tree
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
    {
        if t.kind() != WHITESPACE || !t.text().contains('\n') {
            out.push_str(t.text());
            continue;
        }
        let top = top(&t);
        let prev = t.prev_token().map(|p| p.kind());
        let next = t.next_token().map(|p| p.kind());
        if top || next.is_none() {
            out.push_str(t.text());
        } else if matches!(prev, Some(COMMA | dform::syntax::SyntaxKind::L_BRACE))
            || matches!(next, Some(dform::syntax::SyntaxKind::R_BRACE | COMMA))
            || !separates(&t)
        {
            out.push(' ');
        } else {
            out.push_str(", ");
        }
    }
    out
}

/// Whether the newline `t` separates two entries of a block or a braced
/// body (and so is a comma when joined).
fn separates(t: &dform::syntax::SyntaxToken) -> bool {
    use dform::syntax::SyntaxKind::*;
    t.parent().is_some_and(|p| {
        matches!(
            p.kind(),
            BLOCK | BODY | TYPE_DECL | INPUT | OUTPUT_DECL | ATTR_DECL
        )
    })
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
        "edition 2026\nresource net.vpc main { cidr = \"10.0.0.0/16\", tags = { team: \"x\" } }\nprovider fake\n"
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
/// inputs, then relation inputs), then `decl` and `output` (R-11a), then
/// the body; `use` is a body statement (R-65): `fmt` moves a header
/// statement written below the body, or out of its kind's order, with the
/// comments above it and on its line, and keeps the author's order within
/// a kind. The parser does not enforce `decl` and `output` (only `key` and
/// `input`, R-27), so this is `fmt`'s own placement, not the parser's.
#[test]
fn fmt_puts_the_header_in_order() {
    let src = "# A program.\n\nedition 2026\n\ninput b: int\n# The key.\nkey env: string\n\n\
               provider fake\n\n#| The relation.\ninput p from csv(\"p.csv\")\ndecl p(a)\n\n\
               p2(x) where p(x)\noutput r = p2(1)\nuse m # its modules\ninput a: int\n";
    let want = "# A program.\n\nedition 2026\n\n# The key.\n\
                key env: string\ninput b: int\ninput a: int\n#| The relation.\n\
                input p from csv(\"p.csv\")\ndecl p(a)\noutput r = p2(1)\n\nprovider fake\n\n\
                p2(x) where p(x)\nuse m # its modules\n";
    let got = dform::fmt::format_source("p.df", src).unwrap();
    assert_eq!(got, want);
    assert_eq!(dform::fmt::format_source("p.df", &got).unwrap(), got);
    // Another error is not formatted.
    let e = dform::fmt::format_source("p.df", &format!("{src}p(\n")).unwrap_err();
    assert!(format!("{e:#}").contains("p.df:"), "{e:#}");
}

/// A component's `{ }` block takes the same order (R-11a): `input`, then
/// `decl`, then `output`, each in source order with its comments, before
/// the rest; nothing in a block is a parser error (the order is fmt's
/// alone to place), and `use` and `instance` are body statements, never
/// moved.
#[test]
fn fmt_orders_a_components_interface_first() {
    let src = "edition 2026\n\ncomponent m {\n  input a: int\n\n  p(x) where q(x)\n  \
               use helper\n  decl q(a)\n  instance other o\n  #| the answer\n  \
               output r = p(a)\n}\n";
    let want = "edition 2026\n\ncomponent m {\n  input a: int\n  decl q(a)\n  \
                #| the answer\n  output r = p(a)\n\n  p(x) where q(x)\n  use helper\n  \
                instance other o\n}\n";
    assert_eq!(fmt(src), want);
    assert_eq!(fmt(&want), want);
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
                resource net.subnet s { zone, spec.selector.color @default, cidr = zone, tags += tags }\n";
    assert_eq!(fmt(src), want);
    assert_eq!(fmt(want), want);
}

/// In a project, a literal in a typed position is in its shortest
/// spelling, the providers' schemas read from their schema files with no
/// provider started (R-52, amended): a quantity's string loses its quotes,
/// an `inet` constructor of its own text goes. Outside a project, with no
/// schema, no literal changes.
#[test]
fn fmt_writes_a_typed_literal_in_its_shortest_spelling() {
    let src = "provider k8s\n\nresource k8s.deployment d {\n  spec.replicas = 1\n  \
               spec.template.spec.containers = [{ name: \"a\", resources: { limits: { memory: \"2Gi\" } } }]\n}\n\
               component c {\n  input cidr: inet\n}\ninstance c a { cidr = inet(\"10.1.0.0/16\") }\n";
    let want = "provider k8s\n\nresource k8s.deployment d {\n  spec.replicas = 1\n  \
                spec.template.spec.containers = [{ name: \"a\", resources: { limits: { memory: 2Gi } } }]\n}\n\
                component c {\n  input cidr: inet\n}\ninstance c a { cidr = \"10.1.0.0/16\" }\n";
    let p = Scratch::project("fmt-typed");
    p.write("stacks/app.df", src);
    p.run(&["fmt", "stacks/app.df"]).success();
    assert_eq!(p.read("stacks/app.df"), want);
    let s = Scratch::new("fmt-untyped");
    s.write("app.df", src);
    s.run(&["fmt", "--check", "app.df"]).success();
}
