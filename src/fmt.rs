//! `dform fmt`: print a file from its lossless tree with whitespace and
//! commas normalised. Line breaks are the author's (gofmt's rule): a break
//! between two tokens stays a break, at most one blank line in a row; the
//! spaces within a line, the indentation, and the commas that a newline
//! makes redundant are the formatter's. A file already in this form prints
//! back byte for byte.

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

/// A statement's terminating `.` (a `.` inside a block path is not one).
fn is_terminator(t: &SyntaxToken) -> bool {
    t.kind() == DOT && parent_kind(t) != Some(BLOCK_PATH)
}

/// A comma the formatter drops: before a closer, or where a newline already
/// separates the entries of an assignment or type block.
fn drop_comma(t: &SyntaxToken, next: Option<&SyntaxToken>, newline_after: bool) -> bool {
    if t.kind() != COMMA {
        return false;
    }
    let Some(next) = next else { return false };
    if matches!(next.kind(), R_BRACKET | R_BRACE) && parent_kind(t) != Some(ARG_LIST) {
        return true;
    }
    newline_after && matches!(parent_kind(t), Some(BLOCK | TYPE_DECL | ATTR_DECL))
}

/// The space between two tokens on one line: "" or " ".
fn space(prev: &SyntaxToken, cur: &SyntaxToken) -> &'static str {
    let (p, c) = (prev.kind(), cur.kind());
    let (pp, cp) = (parent_kind(prev), parent_kind(cur));
    if p == COMMENT {
        return " ";
    }
    if c == COMMENT {
        return " ";
    }
    // Glued forms: a block path, an address, `name.Var`, `p/2`.
    if pp == cp && matches!(cp, Some(BLOCK_PATH | ADDR | QNAME_VAR)) {
        return "";
    }
    if (c == SLASH || p == SLASH) && matches!(cp, Some(DECL | EXPORT)) {
        return "";
    }
    if p == MINUS && pp == Some(UNARY_EXPR) {
        return "";
    }
    if matches!(c, COMMA | R_PAREN | COLON) || is_terminator(cur) {
        return "";
    }
    // Calls, atoms, type applications and records hug their name.
    if c == L_PAREN && (p.is_name() || p == QNAME) {
        return "";
    }
    if c == L_BRACE && cp == Some(RECORD_ATOM) {
        return "";
    }
    // Empty brackets.
    if is_open(p) && is_close(c) {
        return "";
    }
    // Lists and argument lists are tight; comprehensions, objects, records
    // and blocks breathe.
    if p == L_PAREN || c == R_PAREN {
        return "";
    }
    if p == L_BRACKET && pp == Some(LIST) || c == R_BRACKET && cp == Some(LIST) {
        return "";
    }
    if c == L_BRACKET && cp == Some(BLOCK_PATH) || p == L_BRACKET && pp == Some(BLOCK_PATH) {
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

/// The indentation of a line: one step per line that opened a bracket (or
/// a rule's `:-`) still open, after the closers that start the line (the
/// first is already popped; `rest` follows it).
fn indent(stack: &[(SyntaxKind, usize)], rest: &[Item]) -> usize {
    let mut stack = stack.to_vec();
    for it in rest {
        if it.newlines > 0 || !is_close(it.tok.kind()) {
            break;
        }
        while let Some((top, _)) = stack.pop() {
            if is_open(top) {
                break;
            }
        }
    }
    let mut lines: Vec<usize> = stack.iter().map(|(_, l)| *l).collect();
    lines.dedup();
    lines.len()
}

/// Format a file's source; a file with syntax errors is not formatted.
pub fn format_source(name: &str, src: &str) -> anyhow::Result<String> {
    let parse = crate::syntax::parser::parse(src);
    if !parse.errors.is_empty() {
        return Err(crate::parser::syntax_diagnostics(name, src, &parse).into());
    }
    Ok(format(&parse.syntax()))
}

/// Format a parsed file. The tree must be free of syntax errors.
pub fn format(root: &SyntaxNode) -> String {
    let items = items(root);
    let mut out = String::new();
    // Open brackets and rule necks: the output line each was printed on.
    let mut stack: Vec<(SyntaxKind, usize)> = Vec::new();
    let mut line = 0usize;
    for (i, it) in items.iter().enumerate() {
        let t = &it.tok;
        let k = t.kind();
        if is_close(k) {
            // Pop through any neck left open by an abandoned statement.
            while let Some((top, _)) = stack.pop() {
                if is_open(top) {
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
                }
                line += 1 + usize::from(blank);
                for _ in 0..indent(&stack, &items[i + 1..]) {
                    out.push_str(INDENT);
                }
            } else {
                out.push_str(space(&items[i - 1].tok, t));
            }
        }
        let text = t.text();
        out.push_str(if k == COMMENT { text.trim_end() } else { text });
        if is_open(k) || k == NECK {
            stack.push((k, line));
        }
        if is_terminator(t) && stack.last().is_some_and(|(top, _)| *top == NECK) {
            stack.pop();
        }
    }
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
        let src = "edition 2026.\n\n# c\np(a, \"b\") :-\n  q(X), # why\n  X != 1.\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn spaces_and_indentation_are_normalised() {
        assert_eq!(
            fmt("p( a ,b ):-q(X),X>1 .\nresource  net.vpc  main{cidr=\"x\",tags={a:1}}.\n"),
            "p(a, b) :- q(X), X > 1.\nresource net.vpc main { cidr = \"x\", tags = { a: 1 } }.\n"
        );
        assert_eq!(
            fmt("module m {\np(X) :-\nq(X).\n}.\n"),
            "module m {\n  p(X) :-\n    q(X).\n}.\n"
        );
    }

    #[test]
    fn commas_a_newline_makes_redundant_are_dropped() {
        assert_eq!(
            fmt("resource t n {\n  a = 1,\n  b = [1, 2,],\n}.\n"),
            "resource t n {\n  a = 1\n  b = [1, 2]\n}.\n"
        );
    }

    #[test]
    fn blank_lines_collapse_to_one() {
        assert_eq!(fmt("p(a).\n\n\n\nq(b).\n\n"), "p(a).\n\nq(b).\n");
    }

    #[test]
    fn brackets_opened_on_one_line_indent_once() {
        let src = "resource t n {\n  c = [{\n    a: 1\n  }]\n}.\n";
        assert_eq!(fmt(src), src);
    }

    #[test]
    fn idempotent() {
        let src = "p(X):-\n q(X) ,\n\n r(X).\nresource t n { a=1, b=2\n c=3 }.\n";
        let once = fmt(src);
        assert_eq!(fmt(&once), once);
    }
}
