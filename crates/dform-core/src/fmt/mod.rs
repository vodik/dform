//! `dform fmt`: print a file in its normal forms (proposal H section 3,
//! [`normal`]) from its lossless tree, in its layout ([`layout`], R-52):
//! what fits in [`WIDTH`] columns on one line, what does not broken from
//! the outside in. The author's line breaks are not kept, their blank
//! lines (at most one in a row) and comments are. In a project, a literal
//! in a typed position is in its shortest spelling ([`typed`]). A file
//! already in this form prints back byte for byte.

mod doc;
mod header;
mod layout;
mod normal;
mod typed;

pub use layout::{INLINE_LITERALS, WIDTH};
pub use typed::Typing;

use crate::syntax::SyntaxKind::EDITION;
use crate::syntax::SyntaxNode;

/// Format a file's source; a file with syntax errors is not formatted, but
/// for a header statement after the body began, which is moved (R-27),
/// and an `edition` line, which is dropped (R-68).
pub fn format_source(name: &str, src: &str) -> anyhow::Result<String> {
    format_source_in(name, src, None)
}

/// [`format_source`] for a file of a project, whose typed positions
/// `typing` knows.
pub fn format_source_in(name: &str, src: &str, typing: Option<&Typing>) -> anyhow::Result<String> {
    if is_signature_file(src) {
        return Ok(format_signature_file(src));
    }
    let parse = crate::syntax::parser::parse(src);
    let editions: Vec<rowan::TextRange> = parse
        .syntax()
        .children()
        .filter(|n| n.kind() == EDITION)
        .map(|n| n.text_range())
        .collect();
    let in_edition = |at: usize| {
        editions
            .iter()
            .any(|r| usize::from(r.start()) <= at && at < usize::from(r.end()))
    };
    if parse
        .errors
        .iter()
        .any(|e| !e.misplaced && !in_edition(e.start))
    {
        return Err(crate::parser::syntax_diagnostics(name, src, &parse).into());
    }
    if !editions.is_empty() {
        return format_source_in(name, &without_editions(src, &editions), typing);
    }
    Ok(format_in(&parse.syntax(), typing))
}

/// Whether `src` is a signature file (R-6, R-24: `std/*.df`), read by
/// `functions::parse` at build time, not a program the normal parser
/// knows: its first line, comments and blank ones aside, is `package
/// NAME`.
fn is_signature_file(src: &str) -> bool {
    src.lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .is_some_and(|l| l.starts_with("package "))
}

/// A signature file's normal form (the std ticket: until R-24 moves this
/// format into the program parser, `fmt` reads it itself): comments and
/// doc comments as written; each `[internal] fn` line's signature with
/// a space after every comma and around `->`, its flags separated by
/// `, ` (`functions::function`, the registry reader's own parser).
fn format_signature_file(src: &str) -> String {
    let mut out = String::new();
    let mut blank_run = 0;
    for line in src.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push('\n');
            }
            continue;
        }
        blank_run = 0;
        let l = trimmed.trim_start();
        let (prefix, rest) = match l.strip_prefix("internal ") {
            Some(r) => ("internal ", r),
            None => ("", l),
        };
        if let Some(sig) = rest.strip_prefix("fn ")
            && let Ok(f) = crate::functions::function(sig.trim())
        {
            out.push_str(prefix);
            out.push_str("fn ");
            out.push_str(&print_signature(&f));
            out.push('\n');
            continue;
        }
        out.push_str(trimmed);
        out.push('\n');
    }
    out
}

/// `name(p: T, p2?: T2, ...) -> T[?] [flags]`, a bare `fn` line's own
/// normal form (its `package.` not written: the file gives it).
fn print_signature(f: &crate::functions::Function) -> String {
    let params: Vec<String> = f
        .params
        .iter()
        .map(|p| format!("{}{}: {}", p.name, if p.optional { "?" } else { "" }, p.ty))
        .chain(f.variadic.then(|| "...".to_string()))
        .collect();
    let mut s = format!("{}({}) -> {}", f.name, params.join(", "), f.ret);
    if f.partial {
        s.push('?');
    }
    let mut flags = Vec::new();
    if f.forwards {
        flags.push("forwards");
    }
    if f.forwards_nulls {
        flags.push("forwards nulls");
    }
    if !flags.is_empty() {
        s.push(' ');
        s.push_str(&flags.join(", "));
    }
    s
}

/// `src` without its `edition` lines, and the blank line after each.
fn without_editions(src: &str, editions: &[rowan::TextRange]) -> String {
    let mut out = String::new();
    let mut at = 0;
    for r in editions {
        let start = usize::from(r.start());
        let line = src[..start].rfind('\n').map_or(0, |i| i + 1);
        out.push_str(&src[at..line]);
        let mut end = usize::from(r.end());
        end += src[end..].find('\n').map_or(src.len() - end, |i| i + 1);
        if src[end..].starts_with('\n') {
            end += 1;
        }
        at = end;
    }
    out.push_str(&src[at..]);
    out
}

