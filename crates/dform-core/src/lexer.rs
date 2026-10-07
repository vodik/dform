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
    /// A string may span lines (R-61), and `\` at a line end joins it
    /// with the next. A `${` in it opens a hole of code to its matching
    /// `}`, whose strings are lexed the same way (R-175): `string_end`.
    #[token("\"", string)]
    String,
    #[regex(r"[0-9]+")]
    Int,
    /// A quantity (R-66, R-62): a number with a unit adjacent (`1Gi`,
    /// `500m`, `1h30m`, `1.5Gi`), or a number with a fraction (`0.5`): a
    /// float (R-75), which a quantity's position reads as one
    /// (`types::ambiguous_literal`).
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

/// A string token: to its closing quote past every hole (`string_end`),
/// or, when a hole is never closed, to the next unescaped quote as
/// before holes nested, so the lowering reports the open `${` where it
/// was written and the rest of the file lexes as it did.
fn string(lx: &mut logos::Lexer<Tok>) -> bool {
    let rest = &lx.source().as_bytes()[lx.span().start..];
    match string_end(rest).or_else(|| plain_end(rest)) {
        Some(end) => {
            lx.bump(end - 1);
            true
        }
        None => false,
    }
}

/// The end of the string literal `src` starts with (`src[0]` is its
/// quote): the byte after its closing quote. An escape is `\` and the
/// character after it, `$${` is a literal `${`, and a `${` opens a hole
/// that runs to its matching `}` (`hole_end`). `None` when the string or
/// a hole in it is never closed.
pub fn string_end(src: &[u8]) -> Option<usize> {
    let mut i = 1;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            b'$' if src.get(i + 1) == Some(&b'$') && src.get(i + 2) == Some(&b'{') => i += 3,
            b'$' if src.get(i + 1) == Some(&b'{') => i = hole_end(src, i + 2)?,
            _ => i += 1,
        }
    }
    None
}

/// The end of the hole whose code starts at `src[at]`, just after its
/// `${`: the byte after the `}` that closes it. The code's braces nest,
/// and a string in it is skipped whole (`string_end`), its own holes and
/// braces included, so `"${f({ a: "}" })}"` is one hole (R-175).
pub fn hole_end(src: &[u8], at: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = at;
    while i < src.len() {
        match src[i] {
            b'"' => i += string_end(&src[i..])?,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' if depth == 0 => return Some(i + 1),
            b'}' => {
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The end of `src`'s string read to the next unescaped quote, holes
/// ignored.
fn plain_end(src: &[u8]) -> Option<usize> {
    let mut i = 1;
    while i < src.len() {
        match src[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
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

/// The keywords, each its token kind: the 14 a statement starts with,
/// the body words, the clause word `where` (R-1) and the literals (H
/// section 4).
pub const KEYWORDS: &[(&str, SyntaxKind)] = &[
    ("key", KEY_KW),
    ("type", TYPE_KW),
    ("decl", DECL_KW),
    ("extern", EXTERN_KW),
    ("input", INPUT_KW),
    ("output", OUTPUT_KW),
    ("let", LET_KW),
    ("set", SET_KW),
    ("component", COMPONENT_KW),
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

/// Whether `s` lexes as one word: an identifier or a keyword (ASCII
/// letters, digits and `_`, not starting with a digit).
pub fn is_word(s: &str) -> bool {
    let mut cs = s.chars();
    cs.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

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

    #[test]
    fn a_string_in_a_hole_is_the_holes() {
        assert_eq!(
            kinds("\"a${f(\"b${\"c\"}\", \"}\")}d\" x"),
            vec![STRING, IDENT]
        );
        assert_eq!(kinds("\"${ {a: \"x\"}.a }\n\" y"), vec![STRING, IDENT]);
        // A hole never closed: the string ends at the next quote, as
        // before holes nested, for the lowering to name the `${`.
        assert_eq!(kinds("\"${f(\" x"), vec![STRING, IDENT]);
    }

    #[test]
    fn a_word_is_an_identifier_or_a_keyword() {
        assert!(is_word("vpc_1") && is_word("_x") && is_word("resource"));
        assert!(!is_word("") && !is_word("1a") && !is_word("a-b") && !is_word("é"));
    }
}
