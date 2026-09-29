//! The lexer (docs/grammar.md "Tokens"). Lossless: whitespace and comments
//! are tokens too, so the concatenated token texts are the file. Newlines
//! are in whitespace tokens; the parser decides where one ends a statement.

use crate::syntax::SyntaxKind::{self, *};
use logos::Logos;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    #[regex(r"[ \t\r\n\f]+")]
    Whitespace,
    #[regex(r"(#|//)[^\n]*", allow_greedy = true)]
    Comment,

    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    #[regex(r#"\.([A-Za-z_][A-Za-z0-9_]*|"([^"\\\n]|\\.)*")(\.([A-Za-z_][A-Za-z0-9_]*|"([^"\\\n]|\\.)*")|\[[0-9]+\])*"#)]
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

/// The keywords, each its token kind.
pub const KEYWORDS: &[(&str, SyntaxKind)] = &[
    ("edition", EDITION_KW),
    ("provider", PROVIDER_KW),
    ("stack", STACK_KW),
    ("import", IMPORT_KW),
    ("input", INPUT_KW),
    ("output", OUTPUT_KW),
    ("export", EXPORT_KW),
    ("contributes", CONTRIBUTES_KW),
    ("module", MODULE_KW),
    ("instance", INSTANCE_KW),
    ("policy", POLICY_KW),
    ("apply", APPLY_KW),
    ("resource", RESOURCE_KW),
    ("settings", SETTINGS_KW),
    ("scenario", SCENARIO_KW),
    ("extern", EXTERN_KW),
    ("type", TYPE_KW),
    ("decl", DECL_KW),
    ("when", WHEN_KW),
    ("not", NOT_KW),
    ("in", IN_KW),
    ("exists", EXISTS_KW),
    ("true", TRUE_KW),
    ("false", FALSE_KW),
    ("null", NULL_KW),
    ("persist", PERSIST_KW),
    ("where", WHERE_KW),
    ("if", IF_KW),
    ("for", FOR_KW),
    ("let", LET_KW),
    ("has", HAS_KW),
    ("some", SOME_KW),
    ("with", WITH_KW),
    ("deny", DENY_KW),
    ("warn", WARN_KW),
    ("constraint", CONSTRAINT_KW),
];

/// Keywords are token kinds. An identifier whose text is one of these lexes
/// as the keyword; the parser takes one as a plain name wherever a name is
/// expected and the keyword's own construct is not.
pub fn keyword(text: &str) -> Option<SyntaxKind> {
    KEYWORDS.iter().find(|(w, _)| *w == text).map(|(_, k)| *k)
}

/// One token: its kind and its byte range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub start: usize,
    pub end: usize,
}

/// Tokens after which a `.` with no whitespace before it is member access
/// (`vpc.cidr`, `f[0].x`, `."a-b".c`) rather than the start of a keypath.
fn joins(k: SyntaxKind) -> bool {
    matches!(k, IDENT | R_PAREN | R_BRACKET | STRING) || k.is_keyword()
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
                Ok(Tok::Path) if out.last().is_some_and(|p| p.end == start && joins(p.kind)) => {
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
    fn case_decides_nothing() {
        assert_eq!(
            kinds("net X _ _env x vpc_net"),
            vec![IDENT, IDENT, IDENT, IDENT, IDENT, IDENT]
        );
    }

    #[test]
    fn a_dot_after_a_name_is_member_access_and_a_keypath_otherwise() {
        assert_eq!(
            kinds("vpc.cidr .tags.team (.a) m.i/n"),
            vec![
                IDENT, DOT, IDENT, PATH, L_PAREN, PATH, R_PAREN, IDENT, DOT, IDENT, SLASH, IDENT
            ]
        );
        assert_eq!(
            kinds("cfg.gke.\"pngu-grpc\".type f[0].x"),
            vec![
                IDENT, DOT, IDENT, DOT, STRING, DOT, TYPE_KW, IDENT, L_BRACKET, INT, R_BRACKET,
                DOT, IDENT
            ]
        );
    }

    #[test]
    fn keywords_are_kinds_but_longer_names_are_not() {
        assert_eq!(
            kinds("resource resources if iff not in inet"),
            vec![RESOURCE_KW, IDENT, IF_KW, IDENT, NOT_KW, IN_KW, IDENT]
        );
    }

    #[test]
    fn lossless() {
        let src = "p(a, \"b{x}\") if q(x), # c\n  x != 1\n\u{1F600}";
        let toks = lex(src);
        let text: String = toks.iter().map(|t| &src[t.start..t.end]).collect();
        assert_eq!(text, src);
        assert_eq!(toks.last().unwrap().kind, ERROR_TOKEN);
    }
}
