//! Positions and file names: LSP's (line, UTF-16 column) and `file://`
//! URIs against the byte offsets and paths dform's spans use.

use lsp_types::{Position, Range, Uri};
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// The byte offset of `pos` in `text` (clamped to the text).
pub fn offset(text: &str, pos: Position) -> usize {
    let mut line = 0u32;
    let mut start = 0usize;
    if pos.line > 0 {
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line += 1;
                if line == pos.line {
                    start = i + 1;
                    break;
                }
            }
        }
        if line < pos.line {
            return text.len();
        }
    }
    let mut units = 0u32;
    for (i, ch) in text[start..].char_indices() {
        if units >= pos.character || ch == '\n' {
            return start + i;
        }
        units += ch.len_utf16() as u32;
    }
    text.len()
}

/// The position of byte offset `at` in `text`.
pub fn position(text: &str, at: usize) -> Position {
    let at = at.min(text.len());
    let before = &text[..floor_char(text, at)];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[line_start..]
        .chars()
        .map(char::len_utf16)
        .sum::<usize>() as u32;
    Position { line, character }
}

fn floor_char(text: &str, mut at: usize) -> usize {
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

pub fn range(text: &str, start: usize, end: usize) -> Range {
    Range {
        start: position(text, start),
        end: position(text, end.max(start)),
    }
}

/// The whole of line `line` (1-based) from column `col` (1-based, in
/// characters), as `file:line:col` names a place.
pub fn line_range(text: &str, line: usize, col: usize) -> Range {
    let start_of_line = text
        .split_inclusive('\n')
        .take(line.saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    let rest = &text[start_of_line.min(text.len())..];
    let line_text = rest.split('\n').next().unwrap_or("");
    let start = start_of_line
        + line_text
            .char_indices()
            .nth(col.saturating_sub(1))
            .map_or(line_text.len(), |(i, _)| i);
    range(text, start, start_of_line + line_text.len())
}

/// A `file://` URI's path.
pub fn path_of(uri: &Uri) -> Option<PathBuf> {
    let s = uri.as_str();
    let rest = s.strip_prefix("file://")?;
    // `file://host/path`: only the local host.
    let rest = rest.strip_prefix("localhost").unwrap_or(rest);
    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(Ok(b)) = raw
                .get(i + 1..i + 3)
                .and_then(|h| std::str::from_utf8(h).ok())
                .map(|h| u8::from_str_radix(h, 16))
        {
            bytes.push(b);
            i += 3;
        } else {
            bytes.push(raw[i]);
            i += 1;
        }
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

/// The `file://` URI of an absolute path.
pub fn uri_of(path: &Path) -> Uri {
    let mut s = String::from("file://");
    for b in path.to_string_lossy().bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'-' | b'_' | b'.' | b'~' => {
                s.push(b as char)
            }
            b => s.push_str(&format!("%{b:02X}")),
        }
    }
    Uri::from_str(&s).expect("a file URI of a path")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_and_positions_agree() {
        let text = "ab\nc\u{e9}d\n\u{1f600}x\n";
        for at in [0, 1, 3, 4, 6, 7, 8, 12, 13] {
            assert_eq!(offset(text, position(text, at)), at, "at {at}");
        }
        // The emoji is two UTF-16 units.
        assert_eq!(position(text, 12), Position::new(2, 2));
    }

    #[test]
    fn uris_round_trip() {
        let p = Path::new("/tmp/a b/c%d.df");
        assert_eq!(path_of(&uri_of(p)).as_deref(), Some(p));
    }
}
