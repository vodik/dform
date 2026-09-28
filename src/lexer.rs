//! The lexer (E §6 "Tokens", docs/grammar.md). Lossless: whitespace and
//! comments are tokens too, so the concatenated token texts are the file.

use crate::syntax::SyntaxKind::{self, *};
use logos::Logos;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    #[regex(r"[ \t\r\n\f]+")]
    Whitespace,
    #[regex(r"(#|//)[^\n]*", allow_greedy = true)]
    Comment,

    #[regex(r"[a-z][A-Za-z0-9_]*")]
    Ident,
    #[regex(r"[A-Z][A-Za-z0-9_]*|_[A-Za-z0-9_]*")]
    Var,
    #[regex(r"[a-z][A-Za-z0-9_]*(\.[a-z_][A-Za-z0-9_]*)+")]
    QName,
    #[regex(r"[A-Z][A-Za-z0-9_]*(\.[a-z_][A-Za-z0-9_]*)+")]
    Field,
    #[regex(r#"\.([a-z_][A-Za-z0-9_]*|"([^"\\\n]|\\.)*")(\.([a-z_][A-Za-z0-9_]*|"([^"\\\n]|\\.)*")|\[[0-9]+\])*"#)]
    Path,
    #[regex(r#""([^"\\]|\\.)*""#)]
    String,
    #[regex(r"[0-9]+")]
    Int,
    #[regex(r"@[a-z_]+")]
    Rank,

    #[token("(")]
    LParen,
    #[token(")")]
    RParen,
    #[token("{")]
    LBrace,
    #[token("}")]
    RBrace,
    #[token("[")]
    LBracket,
    #[token("]")]
    RBracket,
    #[token(",")]
    Comma,
    #[token(".")]
    Dot,
    #[token(":")]
    Colon,
    #[token(":-")]
    Neck,
    #[token("=")]
    Eq,
    #[token("==")]
    Eq2,
    #[token("+=")]
    PlusEq,
    #[token("!=")]
    Neq,
    #[token("<")]
    Lt,
    #[token("<=")]
    Le,
    #[token(">")]
    Gt,
    #[token(">=")]
    Ge,
    #[token("+")]
    Plus,
    #[token("-")]
    Minus,
    #[token("*")]
    Star,
    #[token("/")]
    Slash,
    #[token("%")]
    Percent,
    #[token("|")]
    Pipe,
}

fn kind(t: Tok) -> SyntaxKind {
    match t {
        Tok::Whitespace => WHITESPACE,
        Tok::Comment => COMMENT,
        Tok::Ident => IDENT,
        Tok::Var => VAR,
        Tok::QName => QNAME,
        Tok::Field => FIELD,
        Tok::Path => PATH,
        Tok::String => STRING,
        Tok::Int => INT,
        Tok::Rank => RANK,
        Tok::LParen => L_PAREN,
        Tok::RParen => R_PAREN,
        Tok::LBrace => L_BRACE,
        Tok::RBrace => R_BRACE,
        Tok::LBracket => L_BRACKET,
        Tok::RBracket => R_BRACKET,
        Tok::Comma => COMMA,
        Tok::Dot => DOT,
        Tok::Colon => COLON,
        Tok::Neck => NECK,
        Tok::Eq => EQ,
        Tok::Eq2 => EQ2,
        Tok::PlusEq => PLUS_EQ,
        Tok::Neq => NEQ,
        Tok::Lt => LT,
        Tok::Le => LE,
        Tok::Gt => GT,
        Tok::Ge => GE,
        Tok::Plus => PLUS,
        Tok::Minus => MINUS,
        Tok::Star => STAR,
        Tok::Slash => SLASH,
        Tok::Percent => PERCENT,
        Tok::Pipe => PIPE,
    }
}

