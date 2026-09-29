//! Signature help (`textDocument/signatureHelp`): inside the parentheses of
//! a call of a builtin (`inet_subnet(`) or of an extern the project
//! declares (`dns.lookup(`, or its lookup `dns.lookup[`), the call's
//! signature and the argument the cursor is in. Builtins are
//! `engine::REFERENCE`'s; an extern's parameters are its declaration's,
//! its documentation its doc comment. Read off the tokens, so a call being
//! typed (no `)` yet) has one too.

use dform_core::engine::{self, RefKind};
use dform_core::lexer;
use dform_core::syntax::doc;
use dform_core::syntax::{SyntaxKind, SyntaxNode};
use lsp_types::{
    Documentation, MarkupContent, MarkupKind, ParameterInformation, ParameterLabel, SignatureHelp,
    SignatureInformation,
};

/// The call `text` has open at byte `at`: its callee, whether it is a
/// lookup (`[`), and how many of its arguments come before `at`.
fn open_call(text: &str, at: usize) -> Option<(String, bool, u32)> {
    use SyntaxKind::*;
    let toks: Vec<lexer::Token> = lexer::lex(&text[..at.min(text.len())])
        .into_iter()
        .filter(|t| !t.kind.is_trivia())
        .collect();
    let mut depth = 0usize;
    let mut commas = 0u32;
    let mut i = toks.len();
    while i > 0 {
        i -= 1;
        match toks[i].kind {
            R_PAREN | R_BRACKET | R_BRACE => depth += 1,
            L_BRACE if depth == 0 => return None,
            L_PAREN | L_BRACKET if depth == 0 => {
                // The callee: names and dots glued before the bracket.
                let mut name = String::new();
                let mut end = toks[i].start;
                let mut j = i;
                while j > 0 {
                    let t = &toks[j - 1];
                    if t.end != end || !matches!(t.kind, IDENT | DOT) {
                        break;
                    }
                    name.insert_str(0, &text[t.start..t.end]);
                    end = t.start;
                    j -= 1;
                }
                if name.is_empty() || name.starts_with('.') {
                    return None;
                }
                return Some((name, toks[i].kind == L_BRACKET, commas));
            }
            L_PAREN | L_BRACKET | L_BRACE => depth -= 1,
            COMMA if depth == 0 => commas += 1,
            _ => {}
        }
    }
    None
}

/// The parameters of a signature `name(a, b: t, ...) -> r`: the text
/// between its outermost parentheses, split at the top-level commas.
fn params(signature: &str) -> Vec<String> {
    let Some(open) = signature.find('(') else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut depth = 0;
    let mut cur = String::new();
    for c in signature[open + 1..].chars() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' if depth == 0 => break,
            ')' | ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn markdown(value: String) -> Documentation {
    Documentation::MarkupContent(MarkupContent {
        kind: MarkupKind::Markdown,
        value,
    })
}

/// An extern the trees declare: `name(+a: t, -b: t)` and its doc
/// comment's description.
fn extern_decl(trees: &[SyntaxNode], name: &str) -> Option<(String, Option<String>)> {
    let n = trees
        .iter()
        .flat_map(|t| t.descendants())
        .filter(|n| n.kind() == SyntaxKind::EXTERN)
        .find(|n| doc::item(n).is_some_and(|(_, x)| x == name))?;
    let args: Vec<String> = n
        .children()
        .filter(|c| c.kind() == SyntaxKind::BIND_ARG)
        .map(|b| b.text().to_string())
        .collect();
    let description = doc::comment(&n).and_then(|(_, ps)| {
        ps.into_iter()
            .find(|(k, _)| k == "description")
            .map(|(_, v)| v)
    });
    Some((format!("{name}({})", args.join(", ")), description))
}

/// The signature help at byte `at` of `text`; `trees` are the project's
/// files, for its externs.
pub fn help(text: &str, at: usize, trees: &[SyntaxNode]) -> Option<SignatureHelp> {
    let (name, _lookup, commas) = open_call(text, at)?;
    let (label, documentation) = match extern_decl(trees, &name) {
        Some((label, description)) => (label, description.map(markdown)),
        None => {
            let r = engine::reference(&name, true).filter(|r| r.kind != RefKind::Keyword)?;
            let doc = format!("{}\n\n```dform\n{}\n```", r.summary, r.example);
            (r.signature.to_string(), Some(markdown(doc)))
        }
    };
    let ps = params(&label);
    // A variadic call's arguments past its last parameter are the one
    // before `...`.
    let mut active = commas;
    if ps.last().is_some_and(|p| p == "...") && active as usize >= ps.len() - 1 {
        active = ps.len().saturating_sub(2) as u32;
    }
    let parameters = ps
        .iter()
        .map(|p| ParameterInformation {
            label: ParameterLabel::Simple(p.clone()),
            documentation: None,
        })
        .collect();
    Some(SignatureHelp {
        signatures: vec![SignatureInformation {
            label,
            documentation,
            parameters: Some(parameters),
            active_parameter: Some(active),
        }],
        active_signature: Some(0),
        active_parameter: Some(active),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_open_call_and_its_argument() {
        let text = "x = inet_subnet(vpc.cidr, f(a, b), ";
        assert_eq!(
            open_call(text, text.len()),
            Some(("inet_subnet".into(), false, 2))
        );
        let text = "y = dns.lookup[\"a\"";
        assert_eq!(
            open_call(text, text.len()),
            Some(("dns.lookup".into(), true, 0))
        );
        assert_eq!(open_call("p(a) if q(b)", 12), None);
        assert_eq!(open_call("r { a = ", 8), None);
    }

    #[test]
    fn parameters_split_at_top_level_commas() {
        assert_eq!(
            params("format(template: string, value: any, ...) -> string"),
            vec!["template: string", "value: any", "..."]
        );
        assert_eq!(
            params("split(text: string, sep: string) -> list(string)"),
            vec!["text: string", "sep: string"]
        );
    }
}
