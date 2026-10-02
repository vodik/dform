//! `dform fmt`: print a file in its normal forms (proposal H section 3,
//! [`normal`]) from its lossless tree, with whitespace and commas
//! normalised. Line breaks are the author's (gofmt's rule) but for a body,
//! which is on one line when it fits and in braces when it does not: a
//! break between two tokens stays a break, at most one blank line in a
//! row; the spaces within a line, the indentation, and the commas that a
//! newline makes redundant are the formatter's. A file already in this form
//! prints back byte for byte.
//!
//! Indentation: a line is one step deeper than the line that holds the
//! innermost construct still open at its first token: a bracket, or a
//! statement, block entry or clause that began on an earlier line. A line
//! that starts with a closer sits at the depth of the line that opened it.

mod header;
mod normal;

use crate::syntax::SyntaxKind::{self, *};
use crate::syntax::{SyntaxNode, SyntaxToken};

const INDENT: &str = "  ";

/// A significant token or a comment, with what the source had before it.
struct Item {
    tok: SyntaxToken,
    /// Newlines in the whitespace before this item (since the last item
    /// printed: a dropped comma's surroundings count).
    newlines: usize,
}

fn parent_kind(t: &SyntaxToken) -> Option<SyntaxKind> {
    t.parent().map(|p| p.kind())
}

fn is_open(k: SyntaxKind) -> bool {
    matches!(k, L_PAREN | L_BRACKET | L_BRACE)
}

fn is_close(k: SyntaxKind) -> bool {
    matches!(k, R_PAREN | R_BRACKET | R_BRACE)
}

/// Nodes a line break inside continues: the next line is one step deeper.
fn continues(k: SyntaxKind) -> bool {
    matches!(
        k,
        RULE | FACT
            | CHECK
            | SET
            | CLAUSE
            | ASSIGN
            | LET
            | INPUT
            | INPUT_RELATION
            | OUTPUT_DECL
            | ATTR_DECL
    )
}

/// A `{ }` body: its entries are separated by newlines or commas.
fn is_block_body(n: &SyntaxNode) -> bool {
    n.kind() == BODY
        && n.children_with_tokens()
            .find(|e| !e.kind().is_trivia())
            .is_some_and(|e| e.kind() == L_BRACE)
}

/// A comma the formatter drops: before a closer, or where a newline already
/// separates the entries of a block, a type block or a `{ }` body.
fn drop_comma(t: &SyntaxToken, next: Option<&SyntaxToken>, newline_after: bool) -> bool {
    if t.kind() != COMMA {
        return false;
    }
    let Some(next) = next else { return false };
    let parent = t.parent();
    if matches!(next.kind(), R_BRACKET | R_BRACE) && parent_kind(t) != Some(ARG_LIST) {
        return true;
    }
    newline_after
        && (matches!(parent_kind(t), Some(BLOCK | TYPE_DECL | ATTR_DECL))
            || parent.as_ref().is_some_and(is_block_body))
}

/// The space between two tokens on one line: "" or " ".
fn space(prev: &SyntaxToken, cur: &SyntaxToken) -> &'static str {
    let (p, c) = (prev.kind(), cur.kind());
    let (pp, cp) = (parent_kind(prev), parent_kind(cur));
    if p == COMMENT || c == COMMENT {
        return " ";
    }
    // Chains, dotted names and paths: `a.b[e].c`.
    if p == DOT || c == DOT {
        return "";
    }
    if c == L_BRACKET && matches!(cp, Some(INDEX | BLOCK_PATH)) {
        return "";
    }
    if (p == L_BRACKET && matches!(pp, Some(INDEX | BLOCK_PATH)))
        || (c == R_BRACKET && matches!(cp, Some(INDEX | BLOCK_PATH)))
    {
        return "";
    }
    if matches!(p, PLUS | MINUS) && matches!(pp, Some(UNARY_EXPR | BIND_ARG)) {
        return "";
    }
    if matches!(c, COMMA | R_PAREN | COLON) {
        return "";
    }
    // Calls, atoms, type applications, declarations and records hug their
    // name.
    if c == L_PAREN
        && matches!(
            cp,
            Some(ARG_LIST | TYPE_EXPR | DECL | EXTERN | INPUT_RELATION)
        )
    {
        return "";
    }
    // Empty brackets.
    if is_open(p) && is_close(c) {
        return "";
    }
    // Lists and argument lists are tight; comprehensions, objects, records,
    // bodies and blocks breathe.
    if p == L_PAREN || c == R_PAREN {
        return "";
    }
    if p == L_BRACKET && pp == Some(LIST) || c == R_BRACKET && cp == Some(LIST) {
        return "";
    }
    " "
}

/// Every significant token and comment, in order, with the newlines before
/// each; commas the formatter drops are left out.
fn items(root: &SyntaxNode) -> Vec<Item> {
    let toks: Vec<SyntaxToken> = root
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .collect();
    let mut out = Vec::new();
    let mut newlines = 0;
    for (i, t) in toks.iter().enumerate() {
        if t.kind() == WHITESPACE {
            newlines += t.text().matches('\n').count();
            continue;
        }
        if t.kind() == COMMA {
            // The next significant token, and whether a newline comes first.
            let mut nl = false;
            let mut next = None;
            for u in &toks[i + 1..] {
                match u.kind() {
                    WHITESPACE => nl |= u.text().contains('\n'),
                    COMMENT => {}
                    _ => {
                        next = Some(u);
                        break;
                    }
                }
            }
            if drop_comma(t, next, nl) {
                continue;
            }
        }
        out.push(Item {
            tok: t.clone(),
            newlines,
        });
        newlines = 0;
    }
    out
}

