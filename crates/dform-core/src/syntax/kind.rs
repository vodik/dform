//! Token and node kinds of the lossless syntax tree (docs/grammar.md).

/// Every token the lexer produces and every node the parser builds. Tokens
/// come first; `is_trivia` and `is_keyword` rely on the order.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u16)]
pub enum SyntaxKind {
    // Trivia.
    WHITESPACE = 0,
    COMMENT,
    // Names and literals.
    IDENT,
    STRING,
    INT,
    /// A number with a unit, or with a fraction (R-66): `1Gi`, `500m`, `0.5`.
    QUANTITY,
    RANK,
    // Punctuation.
    L_PAREN,
    R_PAREN,
    L_BRACE,
    R_BRACE,
    L_BRACKET,
    R_BRACKET,
    COMMA,
    DOT,
    COLON,
    /// `:-`: lexed only so an old rule is told it is spelled `where`.
    NECK,
    EQ,
    EQ2,
    PLUS_EQ,
    NEQ,
    LT,
    LE,
    GT,
    GE,
    PLUS,
    MINUS,
    STAR,
    SLASH,
    PERCENT,
    PIPE,
    /// `..` and `..=`: a range's ends (R-56).
    DOT2,
    DOT2_EQ,
    // Keywords: the statement keywords (a statement's first token), then
    // the body words, the clause word and the literals.
    KEY_KW,
    TYPE_KW,
    DECL_KW,
    EXTERN_KW,
    INPUT_KW,
    OUTPUT_KW,
    LET_KW,
    SET_KW,
    COMPONENT_KW,
    USE_KW,
    RESOURCE_KW,
    SETTINGS_KW,
    DENY_KW,
    WARN_KW,
    NOT_KW,
    IN_KW,
    HAS_KW,
    /// `where`: the clause word, after the head (R-1).
    WHERE_KW,
    /// `if`: reserved, so that the clause word of an earlier surface is an
    /// error that prints the `where` form.
    IF_KW,
    TRUE_KW,
    FALSE_KW,
    /// A character no token starts with.
    ERROR_TOKEN,
    /// The end of a line where a newline ends what is being parsed: the
    /// parser's lookahead only, never in the tree.
    NEWLINE,

    // Nodes.
    SOURCE_FILE,
    /// Tokens skipped by error recovery.
    ERROR,
    /// `edition N`, which is gone (R-68): parsed so that the error names
    /// dform.toml and `fmt` drops it.
    EDITION,
    /// `input k: T [= t] [check B]`, or `key k: T [= t] [check B]`.
    INPUT,
    /// `input p(cols) from FORMAT(SOURCE)`: a relation fed from outside.
    INPUT_RELATION,
    /// `output k [: T] = t [where B]`.
    OUTPUT_DECL,
    EXTERN,
    /// An extern's `+name: T`, a column `name [: T]`.
    BIND_ARG,
    TYPE_DECL,
    ATTR_DECL,
    TYPE_EXPR,
    DECL,
    /// `component NAME { stmt* }`: a component declared as an item.
    COMPONENT,
    /// `use PATH [as NAME] [where B]`.
    USE,
    RESOURCE,
    /// `{ entry* }` of a resource or a use.
    BLOCK,
    /// `path (=|+=) term [rank]` in a block.
    ASSIGN,
    BLOCK_PATH,
    /// `{ stmt* }` of a component.
    STMT_BLOCK,
    RULE,
    FACT,
    BODY,
    /// `check body` after an input's or a type attribute's type: a
    /// refinement.
    REFINEMENT,
    LIT_ATOM,
    LIT_NOT,
    LIT_CMP,
    LIT_IN,
    LIT_NOT_IN,
    ARG_LIST,
    /// `name: term` in an argument list.
    NAMED_ARG,
    // Terms.
    LITERAL,
    CALL,
    LIST,
    OBJECT,
    OBJECT_FIELD,
    /// `..term` leading an object's field or a list's element (R-199): a
    /// spread in a literal, the rest `..name` in an object pattern.
    SPREAD,
    COMPREHENSION,
    PAREN,
    /// `(a, b, ..)`: a tuple pattern (R-58), after `in`, on the left of
    /// `=` and as a relation's argument.
    TUPLE,
    BIN_EXPR,
    UNARY_EXPR,
    /// `lo..hi` or `lo..=hi`: a range, enumerated by `in` (R-56).
    RANGE,
    /// `name (.seg | [terms])*`, parsed unresolved.
    CHAIN,
    /// A call and the `.seg` and `[t]` after it: `f(x).p[0]` (R-71). The
    /// call is its first child; the rest is a chain's tail.
    CALL_CHAIN,
    /// `[t, ...]` after a chain.
    INDEX,
    /// `(.name | [_])+` after a `from` term: a path into a document (R-39).
    SELECTOR,
    /// `where body` after a block: its clause.
    CLAUSE,
    /// `let k = t [where B]`; `let f(a, b) = t`, a let with parameters.
    LET,
    /// `(a, b: T, c = d)` after a let's name (R-187): its parameters.
    PARAMS,
    /// `name [: T] [= default]`: one parameter.
    PARAM,
    /// `set chain (=|+=) t [rank] [where B]`: a contribution.
    SET,
    /// `deny|warn "msg" [object] [where body]`.
    CHECK,
    /// A chain alone as a literal: a truth test.
    LIT_TRUTH,
    LIT_HAS,
    /// `not { body }`.
    LIT_NOT_BLOCK,
    /// `type NAME = TYPE`: a type alias.
    TYPE_ALIAS,
    /// `component { stmts }` after `type NAME =`: a component signature
    /// (R-104), the inputs and outputs a component that has it declares.
    SIGNATURE,
    __LAST,
}

