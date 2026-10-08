//! String literals (H-13, R-61): a literal's value with its escapes, and
//! its interpolation holes, read by the one scanner.

/// A string literal holds an interpolation `${..}` (`$${` is a literal
/// `${`); an unclosed one is one, for the lowering to report.
pub(super) fn has_hole(text: &str) -> bool {
    pieces(text).is_none_or(|ps| ps.iter().any(|p| matches!(p, Piece::Hole(..))))
}

/// A string literal with no holes as its value: escapes, and `$${` as
/// `${`.
pub(super) fn string_value(text: &str) -> Result<String, String> {
    Ok(unescape(text)?.replace("$${", "${"))
}

/// A piece of a string literal (H-13): the text between holes as written
/// (escapes kept, `$${` read as `${`), or a `${..}` hole's text with its
/// byte offset in the token.
#[derive(Debug, Clone, PartialEq)]
pub enum Piece<'a> {
    Text(String),
    Hole(&'a str, usize),
}

/// The pieces of a string token's text, quotes included: a text before
/// each hole and one after the last. `None` when a hole is never closed.
/// The one interpolation scanner: the lowering, the binding check and
/// `why`'s printer read a string through it.
pub fn pieces(text: &str) -> Option<Vec<Piece<'_>>> {
    scan(text).ok()
}

/// `pieces`, or the byte offset in the token of the `${` that is never
/// closed. A hole runs to its matching `}`, a string in it skipped whole
/// with its own holes (`lexer::hole_end`, R-175).
pub(super) fn scan(text: &str) -> Result<Vec<Piece<'_>>, usize> {
    let inner = (text.len().checked_sub(1))
        .and_then(|e| text.get(1..e))
        .ok_or(0usize)?;
    let bytes = inner.as_bytes();
    let mut out = Vec::new();
    let mut lit = String::new();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => {
                // `\u{...}` keeps its braces.
                let end = if bytes.get(i + 1) == Some(&b'u') {
                    inner[i..].find('}').map_or(i + 2, |e| i + e + 1)
                } else {
                    i + 2
                };
                lit.push_str(inner.get(i..end.min(inner.len())).ok_or(i)?);
                i = end;
            }
            b'$' if bytes.get(i + 1) == Some(&b'$') && bytes.get(i + 2) == Some(&b'{') => {
                lit.push_str("${");
                i += 3;
            }
            b'$' if bytes.get(i + 1) == Some(&b'{') => {
                let j = crate::lexer::hole_end(bytes, i + 2).ok_or(i + 1)?;
                out.push(Piece::Text(std::mem::take(&mut lit)));
                // +1: the opening quote.
                out.push(Piece::Hole(&inner[i + 2..j - 1], i + 2 + 1));
                i = j;
            }
            _ => {
                let c = inner[i..].chars().next().ok_or(i)?;
                lit.push(c);
                i += c.len_utf8();
            }
        }
    }
    out.push(Piece::Text(lit));
    Ok(out)
}

/// A string literal's value: escapes `\"` `\\` `\n` `\t` `\u{...}`, and
/// `\` at a line end, which joins the line with the next, whose leading
/// whitespace is kept (R-61).
pub fn unescape(lit: &str) -> Result<String, String> {
    let inner = &lit[1..lit.len() - 1];
    let mut out = String::new();
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('\n') => {}
            Some('\r') if chars.peek() == Some(&'\n') => {
                chars.next();
            }
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('"') => out.push('"'),
            Some('\\') => out.push('\\'),
            Some('u') => {
                let rest: String = chars.by_ref().take_while(|c| *c != '}').collect();
                let hex = rest.strip_prefix('{').ok_or("expected `\\u{...}`")?;
                let c = u32::from_str_radix(hex, 16)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("bad unicode escape `\\u{{{hex}}}`"))?;
                out.push(c);
            }
            Some(o) => return Err(format!("unknown escape `\\{o}`")),
            None => return Err("a string ends in `\\`".to_string()),
        }
    }
    Ok(out)
}
