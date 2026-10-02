//! A file's header (R-27): `edition`, then `import`, `key` and `input`
//! lines (value inputs, then relation inputs), then the body. `fmt`
//! places a header statement the author wrote out of that order, with the
//! comments directly above it and on its line, keeping the author's order
//! within a kind; the parser reports one written after the body began
//! (`syntax::parser`), which is what this moves.

use crate::syntax::SyntaxKind::*;
use crate::syntax::{SyntaxElement, SyntaxNode};

/// The rank of a top-level statement in the header, or `None` for the body.
fn rank(n: &SyntaxNode) -> Option<u8> {
    match n.kind() {
        IMPORT => Some(0),
        INPUT if crate::syntax::resolve::is_key(n) => Some(1),
        INPUT => Some(2),
        INPUT_RELATION => Some(3),
        _ => None,
    }
}

/// One top-level statement with the comments it owns: those directly above
/// it (no blank line between) and one after it on its last line, as byte
/// offsets of the source.
struct Unit {
    start: usize,
    end: usize,
    rank: Option<u8>,
    edition: bool,
}

fn units(root: &SyntaxNode) -> Vec<Unit> {
    let elems: Vec<SyntaxElement> = root.children_with_tokens().collect();
    let mut out = Vec::new();
    for (i, e) in elems.iter().enumerate() {
        let SyntaxElement::Node(n) = e else {
            continue;
        };
        let r = n.text_range();
        let mut start = usize::from(r.start());
        // The comments above, back to a blank line or another statement.
        let mut j = i;
        while j >= 2 {
            let (SyntaxElement::Token(ws), SyntaxElement::Token(c)) =
                (&elems[j - 1], &elems[j - 2])
            else {
                break;
            };
            if ws.kind() != WHITESPACE
                || c.kind() != COMMENT
                || ws.text().matches('\n').count() != 1
            {
                break;
            }
            // A comment after code on its line is that line's.
            let own_line = j < 3
                || matches!(&elems[j - 3], SyntaxElement::Token(t)
                    if t.kind() == WHITESPACE && t.text().contains('\n'));
            if !own_line {
                break;
            }
            start = usize::from(c.text_range().start());
            j -= 2;
        }
        let mut end = usize::from(r.end());
        if let (Some(SyntaxElement::Token(ws)), Some(SyntaxElement::Token(c))) =
            (elems.get(i + 1), elems.get(i + 2))
            && ws.kind() == WHITESPACE
            && !ws.text().contains('\n')
            && c.kind() == COMMENT
        {
            end = usize::from(c.text_range().end());
        }
        out.push(Unit {
            start,
            end,
            rank: rank(n),
            edition: n.kind() == EDITION,
        });
    }
    out
}

/// `src` (whose tree is `root`) with its header in order, before its body;
/// `None` when it is already.
pub fn reorder(root: &SyntaxNode, src: &str) -> Option<String> {
    let units = units(root);
    let mut seen_body = false;
    let mut last = 0u8;
    let ordered = units.iter().filter(|u| !u.edition).all(|u| match u.rank {
        None => {
            seen_body = true;
            true
        }
        Some(r) => {
            let ok = !seen_body && r >= last;
            last = r;
            ok
        }
    });
    if ordered {
        return None;
    }
    // Everything up to the edition's line stays where it is.
    let prefix_end = units
        .iter()
        .find(|u| u.edition)
        .map_or(0, |u| line_end(src, u.end));
    let mut header: Vec<&Unit> = units.iter().filter(|u| u.rank.is_some()).collect();
    header.sort_by_key(|u| u.rank);
    let mut out = src[..prefix_end].trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    for u in &header {
        out.push_str(&src[u.start..u.end]);
        out.push('\n');
    }
    // The rest, without the header's statements.
    let mut rest = String::new();
    let mut at = prefix_end;
    for u in units
        .iter()
        .filter(|u| u.rank.is_some() && u.start >= prefix_end)
    {
        rest.push_str(&src[at..u.start]);
        at = line_end(src, u.end);
    }
    rest.push_str(&src[at..]);
    let rest = rest.trim_start();
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(rest);
    }
    Some(out)
}

/// The offset after the newline that ends the line `at` is on (or the
/// end of the text).
fn line_end(src: &str, at: usize) -> usize {
    match src[at..].find('\n') {
        Some(i) => at + i + 1,
        None => src.len(),
    }
}
