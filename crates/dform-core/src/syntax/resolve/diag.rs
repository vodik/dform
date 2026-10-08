//! The resolver's messages built from the tree alone: an unknown name
//! with the nearest name in scope, and a call's arguments as written.

use super::*;

/// The source text of a call's arguments, for a diagnostic.
pub(super) fn arg_texts(n: &SyntaxNode) -> Vec<String> {
    let mut out: Vec<String> = node(n, ARG_LIST)
        .map(|l| {
            l.children()
                .filter(|c| is_term(c.kind()) || c.kind() == NAMED_ARG)
                .map(|c| c.text().to_string().trim().to_string())
                .collect()
        })
        .unwrap_or_default();
    out.resize(out.len().max(5), String::new());
    out
}

/// `unknown name `NAME``, its help the name in scope it is nearest (a
/// slip of the pen), else how a name gets a value; `quoted` is the
/// string it may have meant, a quick fix when `fix`.
pub(super) fn unknown_name<'a>(
    span: Span,
    name: &str,
    quoted: &str,
    fix: bool,
    near: impl IntoIterator<Item = &'a str>,
) -> Diagnostic {
    let help = match crate::diag::nearest(name, near) {
        Some(n) => format!("`{n}` is in scope; a string is quoted, {quoted}"),
        None => format!(
            "nothing in scope is named `{name}`: give it a value after `where` (`{name} in ..`, \
             `{name} = ..`), or quote a string, {quoted}"
        ),
    };
    let d = Diagnostic::error(span, format!("unknown name `{name}`")).with_help(help);
    match fix {
        true => d.with_fix(
            format!("quote it: {quoted}"),
            vec![(span, quoted.to_string())],
        ),
        false => d,
    }
}