/// Format a parsed file: its header in order (R-27), a component's block in
/// the same order (R-11a), its normal forms, then its layout. The tree must
/// be free of syntax errors but for a header statement out of place.
pub fn format(root: &SyntaxNode) -> String {
    format_in(root, None)
}

/// [`format`], with a project's `typing`.
pub fn format_in(root: &SyntaxNode, typing: Option<&Typing>) -> String {
    let placed = header::reorder_all(root, &root.to_string())
        .map(|src| crate::syntax::parser::parse(&src))
        .filter(|p| p.errors.is_empty());
    let root = placed.as_ref().map_or(root.clone(), |p| p.syntax());
    let mut out = print(&root);
    // A normal form can make another one apply (a body joined onto its
    // line compares what the line before it bound): to a fixpoint, bounded.
    for _ in 0..4 {
        let tree = crate::syntax::parser::parse(&out).syntax();
        let next = normal::normalize(&tree, &out)
            .or_else(|| typing.and_then(|t| typed::normalize(&tree, &out, t)));
        let Some(next) = next else {
            break;
        };
        let again = crate::syntax::parser::parse(&next);
        if !again.errors.is_empty() {
            break;
        }
        out = print(&again.syntax());
    }
    out
}

/// Print a parsed file in its layout (R-52).
fn print(root: &SyntaxNode) -> String {
    let mut d = layout::layout(root);
    doc::propagate(&mut d);
    let mut out = doc::print(&d, WIDTH);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syntax::parser::parse;

    fn fmt(src: &str) -> String {
        let p = parse(src);
        assert!(p.errors.is_empty(), "{:?}", p.errors);
        format(&p.syntax())
    }

    #[test]
    fn a_formatted_file_prints_back_unchanged() {
        let src = "# c\np(a, \"b\") where q(a), a != 1\n";
        assert_eq!(fmt(src), src);
    }

    /// `edition` is gone (R-68): fmt drops the line, and the blank after it.
    #[test]
    fn a_stray_edition_is_dropped() {
        assert_eq!(
            format_source("p.df", "# c\n\nedition 2026\n\np(1)\n").unwrap(),
            "# c\n\np(1)\n"
        );
        assert_eq!(
            format_source("p.df", "edition 2026\np(1)\n").unwrap(),
            "p(1)\n"
        );
        assert!(format_source("p.df", "edition 2026\np(\n").is_err());
    }

    #[test]
    fn spaces_and_indentation_are_normalised() {
        assert_eq!(
            fmt("p( a ,b )where q(x),x>1\nresource  net.vpc  main{cidr=\"x\",tags={a:1}}\n"),
            "p(a, b) where q(x), x > 1\nresource net.vpc main { cidr = \"x\", tags = { a: 1 } }\n"
        );
        assert_eq!(
            fmt("component m {\np(x) where q(x)\n}\n"),
            "component m {\n  p(x) where q(x)\n}\n"
        );
        assert_eq!(fmt("p(a /b,t[e].p)\n"), "p(a / b, t[e].p)\n");
        assert_eq!(
            fmt("p(i) where i in 0 .. 3, j in 1 ..= n + 1\n"),
            "p(i) where i in 0..3, j in 1..=n + 1\n"
        );
    }

    /// A string may span lines (R-61): its text is the program's, so fmt
    /// re-indents the lines around it and never the lines inside it; the
    /// groups around it break, but for a body.
    #[test]
    fn a_string_that_spans_lines_is_kept_as_written() {
        assert_eq!(
            fmt("resource t n {\n      a = \"x\n   y ${v}\n\"\n b = 1\n}\n"),
            "resource t n {\n  a = \"x\n   y ${v}\n\"\n  b = 1\n}\n"
        );
        let src = "p(s) where q(v), s = \"one\n  ${v}\ntwo\"\n";
        assert_eq!(fmt(src), src);
    }

    /// A body that fits the line is written on it; one that does not is
    /// in braces, a literal per line (R-52), and so is one of more than
    /// three literals whatever the width (R-10).
    #[test]
    fn a_body_goes_on_one_line_when_it_fits() {
        assert_eq!(
            fmt(
                "resource t n {\nf = x\n} where {\n  a(x)\n  b(x)\n}\ndeny \"m\" { x } where {\na(x)\nnot b(x)\n}\n"
            ),
            "resource t n { f = x } where a(x), b(x)\ndeny \"m\" { x } where a(x), not b(x)\n"
        );
        let long = "p(x) where {\n  q(x, \"a rather long string that fills the line\")\n  \
                    r(x, \"and another one that runs past its end\")\n}\n";
        assert_eq!(fmt(long), long);
        assert_eq!(
            fmt(
                "p(x) where q(x, \"a rather long string that fills the line\"), r(x, \"and another one that runs past its end\")\n"
            ),
            long
        );
        // Three literals stay on the line; four are a literal per line.
        assert_eq!(
            fmt("p(x) where a(x), b(x), c(x)\n"),
            "p(x) where a(x), b(x), c(x)\n"
        );
        assert_eq!(
            fmt("p(x) where a(x), b(x), c(x), d(x)\n"),
            "p(x) where {\n  a(x)\n  b(x)\n  c(x)\n  d(x)\n}\n"
        );
        let four = "deny \"m\" where not { a(x), b(x), c(x), d(x) }\n";
        assert_eq!(
            fmt(four),
            "deny \"m\" where not {\n  a(x)\n  b(x)\n  c(x)\n  d(x)\n}\n"
        );
        // A refinement's body has no braces: it stays on its line.
        let refined = "input n: int check n > 0, n < 10, n != 3, n != 5\n";
        assert_eq!(fmt(refined), refined);
    }

    /// No line break of the author's survives (R-52): what fits on a line
    /// is printed on it, however it was written.
    #[test]
    fn the_authors_line_breaks_do_not_survive() {
        assert_eq!(
            fmt("resource t n {\n  a = 1\n  b = [\n    1,\n    2,\n  ]\n}\n"),
            "resource t n { a = 1, b = [1, 2] }\n"
        );
        assert_eq!(
            fmt("p(x) where q(\n  x,\n  {\n    a: 1\n  }\n)\n"),
            "p(x) where q(x, { a: 1 })\n"
        );
        assert_eq!(
            fmt("component m {\n  input n: int = 1\n}\nf(\"k\", {\n  a: 1\n})\n"),
            "component m {\n  input n: int = 1\n}\nf(\"k\", { a: 1 })\n"
        );
    }

    /// What does not fit breaks from the outside in, an element per line
    /// with a trailing comma; inner groups that then fit stay on one line.
    #[test]
    fn a_long_term_breaks_from_the_outside_in() {
        let wide = "x".repeat(48);
        let src = format!(
            "resource t n {{ tags = {{ a: \"{wide}\", b: {{ c: 1, d: [1, 2] }}, e: \"{wide}\" }} }}\n"
        );
        assert_eq!(
            fmt(&src),
            format!(
                "resource t n {{\n  tags = {{\n    a: \"{wide}\",\n    b: {{ c: 1, d: [1, 2] }},\n    \
                 e: \"{wide}\",\n  }}\n}}\n"
            )
        );
        // A call breaks its arguments.
        let src = format!("f(\"{wide}\", \"{wide}\", [1, 2])\n");
        assert_eq!(
            fmt(&src),
            format!("f(\n  \"{wide}\",\n  \"{wide}\",\n  [1, 2],\n)\n")
        );
        // A comprehension: `[ item |`, a literal per line, a trailing comma,
        // `]`; the comma is read back.
        let src = format!("let l = [ x | q(x, \"{wide}\"), r(x, \"{wide}\") ]\n");
        assert_eq!(
            fmt(&src),
            format!("let l = [ x |\n  q(x, \"{wide}\"),\n  r(x, \"{wide}\"),\n]\n")
        );
        assert_eq!(fmt(&fmt(&src)), fmt(&src));
        // A long chain or string has nowhere to break: left as is.
        let chain = format!("let l = a.{}\n", ["segment"; 14].join("."));
        assert_eq!(fmt(&chain), chain);
    }

    /// A list whose only element is an object hugs it, `[{` .. `}]`, and an
    /// object whose only field is a list hugs that.
    #[test]
    fn a_list_of_one_object_hugs_it() {
        let wide = "x".repeat(50);
        let src = format!("resource t n {{ c = [{{ a: \"{wide}\", b: \"{wide}\" }}] }}\n");
        assert_eq!(
            fmt(&src),
            format!(
                "resource t n {{\n  c = [{{\n    a: \"{wide}\",\n    b: \"{wide}\",\n  }}]\n}}\n"
            )
        );
        let src = format!("let o = {{ k: [\"{wide}\", \"{wide}\"] }}\n");
        assert_eq!(
            fmt(&src),
            format!("let o = {{ k: [\n  \"{wide}\",\n  \"{wide}\",\n] }}\n")
        );
    }

    /// Blocks, type blocks and object inputs have one rule: a comma
    /// between entries on one line, none when broken; lists and objects
    /// have a trailing comma when broken and none on one line.
    #[test]
    fn commas_follow_one_rule() {
        assert_eq!(
            fmt("resource t n {\n  a = 1,\n  b = [1, 2,],\n}\n"),
            "resource t n { a = 1, b = [1, 2] }\n"
        );
        let wide = "x".repeat(40);
        for (open, field) in [
            ("input i {", "a: string"),
            ("type t {", "a: string"),
            ("output o {", "a = 1"),
        ] {
            let src = format!("{open}\n  {field},\n  b: \"{wide}\",\n  c: \"{wide}\",\n}}\n");
            let src = src
                .replace("b: \"", "b: enum(\"")
                .replace("\",\n  c", "\"),\n  c");
            let src = src
                .replace("c: \"", "c: enum(\"")
                .replace("\",\n}", "\"),\n}");
            let want =
                format!("{open}\n  {field}\n  b: enum(\"{wide}\")\n  c: enum(\"{wide}\")\n}}\n");
            assert_eq!(fmt(&src), want, "{open}");
            let one = format!("{open} {field}, b: int }}\n");
            assert_eq!(fmt(&format!("{open}\n  {field},\n  b: int,\n}}\n")), one);
        }
    }

    /// A block entry with a body would run into the next entry on one
    /// line: such a block is an entry per line.
    #[test]
    fn a_block_with_a_rule_in_it_breaks() {
        let src = "use m {\n  p(x) where q(x), r(x)\n  n = 1\n}\n";
        assert_eq!(fmt(src), src);
    }

    /// Comments stay where they were: one on its own line above what
    /// follows it, one after code at the end of that code's line; the
    /// groups around them break.
    #[test]
    fn comments_stay_in_place() {
        let src = "resource t n {\n  # above\n  a = 1 # after\n\n  b = [\n    1, # one\n    2,\n  ]\n  # last\n}\n";
        assert_eq!(fmt(src), src);
        let src = "p(x) where {\n  q(x) # why\n  r(x)\n}\n";
        assert_eq!(fmt(src), src);
        let src = "provider k8s { # later\n}\np(x) where q(x) # a rule\n# the end\n";
        assert_eq!(fmt(src), src);
    }

    /// A resource block's leaves under one parent are one entry (R-52,
    /// amended): two or more leaves, `=` at one rank, nothing below them.
    #[test]
    fn leaves_under_one_parent_fold_into_an_object() {
        assert_eq!(
            fmt(
                "resource t n {\n  metadata.name = \"a\"\n  spec.replicas = 1\n  \
                 metadata.namespace\n  metadata.\"x-y\" = [1]\n}\n"
            ),
            "resource t n { metadata = { name: \"a\", namespace, \"x-y\": [1] }, spec.replicas = 1 }\n"
        );
        // An object under the parent, a path below it, `+=`, two ranks, one
        // leaf: dotted.
        for src in [
            "resource t n { metadata.name = \"a\", metadata.labels = { a: 1 } }\n",
            "resource t n { metadata.name = \"a\", metadata.labels.a = 1 }\n",
            "resource t n { tags.a = 1, tags.b += [2] }\n",
            "resource t n { tags.a = 1 @default, tags.b = 2 }\n",
            "resource t n { spec.replicas = 1 }\n",
            "set { db.size = 1, db.tier = 2 } where env == \"prod\"\n",
        ] {
            assert_eq!(fmt(src), src);
        }
        assert_eq!(
            fmt("resource t n { tags.a = 1 @default, tags.b = 2 @default }\n"),
            "resource t n { tags = { a: 1, b: 2 } @default }\n"
        );
    }

    #[test]
    fn blank_lines_collapse_to_one() {
        assert_eq!(
            fmt("p(\"a\")\n\n\n\nq(\"b\")\n\n"),
            "p(\"a\")\n\nq(\"b\")\n"
        );
    }

    #[test]
    fn idempotent() {
        let src = "p(x)where {\n  q(x)\n  r(x)\n}\nresource t n { a=1, b=2\n c=3 }\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once);
    }

    /// A signature file (R-6, R-24: `package` first) is read by
    /// `functions::parse`, not the program parser: `fmt` normalises its
    /// `fn` lines' spacing and leaves its comments as written.
    #[test]
    fn a_signature_file_is_formatted_by_its_own_normal_form() {
        assert!(is_signature_file("package str\nfn len(s: string) -> int\n"));
        assert!(!is_signature_file("p(x) where q(x)\n"));
        let src = "package p\n\n#| A doc.\n#| example: f(1)\nfn f(a:int,b?:string,...)->int? forwards,forwards nulls\n";
        let want = "package p\n\n#| A doc.\n#| example: f(1)\nfn f(a: int, b?: string, ...) -> int? forwards, forwards nulls\n";
        assert_eq!(format_signature_file(src), want);
        // Every shipped signature file is already its own normal form.
        for (file, text) in crate::functions::SOURCES {
            assert_eq!(&format_signature_file(text), text, "{file}");
        }
    }
}
