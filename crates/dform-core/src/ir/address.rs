//! H-16: one spelling of an address. `T["A"]` is the source term that names
//! the resource `A` of type `T` (`A` the full address, a copy's scope
//! included: `edge/left/vpc`, R-72), and `.path` after it names an
//! attribute. `Display` for
//! [`Address`] is the one printer, [`parse`] the one reader: every address
//! the CLI prints or takes goes through them.

use super::Address;
use crate::syntax::SyntaxKind::{self, *};
use anyhow::{Result, anyhow};
use rowan::NodeOrToken;
use std::fmt;

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}[{}]", self.typ, string_literal(&self.name))
    }
}

impl Address {
    /// The attribute `path` of this address: `T["A"].tags.team`.
    pub fn attr(&self, path: &str) -> String {
        format!("{self}{}", path_suffix(path))
    }
}

/// The scope separator in an address (R-72): a copy's resource is
/// `scope/name`, a nested copy's `outer/inner/name`. A local name may not
/// contain it, so a name that does is already an address.
pub const SCOPE: char = '/';

/// The address of `name` in the scope `scope` (a copy's dotted path,
/// `edge.left`): `edge/left/name`; with `name` empty, the scope's prefix.
pub fn scoped(scope: &str, name: &str) -> String {
    let mut out: String = scope
        .chars()
        .map(|c| if c == '.' { SCOPE } else { c })
        .collect();
    out.push(SCOPE);
    out.push_str(name);
    out
}

/// `name` written with the old scope separator, `main::vpc`, as it is
/// written now (`main/vpc`); `None` when it has no `::`.
pub fn old_scope(name: &str) -> Option<String> {
    name.contains("::").then(|| name.replace("::", "/"))
}

/// An address written with `::` for `/` (R-72): [`parse`]'s error for
/// it, which a reader that takes other text too passes on rather than
/// trying the text as something else.
#[derive(Debug)]
pub struct OldScope {
    src: String,
    fixed: String,
}

impl fmt::Display for OldScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "'{}': a scope in an address is separated by `/`, not `::` (R-72): \"{}\"",
            self.src, self.fixed
        )
    }
}

impl std::error::Error for OldScope {}

/// Whether `name` is a scoped address, not a local name.
pub fn is_scoped(name: &str) -> bool {
    name.contains(SCOPE)
}

