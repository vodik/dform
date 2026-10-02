//! The lexer (docs/grammar.md "Tokens"). Lossless: whitespace and comments
//! are tokens too, so the concatenated token texts are the file. Newlines
//! are in whitespace tokens; the parser decides where one ends a statement.
//! No token depends on the whitespace around it (H section 1, rule 3): `.`
//! is always a dot, `..` a range's (R-56), `/` always a slash.

use crate::syntax::SyntaxKind::{self, *};
use logos::Logos;

#[derive(Logos, Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    #[regex(r"[ \t\r\n\f]+")]
    Whitespace,
    #[regex(r"#[^\n]*", allow_greedy = true)]
    Comment,

    #[regex(r"[A-Za-z_][A-Za-z0-9_]*")]
    Ident,
    #[regex(r#""([^"\\]|\\.)*""#)]
    String,
    #[regex(r"[0-9]+")]
    Int,
    /// A quantity (R-66, R-62): a number with a unit adjacent (`1Gi`,
    /// `500m`, `1h30m`, `1.5Gi`), or a number with a fraction (`0.5`).
    #[regex(r"[0-9]+(\.[0-9]+)?[A-Za-z][A-Za-z0-9]*")]
    #[regex(r"[0-9]+\.[0-9]+")]
    Quantity,
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
    #[token("..")]
    Dot2,
    #[token("..=")]
    Dot2Eq,
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
        Tok::String => STRING,
        Tok::Int => INT,
        Tok::Quantity => QUANTITY,
        Tok::Rank => RANK,
        Tok::LParen => L_PAREN,
        Tok::RParen => R_PAREN,
        Tok::LBrace => L_BRACE,
        Tok::RBrace => R_BRACE,
        Tok::LBracket => L_BRACKET,
        Tok::RBracket => R_BRACKET,
        Tok::Comma => COMMA,
        Tok::Dot => DOT,
        Tok::Dot2 => DOT2,
        Tok::Dot2Eq => DOT2_EQ,
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

/// The keywords, each its token kind: the 18 a statement starts with,
/// the body words, the clause word `where` (R-1) and the literals (H
/// section 4).
pub const KEYWORDS: &[(&str, SyntaxKind)] = &[
    ("edition", EDITION_KW),
    ("provider", PROVIDER_KW),
    ("key", KEY_KW),
    ("type", TYPE_KW),
    ("decl", DECL_KW),
    ("extern", EXTERN_KW),
    ("input", INPUT_KW),
    ("output", OUTPUT_KW),
    ("let", LET_KW),
    ("set", SET_KW),
    ("component", COMPONENT_KW),
    ("instance", INSTANCE_KW),
    ("use", USE_KW),
    ("resource", RESOURCE_KW),
    ("settings", SETTINGS_KW),
    ("deny", DENY_KW),
    ("warn", WARN_KW),
    ("not", NOT_KW),
    ("in", IN_KW),
    ("has", HAS_KW),
    ("where", WHERE_KW),
    ("true", TRUE_KW),
    ("false", FALSE_KW),
];

/// Keywords are token kinds. An identifier whose text is one of these lexes
/// as the keyword; the parser takes one as a plain name wherever a name is
/// expected and the keyword's own construct is not.
pub fn keyword(text: &str) -> Option<SyntaxKind> {
    // `if` is no keyword but reserved: the clause word of an earlier
    // surface, lexed so the parser can print the `where` form (R-1).
    if text == "if" {
        return Some(IF_KW);
    }
    KEYWORDS.iter().find(|(w, _)| *w == text).map(|(_, k)| *k)
}

/// One token: its kind and its byte range in the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token {
    pub kind: SyntaxKind,
    pub start: usize,
    pub end: usize,
}

/// Every token of `src`, trivia included. Never fails: a character no token
/// starts with is an `ERROR_TOKEN` the parser reports.
pub fn lex(src: &str) -> Vec<Token> {
    let mut lx = Tok::lexer(src);
    let mut out = Vec::new();
    while let Some(t) = lx.next() {
        let r = lx.span();
        let kind = match t {
            Ok(Tok::Ident) => keyword(&src[r.clone()]).unwrap_or(IDENT),
            Ok(t) => kind(t),
            Err(()) => ERROR_TOKEN,
        };
        out.push(Token {
            kind,
            start: r.start,
            end: r.end,
        });
    }
    out
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
    fn a_dot_and_a_slash_are_themselves_whatever_the_spaces() {
        assert_eq!(
            kinds("vpc.cidr vpc .cidr a/b a / b"),
            vec![
                IDENT, DOT, IDENT, IDENT, DOT, IDENT, IDENT, SLASH, IDENT, IDENT, SLASH, IDENT
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
            kinds("resource resources where wheres if iff not in inet when for some"),
            vec![
                RESOURCE_KW,
                IDENT,
                WHERE_KW,
                IDENT,
                IF_KW,
                IDENT,
                NOT_KW,
                IN_KW,
                IDENT,
                IDENT,
                IDENT,
                IDENT
            ]
        );
    }

    #[test]
    fn a_range_is_two_dots() {
        assert_eq!(
            kinds("0..3 0..=n a.b"),
            vec![INT, DOT2, INT, INT, DOT2_EQ, IDENT, IDENT, DOT, IDENT]
        );
    }

    #[test]
    fn a_number_with_a_unit_is_one_quantity() {
        assert_eq!(
            kinds("1Gi 500m 1h30m 1.5Gi 0.5 2 0..3 x.y 20GB"),
            vec![
                QUANTITY, QUANTITY, QUANTITY, QUANTITY, QUANTITY, INT, INT, DOT2, INT, IDENT, DOT,
                IDENT, QUANTITY
            ]
        );
    }

    #[test]
    fn a_comment_is_a_hash() {
        assert_eq!(
            kinds("a # b\nc // d"),
            vec![IDENT, IDENT, SLASH, SLASH, IDENT]
        );
    }

    #[test]
    fn lossless() {
        let src = "p(a, \"b${x}\") where q(x), # c\n  x != 1\n\u{1F600}";
        let toks = lex(src);
        let text: String = toks.iter().map(|t| &src[t.start..t.end]).collect();
        assert_eq!(text, src);
        assert_eq!(toks.last().unwrap().kind, ERROR_TOKEN);
    }
}