/// An open construct: a bracket (closed by its closer) or a continuing
/// node (open until its text ends), and the output line it began on.
struct Open {
    bracket: bool,
    end: u32,
    line: usize,
}

/// Format a file's source; a file with syntax errors is not formatted, but
/// for a header statement after the body began, which is moved (R-27).
pub fn format_source(name: &str, src: &str) -> anyhow::Result<String> {
    let parse = crate::syntax::parser::parse(src);
    if parse.errors.iter().any(|e| !e.misplaced) {
        return Err(crate::parser::syntax_diagnostics(name, src, &parse).into());
    }
    Ok(format(&parse.syntax()))
}

/// Format a parsed file: its header in order (R-27), its normal forms,
/// then its layout. The tree must be free of syntax errors but for a
/// header statement out of place.
pub fn format(root: &SyntaxNode) -> String {
    let placed = header::reorder(root, &root.to_string())
        .map(|src| crate::syntax::parser::parse(&src))
        .filter(|p| p.errors.is_empty());
    let root = placed.as_ref().map_or(root.clone(), |p| p.syntax());
    let mut out = print(&root);
    // A normal form can make another one apply (a body joined onto its
    // line compares what the line before it bound): to a fixpoint, bounded.
    for _ in 0..4 {
        let tree = crate::syntax::parser::parse(&out);
        let Some(next) = normal::normalize(&tree.syntax(), &out) else {
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

/// Print a parsed file with its layout normalised.
fn print(root: &SyntaxNode) -> String {
    let items = items(root);
    let mut out = String::new();
    let mut stack: Vec<Open> = Vec::new();
    // The indentation of every output line.
    let mut indents: Vec<usize> = vec![0];
    let mut line = 0usize;
    for (i, it) in items.iter().enumerate() {
        let t = &it.tok;
        let k = t.kind();
        let start: u32 = t.text_range().start().into();
        // Continuations that ended before this token.
        while stack.last().is_some_and(|o| !o.bracket && o.end <= start) {
            stack.pop();
        }
        let mut opener_line = None;
        if is_close(k) {
            while let Some(o) = stack.pop() {
                if o.bracket {
                    opener_line = Some(o.line);
                    break;
                }
            }
        }
        if i > 0 {
            if it.newlines > 0 {
                let blank = it.newlines > 1 && !is_close(k);
                out.push('\n');
                if blank {
                    out.push('\n');
                    indents.push(0);
                }
                line += 1 + usize::from(blank);
                let depth = match opener_line {
                    Some(l) => indents[l],
                    None => stack.last().map_or(0, |o| indents[o.line] + 1),
                };
                indents.push(depth);
                for _ in 0..depth {
                    out.push_str(INDENT);
                }
            } else {
                out.push_str(space(&items[i - 1].tok, t));
            }
        }
        // Continuing nodes that begin at this token.
        let mut starts: Vec<(u32, SyntaxKind)> = t
            .parent_ancestors()
            .filter(|n| continues(n.kind()))
            .filter(|n| first_token(n).is_some_and(|f| f == *t))
            .map(|n| (n.text_range().end().into(), n.kind()))
            .collect();
        starts.reverse();
        for (end, _) in starts {
            stack.push(Open {
                bracket: false,
                end,
                line,
            });
        }
        let text = t.text();
        out.push_str(if k == COMMENT { text.trim_end() } else { text });
        if is_open(k) {
            stack.push(Open {
                bracket: true,
                end: u32::MAX,
                line,
            });
        }
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn first_token(n: &SyntaxNode) -> Option<SyntaxToken> {
    n.descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| !t.kind().is_trivia())
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
        let src = "edition 2026\n\n# c\np(a, \"b\") where q(a), a != 1\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn spaces_and_indentation_are_normalised() {
        assert_eq!(
            fmt("p( a ,b )where q(x),x>1\nresource  net.vpc  main{cidr=\"x\",tags={a:1}}\n"),
            "p(a, b) where q(x), x > 1\nresource net.vpc main { cidr = \"x\", tags = { a: 1 } }\n"
        );
        assert_eq!(
            fmt("module m {\np(x) where q(x)\n}\n"),
            "module m {\n  p(x) where q(x)\n}\n"
        );
        assert_eq!(fmt("p(a /b,t[e].p)\n"), "p(a / b, t[e].p)\n");
    }

    /// A body that fits the line is written on it (section 3's normal
    /// form); one that does not keeps its braces, a literal per line.
    #[test]
    fn a_body_goes_on_one_line_when_it_fits() {
        assert_eq!(
            fmt(
                "resource t n {\nf = x\n} where {\n  a(x)\n  b(x)\n}\ndeny \"m\" { x } where {\na(x)\nnot b(x)\n}\n"
            ),
            "resource t n {\n  f = x\n} where a(x), b(x)\ndeny \"m\" { x } where a(x), not b(x)\n"
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
    }

    #[test]
    fn commas_a_newline_makes_redundant_are_dropped() {
        assert_eq!(
            fmt("resource t n {\n  a = 1,\n  b = [1, 2,],\n}\n"),
            "resource t n {\n  a = 1\n  b = [1, 2]\n}\n"
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
    fn brackets_opened_on_one_line_indent_once() {
        let src = "resource t n {\n  c = [{\n    a: 1\n  }]\n}\nf(\"k\", {\n  a: 1\n})\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn idempotent() {
        let src = "p(x)where {\n  q(x)\n  r(x)\n}\nresource t n { a=1, b=2\n c=3 }\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once);
    }
}
