//! Names as the tree writes them: a dotted path's words, a `use`'s path
//! and the name it binds, and a variable's spelling in the core and back.

use super::*;

/// The name a term is when it is one word and nothing else: `env`.
pub(super) fn bare_name(t: &SyntaxNode) -> Option<String> {
    let mut ws = t
        .descendants_with_tokens()
        .filter_map(|e| e.into_token())
        .filter(|k| !k.kind().is_trivia());
    match (ws.next(), ws.next()) {
        (Some(w), None) if w.kind() == IDENT => Some(w.text().to_string()),
        _ => None,
    }
}

/// The first word token of a node after `skip` others.
pub(super) fn word_text(n: &SyntaxNode, skip: usize) -> String {
    tokens(n)
        .filter(|t| t.kind().is_word())
        .nth(skip)
        .map(|t| t.text().to_string())
        .unwrap_or_default()
}

/// `a.b.c` from the leading words and dots of a node: a type, an extern,
/// a stack name.
pub(super) fn dotted_text(n: &SyntaxNode, skip_words: usize) -> String {
    let mut out = String::new();
    let mut words = 0;
    let mut started = false;
    for t in tokens(n) {
        if t.kind().is_word() {
            if words >= skip_words {
                if started && !out.ends_with('.') {
                    break;
                }
                out.push_str(t.text());
                started = true;
            }
            words += 1;
        } else if t.kind() == DOT && started {
            out.push('.');
        } else if started {
            break;
        }
    }
    out
}

/// The path of a `use` or an `instance` as written (`modules.net.vpc`),
/// its segments' tokens, and the word after it (an instance's name, or a
/// use's `as`).
pub(super) fn path_parts(n: &SyntaxNode) -> (Vec<SyntaxToken>, Vec<SyntaxToken>) {
    let mut path = Vec::new();
    let mut rest = Vec::new();
    let mut dot = true;
    for t in tokens(n).skip(1) {
        match t.kind() {
            DOT if !dot && rest.is_empty() => dot = true,
            k if k.is_word() && dot && rest.is_empty() => {
                path.push(t);
                dot = false;
            }
            k if k.is_word() => rest.push(t),
            _ => break,
        }
    }
    (path, rest)
}

/// A `use` statement (R-65): its path as written, and the name it binds,
/// the word after `as`, else the path's last segment.
pub fn use_parts(n: &SyntaxNode) -> (String, String) {
    let (path, rest) = path_parts(n);
    let text = path.iter().map(|t| t.text()).collect::<Vec<_>>().join(".");
    let name = match rest.as_slice() {
        [r#as, alias, ..] if r#as.text() == "as" => alias.text().to_string(),
        _ => path
            .last()
            .map(|t| t.text().to_string())
            .unwrap_or_default(),
    };
    (text, name)
}

/// `vpc_net` -> `VpcNet`, `_c` -> `_C`: a variable as the core prints it.
pub fn capitalise(s: &str) -> String {
    let lead = s.len() - s.trim_start_matches('_').len();
    let mut out = "_".repeat(lead);
    for part in s[lead..].split('_') {
        let mut cs = part.chars();
        if let Some(c) = cs.next() {
            out.extend(c.to_uppercase());
            out.push_str(cs.as_str());
        }
    }
    out
}

/// A core variable by the name the source gave it: `AvailabilityZone` is
/// `availability_zone` (`resolve::capitalise`, read backwards).
pub(crate) fn source_name(v: &str) -> String {
    let lead = v.len() - v.trim_start_matches('_').len();
    let mut out = v[..lead].to_string();
    for (i, c) in v[lead..].chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}