/// `s` as a source string literal, which reads back as `s`.
pub fn string_literal(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '{' => out.push_str("{{"),
            '}' => out.push_str("}}"),
            c if c.is_control() => out.push_str(&format!("\\u{{{:x}}}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The characters a stored path's segment cannot hold bare (R-77): a key
/// holding one is the segment quoted, `metadata.annotations."a.b/c"`.
const PATH_QUOTED: [char; 5] = ['.', '[', ']', '/', '"'];

/// `key` as one segment of a stored path: itself, or quoted as a source
/// string when it holds `.`, `[`, `]`, `/` or `"` (R-77).
pub fn path_key(key: &str) -> std::borrow::Cow<'_, str> {
    match key.contains(PATH_QUOTED) {
        true => string_literal(key).into(),
        false => key.into(),
    }
}

/// The stored path `path` with the key `key` after it: `a.b` and `c.d` are
/// `a.b."c.d"`.
pub fn path_join(path: &str, key: &str) -> String {
    match path.is_empty() {
        true => path_key(key).into_owned(),
        false => format!("{path}.{}", path_key(key)),
    }
}

/// A stored path's segments as written, each with its index suffix
/// (`containers[name=api]`): split at each `.` outside a quoted segment
/// and an index.
pub fn path_segments(path: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let (mut start, mut depth, mut quoted, mut escaped) = (0, 0usize, false, false);
    for (i, c) in path.char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' if quoted => escaped = true,
            '"' => quoted = !quoted,
            _ if quoted => {}
            '[' => depth += 1,
            ']' => depth = depth.saturating_sub(1),
            '.' if depth == 0 => {
                out.push(&path[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    out.push(&path[start..]);
    out
}

/// A stored segment's key: a quoted one unquoted (an index suffix stays).
pub fn segment_key(seg: &str) -> std::borrow::Cow<'_, str> {
    match segment_parts(seg) {
        (k, "") => k,
        (k, index) => format!("{k}{index}").into(),
    }
}

/// A stored segment's key, unquoted, and its index suffix: `"a.b"[0]` is
/// `a.b` and `[0]`, `tags` is `tags` and nothing.
pub fn segment_parts(seg: &str) -> (std::borrow::Cow<'_, str>, &str) {
    if seg.starts_with('"') {
        let end = quote_end(seg);
        if let Some(k) = constant(&seg[..end]) {
            return (k.into(), &seg[end..]);
        }
    }
    let i = seg.find('[').unwrap_or(seg.len());
    (seg[..i].into(), &seg[i..])
}

/// The byte just past the closing quote of the quoted segment `seg`.
fn quote_end(seg: &str) -> usize {
    let mut escaped = false;
    for (i, c) in seg.char_indices().skip(1) {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return i + 1,
            _ => {}
        }
    }
    seg.len()
}

/// A stored path's keys, unquoted: `a."b.c"` is `a`, `b.c`.
pub fn path_keys(path: &str) -> Vec<String> {
    path_segments(path)
        .into_iter()
        .map(|s| segment_key(s).into_owned())
        .collect()
}

/// A stored path's first segment and the rest: `a."b.c".d` is `a` and
/// `"b.c".d`.
pub fn path_split_first(path: &str) -> Option<(&str, &str)> {
    let first = path_segments(path).into_iter().next()?;
    (first.len() < path.len()).then(|| (first, &path[first.len() + 1..]))
}

/// A stored path's parent and its last segment: `a."b.c"` is `a` and
/// `"b.c"`.
pub fn path_split_last(path: &str) -> Option<(&str, &str)> {
    let last = path_segments(path).into_iter().next_back()?;
    (last.len() < path.len()).then(|| (&path[..path.len() - last.len() - 1], last))
}

/// A stored path (`tags.team`, `subnet_ids[0]`) as member access after an
/// address: `.tags.team`, `.subnet_ids[0]`, `."a-b"` for a segment that is
/// not a name, a quoted one as stored (`."a.b/c"`).
pub fn path_suffix(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    for seg in path_segments(path) {
        out.push('.');
        if seg.starts_with('"') {
            out.push_str(seg);
            continue;
        }
        let base = seg.find('[').map_or(seg, |i| &seg[..i]);
        let index = &seg[base.len()..];
        let indexes = index.split_inclusive(']').all(|x| {
            x.len() > 2
                && x.starts_with('[')
                && x[1..x.len() - 1].bytes().all(|b| b.is_ascii_digit())
        });
        if crate::lexer::is_word(base) && (index.is_empty() || indexes) {
            out.push_str(seg);
        } else {
            out.push_str(&string_literal(seg));
        }
    }
    out
}

/// A null's or a secret's label `T/A#P` as the attribute it stands for:
/// `T["A"].P`, or `T.P` for a label with no address (an input's, an
/// output's). Another deployment's output is read as written,
/// `platform[env=prod].P` (R-73). A resource's identity
/// (`schema::IDENTITY`) is the resource itself, unknown until it exists:
/// `T["A"]` (R-43). Anything else is its own text. The label itself is
/// internal (a Skolem name in the state) and never printed.
pub fn label(l: &str) -> String {
    match crate::value::null_parts(l) {
        Some((typ, name, path)) if name.is_empty() => format!("{typ}{}", path_suffix(&path)),
        Some((typ, name, path)) if typ == crate::transform::OUTPUT => format!("{name}.{path}"),
        Some((typ, name, path)) if path == crate::schema::IDENTITY => {
            Address { typ, name }.to_string()
        }
        Some((typ, name, path)) => Address { typ, name }.attr(&path),
        None => l.to_string(),
    }
}

/// A string literal's value when it is a constant (no interpolation).
fn constant(text: &str) -> Option<String> {
    let s = crate::syntax::resolve::unescape(text).ok()?;
    let mut out = String::new();
    let mut cs = s.chars().peekable();
    while let Some(c) = cs.next() {
        match c {
            '{' | '}' if cs.peek() == Some(&c) => {
                cs.next();
                out.push(c);
            }
            '{' | '}' => return None,
            c => out.push(c),
        }
    }
    Some(out)
}

/// Read an address the way `Display` prints it, `T["A"]`, with an optional
/// attribute path after it (`T["A"].tags.team`, `T["A"].subnet_ids[0]`),
/// by the term parser: the text must be one chain of that shape.
pub fn parse(src: &str) -> Result<(Address, Option<String>)> {
    let bad = || {
        anyhow!(
            "expected an address such as 'net.vpc[\"main\"]', optionally with an \
             attribute path ('net.vpc[\"main\"].cidr'), got '{src}'"
        )
    };
    let parse = crate::syntax::parser::parse_term(src.trim());
    if !parse.errors.is_empty() {
        return Err(bad());
    }
    let root = parse.syntax();
    let mut nodes = root
        .children()
        .filter(|n| !(n.kind() == ERROR && n.text_range().is_empty()));
    let (Some(chain), None) = (nodes.next(), nodes.next()) else {
        return Err(bad());
    };
    if chain.kind() != CHAIN {
        return Err(bad());
    }
    let word = |k: SyntaxKind| k == IDENT || k.is_keyword();
    let (mut typ, mut name, mut path) = (String::new(), None::<String>, String::new());
    let mut dot = false;
    for el in chain.children_with_tokens() {
        match el {
            NodeOrToken::Token(t) if t.kind() == DOT => {
                if dot || (name.is_none() && typ.is_empty()) {
                    return Err(bad());
                }
                dot = true;
            }
            NodeOrToken::Token(t) if name.is_none() && word(t.kind()) => {
                if !typ.is_empty() {
                    typ.push('.');
                }
                typ.push_str(t.text());
                dot = false;
            }
            NodeOrToken::Token(t) if name.is_some() && dot => {
                let seg = match t.kind() {
                    STRING => constant(t.text())
                        .filter(|s| !s.is_empty())
                        .map(|s| path_key(&s).into_owned()),
                    k if word(k) => Some(t.text().to_string()),
                    _ => None,
                }
                .ok_or_else(bad)?;
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(&seg);
                dot = false;
            }
            NodeOrToken::Node(n) if n.kind() == INDEX && !dot && !typ.is_empty() => {
                let inner: Vec<_> = n
                    .descendants_with_tokens()
                    .filter_map(|e| e.into_token())
                    .filter(|t| !t.kind().is_trivia() && !matches!(t.kind(), L_BRACKET | R_BRACKET))
                    .collect();
                let [t] = inner.as_slice() else {
                    return Err(bad());
                };
                match (&name, t.kind()) {
                    (None, STRING) => {
                        let n = constant(t.text()).ok_or_else(bad)?;
                        if let Some(fixed) = old_scope(&n) {
                            return Err(OldScope {
                                src: src.to_string(),
                                fixed,
                            }
                            .into());
                        }
                        name = Some(n);
                    }
                    (Some(_), INT) if !path.is_empty() => {
                        path.push('[');
                        path.push_str(t.text());
                        path.push(']');
                    }
                    _ => return Err(bad()),
                }
            }
            _ => return Err(bad()),
        }
    }
    let (Some(name), false) = (name, dot) else {
        return Err(bad());
    };
    Ok((Address { typ, name }, (!path.is_empty()).then_some(path)))
}

/// An address with no attribute path.
pub fn parse_resource(src: &str) -> Result<Address> {
    match parse(src)? {
        (a, None) => Ok(a),
        (_, Some(_)) => Err(anyhow!(
            "expected a resource's address, not an attribute: '{src}'"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(t: &str, n: &str) -> Address {
        Address {
            typ: t.into(),
            name: n.into(),
        }
    }

    /// A quoted segment is one key (R-77): split, joined and printed whole.
    #[test]
    fn a_quoted_segment_is_one_key() {
        let p = path_join("metadata.annotations", "a.b/c");
        assert_eq!(p, r#"metadata.annotations."a.b/c""#);
        assert_eq!(path_join("labels", "app"), "labels.app");
        assert_eq!(
            path_segments(r#"a."b.c"[0].d[name=x.y]"#),
            [r#"a"#, r#""b.c"[0]"#, "d[name=x.y]"]
        );
        assert_eq!(path_keys(&p), ["metadata", "annotations", "a.b/c"]);
        assert_eq!(segment_parts(r#""b.c"[0]"#), ("b.c".into(), "[0]"));
        assert_eq!(
            path_split_first(&p),
            Some(("metadata", r#"annotations."a.b/c""#))
        );
        assert_eq!(
            path_split_last(&p),
            Some(("metadata.annotations", r#""a.b/c""#))
        );
        assert_eq!(path_join("x", r#"q"t"#), r#"x."q\"t""#);
        assert_eq!(path_keys(r#"x."q\"t""#), ["x", r#"q"t"#]);
        assert_eq!(
            a("t", "x").attr(&p),
            r#"t["x"].metadata.annotations."a.b/c""#
        );
        let (_, parsed) = parse(r#"t["x"].metadata.annotations."a.b/c""#).unwrap();
        assert_eq!(parsed.as_deref(), Some(p.as_str()));
        let (_, parsed) = parse(r#"t["x"].labels."app-name""#).unwrap();
        assert_eq!(parsed.as_deref(), Some("labels.app-name"));
    }

    #[test]
    fn prints_as_source() {
        assert_eq!(
            a("net.vpc", "network/main/vpc").to_string(),
            r#"net.vpc["network/main/vpc"]"#
        );
        assert_eq!(a("t", "a\"{b}").to_string(), r#"t["a\"{{b}}"]"#);
        assert_eq!(
            a("net.subnet", "x").attr("tags.team"),
            r#"net.subnet["x"].tags.team"#
        );
        assert_eq!(a("t", "x").attr("subnet_ids[0]"), r#"t["x"].subnet_ids[0]"#);
        assert_eq!(
            a("t", "x").attr("labels.app-name"),
            r#"t["x"].labels."app-name""#
        );
        // A resource's identity is the resource (R-43).
        assert_eq!(
            label("net.vpc/network/main/vpc#id"),
            r#"net.vpc["network/main/vpc"]"#
        );
        assert_eq!(
            label("db.postgres/main#endpoint"),
            r#"db.postgres["main"].endpoint"#
        );
    }

    #[test]
    fn reads_what_it_prints() {
        for (addr, path) in [
            (a("net.vpc", "network/main/vpc"), None),
            (a("t", "a\"{b}/c"), None),
            (a("k8s.cluster", "x"), Some("tags.team")),
            (a("t", "x"), Some("subnet_ids[0]")),
            (a("t", "x"), Some("labels.app-name")),
            (a("t", "x"), Some("type")),
        ] {
            let text = match path {
                Some(p) => addr.attr(p),
                None => addr.to_string(),
            };
            assert_eq!(
                parse(&text).unwrap(),
                (addr, path.map(String::from)),
                "{text}"
            );
        }
    }

    /// A copy's address is `scope/name`, a nested copy's every scope in
    /// front (R-72); `::` is the old separator, refused naming `/`.
    #[test]
    fn a_scope_is_separated_by_a_slash() {
        assert_eq!(scoped("edge.left", "vpc"), "edge/left/vpc");
        assert_eq!(scoped("blue", ""), "blue/");
        assert!(is_scoped("blue/vpc") && !is_scoped("vpc"));
        let e = parse(r#"aws.vpc["blue::vpc"]"#).unwrap_err().to_string();
        assert!(e.contains("not `::`") && e.contains(r#""blue/vpc""#), "{e}");
    }

    #[test]
    fn refuses_anything_else() {
        for s in [
            "net.vpc/x",
            "net.vpc.x",
            "net.vpc[x]",
            r#"net.vpc["a{x}"]"#,
            r#"net.vpc["a"]."#,
            r#"net.vpc["a"][0]"#,
            r#"["a"]"#,
            r#"net.vpc["a"] b"#,
            r#"want(net.vpc, "a")"#,
        ] {
            assert!(parse(s).is_err(), "{s}");
        }
    }
}
