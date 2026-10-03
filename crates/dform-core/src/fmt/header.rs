//! A file's header (R-27): `key` and `input` lines (value
//! inputs, then relation inputs), then `decl` and `output` (R-11a), then
//! the body. `fmt` places a header statement the author wrote out of that
//! order, with the comments directly above it and on its line, keeping the
//! author's order within a kind; the parser reports a `key` or `input`
//! written after the body began (`syntax::parser`), which is what this
//! moves, but `decl` and `output` are fmt's own to place: the parser
//! accepts them anywhere, so this is the only thing that enforces their
//! order. A component's `{ }` block ([`reorder_block`]) takes the same
//! order, nothing in it enforced by the parser at all.

use crate::syntax::SyntaxKind::*;
use crate::syntax::{SyntaxElement, SyntaxNode};

/// The rank of a statement in a header (a file's, or a component's block),
/// or `None` for the body.
fn rank(n: &SyntaxNode) -> Option<u8> {
    match n.kind() {
        INPUT if crate::syntax::resolve::is_key(n) => Some(1),
        INPUT => Some(2),
        INPUT_RELATION => Some(3),
        DECL => Some(4),
        OUTPUT_DECL => Some(5),
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
        });
    }
    out
}

/// `units` (a file's, or one component block's) reordered: its ranked
/// statements first, sorted by rank and kept in source order within one,
/// then the rest as written, from `prefix_end` to `end`; `None` when it is
/// already in that order. `end` is the file's length for a file, or a
/// component block's `}` for [`reorder_block`].
fn reordered_span(units: &[Unit], src: &str, prefix_end: usize, end: usize) -> Option<String> {
    let mut seen_body = false;
    let mut last = 0u8;
    let ordered = units.iter().all(|u| match u.rank {
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
    let mut header: Vec<&Unit> = units.iter().filter(|u| u.rank.is_some()).collect();
    header.sort_by_key(|u| u.rank);
    let mut out = String::new();
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
    rest.push_str(&src[at..end]);
    let rest = rest.trim_start();
    if !rest.is_empty() {
        out.push('\n');
        out.push_str(rest);
    }
    Some(out)
}

/// `src` (whose tree is `root`) with its header in order, before its body;
/// `None` when it is already.
fn reorder(root: &SyntaxNode, src: &str) -> Option<String> {
    let units = units(root);
    // What is above the first statement and not its own (the file's
    // leading comments) stays where it is.
    let prefix_end = units.first().map_or(0, |u| u.start);
    let span = reordered_span(&units, src, prefix_end, src.len())?;
    let mut out = src[..prefix_end].trim_end().to_string();
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    out.push_str(&span);
    Some(out)
}

/// The same order as [`reorder`], scoped to one component's `{ }` block
/// (R-11a): an edit (its byte range and replacement) that places the
/// block's statements, or `None` when it is already in order. Nothing in
/// a component's block is a parser error (`syntax::parser` never checks
/// one); this is the only thing that places it.
fn reorder_block(block: &SyntaxNode, src: &str) -> Option<(usize, usize, String)> {
    let l_brace = block
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == L_BRACE)?;
    let r_brace = block
        .children_with_tokens()
        .filter_map(|e| e.into_token())
        .find(|t| t.kind() == R_BRACE)?;
    let prefix_end: usize = l_brace.text_range().end().into();
    let end: usize = r_brace.text_range().start().into();
    let span = reordered_span(&units(block), src, prefix_end, end)?;
    // The replacement sits right after `{`, which `reordered_span` (built
    // for a file, where the header's own separator does this) does not
    // open with a newline of its own.
    Some((prefix_end, end, format!("\n{span}")))
}

/// [`reorder`], then [`reorder_block`] on every component in the result
/// (R-11a): the file's header first (reparsed, since a component's byte
/// offsets are the moved file's), then each component's own block, all at
/// once. `None` when nothing moves.
pub fn reorder_all(root: &SyntaxNode, src: &str) -> Option<String> {
    let (root, src, changed) = match reorder(root, src) {
        Some(next) => {
            let p = crate::syntax::parser::parse(&next);
            if p.errors.is_empty() {
                (p.syntax(), next, true)
            } else {
                (root.clone(), src.to_string(), false)
            }
        }
        None => (root.clone(), src.to_string(), false),
    };
    let edits: Vec<(usize, usize, String)> = root
        .descendants()
        .filter(|n| n.kind() == STMT_BLOCK)
        .filter_map(|b| reorder_block(&b, &src))
        .collect();
    if edits.is_empty() {
        return changed.then_some(src);
    }
    Some(super::normal::apply(&src, edits).unwrap_or(src))
}

/// The offset after the newline that ends the line `at` is on (or the
/// end of the text).
fn line_end(src: &str, at: usize) -> usize {
    match src[at..].find('\n') {
        Some(i) => at + i + 1,
        None => src.len(),
    }
}