use SyntaxKind::*;

impl SyntaxKind {
    pub fn is_trivia(self) -> bool {
        matches!(self, WHITESPACE | COMMENT)
    }

    /// A keyword a statement starts with (H section 4).
    pub fn is_stmt_keyword(self) -> bool {
        (KEY_KW as u16..=WARN_KW as u16).contains(&(self as u16))
    }

    pub fn is_keyword(self) -> bool {
        (KEY_KW as u16..=FALSE_KW as u16).contains(&(self as u16))
    }

    /// Any word: a name, or a keyword where a name stands (a key, a path
    /// segment, a declared name).
    pub fn is_word(self) -> bool {
        self == IDENT || self.is_keyword()
    }

    /// A comparison operator: `=`, `==`, `!=`, `<`, `<=`, `>`, `>=`.
    pub fn is_cmp(self) -> bool {
        matches!(self, EQ | EQ2 | NEQ | LT | LE | GT | GE)
    }

    /// How a token kind is named in "expected ..." diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            WHITESPACE => "whitespace",
            COMMENT => "a comment",
            IDENT => "a name",
            STRING => "a string",
            INT => "an integer",
            QUANTITY => "a quantity",
            RANK => "a rank",
            L_PAREN => "`(`",
            R_PAREN => "`)`",
            L_BRACE => "`{`",
            R_BRACE => "`}`",
            L_BRACKET => "`[`",
            R_BRACKET => "`]`",
            COMMA => "`,`",
            DOT => "`.`",
            COLON => "`:`",
            NECK => "`:-`",
            EQ => "`=`",
            EQ2 => "`==`",
            PLUS_EQ => "`+=`",
            NEQ => "`!=`",
            LT => "`<`",
            LE => "`<=`",
            GT => "`>`",
            GE => "`>=`",
            PLUS => "`+`",
            MINUS => "`-`",
            STAR => "`*`",
            SLASH => "`/`",
            PERCENT => "`%`",
            PIPE => "`|`",
            DOT2 => "`..`",
            DOT2_EQ => "`..=`",
            ERROR_TOKEN => "an unknown character",
            NEWLINE => "the end of the line",
            k if k.is_keyword() => "a keyword",
            _ => "a node",
        }
    }
}

impl From<SyntaxKind> for rowan::SyntaxKind {
    fn from(k: SyntaxKind) -> Self {
        Self(k as u16)
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lang {}

impl rowan::Language for Lang {
    type Kind = SyntaxKind;
    fn kind_from_raw(raw: rowan::SyntaxKind) -> SyntaxKind {
        assert!(raw.0 < __LAST as u16);
        // SAFETY: SyntaxKind is repr(u16) and raw.0 is in range.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(raw.0) }
    }
    fn kind_to_raw(kind: SyntaxKind) -> rowan::SyntaxKind {
        kind.into()
    }
}

pub type SyntaxNode = rowan::SyntaxNode<Lang>;
pub type SyntaxToken = rowan::SyntaxToken<Lang>;
pub type SyntaxElement = rowan::SyntaxElement<Lang>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_is_a_name_or_a_keyword_and_a_comparison_one_of_seven() {
        assert!(IDENT.is_word() && WHERE_KW.is_word() && !STRING.is_word());
        let cmp = [EQ, EQ2, NEQ, LT, LE, GT, GE];
        assert!(cmp.iter().all(|k| k.is_cmp()) && !DOT.is_cmp());
    }
}