/// Keywords are token kinds (E §6). An identifier whose text is one of these
/// lexes as the keyword.
pub fn keyword(text: &str) -> Option<SyntaxKind> {
    Some(match text {
        "edition" => EDITION_KW,
        "provider" => PROVIDER_KW,
        "stack" => STACK_KW,
        "import" => IMPORT_KW,
        "input" => INPUT_KW,
        "output" => OUTPUT_KW,
        "export" => EXPORT_KW,
        "contributes" => CONTRIBUTES_KW,
        "module" => MODULE_KW,
        "instance" => INSTANCE_KW,
        "policy" => POLICY_KW,
        "apply" => APPLY_KW,
        "resource" => RESOURCE_KW,
        "settings" => SETTINGS_KW,
        "scenario" => SCENARIO_KW,
        "extern" => EXTERN_KW,
        "type" => TYPE_KW,
        "decl" => DECL_KW,
        "when" => WHEN_KW,
        "not" => NOT_KW,
        "in" => IN_KW,
        "exists" => EXISTS_KW,
        "collect_set" => COLLECT_SET_KW,
        "collect_list" => COLLECT_LIST_KW,
        "collect_ordered" => COLLECT_ORDERED_KW,
        "count" => COUNT_KW,
        "sum" => SUM_KW,
        "min" => MIN_KW,
        "max" => MAX_KW,
        "lub_ranked" => LUB_RANKED_KW,
        "allocate" => ALLOCATE_KW,
        "true" => TRUE_KW,
        "false" => FALSE_KW,
        "null" => NULL_KW,
        "secret" => SECRET_KW,
        "persist" => PERSIST_KW,
        "where" => WHERE_KW,
        "moved" => MOVED_KW,
        "adopt" => ADOPT_KW,
        "lifecycle" => LIFECYCLE_KW,
        "ignore_changes" => IGNORE_CHANGES_KW,
        _ => return None,
    })
}

/// One token: its kind and its byte range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub start: usize,
    pub end: usize,
}

/// Tokens after which a `.` with no whitespace before it ends a statement
/// rather than starting a keypath: `p(a).q(b).` is two statements.
fn closes(k: SyntaxKind) -> bool {
    matches!(k, R_PAREN | R_BRACE | R_BRACKET | INT | STRING)
}

/// Every token of `src`, trivia included. Never fails: a character no token
/// starts with is an `ERROR_TOKEN` the parser reports.
pub fn lex(src: &str) -> Vec<Token> {
    let mut out: Vec<Token> = Vec::new();
    let mut base = 0;
    'restart: loop {
        let mut lx = Tok::lexer(&src[base..]);
        while let Some(t) = lx.next() {
            let r = lx.span();
            let (start, end) = (base + r.start, base + r.end);
            let k = match t {
                Ok(Tok::Ident) => keyword(&src[start..end]).unwrap_or(IDENT),
                Ok(Tok::Path) if out.last().is_some_and(|p| p.end == start && closes(p.kind)) => {
                    out.push(Token {
                        kind: DOT,
                        start,
                        end: start + 1,
                    });
                    base = start + 1;
                    continue 'restart;
                }
                Ok(t) => kind(t),
                Err(()) => ERROR_TOKEN,
            };
            out.push(Token {
                kind: k,
                start,
                end,
            });
        }
        return out;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<SyntaxKind> {
        lex(src)
            .into_iter()
            .map(|t| t.kind)
            .filter(|k| !k.is_trivia())
            .collect()
    }

    #[test]
    fn names_by_case_and_shape() {
        assert_eq!(
            kinds("net.vpc X _ _Env X.cidr .tags.team .\"my-key\".x[0] a"),
            vec![QNAME, VAR, VAR, VAR, FIELD, PATH, PATH, IDENT]
        );
    }

    #[test]
    fn keywords_are_kinds_but_longer_names_are_not() {
        assert_eq!(
            kinds("resource resources apply_policy not in inet"),
            vec![RESOURCE_KW, IDENT, IDENT, NOT_KW, IN_KW, IDENT]
        );
    }

    #[test]
    fn minus_and_slash_are_operators() {
        assert_eq!(
            kinds("a-b a/b 10-1"),
            vec![IDENT, MINUS, IDENT, IDENT, SLASH, IDENT, INT, MINUS, INT]
        );
    }

    #[test]
    fn a_dot_after_a_closer_ends_the_statement() {
        assert_eq!(
            kinds("p(a).q(b)."),
            vec![
                IDENT, L_PAREN, IDENT, R_PAREN, DOT, IDENT, L_PAREN, IDENT, R_PAREN, DOT
            ]
        );
        assert_eq!(kinds("x(.a)"), vec![IDENT, L_PAREN, PATH, R_PAREN]);
    }

    #[test]
    fn lossless() {
        let src = "p(a, \"b\") :- q(X), # c\n  X != 1.\n\u{1F600}";
        let toks = lex(src);
        let text: String = toks.iter().map(|t| &src[t.start..t.end]).collect();
        assert_eq!(text, src);
        assert_eq!(toks.last().unwrap().kind, ERROR_TOKEN);
    }
}
